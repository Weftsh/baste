//! Code that runs inside the job VM (or, for the host backend, as a child
//! process): the agent entry point, the Firecracker guest service, and the
//! Firecracker PID 1 that assembles the copy-on-write root filesystem.

use anyhow::{bail, Context, Result};
use baste_agent::{run_bundle, EventSink, JsonLinesSink};
use baste_protocol::{Event, Outcome};
use clap::Subcommand;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// Vsock port the guest agent connects to on the host.
pub const VSOCK_PORT: u32 = 5000;

#[derive(Subcommand)]
pub enum AgentCmd {
    /// Run the job in a bundle directory; events go to stdout.
    Run {
        #[arg(long)]
        bundle: PathBuf,
    },
    /// Firecracker guest service: read the bundle from a block device, run it,
    /// and stream events to the host over vsock.
    Guest {
        #[arg(long, default_value = "/dev/vdd")]
        bundle_device: PathBuf,
        #[arg(long, default_value_t = VSOCK_PORT)]
        port: u32,
        /// `run` a job, or `prepare` the image (run provision.sh).
        #[arg(long, default_value = "run")]
        mode: String,
        /// Where events go: `vsock` (default) or a device such as
        /// `/dev/hvc0` (for debugging a VM without vsock).
        #[arg(long, default_value = "vsock")]
        events: String,
    },
}

static CANCEL: OnceLock<Arc<AtomicBool>> = OnceLock::new();

extern "C" fn on_term(_: libc::c_int) {
    if let Some(c) = CANCEL.get() {
        c.store(true, Ordering::SeqCst);
    }
}

fn cancel_on_sigterm() -> Arc<AtomicBool> {
    let flag = CANCEL
        .get_or_init(|| Arc::new(AtomicBool::new(false)))
        .clone();
    // SAFETY: the handler only touches an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as *const () as libc::sighandler_t);
    }
    flag
}

pub fn agent(cmd: AgentCmd) -> Result<bool> {
    match cmd {
        AgentCmd::Run { bundle } => {
            let cancel = cancel_on_sigterm();
            let sink = JsonLinesSink::new(std::io::stdout());
            let outcome = run_bundle(&bundle, cancel, &sink)?;
            Ok(outcome == Outcome::Success)
        }
        AgentCmd::Guest {
            bundle_device,
            port,
            mode,
            events,
        } => {
            let r = guest_service(&bundle_device, port, &mode, &events);
            if let Err(e) = &r {
                eprintln!("baste guest: {e:#}");
            }
            // A prepared layer must be unmounted cleanly; a job VM is thrown away.
            power_off(mode == "prepare");
            r.map(|_| true)
        }
    }
}

/// Unpack a tar read from a block device (trailing zero blocks end it).
fn unpack_bundle(device: &Path, dest: &Path) -> Result<()> {
    let f = std::fs::File::open(device).with_context(|| format!("opening {}", device.display()))?;
    std::fs::create_dir_all(dest)?;
    let mut archive = tar::Archive::new(std::io::BufReader::new(f));
    archive.set_preserve_permissions(true);
    archive.unpack(dest).context("unpacking the job bundle")?;
    Ok(())
}

fn guest_service(device: &Path, port: u32, mode: &str, events: &str) -> Result<()> {
    let bundle = PathBuf::from("/opt/baste/bundle");
    let stream = if events == "vsock" {
        connect_host(port)?
    } else {
        std::fs::OpenOptions::new()
            .write(true)
            .open(events)
            .with_context(|| format!("opening {events}"))?
    };
    let sink = JsonLinesSink::new(stream);
    let result = (|| -> Result<()> {
        unpack_bundle(device, &bundle)?;
        match mode {
            "prepare" => prepare(&bundle, &sink),
            _ => {
                let cancel = cancel_on_sigterm();
                run_bundle(&bundle, cancel, &sink)?;
                Ok(())
            }
        }
    })();
    if let Err(e) = &result {
        sink.send(Event::Log {
            step: 0,
            line: format!("##[error]{e:#}"),
        });
        sink.send(Event::JobFinished {
            result: Outcome::Failure,
            outputs: Default::default(),
            error: Some(format!("{e:#}")),
            at: String::new(),
        });
    }
    // SAFETY: flush filesystems before the VM goes away.
    unsafe { libc::sync() };
    result
}

/// Run the bundle's provisioning script, streaming its output.
fn prepare(bundle: &Path, sink: &dyn EventSink) -> Result<()> {
    use std::io::BufRead;
    let script = bundle.join("provision.sh");
    let mut child = std::process::Command::new("/bin/bash")
        .arg(&script)
        .current_dir(bundle)
        .env("DEBIAN_FRONTEND", "noninteractive")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .context("starting provision.sh")?;
    let out = child.stdout.take().unwrap();
    for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
        sink.send(Event::Log { step: 0, line });
    }
    let status = child.wait()?;
    sink.send(Event::JobFinished {
        result: if status.success() {
            Outcome::Success
        } else {
            Outcome::Failure
        },
        outputs: Default::default(),
        error: (!status.success()).then(|| format!("provision.sh exited with {status}")),
        at: String::new(),
    });
    Ok(())
}

#[cfg(target_os = "linux")]
fn connect_host(port: u32) -> Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    // SAFETY: a plain AF_VSOCK socket; the fd is owned by the returned File.
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            bail!("vsock socket: {}", std::io::Error::last_os_error());
        }
        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_cid = libc::VMADDR_CID_HOST;
        addr.svm_port = port;
        for attempt in 0..50 {
            let rc = libc::connect(
                fd,
                &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            );
            if rc == 0 {
                return Ok(std::fs::File::from_raw_fd(fd));
            }
            if attempt == 49 {
                let e = std::io::Error::last_os_error();
                libc::close(fd);
                bail!("connecting to the host over vsock: {e}");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        unreachable!()
    }
}

#[cfg(not(target_os = "linux"))]
fn connect_host(_port: u32) -> Result<std::fs::File> {
    bail!("the guest service only runs inside a Linux VM")
}

/// End the VM. Firecracker exits when the guest reboots (`reboot=k`).
fn power_off(clean: bool) {
    let rebooting = clean
        && std::process::Command::new("systemctl")
            .arg("reboot")
            .status()
            .is_ok_and(|s| s.success());
    #[cfg(target_os = "linux")]
    if !rebooting {
        // SAFETY: we are the guest; nothing else needs to survive.
        unsafe {
            libc::sync();
            if std::process::id() != 1 {
                libc::reboot(libc::RB_AUTOBOOT);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = rebooting;
}

// ----- PID 1 in the Firecracker initramfs ------------------------------------

/// Kernel command line parameters we use (`baste.*`).
#[derive(Debug, Default, PartialEq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct BootParams {
    pub lower: String,
    pub prepared: Option<String>,
    pub upper: String,
    pub bundle: String,
    pub mode: String,
    pub dns: Vec<String>,
    /// Event transport for the guest service (default vsock).
    pub events: Option<String>,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_cmdline(cmdline: &str) -> BootParams {
    let mut p = BootParams {
        lower: "/dev/vda".into(),
        upper: "/dev/vdb".into(),
        bundle: "/dev/vdc".into(),
        mode: "run".into(),
        ..Default::default()
    };
    for arg in cmdline.split_whitespace() {
        let Some((k, v)) = arg.split_once('=') else {
            continue;
        };
        match k {
            "baste.lower" => p.lower = v.into(),
            "baste.prepared" => p.prepared = Some(v.into()),
            "baste.upper" => p.upper = v.into(),
            "baste.bundle" => p.bundle = v.into(),
            "baste.mode" => p.mode = v.into(),
            "baste.events" => p.events = Some(v.into()),
            "baste.dns" => {
                p.dns = v
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            }
            _ => {}
        }
    }
    p
}

/// The systemd unit that starts the guest service after boot.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn agent_unit(p: &BootParams) -> String {
    format!(
        "[Unit]\nDescription=Baste agent\nAfter=network.target docker.service containerd.service\nWants=docker.service\n\n[Service]\nType=simple\nExecStart=/usr/local/bin/baste agent guest --bundle-device {} --mode {}{}\nEnvironment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/snap/bin\nEnvironment=HOME=/root\nStandardOutput=journal+console\nStandardError=journal+console\n\n[Install]\nWantedBy=multi-user.target\n",
        p.bundle,
        p.mode,
        p.events
            .as_deref()
            .map(|e| format!(" --events {e}"))
            .unwrap_or_default()
    )
}

#[cfg(target_os = "linux")]
pub fn pid1() -> Result<()> {
    use std::ffi::CString;
    fn mount(
        src: &str,
        target: &str,
        fstype: &str,
        flags: libc::c_ulong,
        data: &str,
    ) -> Result<()> {
        let c = |s: &str| CString::new(s).unwrap();
        let (src_c, target_c, fs_c, data_c) = (c(src), c(target), c(fstype), c(data));
        // SAFETY: valid NUL-terminated strings.
        let rc = unsafe {
            libc::mount(
                src_c.as_ptr(),
                target_c.as_ptr(),
                if fstype.is_empty() {
                    std::ptr::null()
                } else {
                    fs_c.as_ptr()
                },
                flags,
                if data.is_empty() {
                    std::ptr::null()
                } else {
                    data_c.as_ptr() as *const libc::c_void
                },
            )
        };
        if rc != 0 {
            bail!(
                "mount {src} on {target} ({fstype}): {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }
    for d in [
        "/dev", "/proc", "/sys", "/lower", "/prep", "/upper", "/newroot",
    ] {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = mount("devtmpfs", "/dev", "devtmpfs", 0, "");
    mount("proc", "/proc", "proc", 0, "")?;
    mount("sysfs", "/sys", "sysfs", 0, "")?;
    let p = parse_cmdline(&std::fs::read_to_string("/proc/cmdline").unwrap_or_default());
    eprintln!("baste: assembling the root filesystem ({:?})", p);

    mount(&p.lower, "/lower", "squashfs", libc::MS_RDONLY, "")?;
    mount(&p.upper, "/upper", "ext4", 0, "")?;
    std::fs::create_dir_all("/upper/upper")?;
    std::fs::create_dir_all("/upper/work")?;
    let lowerdir = match &p.prepared {
        Some(dev) => {
            mount(dev, "/prep", "ext4", libc::MS_RDONLY, "noload")?;
            "/prep/upper:/lower".to_string()
        }
        None => "/lower".to_string(),
    };
    mount(
        "overlay",
        "/newroot",
        "overlay",
        0,
        &format!("lowerdir={lowerdir},upperdir=/upper/upper,workdir=/upper/work"),
    )?;

    let root = Path::new("/newroot");
    // Docker and containerd can't layer overlay2 on an overlayfs root; give
    // them directories on the job's ext4 disk instead. While preparing, their
    // data shouldn't become part of the layer at all.
    for (src, dst) in [
        ("/upper/docker", "var/lib/docker"),
        ("/upper/containerd", "var/lib/containerd"),
    ] {
        std::fs::create_dir_all(root.join(dst))?;
        let target = format!("/newroot/{dst}");
        if p.mode == "run" {
            std::fs::create_dir_all(src)?;
            mount(src, &target, "", libc::MS_BIND, "")?;
        } else {
            mount("tmpfs", &target, "tmpfs", 0, "mode=0711")?;
        }
    }

    // Inject the agent, its service, DNS, and quiet down units that would
    // fight the kernel's static network config.
    std::fs::create_dir_all(root.join("usr/local/bin"))?;
    std::fs::copy("/init", root.join("usr/local/bin/baste"))?;
    let units = root.join("etc/systemd/system");
    std::fs::create_dir_all(units.join("multi-user.target.wants"))?;
    std::fs::write(units.join("baste-agent.service"), agent_unit(&p))?;
    let _ = std::fs::remove_file(units.join("multi-user.target.wants/baste-agent.service"));
    std::os::unix::fs::symlink(
        "/etc/systemd/system/baste-agent.service",
        units.join("multi-user.target.wants/baste-agent.service"),
    )?;
    for masked in [
        "fcnet.service",
        "systemd-networkd.service",
        "systemd-networkd-wait-online.service",
        "systemd-resolved.service",
        "serial-getty@ttyS0.service",
        "getty.target",
        "ssh.service",
        "ssh.socket",
    ] {
        let path = units.join(masked);
        let _ = std::fs::remove_file(&path);
        let _ = std::os::unix::fs::symlink("/dev/null", path);
    }
    let resolv = root.join("etc/resolv.conf");
    let _ = std::fs::remove_file(&resolv);
    let dns = if p.dns.is_empty() {
        vec!["1.1.1.1".to_string(), "8.8.8.8".to_string()]
    } else {
        p.dns.clone()
    };
    std::fs::write(
        &resolv,
        dns.iter()
            .map(|d| format!("nameserver {d}\n"))
            .collect::<String>(),
    )?;
    std::fs::write(root.join("etc/hostname"), "baste\n")?;
    std::fs::write(
        root.join("etc/hosts"),
        "127.0.0.1 localhost\n127.0.1.1 baste\n::1 localhost ip6-localhost ip6-loopback\n",
    )?;
    std::fs::write(root.join("etc/fstab"), "# managed by baste\n")?;

    // switch_root: move the new root over / and hand off to systemd.
    for d in ["dev", "proc", "sys"] {
        let _ = std::fs::create_dir_all(root.join(d));
        mount(
            &format!("/{d}"),
            &format!("/newroot/{d}"),
            "",
            libc::MS_MOVE,
            "",
        )?;
    }
    std::env::set_current_dir("/newroot")?;
    mount("/newroot", "/", "", libc::MS_MOVE, "")?;
    let dot = CString::new(".").unwrap();
    // SAFETY: chroot into the directory we just moved to /.
    if unsafe { libc::chroot(dot.as_ptr()) } != 0 {
        bail!("chroot: {}", std::io::Error::last_os_error());
    }
    std::env::set_current_dir("/")?;
    let init = CString::new("/sbin/init").unwrap();
    let args = [init.as_ptr(), std::ptr::null()];
    // SAFETY: execv replaces this process; args are NUL-terminated.
    unsafe { libc::execv(init.as_ptr(), args.as_ptr()) };
    bail!("exec /sbin/init: {}", std::io::Error::last_os_error())
}

#[cfg(not(target_os = "linux"))]
pub fn pid1() -> Result<()> {
    bail!("baste only runs as init inside a Linux VM")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_boot_params() {
        let p = parse_cmdline(
            "console=ttyS0 reboot=k baste.lower=/dev/vda baste.prepared=/dev/vdb baste.upper=/dev/vdc baste.bundle=/dev/vdd baste.mode=prepare baste.dns=10.0.0.1,1.1.1.1",
        );
        assert_eq!(p.prepared.as_deref(), Some("/dev/vdb"));
        assert_eq!(p.bundle, "/dev/vdd");
        assert_eq!(p.mode, "prepare");
        assert_eq!(p.dns, vec!["10.0.0.1", "1.1.1.1"]);
        assert!(agent_unit(&p).contains("--bundle-device /dev/vdd --mode prepare"));
    }
}
