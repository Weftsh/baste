//! The Baste runner protocol.
//!
//! A runner receives one [`JobSpec`] (plus a bundle of files it references) and
//! answers with a stream of [`Event`]s, one JSON object per line. The same
//! protocol is spoken by the local agent inside a VM today and is meant to be
//! spoken by cloud runners later, so nothing here is specific to a transport
//! or a VM backend.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Bumped on any incompatible change to [`JobSpec`] or [`Event`].
pub const PROTOCOL_VERSION: u32 = 1;

/// File name of the job spec inside a bundle.
pub const JOB_SPEC_FILE: &str = "job.json";

/// Everything a runner needs to execute one job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub protocol: u32,
    pub run_id: String,
    /// Unique key of this job within the run (stable across matrix expansion).
    pub job_key: String,
    /// The job's id in the workflow file.
    pub job_id: String,
    /// Display name (after matrix expansion and expression evaluation).
    pub job_name: String,
    pub workflow: WorkflowInfo,
    /// Workflow-level `env`, unevaluated.
    #[serde(default)]
    pub workflow_env: Map<String, Value>,
    /// Job-level `env`, unevaluated.
    #[serde(default)]
    pub job_env: Map<String, Value>,
    /// Merged `defaults.run` (job overrides workflow), unevaluated.
    #[serde(default)]
    pub defaults: RunDefaults,
    /// The job's steps exactly as written in the workflow (YAML converted to JSON).
    pub steps: Vec<Value>,
    /// Job `outputs:` templates, evaluated after the last step.
    #[serde(default)]
    pub outputs: Map<String, Value>,
    #[serde(default)]
    pub timeout_minutes: Option<f64>,
    /// Read-only contexts computed by the orchestrator: `github`, `matrix`,
    /// `strategy`, `needs`, `vars`, `inputs`, `job`.
    pub contexts: Map<String, Value>,
    /// Secret values available to the job, by name.
    #[serde(default)]
    pub secrets: Map<String, Value>,
    /// Secrets the job references that are not set locally. The runner fails
    /// the job before any step runs if this is not empty.
    #[serde(default)]
    pub missing_secrets: Vec<String>,
    /// Extra values to mask in logs (beyond secret values).
    #[serde(default)]
    pub masks: Vec<String>,
    /// Remote actions the job uses, already downloaded into the bundle.
    #[serde(default)]
    pub actions: Vec<ActionSource>,
    pub checkout: CheckoutSource,
    /// Artifacts uploaded by jobs this job needs, available to download-artifact.
    #[serde(default)]
    pub artifacts: Vec<ArtifactSource>,
    /// Path of the event payload inside the bundle.
    pub event_file: String,
    pub runner: RunnerInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowInfo {
    pub name: String,
    /// Path of the workflow file relative to the repository root.
    pub file: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunDefaults {
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub working_directory: Option<String>,
}

/// A remote action (`owner/repo[/path]@ref`) and where its files live in the bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionSource {
    /// `owner/repo@ref` (without any sub-path).
    pub repo_ref: String,
    /// Directory in the bundle holding the repository at that ref.
    pub path: String,
}

/// How the runner materialises the pushed commit without talking to GitHub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckoutSource {
    /// `owner/repo` of the repository under test.
    pub repository: String,
    /// The commit to check out (the pushed SHA, or a local test-merge commit
    /// for `pull_request` events).
    pub sha: String,
    /// The ref the commit is checked out as, e.g. `refs/heads/main`.
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// Git packs holding the objects needed for each supported fetch depth.
    pub packs: Vec<CheckoutPack>,
    /// `https://github.com` or a GitHub Enterprise URL.
    pub server_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckoutPack {
    /// `0` means full history.
    pub depth: u32,
    /// Pack file path in the bundle.
    pub file: String,
    /// Commits whose parents are missing from the pack (written to `.git/shallow`).
    #[serde(default)]
    pub shallow: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactSource {
    pub name: String,
    /// Tar file in the bundle.
    pub file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerInfo {
    /// `Linux`
    pub os: String,
    /// `X64` or `ARM64`
    pub arch: String,
    pub name: String,
    /// Root of the runner's work tree, e.g. `/home/runner/work`.
    pub work_root: String,
    pub tool_cache: String,
    /// Run steps as this user (dropping privileges), if set.
    #[serde(default)]
    pub user: Option<String>,
    /// Node binaries for JavaScript actions, keyed by `runs.using` (`node20`, `node24`).
    #[serde(default)]
    pub node: Map<String, Value>,
    /// Set as `DOCKER_DEFAULT_PLATFORM` for Docker actions (e.g. `linux/amd64` under Rosetta).
    #[serde(default)]
    pub docker_platform: Option<String>,
    /// Extra environment for every step (e.g. `BASTE=true`).
    #[serde(default)]
    pub env: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepPhase {
    Setup,
    Pre,
    Main,
    Post,
    Complete,
}

/// Result of a step or job, following GitHub's naming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Success,
    Failure,
    Cancelled,
    Skipped,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Failure => "failure",
            Outcome::Cancelled => "cancelled",
            Outcome::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationLevel {
    Debug,
    Notice,
    Warning,
    Error,
}

/// One message from runner to host. Serialized as a single JSON line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Hello {
        protocol: u32,
        agent_version: String,
        os: String,
        arch: String,
    },
    StepStarted {
        index: usize,
        name: String,
        phase: StepPhase,
        at: String,
    },
    /// A line of output. `step` is the index from `StepStarted`.
    Log { step: usize, line: String },
    Annotation {
        step: usize,
        level: AnnotationLevel,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    StepFinished {
        index: usize,
        /// Result before `continue-on-error` is applied.
        outcome: Outcome,
        /// Result after `continue-on-error` is applied.
        conclusion: Outcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        duration_ms: u64,
        at: String,
    },
    /// A chunk of an uploaded artifact (a tar stream, base64 encoded).
    ArtifactChunk { name: String, data: String },
    ArtifactEnd {
        name: String,
        files: u64,
        bytes: u64,
    },
    /// Content a step appended to `GITHUB_STEP_SUMMARY`.
    Summary { step: usize, markdown: String },
    JobFinished {
        result: Outcome,
        #[serde(default)]
        outputs: Map<String, Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        at: String,
    },
}

impl Event {
    /// Serialize as one line of JSON (without the trailing newline).
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("events always serialize")
    }

    pub fn from_line(line: &str) -> Result<Event, serde_json::Error> {
        serde_json::from_str(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_round_trip() {
        let events = vec![
            Event::Hello {
                protocol: PROTOCOL_VERSION,
                agent_version: "0.1.0".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
            },
            Event::StepStarted {
                index: 1,
                name: "Run tests".into(),
                phase: StepPhase::Main,
                at: "2026-09-27T10:00:00Z".into(),
            },
            Event::Log {
                step: 1,
                line: "ok".into(),
            },
            Event::StepFinished {
                index: 1,
                outcome: Outcome::Failure,
                conclusion: Outcome::Success,
                exit_code: Some(2),
                duration_ms: 1200,
                at: "2026-09-27T10:00:01Z".into(),
            },
            Event::JobFinished {
                result: Outcome::Success,
                outputs: Map::new(),
                error: None,
                at: "2026-09-27T10:00:02Z".into(),
            },
        ];
        for e in events {
            let line = e.to_line();
            assert!(!line.contains('\n'));
            assert_eq!(Event::from_line(&line).unwrap(), e);
        }
    }

    #[test]
    fn event_tag_is_snake_case() {
        let line = Event::Log {
            step: 0,
            line: "x".into(),
        }
        .to_line();
        assert!(line.contains(r#""type":"log""#), "{line}");
    }
}
