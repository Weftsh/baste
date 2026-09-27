//! VM backends. Every backend speaks the same runner protocol: it receives a
//! bundle (job spec plus files) and returns the agent's event stream.

use crate::config::Config;
use crate::sys::Platform;
use anyhow::{bail, Result};
use baste_protocol::{Event, RunnerInfo};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub mod agent_bin;
pub mod firecracker;
pub mod host;
pub mod tart;

/// One job to run.
pub struct JobLaunch<'a> {
    pub run_id: &'a str,
    /// Directory holding `job.json` and the files it references.
    pub bundle: &'a Path,
    /// Scratch space for this job (disks, sockets).
    pub job_dir: &'a Path,
    /// Which VM slot this job holds (for per-slot resources like tap devices).
    pub slot: usize,
    pub cancel: Arc<AtomicBool>,
}

/// A result of a doctor check.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    /// Blocking checks make `init` stop without changing anything.
    pub blocking: bool,
    pub detail: String,
}

impl Check {
    pub fn pass(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            ok: true,
            blocking: false,
            detail: detail.into(),
        }
    }

    pub fn fail(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            ok: false,
            blocking: true,
            detail: detail.into(),
        }
    }

    pub fn warn(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            ok: false,
            blocking: false,
            detail: detail.into(),
        }
    }
}

pub trait Backend: Send + Sync {
    fn name(&self) -> &'static str;

    /// Checks for `baste doctor` / `baste init`.
    fn doctor(&self) -> Vec<Check>;

    /// Runner facts to put in the job spec. `job_dir` is where a host-side
    /// work tree may live.
    fn runner(&self, job_dir: &Path) -> RunnerInfo;

    /// A description of the pinned image, for provenance.
    fn image(&self) -> Option<String>;

    /// Make sure the pinned image is downloaded and provisioned. Called once
    /// per run before any job starts.
    fn prepare(&self, log: &mut dyn FnMut(&str)) -> Result<()>;

    /// Boot a fresh VM for the job, run the agent, stream its events, and
    /// tear the VM down.
    fn run(
        &self,
        launch: &JobLaunch,
        on_event: &mut dyn FnMut(Event),
        log: &mut dyn FnMut(&str),
    ) -> Result<()>;
}

/// The backend that fits this machine.
pub fn default_name() -> &'static str {
    match crate::sys::platform() {
        Platform::MacAppleSilicon | Platform::MacIntel => "tart",
        _ => "firecracker",
    }
}

pub fn select(config: &Config, override_name: Option<&str>) -> Result<Arc<dyn Backend>> {
    let name = override_name
        .map(str::to_string)
        .or_else(|| {
            std::env::var("BASTE_BACKEND")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| config.backend.clone());
    let name = if name == "auto" {
        default_name().to_string()
    } else {
        name
    };
    Ok(match name.as_str() {
        "firecracker" => Arc::new(firecracker::Firecracker::new(config)),
        "tart" => Arc::new(tart::Tart::new(config)),
        "host" => Arc::new(host::HostBackend::new()),
        other => bail!("unknown backend '{other}' (use auto, firecracker, tart or host)"),
    })
}

/// `X64` / `ARM64` for an architecture name.
pub fn runner_arch(arch: &str) -> String {
    match arch {
        "x86_64" | "amd64" => "X64".into(),
        "aarch64" | "arm64" => "ARM64".into(),
        other => other.to_ascii_uppercase(),
    }
}

/// Forward a stream of JSON-line events, passing non-event lines to `log`.
pub fn pump_events(
    reader: impl std::io::Read,
    on_event: &mut dyn FnMut(Event),
    log: &mut dyn FnMut(&str),
) -> bool {
    use std::io::BufRead;
    let mut finished = false;
    for line in std::io::BufReader::new(reader).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        match Event::from_line(&line) {
            Ok(e) => {
                if matches!(e, Event::JobFinished { .. }) {
                    finished = true;
                }
                on_event(e);
            }
            Err(_) => log(&line),
        }
    }
    finished
}

/// A machine-wide VM slot, held while a VM runs. Slots bound how many VMs run
/// at once across all runs and pick per-slot resources (tap devices).
pub struct SlotGuard {
    _file: std::fs::File,
    pub index: usize,
}

/// Wait for a free slot. Returns `None` if `stop()` becomes true first.
pub fn acquire_slot(max: usize, stop: &dyn Fn() -> bool) -> Option<SlotGuard> {
    use std::os::unix::io::AsRawFd;
    let dir = crate::config::state_dir().join("slots");
    let _ = std::fs::create_dir_all(&dir);
    loop {
        for index in 0..max.max(1) {
            let Ok(file) = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join(format!("{index}.lock")))
            else {
                continue;
            };
            // SAFETY: flock on an fd we own.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Some(SlotGuard { _file: file, index });
            }
        }
        if stop() {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}
