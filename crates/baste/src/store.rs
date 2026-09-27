//! Local run records and logs, kept in `<git-common-dir>/baste/runs/<id>/`.
//!
//! The worker process that owns a run is its only writer; commands like
//! `status` and `logs` read snapshots. Records are replaced atomically.

use crate::git::{Git, RepoRef};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::PathBuf;

pub const RECORD_VERSION: u32 = 1;
const ID_ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Running,
    Passed,
    Failed,
    Cancelled,
    Error,
}

impl RunState {
    pub fn is_done(self) -> bool {
        !matches!(self, RunState::Queued | RunState::Running)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Passed,
    Failed,
    /// `if:` was false.
    Skipped,
    Cancelled,
    /// Runs on GitHub instead (Windows/macOS, unsupported features, ...).
    HandedToGithub,
    /// Not run because a job it needs failed.
    NotRun,
}

impl JobState {
    pub fn is_done(self) -> bool {
        !matches!(self, JobState::Queued | JobState::Running)
    }

    pub fn label(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Passed => "passed",
            JobState::Failed => "failed",
            JobState::Skipped => "skipped",
            JobState::Cancelled => "cancelled",
            JobState::HandedToGithub => "handed to GitHub",
            JobState::NotRun => "not run",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Running,
    Success,
    Failure,
    Skipped,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub index: usize,
    pub name: String,
    pub phase: String,
    pub state: StepState,
    /// Failed but `continue-on-error` let the job go on.
    #[serde(default)]
    pub continued: bool,
    pub exit_code: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub duration_ms: Option<u64>,
    /// Log file relative to the job directory.
    pub log: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    /// Unique within the run; also the job's directory name.
    pub key: String,
    pub workflow: String,
    pub workflow_file: String,
    pub event: String,
    pub job_id: String,
    pub name: String,
    #[serde(default)]
    pub matrix: Option<Value>,
    /// The commit status context, for jobs that run locally.
    pub context: Option<String>,
    pub state: JobState,
    /// Why a job was handed to GitHub, skipped, or not run.
    pub reason: Option<String>,
    pub needs: Vec<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub steps: Vec<StepRecord>,
    #[serde(default)]
    pub outputs: Map<String, Value>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub continue_on_error: bool,
    /// A placeholder whose matrix/runs-on depends on outputs of other jobs.
    #[serde(default)]
    pub pending_expansion: bool,
}

impl JobRecord {
    pub fn duration_ms(&self) -> Option<i64> {
        match (self.started_at, self.finished_at) {
            (Some(s), Some(f)) => Some((f - s).num_milliseconds()),
            (Some(s), None) => Some((Utc::now() - s).num_milliseconds()),
            _ => None,
        }
    }

    /// Display as `<workflow> / <job name>`.
    pub fn title(&self) -> String {
        format!("{} / {}", self.workflow, self.name)
    }

    /// The step that failed the job, if any.
    pub fn failed_step(&self) -> Option<&StepRecord> {
        self.steps
            .iter()
            .find(|s| s.state == StepState::Failure && !s.continued)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Trigger {
    Push,
    Manual,
    Rerun { of: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequestInfo {
    pub number: u64,
    pub base_ref: String,
    pub base_sha: String,
    pub head_ref: String,
}

/// Where and how a run executed, recorded from day one so routing policies
/// ("main requires cloud") can be added later without a migration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Provenance {
    /// `local` today; `cloud` later.
    pub executor: String,
    pub host: String,
    pub os: String,
    pub arch: String,
    pub backend: String,
    #[serde(default)]
    pub image: Option<String>,
    pub baste_version: String,
    /// The routing policy that picked this executor.
    pub policy: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowInsight {
    pub workflow: String,
    pub workflow_file: String,
    pub local_ms: i64,
    #[serde(default)]
    pub github_run_id: Option<u64>,
    /// GitHub's queue + run time for its last completed run of this workflow.
    #[serde(default)]
    pub github_total_ms: Option<i64>,
    #[serde(default)]
    pub github_run_ms: Option<i64>,
    /// Billed Actions minutes of the jobs that ran locally.
    #[serde(default)]
    pub minutes_saved: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunInsights {
    pub workflows: Vec<WorkflowInsight>,
    /// Time from push to the last local status.
    pub local_total_ms: i64,
    #[serde(default)]
    pub time_saved_ms: Option<i64>,
    #[serde(default)]
    pub minutes_saved: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub version: u32,
    pub id: String,
    pub repo: RepoRef,
    /// The pushed commit; statuses go here.
    pub sha: String,
    /// What the VM checks out (a test merge commit for `pull_request`).
    pub checkout_sha: String,
    pub git_ref: String,
    pub before: Option<String>,
    pub remote: Option<String>,
    pub trigger: Trigger,
    pub state: RunState,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub pull_request: Option<PullRequestInfo>,
    pub provenance: Provenance,
    #[serde(default)]
    pub jobs: Vec<JobRecord>,
    #[serde(default)]
    pub notes: Vec<String>,
    pub error: Option<String>,
    pub worker_pid: Option<u32>,
    /// Post commit statuses for this run.
    pub post_statuses: bool,
    #[serde(default)]
    pub insights: Option<RunInsights>,
    #[serde(default)]
    pub run_attempt: u32,
    /// Only run these workflows (file paths) / jobs (ids), for manual runs.
    #[serde(default)]
    pub only_workflows: Vec<String>,
    #[serde(default)]
    pub only_jobs: Vec<String>,
    /// Force the event instead of detecting it (manual runs).
    #[serde(default)]
    pub event_override: Option<String>,
    #[serde(default)]
    pub backend_override: Option<String>,
}

impl Run {
    pub fn new(id: String, repo: RepoRef, sha: String, git_ref: String, trigger: Trigger) -> Run {
        Run {
            version: RECORD_VERSION,
            id,
            repo,
            checkout_sha: sha.clone(),
            sha,
            git_ref,
            before: None,
            remote: None,
            trigger,
            state: RunState::Queued,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            pull_request: None,
            provenance: Provenance::default(),
            jobs: vec![],
            notes: vec![],
            error: None,
            worker_pid: None,
            post_statuses: true,
            insights: None,
            run_attempt: 1,
            only_workflows: vec![],
            only_jobs: vec![],
            event_override: None,
            backend_override: None,
        }
    }

    pub fn branch(&self) -> String {
        self.git_ref
            .strip_prefix("refs/heads/")
            .or_else(|| self.git_ref.strip_prefix("refs/tags/"))
            .unwrap_or(&self.git_ref)
            .to_string()
    }

    pub fn short_sha(&self) -> &str {
        &self.sha[..self.sha.len().min(7)]
    }

    pub fn duration_ms(&self) -> Option<i64> {
        let start = self.created_at;
        Some(match self.finished_at {
            Some(f) => (f - start).num_milliseconds(),
            None if !self.state.is_done() => (Utc::now() - start).num_milliseconds(),
            None => return None,
        })
    }

    pub fn job(&self, key: &str) -> Option<&JobRecord> {
        self.jobs.iter().find(|j| j.key == key)
    }

    pub fn job_mut(&mut self, key: &str) -> Option<&mut JobRecord> {
        self.jobs.iter_mut().find(|j| j.key == key)
    }

    /// Is the worker that owns this run still alive?
    pub fn worker_alive(&self) -> bool {
        match self.worker_pid {
            // SAFETY: signal 0 only checks for existence.
            Some(pid) => unsafe { libc::kill(pid as i32, 0) == 0 },
            None => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Store {
        Store { root }
    }

    pub fn for_repo(git: &Git) -> Store {
        Store::new(git.common_dir.join("baste"))
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.root.join("runs")
    }

    pub fn run_dir(&self, id: &str) -> PathBuf {
        self.runs_dir().join(id)
    }

    pub fn job_dir(&self, id: &str, job_key: &str) -> PathBuf {
        self.run_dir(id).join("jobs").join(job_key)
    }

    pub fn new_id(&self) -> String {
        loop {
            let id: String = (0..6)
                .map(|_| ID_ALPHABET[rand::random_range(0..ID_ALPHABET.len())] as char)
                .collect();
            if !self.run_dir(&id).exists() {
                return id;
            }
        }
    }

    pub fn save(&self, run: &Run) -> Result<()> {
        let dir = self.run_dir(&run.id);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = dir.join(format!("run.json.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(run)?)?;
        std::fs::rename(&tmp, dir.join("run.json"))?;
        Ok(())
    }

    pub fn load(&self, id: &str) -> Result<Run> {
        let path = self.run_dir(id).join("run.json");
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("run '{id}' not found"))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// All runs, newest first.
    pub fn list(&self) -> Result<Vec<Run>> {
        let mut runs = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.runs_dir()) else {
            return Ok(runs);
        };
        for e in entries.flatten() {
            if let Ok(run) = self.load(&e.file_name().to_string_lossy()) {
                runs.push(run);
            }
        }
        runs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(runs)
    }

    /// Find a run by id, unique id prefix, `latest`, or a commit sha prefix.
    pub fn resolve(&self, query: &str) -> Result<Run> {
        let runs = self.list()?;
        if runs.is_empty() {
            bail!("no runs yet in this repository. Push, or start one with `baste run`.");
        }
        if query == "latest" || query == "last" {
            return Ok(runs[0].clone());
        }
        if let Some(r) = runs.iter().find(|r| r.id == query) {
            return Ok(r.clone());
        }
        let by_id: Vec<&Run> = runs.iter().filter(|r| r.id.starts_with(query)).collect();
        if by_id.len() == 1 {
            return Ok(by_id[0].clone());
        }
        if query.len() >= 4 && query.chars().all(|c| c.is_ascii_hexdigit()) {
            if let Some(r) = runs.iter().find(|r| r.sha.starts_with(query)) {
                return Ok(r.clone());
            }
        }
        if by_id.len() > 1 {
            bail!("'{query}' matches several runs; use more characters");
        }
        Err(anyhow!(
            "no run '{query}'. See `baste status` for recent runs."
        ))
    }

    /// Relative log path of a step within its job directory.
    pub fn step_log_name(index: usize) -> String {
        format!("steps/{index:03}.log")
    }

    /// Delete the oldest finished runs beyond `keep`.
    pub fn prune(&self, keep: usize) -> Result<usize> {
        let runs = self.list()?;
        let mut removed = 0;
        for run in runs.iter().skip(keep) {
            if run.state.is_done() && !run.worker_alive() {
                let _ = std::fs::remove_dir_all(self.run_dir(&run.id));
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// Slug for a job key: lowercase alphanumerics and dashes.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').chars().take(60).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> RepoRef {
        RepoRef {
            host: "github.com".into(),
            owner: "o".into(),
            name: "r".into(),
        }
    }

    #[test]
    fn save_load_resolve() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::new(d.path().to_path_buf());
        let id = store.new_id();
        assert_eq!(id.len(), 6);
        let run = Run::new(
            id.clone(),
            repo(),
            "abcdef1234".into(),
            "refs/heads/main".into(),
            Trigger::Push,
        );
        store.save(&run).unwrap();
        let mut second = Run::new(
            store.new_id(),
            repo(),
            "99887766".into(),
            "refs/heads/x".into(),
            Trigger::Manual,
        );
        second.created_at = run.created_at + chrono::Duration::seconds(5);
        store.save(&second).unwrap();
        assert_eq!(store.resolve(&id).unwrap().id, id);
        assert_eq!(store.resolve("latest").unwrap().id, second.id);
        assert_eq!(store.resolve("abcdef").unwrap().id, id);
        assert!(store.resolve("zzzzzz").is_err());
        assert_eq!(store.list().unwrap().len(), 2);
        assert_eq!(run.branch(), "main");
    }

    #[test]
    fn slugs() {
        assert_eq!(
            slug("CI / test (ubuntu-latest, 20)"),
            "ci-test-ubuntu-latest-20"
        );
    }
}
