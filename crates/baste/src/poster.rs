//! Posts commit statuses from a background thread.
//!
//! The pre-push hook starts a run before the push reaches GitHub, so the
//! commit may not exist there yet. Updates are queued per context (only the
//! latest matters) and retried until the commit appears or the wait runs out.

use crate::github::{is_missing_commit, ApiError, GitHub};
use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// GitHub rejects commit status descriptions longer than this (characters).
pub const MAX_DESCRIPTION: usize = 140;

/// `s` cut to at most `max` characters, ending in "…" when shortened.
pub fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

/// A description that fits GitHub's limit. The last " · " part (how to see
/// the logs) is kept whole; the text before it is shortened.
pub fn fit_description(description: &str) -> String {
    if description.chars().count() <= MAX_DESCRIPTION {
        return description.to_string();
    }
    match description.rfind(" · ") {
        Some(i) if description[i..].chars().count() <= MAX_DESCRIPTION / 2 => {
            let (head, tail) = description.split_at(i);
            let room = MAX_DESCRIPTION - tail.chars().count();
            format!("{}{tail}", shorten(head, room))
        }
        _ => shorten(description, MAX_DESCRIPTION),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusUpdate {
    pub context: String,
    pub state: &'static str,
    pub description: String,
    pub target_url: Option<String>,
}

enum Msg {
    Update(StatusUpdate),
    Finish,
}

pub struct StatusPoster {
    tx: Option<mpsc::Sender<Msg>>,
    handle: Option<JoinHandle<()>>,
    /// Notes for the run record (why something wasn't posted).
    pub notes: Arc<Mutex<Vec<String>>>,
}

impl StatusPoster {
    /// `api` is `None` when statuses are disabled for the run.
    pub fn start(api: Option<GitHub>, sha: String, wait: Duration) -> StatusPoster {
        let notes = Arc::new(Mutex::new(Vec::new()));
        let Some(api) = api else {
            return StatusPoster {
                tx: None,
                handle: None,
                notes,
            };
        };
        let (tx, rx) = mpsc::channel();
        let thread_notes = notes.clone();
        let handle = std::thread::spawn(move || {
            post_loop(api, sha, wait, rx, thread_notes);
        });
        StatusPoster {
            tx: Some(tx),
            handle: Some(handle),
            notes,
        }
    }

    pub fn update(&self, update: StatusUpdate) {
        if let Some(tx) = &self.tx {
            let description = fit_description(&update.description);
            let _ = tx.send(Msg::Update(StatusUpdate {
                description,
                ..update
            }));
        }
    }

    /// Flush everything queued (waiting for the commit if needed) and stop.
    pub fn finish(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Msg::Finish);
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for StatusPoster {
    fn drop(&mut self) {
        self.finish();
    }
}

fn post_loop(
    api: GitHub,
    sha: String,
    wait: Duration,
    rx: mpsc::Receiver<Msg>,
    notes: Arc<Mutex<Vec<String>>>,
) {
    let started = Instant::now();
    let mut queue: Vec<String> = Vec::new();
    let mut latest: HashMap<String, StatusUpdate> = HashMap::new();
    let mut finishing = false;
    let mut disabled = false;
    let mut backoff = Duration::from_millis(0);
    let mut commit_missing_since: Option<Instant> = None;
    loop {
        // Collect new updates, waiting at most `backoff` for the first one.
        let first = if backoff.is_zero() && !queue.is_empty() {
            rx.try_recv().map_err(|e| match e {
                mpsc::TryRecvError::Empty => RecvTimeoutError::Timeout,
                mpsc::TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
            })
        } else if queue.is_empty() && !finishing {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            rx.recv_timeout(backoff)
        };
        let mut incoming = Vec::new();
        match first {
            Ok(m) => incoming.push(m),
            Err(RecvTimeoutError::Disconnected) => finishing = true,
            Err(RecvTimeoutError::Timeout) => {}
        }
        while let Ok(m) = rx.try_recv() {
            incoming.push(m);
        }
        for m in incoming {
            match m {
                Msg::Update(u) => {
                    if !queue.contains(&u.context) {
                        queue.push(u.context.clone());
                    }
                    latest.insert(u.context.clone(), u);
                }
                Msg::Finish => finishing = true,
            }
        }
        if disabled {
            queue.clear();
        }
        if queue.is_empty() {
            if finishing {
                return;
            }
            backoff = Duration::ZERO;
            continue;
        }

        // Post in order; stop at the first "commit not found".
        let mut missing = false;
        while let Some(context) = queue.first().cloned() {
            let u = latest
                .get(&context)
                .cloned()
                .expect("queued contexts have updates");
            match api.create_status(
                &sha,
                u.state,
                &u.context,
                &u.description,
                u.target_url.as_deref(),
            ) {
                Ok(()) => {
                    queue.remove(0);
                    commit_missing_since = None;
                }
                Err(e) if is_missing_commit(&e) => {
                    missing = true;
                    break;
                }
                Err(e) => {
                    queue.remove(0);
                    let fatal = e
                        .downcast_ref::<ApiError>()
                        .is_some_and(|a| matches!(a.status, 401 | 403 | 404));
                    let mut n = notes.lock().unwrap();
                    if fatal {
                        n.push(format!(
                            "Couldn't post commit statuses: {e}. Check that your gh token can write statuses to {}.",
                            api.repo.full_name()
                        ));
                        disabled = true;
                        break;
                    }
                    n.push(format!("Couldn't post status '{}': {e}", u.context));
                }
            }
        }
        if missing {
            let since = *commit_missing_since.get_or_insert_with(Instant::now);
            if started.elapsed() > wait && since.elapsed() > Duration::from_secs(5) {
                notes.lock().unwrap().push(format!(
                    "Commit {} never appeared on GitHub within {} minutes (was the push rejected?); statuses were not posted.",
                    &sha[..sha.len().min(12)],
                    wait.as_secs() / 60
                ));
                disabled = true;
                continue;
            }
            backoff = (backoff * 2).clamp(Duration::from_millis(500), Duration::from_secs(5));
        } else {
            backoff = Duration::ZERO;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_descriptions_are_unchanged() {
        let d = "Passed in 41s · baste logs q7hz2m";
        assert_eq!(fit_description(d), d);
        let exact = "x".repeat(MAX_DESCRIPTION);
        assert_eq!(fit_description(&exact), exact);
    }

    #[test]
    fn long_descriptions_keep_the_logs_command() {
        let d = format!("Failed: {} · baste logs q7hz2m", "error ".repeat(40));
        let fit = fit_description(&d);
        assert_eq!(fit.chars().count(), MAX_DESCRIPTION);
        assert!(fit.starts_with("Failed: error error"), "{fit}");
        assert!(fit.ends_with("… · baste logs q7hz2m"), "{fit}");
    }

    #[test]
    fn counts_characters_not_bytes() {
        let d = format!("{} · run q7hz2m", "é".repeat(200));
        let fit = fit_description(&d);
        assert!(fit.chars().count() <= MAX_DESCRIPTION);
        assert!(fit.ends_with(" · run q7hz2m"));
    }

    #[test]
    fn without_a_separator_the_end_is_cut() {
        let fit = fit_description(&"y".repeat(500));
        assert_eq!(fit.chars().count(), MAX_DESCRIPTION);
        assert!(fit.ends_with('…'));
    }

    #[test]
    fn shorten_trims_to_the_limit() {
        assert_eq!(shorten("Run tests", 60), "Run tests");
        let s = shorten(&format!("Run {}", "a ".repeat(50)), 20);
        assert_eq!(s.chars().count(), 20);
        assert_eq!(s, "Run a a a a a a a a…");
    }
}
