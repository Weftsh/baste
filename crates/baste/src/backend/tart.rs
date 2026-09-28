//! Tart backend (macOS on Apple Silicon).
//!
//! The base image is `ghcr.io/cirruslabs/ubuntu` pinned by digest. It is
//! provisioned once into a local VM, and every job runs in an APFS
//! copy-on-write clone of that VM that is deleted afterwards. Rosetta for
//! Linux is enabled so x86_64 binaries and `linux/amd64` images run as they do
//! on GitHub's `ubuntu-latest`. The bundle is shared read-only over virtiofs
//! and the agent is driven with `tart exec`.

use super::{Backend, Check, JobLaunch};
use crate::config::Config;
use anyhow::{anyhow, bail, Context, Result};
use baste_protocol::{Event, RunnerInfo};
use serde_json::{json, Map};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

pub const IMAGE: &str = "ghcr.io/cirruslabs/ubuntu";
pub const DIGEST: &str = "sha256:15e9ae24025971b0fd3b32b99a0b2914c09b677460d0236018297c4dbf635f82";
const PROVISION: &str = include_str!("provision.sh");

fn tart_bin() -> String {
    std::env::var("BASTE_TART").unwrap_or_else(|_| "tart".into())
}

/// Whether this Tart can give VMs nested virtualization (`tart run --nested`,
/// "if possible": Apple silicon from M3 on macOS 15 and later). With it, jobs
/// get /dev/kvm and can run VMs of their own, as on GitHub's runners.
fn supports_nested() -> bool {
    static NESTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *NESTED.get_or_init(|| {
        Command::new(tart_bin())
            .args(["run", "--help"])
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("--nested"))
    })
}

fn tart(args: &[&str]) -> Result<String> {
    let out = Command::new(tart_bin())
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running tart (install it with `brew install cirruslabs/cli/tart`, or from https://github.com/cirruslabs/tart/releases)")?;
    if !out.status.success() {
        bail!(
            "tart {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn vm_exists(name: &str) -> bool {
    tart(&["list", "--format", "json"])
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.as_array().cloned())
        .is_some_and(|vms| vms.iter().any(|vm| vm["Name"].as_str() == Some(name)))
}

pub struct Tart {
    cpus: u32,
    memory_mb: u32,
}

impl Tart {
    pub fn new(config: &Config) -> Self {
        Tart {
            cpus: config.vm_cpus(),
            memory_mb: config.vm_memory_mb(),
        }
    }

    fn base_name() -> String {
        format!("baste-base-{}", &DIGEST[7..19])
    }

    fn prepared_name() -> String {
        let mut h = Sha256::new();
        h.update(DIGEST);
        h.update(PROVISION);
        format!("baste-prepared-{}", &hex::encode(h.finalize())[..16])
    }

    /// Start `tart run` for a VM in the background.
    fn start(&self, name: &str, share: Option<&Path>, log_path: &Path) -> Result<Child> {
        let log = std::fs::File::create(log_path)?;
        let mut cmd = Command::new(tart_bin());
        cmd.args(["run", name, "--no-graphics", "--rosetta=rosetta"]);
        if supports_nested() {
            cmd.arg("--nested");
        }
        if let Some(dir) = share {
            cmd.arg(format!("--dir=baste:{}:ro", dir.display()));
        }
        cmd.stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .context("starting tart run")
    }

    /// Wait until the guest agent answers `tart exec`.
    fn wait_ready(
        &self,
        name: &str,
        child: &mut Child,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        let started = Instant::now();
        loop {
            let ok = Command::new(tart_bin())
                .args(["exec", name, "true"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if ok {
                return Ok(());
            }
            if let Some(status) = child.try_wait()? {
                bail!("the VM stopped during boot ({status})");
            }
            if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
                bail!("cancelled");
            }
            if started.elapsed() > Duration::from_secs(180) {
                bail!("the VM's guest agent didn't answer within 3 minutes (the image needs the Tart Guest Agent)");
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    fn stop(&self, name: &str, child: &mut Child) {
        let _ = tart(&["stop", name, "--timeout", "5"]);
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Backend for Tart {
    fn name(&self) -> &'static str {
        "tart"
    }

    fn doctor(&self) -> Vec<Check> {
        let mut checks = Vec::new();
        let macos = Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let major: u32 = macos
            .split('.')
            .next()
            .and_then(|m| m.parse().ok())
            .unwrap_or(0);
        checks.push(if major >= 14 {
            Check::pass("macOS", macos.clone())
        } else {
            Check::fail(
                "macOS",
                format!("macOS 14 (Sonoma) or newer is needed, found {macos}"),
            )
        });
        checks.push(match tart(&["--version"]) {
            Ok(v) => Check::pass("Tart", v),
            Err(_) => Check::fail(
                "Tart",
                "not installed: `brew install cirruslabs/cli/tart`, or download tart.app from https://github.com/cirruslabs/tart/releases",
            ),
        });
        checks.push(
            if Path::new("/Library/Apple/usr/share/rosetta/rosetta").exists() {
                Check::pass(
                    "Rosetta",
                    "installed (x86_64 binaries and amd64 images run as on GitHub)",
                )
            } else {
                Check::fail(
                    "Rosetta",
                    "not installed: `softwareupdate --install-rosetta --agree-to-license`",
                )
            },
        );
        checks.push(Check::pass(
            "Image",
            format!(
                "{IMAGE}@{} (downloaded and provisioned on first run)",
                &DIGEST[..19]
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
        env.insert("DOCKER_DEFAULT_PLATFORM".into(), json!("linux/amd64"));
        RunnerInfo {
            os: "Linux".into(),
            arch: "ARM64".into(),
            name: format!("baste-{}", crate::sys::hostname()),
            work_root: "/home/runner/work".into(),
            tool_cache: "/opt/hostedtoolcache".into(),
            user: Some("runner".into()),
            node,
            docker_platform: Some("linux/amd64".into()),
            env,
        }
    }

    fn image(&self) -> Option<String> {
        Some(format!("{IMAGE}@{DIGEST}"))
    }

    fn prepare(&self, log: &mut dyn FnMut(&str)) -> Result<()> {
        let prepared = Self::prepared_name();
        if vm_exists(&prepared) {
            return Ok(());
        }
        let base = Self::base_name();
        if !vm_exists(&base) {
            log(&format!("Pulling {IMAGE}@{DIGEST} (first run only)…"));
            tart(&["clone", &format!("{IMAGE}@{DIGEST}"), &base])?;
        }
        let tmp = "baste-prepare-tmp";
        if vm_exists(tmp) {
            let _ = tart(&["stop", tmp, "--timeout", "1"]);
            let _ = tart(&["delete", tmp]);
        }
        tart(&["clone", &base, tmp])?;
        let cpus = self.cpus.to_string();
        let mem = self.memory_mb.to_string();
        tart(&["set", tmp, "--cpu", &cpus, "--memory", &mem])?;
        log("Provisioning the image (first run only, a few minutes)…");
        let dir = crate::config::cache_dir().join("tart");
        std::fs::create_dir_all(&dir)?;
        let mut child = self.start(tmp, None, &dir.join("prepare-console.log"))?;
        let result = (|| -> Result<()> {
            self.wait_ready(tmp, &mut child, None)?;
            let mut put = Command::new(tart_bin())
                .args(["exec", "-i", tmp, "sudo", "tee", "/tmp/provision.sh"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()?;
            put.stdin.take().unwrap().write_all(PROVISION.as_bytes())?;
            if !put.wait()?.success() {
                bail!("copying provision.sh into the VM failed");
            }
            let mut run = Command::new(tart_bin())
                .args([
                    "exec",
                    tmp,
                    "sudo",
                    "env",
                    "BASTE_ROSETTA=1",
                    "bash",
                    "/tmp/provision.sh",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let stderr = run.stderr.take().unwrap();
            let err_thread = std::thread::spawn(move || {
                use std::io::BufRead;
                std::io::BufReader::new(stderr)
                    .lines()
                    .map_while(Result::ok)
                    .collect::<Vec<_>>()
            });
            {
                use std::io::BufRead;
                for line in std::io::BufReader::new(run.stdout.take().unwrap())
                    .lines()
                    .map_while(Result::ok)
                {
                    log(&line);
                }
            }
            let status = run.wait()?;
            let errors = err_thread.join().unwrap_or_default();
            if !status.success() {
                for l in errors.iter().rev().take(20).rev() {
                    log(l);
                }
                bail!("provisioning failed ({status})");
            }
            let _ = tart(&["exec", tmp, "sudo", "sync"]);
            Ok(())
        })();
        self.stop(tmp, &mut child);
        match result {
            Ok(()) => {
                tart(&["rename", tmp, &prepared])?;
                log("Image ready");
                Ok(())
            }
            Err(e) => {
                let _ = tart(&["delete", tmp]);
                Err(e)
            }
        }
    }

    fn run(
        &self,
        launch: &JobLaunch,
        on_event: &mut dyn FnMut(Event),
        log: &mut dyn FnMut(&str),
    ) -> Result<()> {
        let prepared = Self::prepared_name();
        if !vm_exists(&prepared) {
            bail!("the VM image isn't prepared yet");
        }
        // The guest runs Linux; ship a Linux build of the agent in the bundle.
        let agent = super::agent_bin::agent_binary("aarch64")?;
        let bin_dir = launch.bundle.join("bin");
        std::fs::create_dir_all(&bin_dir)?;
        std::fs::copy(&agent, bin_dir.join("baste"))
            .context("copying the agent into the bundle")?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                bin_dir.join("baste"),
                std::fs::Permissions::from_mode(0o755),
            )?;
        }

        let vm = format!("baste-{}-{}", launch.run_id, launch.slot);
        if vm_exists(&vm) {
            let _ = tart(&["stop", &vm, "--timeout", "1"]);
            let _ = tart(&["delete", &vm]);
        }
        log(&format!("Cloning {prepared} (copy-on-write)"));
        tart(&["clone", &prepared, &vm])?;
        let cpus = self.cpus.to_string();
        let mem = self.memory_mb.to_string();
        tart(&["set", &vm, "--cpu", &cpus, "--memory", &mem])?;
        let started = Instant::now();
        let mut child = self.start(
            &vm,
            Some(launch.bundle),
            &launch.job_dir.join("console.log"),
        )?;
        let result = (|| -> Result<()> {
            if let Err(e) = self.wait_ready(&vm, &mut child, Some(&launch.cancel)) {
                if launch.cancel.load(Ordering::SeqCst) {
                    return Ok(());
                }
                return Err(e);
            }
            log(&format!(
                "VM booted in {}",
                crate::ui::duration(started.elapsed().as_millis() as i64)
            ));
            tart(&[
                "exec",
                &vm,
                "sudo",
                "sh",
                "-c",
                "mountpoint -q /mnt/shared || { mkdir -p /mnt/shared && mount -t virtiofs com.apple.virtio-fs.automount /mnt/shared; }",
            ])
            .map_err(|e| anyhow!("mounting the job bundle: {e}"))?;
            let mut agent = Command::new(tart_bin())
                .args([
                    "exec",
                    &vm,
                    "sudo",
                    "/mnt/shared/baste/bin/baste",
                    "agent",
                    "run",
                    "--bundle",
                    "/mnt/shared/baste",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .context("starting the agent in the VM")?;
            let stderr = agent.stderr.take().unwrap();
            let err_thread = std::thread::spawn(move || {
                use std::io::BufRead;
                std::io::BufReader::new(stderr)
                    .lines()
                    .map_while(Result::ok)
                    .collect::<Vec<_>>()
            });
            // Stop the VM on cancel; that ends `tart exec` and the stream.
            let cancel = launch.cancel.clone();
            let vm_name = vm.clone();
            let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let watch_done = done.clone();
            let watcher = std::thread::spawn(move || {
                while !watch_done.load(Ordering::SeqCst) {
                    if cancel.load(Ordering::SeqCst) {
                        let _ = tart(&["stop", &vm_name, "--timeout", "0"]);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            });
            super::pump_events(agent.stdout.take().unwrap(), on_event, log);
            let _ = agent.wait();
            done.store(true, Ordering::SeqCst);
            let _ = watcher.join();
            for line in err_thread.join().unwrap_or_default() {
                log(&line);
            }
            Ok(())
        })();
        self.stop(&vm, &mut child);
        let _ = tart(&["delete", &vm]);
        let _ = std::fs::remove_dir_all(&bin_dir);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_stable() {
        assert!(Tart::base_name().starts_with("baste-base-15e9ae24"));
        assert_eq!(Tart::prepared_name(), Tart::prepared_name());
        assert!(DIGEST.starts_with("sha256:") && DIGEST.len() == 71);
    }
}
