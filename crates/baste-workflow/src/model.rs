//! Typed view of a workflow file.
//!
//! Anything that can hold an expression is kept as a raw JSON value or string
//! and evaluated later, when the contexts it needs exist.

use crate::trigger::Triggers;
use crate::yaml;
use serde_json::{Map, Value};
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowError {
    pub file: String,
    pub message: String,
}

impl fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.file, self.message)
    }
}

impl std::error::Error for WorkflowError {}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunDefaults {
    pub shell: Option<String>,
    pub working_directory: Option<String>,
}

impl RunDefaults {
    fn parse(v: Option<&Value>) -> RunDefaults {
        let run = v.and_then(|d| d.get("run"));
        RunDefaults {
            shell: run.and_then(|r| r.get("shell")).and_then(scalar_string),
            working_directory: run
                .and_then(|r| r.get("working-directory"))
                .and_then(scalar_string),
        }
    }

    /// `self` with unset fields filled from `fallback`.
    pub fn or(&self, fallback: &RunDefaults) -> RunDefaults {
        RunDefaults {
            shell: self.shell.clone().or_else(|| fallback.shell.clone()),
            working_directory: self
                .working_directory
                .clone()
                .or_else(|| fallback.working_directory.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Workflow {
    /// Path relative to the repository root, e.g. `.github/workflows/ci.yml`.
    pub file: String,
    pub name: Option<String>,
    pub on: Triggers,
    pub env: Map<String, Value>,
    pub defaults: RunDefaults,
    pub permissions: Option<Value>,
    /// Jobs in file order.
    pub jobs: Vec<Job>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Strategy {
    pub matrix: Option<Value>,
    pub fail_fast: Option<Value>,
    pub max_parallel: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub id: String,
    pub name: Option<String>,
    pub needs: Vec<String>,
    pub runs_on: Option<Value>,
    pub condition: Option<String>,
    pub env: Map<String, Value>,
    pub defaults: RunDefaults,
    pub outputs: Map<String, Value>,
    pub strategy: Strategy,
    pub timeout_minutes: Option<Value>,
    pub continue_on_error: Option<Value>,
    pub container: Option<Value>,
    pub services: Option<Value>,
    pub environment: Option<Value>,
    pub permissions: Option<Value>,
    /// A reusable workflow call (`jobs.<id>.uses`).
    pub uses: Option<String>,
    /// Steps as written, parsed on demand with [`Step::parse`].
    pub steps: Vec<Value>,
}

impl Workflow {
    pub fn parse(file: &str, src: &str) -> Result<Workflow, WorkflowError> {
        let err = |message: String| WorkflowError {
            file: file.to_string(),
            message,
        };
        let root = yaml::parse(src).map_err(|e| err(format!("invalid YAML: {e}")))?;
        let root = root
            .as_object()
            .ok_or_else(|| err("a workflow must be a mapping".into()))?;

        let on = root
            .get("on")
            .ok_or_else(|| err("missing 'on'".into()))
            .and_then(|v| Triggers::parse(v).map_err(err))?;

        let jobs_value = root
            .get("jobs")
            .and_then(Value::as_object)
            .ok_or_else(|| err("missing 'jobs'".into()))?;
        let mut jobs = Vec::new();
        for (id, v) in jobs_value {
            jobs.push(Job::parse(id, v).map_err(|e| err(format!("job '{id}': {e}")))?);
        }
        for job in &jobs {
            for need in &job.needs {
                if !jobs.iter().any(|j| &j.id == need) {
                    return Err(err(format!("job '{}' needs unknown job '{need}'", job.id)));
                }
            }
        }

        let wf = Workflow {
            file: file.to_string(),
            name: root.get("name").and_then(scalar_string),
            on,
            env: object_or_empty(root.get("env")),
            defaults: RunDefaults::parse(root.get("defaults")),
            permissions: root.get("permissions").cloned(),
            jobs,
        };
        wf.job_order().map_err(err)?;
        Ok(wf)
    }

    /// The name GitHub shows: `name:` or the file path.
    pub fn display_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.file.clone())
    }

    pub fn job(&self, id: &str) -> Option<&Job> {
        self.jobs.iter().find(|j| j.id == id)
    }

    /// Job ids in an order where every job comes after the jobs it needs.
    /// Ties keep file order. Fails on cycles.
    pub fn job_order(&self) -> Result<Vec<String>, String> {
        let mut done: Vec<String> = Vec::new();
        while done.len() < self.jobs.len() {
            let next = self
                .jobs
                .iter()
                .find(|j| !done.contains(&j.id) && j.needs.iter().all(|n| done.contains(n)));
            match next {
                Some(j) => done.push(j.id.clone()),
                None => {
                    let stuck: Vec<_> = self
                        .jobs
                        .iter()
                        .filter(|j| !done.contains(&j.id))
                        .map(|j| j.id.as_str())
                        .collect();
                    return Err(format!(
                        "dependency cycle between jobs: {}",
                        stuck.join(", ")
                    ));
                }
            }
        }
        Ok(done)
    }
}

impl Job {
    fn parse(id: &str, v: &Value) -> Result<Job, String> {
        let obj = v.as_object().ok_or("a job must be a mapping")?;
        let needs = match obj.get("needs") {
            None | Some(Value::Null) => vec![],
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(a)) => a
                .iter()
                .map(|n| scalar_string(n).ok_or("'needs' entries must be strings"))
                .collect::<Result<_, _>>()?,
            Some(_) => return Err("'needs' must be a string or a list".into()),
        };
        let strategy = match obj.get("strategy") {
            Some(Value::Object(s)) => Strategy {
                matrix: s.get("matrix").cloned(),
                fail_fast: s.get("fail-fast").cloned(),
                max_parallel: s.get("max-parallel").cloned(),
            },
            None | Some(Value::Null) => Strategy::default(),
            Some(_) => return Err("'strategy' must be a mapping".into()),
        };
        let steps = match obj.get("steps") {
            Some(Value::Array(a)) => a.clone(),
            None | Some(Value::Null) => vec![],
            Some(_) => return Err("'steps' must be a list".into()),
        };
        let uses = obj.get("uses").and_then(scalar_string);
        if uses.is_none() && steps.is_empty() {
            return Err("a job needs 'steps' or 'uses'".into());
        }
        for (i, s) in steps.iter().enumerate() {
            Step::parse(s).map_err(|e| format!("step {}: {e}", i + 1))?;
        }
        Ok(Job {
            id: id.to_string(),
            name: obj.get("name").and_then(scalar_string),
            needs,
            runs_on: obj.get("runs-on").cloned(),
            condition: obj.get("if").and_then(scalar_string),
            env: object_or_empty(obj.get("env")),
            defaults: RunDefaults::parse(obj.get("defaults")),
            outputs: object_or_empty(obj.get("outputs")),
            strategy,
            timeout_minutes: obj.get("timeout-minutes").cloned(),
            continue_on_error: obj.get("continue-on-error").cloned(),
            container: obj.get("container").filter(|v| !v.is_null()).cloned(),
            services: obj.get("services").filter(|v| !v.is_null()).cloned(),
            environment: obj.get("environment").filter(|v| !v.is_null()).cloned(),
            permissions: obj.get("permissions").cloned(),
            uses,
            steps,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Step {
    pub id: Option<String>,
    pub name: Option<String>,
    pub condition: Option<String>,
    pub uses: Option<String>,
    pub run: Option<String>,
    pub shell: Option<String>,
    pub with: Map<String, Value>,
    pub env: Map<String, Value>,
    pub working_directory: Option<String>,
    pub continue_on_error: Option<Value>,
    pub timeout_minutes: Option<Value>,
}

impl Step {
    pub fn parse(v: &Value) -> Result<Step, String> {
        let obj = v.as_object().ok_or("a step must be a mapping")?;
        let step = Step {
            id: obj.get("id").and_then(scalar_string),
            name: obj.get("name").and_then(scalar_string),
            condition: obj.get("if").and_then(scalar_string),
            uses: obj.get("uses").and_then(scalar_string),
            run: obj.get("run").and_then(scalar_string),
            shell: obj.get("shell").and_then(scalar_string),
            with: object_or_empty(obj.get("with")),
            env: object_or_empty(obj.get("env")),
            working_directory: obj.get("working-directory").and_then(scalar_string),
            continue_on_error: obj.get("continue-on-error").cloned(),
            timeout_minutes: obj.get("timeout-minutes").cloned(),
        };
        match (&step.uses, &step.run) {
            (Some(_), Some(_)) => Err("a step can't have both 'uses' and 'run'".into()),
            (None, None) => Err("a step needs 'uses' or 'run'".into()),
            _ => Ok(step),
        }
    }

    /// The name GitHub shows for the step when `name:` is not set.
    pub fn default_name(&self) -> String {
        if let Some(uses) = &self.uses {
            return format!("Run {uses}");
        }
        let first = self
            .run
            .as_deref()
            .unwrap_or("")
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        format!("Run {first}")
    }
}

/// A parsed `uses:` reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Uses {
    /// `./path/to/action` in the workspace.
    Local { path: String },
    /// `docker://image:tag`
    Docker { image: String },
    /// `owner/repo[/path]@ref`
    Remote {
        owner: String,
        repo: String,
        path: Option<String>,
        git_ref: String,
    },
}

impl Uses {
    pub fn parse(s: &str) -> Result<Uses, String> {
        let s = s.trim();
        if s.starts_with("./") || s == "." {
            return Ok(Uses::Local {
                path: s.to_string(),
            });
        }
        if let Some(image) = s.strip_prefix("docker://") {
            return Ok(Uses::Docker {
                image: image.to_string(),
            });
        }
        let (target, git_ref) = s
            .rsplit_once('@')
            .ok_or_else(|| format!("'{s}' is missing a version, e.g. '{s}@v1'"))?;
        let mut parts = target.splitn(3, '/');
        let owner = parts.next().unwrap_or_default();
        let repo = parts.next().unwrap_or_default();
        if owner.is_empty() || repo.is_empty() || git_ref.is_empty() {
            return Err(format!("'{s}' is not a valid action reference"));
        }
        Ok(Uses::Remote {
            owner: owner.to_string(),
            repo: repo.to_string(),
            path: parts.next().map(str::to_string).filter(|p| !p.is_empty()),
            git_ref: git_ref.to_string(),
        })
    }

    /// `owner/repo@ref` for remote actions.
    pub fn repo_ref(&self) -> Option<String> {
        match self {
            Uses::Remote {
                owner,
                repo,
                git_ref,
                ..
            } => Some(format!("{owner}/{repo}@{git_ref}")),
            _ => None,
        }
    }

    /// `owner/repo` (lowercase) for remote actions.
    pub fn repository(&self) -> Option<String> {
        match self {
            Uses::Remote { owner, repo, .. } => {
                Some(format!("{owner}/{repo}").to_ascii_lowercase())
            }
            _ => None,
        }
    }
}

/// A scalar as a string: strings as-is, numbers and booleans formatted.
pub fn scalar_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(match n.as_f64() {
            Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
            _ => n.to_string(),
        }),
        _ => None,
    }
}

fn object_or_empty(v: Option<&Value>) -> Map<String, Value> {
    v.and_then(Value::as_object).cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CI: &str = r#"
name: CI
on:
  push:
    branches: [main]
  pull_request:
env:
  GLOBAL: 1
defaults:
  run:
    shell: bash
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: |
          make
          make test
  test:
    needs: build
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: [ubuntu-latest, windows-latest]
    steps:
      - name: Test
        run: echo hi
        if: success()
"#;

    #[test]
    fn parses_a_workflow() {
        let wf = Workflow::parse(".github/workflows/ci.yml", CI).unwrap();
        assert_eq!(wf.display_name(), "CI");
        assert_eq!(wf.jobs.len(), 2);
        assert_eq!(wf.env["GLOBAL"], json!(1));
        assert_eq!(wf.defaults.shell.as_deref(), Some("bash"));
        let test = wf.job("test").unwrap();
        assert_eq!(test.needs, vec!["build"]);
        assert!(test.strategy.matrix.is_some());
        let step = Step::parse(&test.steps[0]).unwrap();
        assert_eq!(step.condition.as_deref(), Some("success()"));
        let build = wf.job("build").unwrap();
        assert_eq!(
            Step::parse(&build.steps[1]).unwrap().default_name(),
            "Run make"
        );
        assert_eq!(
            Step::parse(&build.steps[0]).unwrap().default_name(),
            "Run actions/checkout@v4"
        );
        assert_eq!(wf.job_order().unwrap(), vec!["build", "test"]);
    }

    #[test]
    fn rejects_bad_workflows() {
        let e = Workflow::parse(
            "x.yml",
            "on: push\njobs:\n  a:\n    needs: b\n    runs-on: x\n    steps: [{run: x}]\n",
        )
        .unwrap_err();
        assert!(e.message.contains("unknown job 'b'"), "{e}");
        let e = Workflow::parse(
            "x.yml",
            "on: push\njobs:\n  a:\n    needs: b\n    steps: [{run: x}]\n  b:\n    needs: a\n    steps: [{run: x}]\n",
        )
        .unwrap_err();
        assert!(e.message.contains("cycle"), "{e}");
        let e = Workflow::parse(
            "x.yml",
            "on: push\njobs:\n  a:\n    steps: [{run: x, uses: y@v1}]\n",
        )
        .unwrap_err();
        assert!(e.message.contains("both"), "{e}");
        assert!(Workflow::parse("x.yml", "jobs: {}").is_err());
    }

    #[test]
    fn parses_uses() {
        assert_eq!(
            Uses::parse("actions/checkout@v4").unwrap(),
            Uses::Remote {
                owner: "actions".into(),
                repo: "checkout".into(),
                path: None,
                git_ref: "v4".into()
            }
        );
        assert_eq!(
            Uses::parse("github/codeql-action/init@v3")
                .unwrap()
                .repo_ref(),
            Some("github/codeql-action@v3".into())
        );
        assert!(matches!(
            Uses::parse("./.github/actions/x").unwrap(),
            Uses::Local { .. }
        ));
        assert!(matches!(
            Uses::parse("docker://alpine:3").unwrap(),
            Uses::Docker { .. }
        ));
        assert!(Uses::parse("actions/checkout").is_err());
    }

    #[test]
    fn scalar_strings() {
        assert_eq!(scalar_string(&json!(20)), Some("20".into()));
        assert_eq!(scalar_string(&json!(3.1)), Some("3.1".into()));
        assert_eq!(scalar_string(&json!(true)), Some("true".into()));
        assert_eq!(scalar_string(&json!(null)), None);
    }
}
