//! Spawning step processes: merged stdout/stderr, line streaming, timeouts,
//! cancellation and dropping privileges to the runner user.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// Numeric identity to run steps as.
#[derive(Debug, Clone)]
pub struct UserIds {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
    pub home: PathBuf,
}

/// Look up a user and its supplementary groups.
pub fn lookup_user(name: &str) -> Option<UserIds> {
    let cname = CString::new(name).ok()?;
    // SAFETY: getpwnam returns a pointer to static storage or null; we copy
    // what we need before any other call that could overwrite it.
    unsafe {
        let pw = libc::getpwnam(cname.as_ptr());
        if pw.is_null() {
            return None;
        }
        let uid = (*pw).pw_uid;
        let gid = (*pw).pw_gid;
        let home = std::ffi::CStr::from_ptr((*pw).pw_dir)
            .to_string_lossy()
            .into_owned();
        let mut n: libc::c_int = 64;
        let mut groups: Vec<libc::gid_t> = vec![0; n as usize];
        #[cfg(target_os = "macos")]
        let rc = libc::getgrouplist(
            cname.as_ptr(),
            gid as libc::c_int,
            groups.as_mut_ptr() as *mut libc::c_int,
            &mut n,
        );
        #[cfg(not(target_os = "macos"))]
        let rc = libc::getgrouplist(cname.as_ptr(), gid, groups.as_mut_ptr(), &mut n);
        let groups = if rc < 0 {
            vec![gid]
        } else {
            groups.truncate(n.max(0) as usize);
            groups.into_iter().map(|g| g as u32).collect()
        };
        Some(UserIds {
            name: name.to_string(),
            uid,
            gid,
            groups,
            home: PathBuf::from(home),
        })
    }
}

/// How a process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
    TimedOut,
    Cancelled,
}

impl Exit {
    pub fn success(self) -> bool {
        self == Exit::Code(0)
    }

    pub fn code(self) -> Option<i32> {
        match self {
            Exit::Code(c) => Some(c),
            Exit::Signal(s) => Some(128 + s),
            _ => None,
        }
    }
}

pub struct Spawn {
    pub program: String,
    pub args: Vec<String>,
    /// The complete environment (the parent's is not inherited).
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub user: Option<UserIds>,
    pub timeout: Option<Duration>,
}

/// Run a process to completion, calling `on_line` for every line it prints
/// on stdout or stderr, in order. Returns the exit and the process group id,
/// which stays alive if the process left background children behind.
pub fn run(
    spawn: &Spawn,
    cancel: &AtomicBool,
    on_line: &mut dyn FnMut(String),
) -> std::io::Result<(Exit, i32)> {
    let (reader, writer) = std::io::pipe()?;
    let mut cmd = Command::new(&spawn.program);
    cmd.args(&spawn.args)
        .env_clear()
        .envs(&spawn.env)
        .current_dir(&spawn.cwd)
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer)
        .process_group(0);
    if let Some(user) = &spawn.user {
        let (uid, gid) = (user.uid, user.gid);
        let groups: Vec<libc::gid_t> = user.groups.iter().map(|g| *g as libc::gid_t).collect();
        // SAFETY: only async-signal-safe libc calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setgroups(groups.len() as _, groups.as_ptr()) != 0
                    || libc::setgid(gid) != 0
                    || libc::setuid(uid) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("could not start '{}': {e}", spawn.program),
        )
    })?;
    // Drop our copies of the write end so EOF arrives when the process exits.
    drop(cmd);
    let pgid = child.id() as i32;

    let (tx, rx) = mpsc::channel::<String>();
    let closed = Arc::new(AtomicBool::new(false));
    let reader_closed = closed.clone();
    std::thread::spawn(move || {
        let mut r = BufReader::with_capacity(64 * 1024, reader);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match r.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if reader_closed.load(Ordering::Relaxed) {
                        continue; // a background process outlived its step
                    }
                    while buf.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                        buf.pop();
                    }
                    if tx.send(String::from_utf8_lossy(&buf).into_owned()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let start = Instant::now();
    let mut forced: Option<Exit> = None;
    let mut term_sent: Option<Instant> = None;
    let status = loop {
        while let Ok(line) = rx.try_recv() {
            on_line(line);
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if forced.is_none() {
            if cancel.load(Ordering::Relaxed) {
                forced = Some(Exit::Cancelled);
            } else if spawn.timeout.is_some_and(|t| start.elapsed() > t) {
                forced = Some(Exit::TimedOut);
            }
            if forced.is_some() {
                kill_group(pgid, libc::SIGTERM);
                term_sent = Some(Instant::now());
            }
        } else if term_sent.is_some_and(|t| t.elapsed() > Duration::from_millis(7500)) {
            kill_group(pgid, libc::SIGKILL);
            term_sent = None;
        }
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(line) => on_line(line),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };

    // Drain what the process printed before exiting. Background children may
    // hold the pipe open, so stop after a short quiet period.
    let quiet = Duration::from_millis(150);
    while let Ok(line) = rx.recv_timeout(quiet) {
        on_line(line);
    }
    closed.store(true, Ordering::Relaxed);

    let exit = forced.unwrap_or_else(|| {
        use std::os::unix::process::ExitStatusExt;
        match (status.code(), status.signal()) {
            (Some(c), _) => Exit::Code(c),
            (None, Some(s)) => Exit::Signal(s),
            _ => Exit::Code(1),
        }
    });
    Ok((exit, pgid))
}

/// Signal every process in a process group, ignoring errors.
pub fn kill_group(pgid: i32, signal: i32) {
    if pgid > 1 {
        // SAFETY: plain syscall.
        unsafe {
            libc::kill(-pgid, signal);
        }
    }
}

/// Whether any process of the group is still alive.
pub fn group_alive(pgid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    pgid > 1 && unsafe { libc::kill(-pgid, 0) } == 0
}

/// Split a command template the way GitHub does for custom shells:
/// whitespace separated, with double quotes grouping.
pub fn split_command(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

/// Find `program` in a `PATH` string.
pub fn which(program: &str, path: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let p = PathBuf::from(program);
        return p.is_file().then_some(p);
    }
    path.split(':')
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d).join(program))
        .find(|p| {
            use std::os::unix::fs::PermissionsExt;
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> BTreeMap<String, String> {
        let mut e = BTreeMap::new();
        e.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
        e
    }

    fn spawn(script: &str) -> Spawn {
        Spawn {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            env: env(),
            cwd: std::env::temp_dir(),
            user: None,
            timeout: None,
        }
    }

    #[test]
    fn merges_output_in_order() {
        let mut lines = Vec::new();
        let (exit, _) = run(
            &spawn("echo one; echo two >&2; echo three; exit 3"),
            &AtomicBool::new(false),
            &mut |l| lines.push(l),
        )
        .unwrap();
        assert_eq!(exit, Exit::Code(3));
        assert_eq!(lines, vec!["one", "two", "three"]);
    }

    #[test]
    fn times_out() {
        let mut s = spawn("sleep 30");
        s.timeout = Some(Duration::from_millis(200));
        let start = Instant::now();
        let (exit, _) = run(&s, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(exit, Exit::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn background_children_do_not_block() {
        let start = Instant::now();
        let (exit, pgid) = run(
            &spawn("sleep 30 & echo started"),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(exit, Exit::Code(0));
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(group_alive(pgid));
        kill_group(pgid, libc::SIGKILL);
    }

    #[test]
    fn splits_commands() {
        assert_eq!(
            split_command(r#"pwsh -command ". '{0}'""#),
            vec!["pwsh", "-command", ". '{0}'"]
        );
        assert_eq!(split_command("bash -e {0}"), vec!["bash", "-e", "{0}"]);
    }
}
