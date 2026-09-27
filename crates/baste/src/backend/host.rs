//! The `host` backend runs the agent as a plain process on this machine, with
//! no VM. It exists for developing Baste itself and for trying it on machines
//! without virtualization. Results are not isolated and can differ from GitHub.

use super::{pump_events, runner_arch, Backend, Check, JobLaunch};
use anyhow::{Context, Result};
use baste_protocol::{Event, RunnerInfo};
use serde_json::Map;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::Duration;

pub struct HostBackend;

impl HostBackend {
    pub fn new() -> Self {
        HostBackend
    }
}

impl Default for HostBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for HostBackend {
    fn name(&self) -> &'static str {
        "host"
    }

    fn doctor(&self) -> Vec<Check> {
        let mut checks = vec![Check::warn(
            "Backend",
            "host (no VM): jobs run directly on this machine without isolation or a pinned image. Use it for development only.",
        )];
        for tool in ["git", "bash"] {
            checks.push(match crate::sys::which(tool) {
                Some(p) => Check::pass(tool, p.display().to_string()),
                None => Check::fail(tool, format!("{tool} is required by the host backend")),
            });
        }
        checks.push(match crate::sys::which("node") {
            Some(p) => Check::pass("node", p.display().to_string()),
            None => Check::warn("node", "JavaScript actions need Node.js on PATH"),
        });
        checks
    }

    fn runner(&self, job_dir: &Path) -> RunnerInfo {
        let os = if cfg!(target_os = "macos") {
            "macOS"
        } else {
            "Linux"
        };
        RunnerInfo {
            os: os.into(),
            arch: runner_arch(std::env::consts::ARCH),
            name: format!("baste-host-{}", crate::sys::hostname()),
            work_root: job_dir.join("work").display().to_string(),
            tool_cache: crate::config::cache_dir()
                .join("toolcache")
                .display()
                .to_string(),
            user: None,
            node: Map::new(),
            docker_platform: None,
            env: Map::new(),
        }
    }

    fn image(&self) -> Option<String> {
        None
    }

    fn prepare(&self, _log: &mut dyn FnMut(&str)) -> Result<()> {
        Ok(())
    }

    fn run(
        &self,
        launch: &JobLaunch,
        on_event: &mut dyn FnMut(Event),
        log: &mut dyn FnMut(&str),
    ) -> Result<()> {
        let exe = std::env::current_exe().context("locating the baste binary")?;
        log("Running on the host (no VM)");
        let mut child = Command::new(exe)
            .args(["agent", "run", "--bundle"])
            .arg(launch.bundle)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .context("starting the agent")?;
        let pid = child.id() as i32;
        let cancel = launch.cancel.clone();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watcher_done = done.clone();
        let watcher = std::thread::spawn(move || {
            let mut signalled = false;
            while !watcher_done.load(Ordering::Relaxed) {
                if cancel.load(Ordering::Relaxed) && !signalled {
                    // The agent turns SIGTERM into a graceful cancellation.
                    // SAFETY: plain syscall.
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                    signalled = true;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let stderr = child.stderr.take().expect("piped");
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let err_thread = std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stderr)
                .lines()
                .map_while(Result::ok)
            {
                let _ = tx.send(line);
            }
        });
        let stdout = child.stdout.take().expect("piped");
        let mut logged = |l: &str| log(l);
        pump_events(stdout, on_event, &mut logged);
        let status = child.wait()?;
        done.store(true, Ordering::Relaxed);
        let _ = watcher.join();
        let _ = err_thread.join();
        for line in rx.try_iter() {
            log(&line);
        }
        if !status.success() && status.code() != Some(1) {
            log(&format!("agent exited with {status}"));
        }
        Ok(())
    }
}

use std::os::unix::process::CommandExt;
