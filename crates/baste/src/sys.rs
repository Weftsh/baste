//! Small host facts: platform, memory, battery, hostname.

use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacAppleSilicon,
    MacIntel,
    Linux,
    Wsl2,
    Other,
}

pub fn platform() -> Platform {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Platform::MacAppleSilicon,
        ("macos", _) => {
            // An arm64 Mac running an x86_64 build under Rosetta reports x86_64.
            if sysctl("sysctl.proc_translated").as_deref() == Some("1")
                || sysctl("hw.optional.arm64").as_deref() == Some("1")
            {
                Platform::MacAppleSilicon
            } else {
                Platform::MacIntel
            }
        }
        ("linux", _) => {
            let version = std::fs::read_to_string("/proc/version").unwrap_or_default();
            if version.to_ascii_lowercase().contains("microsoft")
                || std::env::var_os("WSL_DISTRO_NAME").is_some()
            {
                Platform::Wsl2
            } else {
                Platform::Linux
            }
        }
        _ => Platform::Other,
    }
}

fn sysctl(name: &str) -> Option<String> {
    let out = Command::new("sysctl").args(["-n", name]).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn total_memory_mb() -> Option<u32> {
    if cfg!(target_os = "macos") {
        return sysctl("hw.memsize")?
            .parse::<u64>()
            .ok()
            .map(|b| (b / 1024 / 1024) as u32);
    }
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = meminfo
        .lines()
        .find(|l| l.starts_with("MemTotal:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some((kb / 1024) as u32)
}

/// `Some(true)` when running on battery, `None` when unknown.
pub fn on_battery() -> Option<bool> {
    if cfg!(target_os = "macos") {
        let out = Command::new("pmset").args(["-g", "batt"]).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        return Some(text.contains("'Battery Power'"));
    }
    let dir = std::fs::read_dir("/sys/class/power_supply").ok()?;
    let mut saw_mains = false;
    for entry in dir.flatten() {
        let p = entry.path();
        let kind = std::fs::read_to_string(p.join("type")).unwrap_or_default();
        if kind.trim() == "Mains" {
            saw_mains = true;
            if std::fs::read_to_string(p.join("online"))
                .unwrap_or_default()
                .trim()
                == "1"
            {
                return Some(false);
            }
        }
    }
    saw_mains.then_some(true)
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most buf.len() bytes.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else {
        "localhost".into()
    }
}

/// Best-effort desktop notification (Notification Center or notify-send).
pub fn notify(title: &str, body: &str) {
    if std::env::var_os("BASTE_NO_NOTIFY").is_some() {
        return;
    }
    let quiet = |mut c: Command| {
        let _ = c
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    };
    if cfg!(target_os = "macos") {
        let q = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
        let mut c = Command::new("osascript");
        c.args([
            "-e",
            &format!(
                "display notification {} with title \"Baste\" subtitle {}",
                q(body),
                q(title)
            ),
        ]);
        quiet(c);
    } else if which("notify-send").is_some() {
        let mut c = Command::new("notify-send");
        c.args(["--app-name=Baste", &format!("Baste: {title}"), body]);
        quiet(c);
    }
}

/// Look up an executable in `PATH`.
pub fn which(program: &str) -> Option<std::path::PathBuf> {
    baste_agent::process::which(program, &std::env::var("PATH").unwrap_or_default())
}
