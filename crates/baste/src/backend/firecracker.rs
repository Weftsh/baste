//! Firecracker backend (Linux, and Windows through WSL2).
//!
//! Every job boots a fresh microVM from pinned upstream artifacts (the
//! Firecracker project's CI kernel and Ubuntu 24.04 squashfs, checked by
//! sha256), with copy-on-write layering and no root privileges at run time:
//!
//! - `/dev/vda`: the pinned squashfs, read-only
//! - `/dev/vdb`: the provisioned layer (tools, runner user), read-only, cached
//! - `/dev/vdc`: a sparse per-job ext4 that takes all writes, deleted after
//! - `/dev/vdd`: the job bundle as a raw tar
//!
//! A tiny initramfs whose `/init` is this binary stacks those with overlayfs,
//! injects the agent service and hands off to systemd. The agent streams
//! protocol events back over vsock. Networking uses tap devices created once
//! by `sudo baste setup-network`.

use super::{runner_arch, Backend, Check, JobLaunch};
use crate::config::Config;
use anyhow::{anyhow, bail, Context, Result};
use baste_protocol::{Event, RunnerInfo};
use serde_json::{json, Map};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const FIRECRACKER_VERSION: &str = "v1.12.1";
const ARTIFACTS: &str = "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.13";
const RELEASES: &str = "https://github.com/firecracker-microvm/firecracker/releases/download";

struct Pinned {
    kernel_sha: &'static str,
    rootfs_sha: &'static str,
    firecracker_sha: &'static str,
}

fn pinned(arch: &str) -> Option<Pinned> {
    match arch {
        "x86_64" => Some(Pinned {
            kernel_sha: "b36a4a1b10f33b9cfdcde3d1a787d9c090556a3edb211cd06d1f3f9a6c7e8724",
            rootfs_sha: "2332f3bb101fc8645c6322c7445acd353a51845a33e66b58c711905d685b2aa7",
            firecracker_sha: "0a75e67ef6e4c540a2cf248b06822b0be9820cbba9fe19f9e0321200fe76ff6b",
        }),
        "aarch64" => Some(Pinned {
            kernel_sha: "69aa3308219ec1a070bc9a8e7f80c3b34056fed8ae05efb44e55f73b31adde44",
            rootfs_sha: "4fd59b607fd17fd711835ffb95e8d14fc49c64bdf5e57d4ca6051dd9b2aa5091",
            firecracker_sha: "785bc9d30756bff0c03fb275baac957363f9ba98f78a2272405dcb6929ebb953",
        }),
        _ => None,
    }
}

/// Changes to provisioning produce a new prepared layer.
const PROVISION: &str = include_str!("provision.sh");

pub fn tap_name(slot: usize) -> String {
    format!("baste-tap{slot}")
}

fn slot_net(slot: usize) -> (String, String) {
    (format!("172.30.{slot}.1"), format!("172.30.{slot}.2"))
}

pub struct Firecracker {
    cpus: u32,
    memory_mb: u32,
    disk_gb: u32,
    slots: usize,
    arch: &'static str,
}

impl Firecracker {
    pub fn new(config: &Config) -> Self {
        Firecracker {
            cpus: config.vm_cpus(),
            memory_mb: config.vm_memory_mb(),
            disk_gb: config.disk_gb.max(8),
            slots: config.max_parallel_jobs.max(1),
            arch: std::env::consts::ARCH,
        }
    }

    fn dir(&self) -> PathBuf {
        crate::config::cache_dir()
            .join("firecracker")
            .join(self.arch)
    }

    fn pinned(&self) -> Result<Pinned> {
        pinned(self.arch).ok_or_else(|| {
            anyhow!(
                "Firecracker needs an x86_64 or aarch64 host, not {}",
                self.arch
            )
        })
    }

    fn kernel(&self) -> Result<PathBuf> {
        let p = self.pinned()?;
        let path = self
            .dir()
            .join(format!("vmlinux-6.1.141-{}", &p.kernel_sha[..12]));
        fetch(
            &format!("{ARTIFACTS}/{}/vmlinux-6.1.141", self.arch),
            &path,
            p.kernel_sha,
        )?;
        Ok(path)
    }

    fn rootfs(&self) -> Result<PathBuf> {
        let p = self.pinned()?;
        let path = self
            .dir()
            .join(format!("ubuntu-24.04-{}.squashfs", &p.rootfs_sha[..12]));
        fetch(
            &format!("{ARTIFACTS}/{}/ubuntu-24.04.squashfs", self.arch),
            &path,
            p.rootfs_sha,
        )?;
        Ok(path)
    }

    fn firecracker_bin(&self) -> Result<PathBuf> {
        if let Some(p) = std::env::var_os("BASTE_FIRECRACKER").filter(|p| !p.is_empty()) {
            return Ok(PathBuf::from(p));
        }
        let p = self.pinned()?;
        let path = self
            .dir()
            .join(format!("firecracker-{FIRECRACKER_VERSION}"));
        if path.is_file() {
            return Ok(path);
        }
        let tgz = self
            .dir()
            .join(format!("firecracker-{FIRECRACKER_VERSION}.tgz"));
        fetch(
            &format!(
                "{RELEASES}/{FIRECRACKER_VERSION}/firecracker-{FIRECRACKER_VERSION}-{}.tgz",
                self.arch
            ),
            &tgz,
            p.firecracker_sha,
        )?;
        let want = format!("firecracker-{FIRECRACKER_VERSION}-{}", self.arch);
        let mut archive =
            tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(&tgz)?));
        for entry in archive.entries()? {
            let mut entry = entry?;
            if entry
                .path()?
                .file_name()
                .is_some_and(|n| n.to_string_lossy() == want)
            {
                let tmp = path.with_extension("tmp");
                entry.unpack(&tmp)?;
                std::fs::rename(&tmp, &path)?;
                let _ = std::fs::remove_file(&tgz);
                return Ok(path);
            }
        }
        bail!("{want} not found in the Firecracker release")
    }

    /// The provisioned layer for the pinned rootfs, if prepared.
    fn prepared_path(&self) -> Result<PathBuf> {
        let p = self.pinned()?;
        let mut h = Sha256::new();
        h.update(p.rootfs_sha);
        h.update(PROVISION);
        let id = hex::encode(h.finalize());
        Ok(self.dir().join(format!("prepared-{}.ext4", &id[..16])))
    }

    /// A cpio initramfs whose `/init` is the (static) Linux agent binary.
    pub fn initramfs(&self) -> Result<PathBuf> {
        let agent = super::agent_bin::agent_binary(self.arch)?;
        let bytes =
            std::fs::read(&agent).with_context(|| format!("reading {}", agent.display()))?;
        let id = hex::encode(Sha256::digest(&bytes));
        let path = self.dir().join(format!("initramfs-{}.cpio", &id[..16]));
        if !path.is_file() {
            std::fs::create_dir_all(self.dir())?;
            let tmp = path.with_extension("tmp");
            let mut w = cpio::Writer::new(std::fs::File::create(&tmp)?);
            for d in ["dev", "proc", "sys", "lower", "prep", "upper", "newroot"] {
                w.dir(d)?;
            }
            w.char_device("dev/console", 5, 1)?;
            w.char_device("dev/null", 1, 3)?;
            w.file("init", 0o755, &bytes)?;
            w.finish()?;
            std::fs::rename(&tmp, &path)?;
        }
        Ok(path)
    }

    fn check_taps(&self) -> Vec<usize> {
        // SAFETY: plain syscall.
        let uid = unsafe { libc::getuid() };
        (0..self.slots)
            .filter(|s| {
                let owner =
                    std::fs::read_to_string(format!("/sys/class/net/{}/owner", tap_name(*s)));
                !matches!(owner.map(|o| o.trim().parse::<u32>().ok()), Ok(Some(o)) if o == uid)
            })
            .collect()
    }

    /// Boot a VM and stream its agent's events until it's done.
    #[allow(clippy::too_many_arguments)]
    fn boot(
        &self,
        slot: usize,
        mode: &str,
        prepared: Option<&Path>,
        upper: &Path,
        bundle_tar: &Path,
        launch_cancel: &Arc<AtomicBool>,
        on_event: &mut dyn FnMut(Event),
        log: &mut dyn FnMut(&str),
    ) -> Result<()> {
        let cancel = launch_cancel;
        let fc = self.firecracker_bin()?;
        let kernel = self.kernel()?;
        let rootfs = self.rootfs()?;
        let initrd = self.initramfs()?;
        // Unix socket paths are limited to ~108 bytes, so keep them short.
        let vm_dir = crate::config::state_dir().join("vm").join(slot.to_string());
        let _ = std::fs::remove_dir_all(&vm_dir);
        std::fs::create_dir_all(&vm_dir)?;
        let vsock = vm_dir.join("v.sock");
        let listener =
            UnixListener::bind(format!("{}_{}", vsock.display(), crate::guest::VSOCK_PORT))
                .context("listening for the guest on vsock")?;
        listener.set_nonblocking(true)?;

        let (host_ip, guest_ip) = slot_net(slot);
        let dns = host_dns().join(",");
        let mut drives = vec![
            json!({"drive_id": "base", "path_on_host": rootfs, "is_root_device": false, "is_read_only": true}),
        ];
        let mut letter = b'b';
        let dev = |l: &mut u8| {
            let d = format!("/dev/vd{}", *l as char);
            *l += 1;
            d
        };
        let prepared_dev = prepared.map(|p| {
            drives.push(json!({"drive_id": "prepared", "path_on_host": p, "is_root_device": false, "is_read_only": true}));
            dev(&mut letter)
        });
        drives.push(json!({"drive_id": "upper", "path_on_host": upper, "is_root_device": false, "is_read_only": false}));
        let upper_dev = dev(&mut letter);
        drives.push(json!({"drive_id": "bundle", "path_on_host": bundle_tar, "is_root_device": false, "is_read_only": true}));
        let bundle_dev = dev(&mut letter);
        let boot_args = format!(
            "console=ttyS0 reboot=k panic=1 pci=off quiet loglevel=3 ip={guest_ip}::{host_ip}:255.255.255.252::eth0:off baste.lower=/dev/vda{} baste.upper={upper_dev} baste.bundle={bundle_dev} baste.mode={mode} baste.dns={dns}",
            prepared_dev.map(|d| format!(" baste.prepared={d}")).unwrap_or_default()
        );
        let config = json!({
            "boot-source": {"kernel_image_path": kernel, "initrd_path": initrd, "boot_args": boot_args},
            "drives": drives,
            "machine-config": {"vcpu_count": self.cpus, "mem_size_mib": self.memory_mb, "smt": false},
            "network-interfaces": [{"iface_id": "eth0", "guest_mac": format!("AA:BA:57:E0:00:{slot:02X}"), "host_dev_name": tap_name(slot)}],
            "vsock": {"guest_cid": 3, "uds_path": vsock},
        });
        let config_path = vm_dir.join("vm.json");
        std::fs::write(&config_path, serde_json::to_vec_pretty(&config)?)?;
        let console_path = vm_dir.join("console.log");
        let console = std::fs::File::create(&console_path)?;
        log(&format!(
            "Booting Firecracker microVM ({} vCPU, {} MiB) on {}",
            self.cpus,
            self.memory_mb,
            tap_name(slot)
        ));
        let mut child = Command::new(&fc)
            .arg("--no-api")
            .arg("--config-file")
            .arg(&config_path)
            .arg("--level")
            .arg("Error")
            .current_dir(&vm_dir)
            .stdin(Stdio::null())
            .stdout(console.try_clone()?)
            .stderr(console)
            .spawn()
            .with_context(|| format!("starting {}", fc.display()))?;

        let started = Instant::now();
        let stream = loop {
            match listener.accept() {
                Ok((s, _)) => break Some(s),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    let _ = child.kill();
                    return Err(e).context("accepting the guest connection");
                }
            }
            if let Some(status) = child.try_wait()? {
                log(&format!("The VM exited during boot ({status})"));
                break None;
            }
            if cancel.load(Ordering::SeqCst) {
                break None;
            }
            if started.elapsed() > Duration::from_secs(180) {
                log("The VM didn't start the agent within 3 minutes");
                break None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let Some(stream) = stream else {
            let _ = child.kill();
            let _ = child.wait();
            for line in tail(&console_path, 30) {
                log(&format!("[console] {line}"));
            }
            if cancel.load(Ordering::SeqCst) {
                return Ok(());
            }
            bail!("the VM failed to boot (console output is in the setup log)");
        };
        log(&format!(
            "VM booted in {}",
            crate::ui::duration(started.elapsed().as_millis() as i64)
        ));
        stream.set_nonblocking(false)?;

        // Kill the VM on cancel; that ends the event stream.
        let pid = child.id() as i32;
        let done = Arc::new(AtomicBool::new(false));
        let watch_done = done.clone();
        let cancel = cancel.clone();
        let watcher = std::thread::spawn(move || {
            while !watch_done.load(Ordering::SeqCst) {
                if cancel.load(Ordering::SeqCst) {
                    // SAFETY: plain syscall.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let finished = super::pump_events(stream, on_event, log);
        done.store(true, Ordering::SeqCst);
        let _ = watcher.join();
        let cancel = launch_cancel;

        // Give the guest a moment to power off cleanly, then make sure.
        let deadline = Instant::now() + Duration::from_secs(if mode == "prepare" { 60 } else { 5 });
        while Instant::now() < deadline {
            if child.try_wait()?.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
        if !finished && !cancel.load(Ordering::SeqCst) {
            for line in tail(&console_path, 30) {
                log(&format!("[console] {line}"));
            }
        }
        let _ = std::fs::remove_dir_all(&vm_dir);
        Ok(())
    }
}

/// A sparse ext4 image of `gb` GiB.
fn make_ext4(path: &Path, gb: u32) -> Result<()> {
    let f = std::fs::File::create(path)?;
    f.set_len(u64::from(gb) << 30)?;
    drop(f);
    let out = Command::new("mkfs.ext4")
        .args([
            "-q",
            "-F",
            "-E",
            "lazy_itable_init=1,lazy_journal_init=1",
            "-m",
            "0",
        ])
        .arg(path)
        .output()
        .context("running mkfs.ext4 (install e2fsprogs)")?;
    if !out.status.success() {
        bail!(
            "mkfs.ext4 failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Pack a directory into a tar file the guest reads from a raw block device.
fn tar_dir(dir: &Path, out: &Path) -> Result<()> {
    let f = std::fs::File::create(out)?;
    let mut b = tar::Builder::new(f);
    b.follow_symlinks(false);
    b.append_dir_all(".", dir)?;
    let mut f = b.into_inner()?;
    // Block devices want whole 512-byte sectors; add slack for good measure.
    let len = f.metadata()?.len();
    let padded = len.div_ceil(512) * 512 + 4096;
    f.set_len(padded)?;
    f.flush()?;
    Ok(())
}

/// Nameservers the guest can reach (the host's, minus loopback stubs).
fn host_dns() -> Vec<String> {
    let mut out = Vec::new();
    for file in ["/run/systemd/resolve/resolv.conf", "/etc/resolv.conf"] {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for line in text.lines() {
            if let Some(ns) = line.strip_prefix("nameserver") {
                let ns = ns.trim();
                if !ns.starts_with("127.")
                    && ns != "::1"
                    && ns.parse::<std::net::Ipv4Addr>().is_ok()
                    && !out.contains(&ns.to_string())
                {
                    out.push(ns.to_string());
                }
            }
        }
        if !out.is_empty() {
            break;
        }
    }
    if out.is_empty() {
        out = vec!["1.1.1.1".into(), "8.8.8.8".into()];
    }
    out
}

fn tail(path: &Path, n: usize) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

/// Download `url` to `path` unless it's already there, verifying sha256.
pub fn fetch(url: &str, path: &Path, sha256: &str) -> Result<()> {
    if path.is_file() {
        return Ok(());
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(1800)))
        .build()
        .into();
    let mut resp = agent
        .get(url)
        .call()
        .with_context(|| format!("downloading {url}"))?;
    if !resp.status().is_success() {
        bail!("downloading {url}: HTTP {}", resp.status());
    }
    let tmp = path.with_extension("partial");
    let mut out = std::fs::File::create(&tmp)?;
    let mut reader = resp.body_mut().with_config().limit(4 << 30).reader();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
    }
    let got = hex::encode(hasher.finalize());
    if got != sha256 {
        let _ = std::fs::remove_file(&tmp);
        bail!("{url} doesn't match its pinned digest (expected sha256 {sha256}, got {got}). Nothing was used; update baste.");
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn exclusive_lock(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    // SAFETY: flock on an fd we own; blocks until the lock is free.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        bail!(
            "locking {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(f)
}

impl Backend for Firecracker {
    fn name(&self) -> &'static str {
        "firecracker"
    }

    fn doctor(&self) -> Vec<Check> {
        let mut checks = Vec::new();
        let wsl = crate::sys::platform() == crate::sys::Platform::Wsl2;
        if pinned(self.arch).is_none() {
            checks.push(Check::fail(
                "Architecture",
                format!("{} hosts are not supported", self.arch),
            ));
            return checks;
        }
        let kvm = Path::new("/dev/kvm");
        if !kvm.exists() {
            checks.push(Check::fail(
                "Virtualization",
                if wsl {
                    "/dev/kvm is missing: WSL2 needs nested virtualization. Set `nestedVirtualization=true` under [wsl2] in %UserProfile%\\.wslconfig and run `wsl --shutdown`. Managed Windows machines may block this by policy."
                } else {
                    "/dev/kvm is missing: enable virtualization (VT-x/AMD-V) in firmware and load the kvm module."
                },
            ));
        } else {
            let c = std::ffi::CString::new("/dev/kvm").unwrap();
            // SAFETY: access() on a constant path.
            let ok = unsafe { libc::access(c.as_ptr(), libc::R_OK | libc::W_OK) } == 0;
            checks.push(if ok {
                Check::pass("Virtualization", "KVM available")
            } else {
                Check::fail(
                    "Virtualization",
                    "no access to /dev/kvm: run `sudo usermod -aG kvm $USER` and log in again",
                )
            });
        }
        checks.push(match crate::sys::which("mkfs.ext4") {
            Some(_) => Check::pass("mkfs.ext4", "available"),
            None => Check::fail(
                "mkfs.ext4",
                "install e2fsprogs (needed to create per-job disks)",
            ),
        });
        let missing = self.check_taps();
        checks.push(if missing.is_empty() {
            Check::pass("Network", format!("{} tap device(s) ready", self.slots))
        } else {
            Check::fail(
                "Network",
                format!(
                    "tap devices for VM networking are missing ({}); run `sudo {} setup-network --slots {}` once",
                    missing.iter().map(|s| tap_name(*s)).collect::<Vec<_>>().join(", "),
                    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "baste".into()),
                    self.slots
                ),
            )
        });
        checks.push(Check::pass(
            "Image",
            format!(
                "Firecracker {FIRECRACKER_VERSION}, Ubuntu 24.04 rootfs sha256:{} (downloaded and provisioned on first run)",
                &self.pinned().map(|p| p.rootfs_sha).unwrap_or_default()[..12]
            ),
        ));
        checks
    }

    fn runner(&self, _job_dir: &Path) -> RunnerInfo {
        let mut node = Map::new();
        node.insert("node20".into(), json!("/opt/baste/node20/bin/node"));
        node.insert("node24".into(), json!("/opt/baste/node24/bin/node"));
        let mut env = Map::new();
        env.insert("BASTE".into(), json!("true"));
        RunnerInfo {
            os: "Linux".into(),
            arch: runner_arch(self.arch),
            name: format!("baste-{}", crate::sys::hostname()),
            work_root: "/home/runner/work".into(),
            tool_cache: "/opt/hostedtoolcache".into(),
            user: Some("runner".into()),
            node,
            docker_platform: None,
            env,
        }
    }

    fn image(&self) -> Option<String> {
        let p = self.pinned().ok()?;
        Some(format!(
            "firecracker-ci ubuntu-24.04 sha256:{} + kernel 6.1.141 sha256:{}",
            p.rootfs_sha,
            &p.kernel_sha[..12]
        ))
    }

    fn prepare(&self, log: &mut dyn FnMut(&str)) -> Result<()> {
        let target = self.prepared_path()?;
        if target.is_file() {
            return Ok(());
        }
        let _lock = exclusive_lock(&self.dir().join("prepare.lock"))?;
        if target.is_file() {
            return Ok(()); // another run prepared it while we waited
        }
        log("Downloading the pinned image (first run only)…");
        self.firecracker_bin()?;
        self.kernel()?;
        self.rootfs()?;
        self.initramfs()?;
        let work = self.dir().join("prepare-work");
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(work.join("bundle"))?;
        std::fs::write(work.join("bundle/provision.sh"), PROVISION)?;
        let bundle_tar = work.join("bundle.tar");
        tar_dir(&work.join("bundle"), &bundle_tar)?;
        let upper = target.with_extension("partial");
        make_ext4(&upper, 20)?;
        log("Provisioning the image (first run only, a few minutes)…");
        let cancel = Arc::new(AtomicBool::new(false));
        let slot =
            super::acquire_slot(self.slots, &|| false).ok_or_else(|| anyhow!("no VM slot free"))?;
        let mut ok = false;
        let mut error = None;
        let log_cell = std::cell::RefCell::new(&mut *log);
        self.boot(
            slot.index,
            "prepare",
            None,
            &upper,
            &bundle_tar,
            &cancel,
            &mut |e| match e {
                Event::Log { line, .. } => (*log_cell.borrow_mut())(&line),
                Event::JobFinished {
                    result, error: err, ..
                } => {
                    ok = result == baste_protocol::Outcome::Success;
                    error = err;
                }
                _ => {}
            },
            &mut |l| (*log_cell.borrow_mut())(l),
        )?;
        drop(slot);
        let _ = std::fs::remove_dir_all(&work);
        if !ok {
            let _ = std::fs::remove_file(&upper);
            bail!(
                "provisioning failed: {}",
                error.unwrap_or_else(|| "see the log".into())
            );
        }
        // Replay the journal so the layer mounts read-only without recovery.
        let _ = Command::new("e2fsck")
            .args(["-p", "-f"])
            .arg(&upper)
            .output();
        std::fs::rename(&upper, &target)?;
        log("Image ready");
        Ok(())
    }

    fn run(
        &self,
        launch: &JobLaunch,
        on_event: &mut dyn FnMut(Event),
        log: &mut dyn FnMut(&str),
    ) -> Result<()> {
        let prepared = self.prepared_path()?;
        if !prepared.is_file() {
            bail!("the VM image isn't prepared yet");
        }
        // The agent binary rides in the initramfs; the bundle is a raw tar.
        let bundle_tar = launch.job_dir.join("bundle.tar");
        tar_dir(launch.bundle, &bundle_tar)?;
        let upper = launch.job_dir.join("upper.ext4");
        make_ext4(&upper, self.disk_gb)?;
        let r = self.boot(
            launch.slot,
            "run",
            Some(&prepared),
            &upper,
            &bundle_tar,
            &launch.cancel,
            on_event,
            log,
        );
        let _ = std::fs::remove_file(&upper);
        let _ = std::fs::remove_file(&bundle_tar);
        r
    }
}

/// `sudo baste setup-network`: tap devices owned by the user, plus NAT.
pub fn setup_network(user: Option<&str>, slots: usize) -> Result<()> {
    // SAFETY: plain syscall.
    if unsafe { libc::geteuid() } != 0 {
        bail!("setup-network needs root: run `sudo baste setup-network`");
    }
    let user = user
        .map(str::to_string)
        .or_else(|| std::env::var("SUDO_USER").ok())
        .ok_or_else(|| anyhow!("pass --user (the account that runs baste)"))?;
    let run = |args: &[&str]| -> Result<bool> {
        let out = Command::new(args[0])
            .args(&args[1..])
            .output()
            .with_context(|| format!("running {}", args[0]))?;
        Ok(out.status.success())
    };
    for slot in 0..slots {
        let tap = tap_name(slot);
        let (host_ip, _) = slot_net(slot);
        let _ = run(&["ip", "link", "del", &tap]);
        if !run(&[
            "ip", "tuntap", "add", "dev", &tap, "mode", "tap", "user", &user,
        ])? {
            bail!("creating {tap} failed");
        }
        run(&["ip", "addr", "add", &format!("{host_ip}/30"), "dev", &tap])?;
        run(&["ip", "link", "set", &tap, "up"])?;
        println!("{tap}: {host_ip}/30, owned by {user}");
    }
    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1").context("enabling IP forwarding")?;
    let rules: [&[&str]; 3] = [
        &[
            "-t",
            "nat",
            "POSTROUTING",
            "-s",
            "172.30.0.0/16",
            "!",
            "-o",
            "baste-tap+",
            "-j",
            "MASQUERADE",
        ],
        &["FORWARD", "-i", "baste-tap+", "-j", "ACCEPT"],
        &[
            "FORWARD",
            "-o",
            "baste-tap+",
            "-m",
            "conntrack",
            "--ctstate",
            "RELATED,ESTABLISHED",
            "-j",
            "ACCEPT",
        ],
    ];
    for rule in rules {
        let (table, rest): (Vec<&str>, &[&str]) = if rule[0] == "-t" {
            (vec!["-t", rule[1]], &rule[2..])
        } else {
            (vec![], rule)
        };
        let mut check = vec!["iptables"];
        check.extend(&table);
        check.push("-C");
        check.extend(rest);
        if !run(&check)? {
            let mut add = vec!["iptables"];
            add.extend(&table);
            add.push("-A");
            add.extend(rest);
            if !run(&add)? {
                bail!("adding iptables rule {:?} failed", rest);
            }
        }
    }
    println!("NAT enabled for 172.30.0.0/16");
    if Path::new("/run/systemd/system").exists() {
        let exe = std::env::current_exe()?;
        let unit = format!(
            "[Unit]\nDescription=Baste VM networking\nAfter=network-online.target\n\n[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart={} setup-network --user {user} --slots {slots}\n\n[Install]\nWantedBy=multi-user.target\n",
            exe.display()
        );
        std::fs::write("/etc/systemd/system/baste-network.service", unit)?;
        let _ = run(&["systemctl", "daemon-reload"]);
        let _ = run(&["systemctl", "enable", "baste-network.service"]);
        println!("Installed baste-network.service so this survives reboots.");
    }
    Ok(())
}

/// A minimal writer for the "newc" cpio format used by Linux initramfs.
/// Entries are always owned by root, which needs no privileges to write.
pub mod cpio {
    use std::io::Write;

    pub struct Writer<W: Write> {
        out: W,
        ino: u32,
    }

    impl<W: Write> Writer<W> {
        pub fn new(out: W) -> Self {
            Writer { out, ino: 1 }
        }

        fn entry(
            &mut self,
            name: &str,
            mode: u32,
            rdev: (u32, u32),
            data: &[u8],
        ) -> std::io::Result<()> {
            let name_len = name.len() + 1;
            let header = format!(
                "070701{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}",
                self.ino,
                mode,
                0, // uid
                0, // gid
                1, // nlink
                0, // mtime
                data.len(),
                0, // dev major
                0, // dev minor
                rdev.0,
                rdev.1,
                name_len,
                0 // check
            );
            self.ino += 1;
            self.out.write_all(header.as_bytes())?;
            self.out.write_all(name.as_bytes())?;
            self.out.write_all(&[0])?;
            let pad = (4 - (110 + name_len) % 4) % 4;
            self.out.write_all(&[0u8; 4][..pad])?;
            self.out.write_all(data)?;
            let pad = (4 - data.len() % 4) % 4;
            self.out.write_all(&[0u8; 4][..pad])?;
            Ok(())
        }

        pub fn dir(&mut self, name: &str) -> std::io::Result<()> {
            self.entry(name, 0o040755, (0, 0), &[])
        }

        pub fn file(&mut self, name: &str, perm: u32, data: &[u8]) -> std::io::Result<()> {
            self.entry(name, 0o100000 | perm, (0, 0), data)
        }

        pub fn char_device(&mut self, name: &str, major: u32, minor: u32) -> std::io::Result<()> {
            self.entry(name, 0o020600, (major, minor), &[])
        }

        pub fn finish(mut self) -> std::io::Result<W> {
            self.entry("TRAILER!!!", 0, (0, 0), &[])?;
            self.out.flush()?;
            Ok(self.out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpio_is_valid_newc() {
        let mut w = cpio::Writer::new(Vec::new());
        w.dir("dev").unwrap();
        w.char_device("dev/console", 5, 1).unwrap();
        w.file("init", 0o755, b"#!/bin/sh\necho hi\n").unwrap();
        let bytes = w.finish().unwrap();
        assert_eq!(bytes.len() % 4, 0);
        assert!(bytes.starts_with(b"070701"));
        // If the system cpio tool is available, make sure it can list it.
        if crate::sys::which("cpio").is_some() {
            let mut child = Command::new("cpio")
                .args(["-t", "--quiet"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(&bytes).unwrap();
            let out = child.wait_with_output().unwrap();
            let listing = String::from_utf8_lossy(&out.stdout);
            assert!(listing.contains("dev/console"), "{listing}");
            assert!(listing.contains("init"));
        }
    }

    #[test]
    fn pins_exist_for_supported_arches() {
        assert!(pinned("x86_64").is_some());
        assert!(pinned("aarch64").is_some());
        assert!(pinned("riscv64").is_none());
    }

    #[test]
    fn dns_skips_loopback() {
        for ns in host_dns() {
            assert!(!ns.starts_with("127."));
        }
    }

    #[test]
    fn tars_are_sector_aligned() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("a"), "x").unwrap();
        let out = d.path().join("b.tar");
        tar_dir(&src, &out).unwrap();
        assert_eq!(std::fs::metadata(&out).unwrap().len() % 512, 0);
        let names: Vec<String> = tar::Archive::new(std::fs::File::open(&out).unwrap())
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().display().to_string())
            .collect();
        assert!(names.iter().any(|n| n.ends_with('a')), "{names:?}");
    }
}
