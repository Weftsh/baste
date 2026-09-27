//! Turning a workflow job into concrete job instances and deciding where each runs.

use crate::matrix::{self, Combination};
use crate::model::{Job, Workflow};
use baste_expr::{Env, Map, NoFunctions, Value};

/// Where a job instance runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// In a local Linux VM.
    Local,
    /// Left to GitHub, with the reason shown to the developer.
    GitHub(String),
}

/// One concrete job: a job, or one combination of its matrix.
#[derive(Debug, Clone, PartialEq)]
pub struct JobInstance {
    pub job_id: String,
    /// Display name, e.g. `test (ubuntu-latest, 20)`.
    pub name: String,
    pub matrix: Option<Combination>,
    /// The `strategy` context for this instance.
    pub strategy: Value,
    pub runs_on: Vec<String>,
    pub placement: Placement,
}

/// Labels from a `runs-on` value (string, list, or `{group, labels}`).
pub fn runs_on_labels(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.trim().to_string()],
        Value::Array(items) => items
            .iter()
            .filter_map(|i| i.as_str().map(|s| s.trim().to_string()))
            .collect(),
        Value::Object(o) => {
            let mut labels = runs_on_labels(o.get("labels").unwrap_or(&Value::Null));
            if let Some(g) = o.get("group").and_then(Value::as_str) {
                labels.insert(0, format!("group:{g}"));
            }
            labels
        }
        _ => vec![],
    }
}

/// Decide whether labels describe a runner Baste can stand in for.
///
/// Only GitHub-hosted Ubuntu runners (`ubuntu-*`) run locally; everything else
/// (Windows, macOS, self-hosted, runner groups) is handed to GitHub.
pub fn classify_labels(labels: &[String]) -> Placement {
    if labels.is_empty() {
        return Placement::GitHub("the job has no 'runs-on'".into());
    }
    let lower: Vec<String> = labels.iter().map(|l| l.to_ascii_lowercase()).collect();
    if lower
        .iter()
        .any(|l| l == "self-hosted" || l.starts_with("group:"))
    {
        return Placement::GitHub(format!("'{}' is a self-hosted runner", labels.join(", ")));
    }
    if lower.len() == 1 && lower[0].starts_with("ubuntu-") {
        return Placement::Local;
    }
    if let Some(l) = lower
        .iter()
        .find(|l| l.starts_with("windows") || l.starts_with("macos"))
    {
        return Placement::GitHub(format!("{l} jobs run on GitHub"));
    }
    Placement::GitHub(format!(
        "'{}' is not a GitHub-hosted Ubuntu runner",
        labels.join(", ")
    ))
}

/// Features Baste v1 does not run locally. Such jobs are handed to GitHub
/// before the run starts rather than half-run.
pub fn unsupported_reason(wf: &Workflow, job: &Job) -> Option<String> {
    if job.uses.is_some() {
        return Some("calls a reusable workflow".into());
    }
    if job.services.is_some() {
        return Some("uses service containers (not yet supported locally)".into());
    }
    if job.container.is_some() {
        return Some("runs in a job container (not yet supported locally)".into());
    }
    if job.environment.is_some() {
        return Some("deploys to an environment".into());
    }
    let wants_oidc = |p: &Option<Value>| {
        p.as_ref()
            .and_then(|p| p.get("id-token"))
            .and_then(Value::as_str)
            .is_some_and(|v| v == "write")
    };
    if wants_oidc(&job.permissions) || wants_oidc(&wf.permissions) {
        return Some("requests an OIDC token".into());
    }
    None
}

/// Whether expanding the job needs outputs of the jobs it depends on
/// (a dynamic matrix, `runs-on` or name).
pub fn depends_on_needs(job: &Job) -> bool {
    let mut texts: Vec<String> = Vec::new();
    collect_strings(job.strategy.matrix.as_ref(), &mut texts);
    collect_strings(job.runs_on.as_ref(), &mut texts);
    collect_strings(job.strategy.fail_fast.as_ref(), &mut texts);
    collect_strings(job.strategy.max_parallel.as_ref(), &mut texts);
    if let Some(n) = &job.name {
        texts.push(n.clone());
    }
    texts.iter().any(|t| {
        baste_expr::referenced_properties(t, false)
            .iter()
            .any(|(ctx, _)| ctx == "needs")
    })
}

fn collect_strings(v: Option<&Value>, out: &mut Vec<String>) {
    match v {
        Some(Value::String(s)) => out.push(s.clone()),
        Some(Value::Array(a)) => a.iter().for_each(|x| collect_strings(Some(x), out)),
        Some(Value::Object(o)) => o.values().for_each(|x| collect_strings(Some(x), out)),
        _ => {}
    }
}

/// Evaluate every string leaf of `v` as a template. A leaf that is exactly one
/// `${{ }}` keeps the expression's type (so `${{ fromJSON(x) }}` yields a list).
pub fn evaluate_tree(v: &Value, env: &Env) -> Result<Value, baste_expr::Error> {
    Ok(match v {
        Value::String(s) => baste_expr::interpolate_value(s, env)?,
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|x| evaluate_tree(x, env))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(o) => {
            let mut m = Map::new();
            for (k, x) in o {
                m.insert(k.clone(), evaluate_tree(x, env)?);
            }
            Value::Object(m)
        }
        other => other.clone(),
    })
}

/// Expand `job` into instances. `contexts` holds the contexts available to
/// job-level keys: `github`, `needs`, `vars`, `inputs`.
pub fn expand_job(
    wf: &Workflow,
    job: &Job,
    contexts: &Map<String, Value>,
) -> Result<Vec<JobInstance>, String> {
    let env = Env {
        contexts,
        functions: &NoFunctions,
    };
    let err = |what: &str, e: baste_expr::Error| format!("evaluating {what}: {e}");

    let combos: Vec<Option<Combination>> = match &job.strategy.matrix {
        None | Some(Value::Null) => vec![None],
        Some(m) => {
            let evaluated = evaluate_tree(m, &env).map_err(|e| err("strategy.matrix", e))?;
            matrix::expand(&evaluated)?.into_iter().map(Some).collect()
        }
    };
    let fail_fast = match &job.strategy.fail_fast {
        None | Some(Value::Null) => true,
        Some(v) => baste_expr::is_truthy(
            &evaluate_tree(v, &env).map_err(|e| err("strategy.fail-fast", e))?,
        ),
    };
    let max_parallel = match &job.strategy.max_parallel {
        None | Some(Value::Null) => None,
        Some(v) => {
            let v = evaluate_tree(v, &env).map_err(|e| err("strategy.max-parallel", e))?;
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                .map(|n| n.max(1.0) as u64)
        }
    };
    let total = combos.len();
    let unsupported = unsupported_reason(wf, job);

    let mut out = Vec::with_capacity(total);
    for (index, combo) in combos.into_iter().enumerate() {
        let strategy = serde_json::json!({
            "fail-fast": fail_fast,
            "job-index": index,
            "job-total": total,
            "max-parallel": max_parallel.unwrap_or(total as u64),
        });
        let mut ctx = contexts.clone();
        ctx.insert(
            "matrix".into(),
            Value::Object(combo.clone().unwrap_or_default()),
        );
        ctx.insert("strategy".into(), strategy.clone());
        let env = Env {
            contexts: &ctx,
            functions: &NoFunctions,
        };

        let runs_on = match &job.runs_on {
            Some(v) => runs_on_labels(&evaluate_tree(v, &env).map_err(|e| err("runs-on", e))?),
            None => vec![],
        };
        let name = match &job.name {
            Some(n) => baste_expr::interpolate(n, &env).map_err(|e| err("name", e))?,
            None => default_name(&job.id, combo.as_ref()),
        };
        let placement = match (&unsupported, classify_labels(&runs_on)) {
            (_, Placement::GitHub(r)) => Placement::GitHub(r),
            (Some(r), Placement::Local) => Placement::GitHub(format!("the job {r}")),
            (None, Placement::Local) => Placement::Local,
        };
        out.push(JobInstance {
            job_id: job.id.clone(),
            name,
            matrix: combo,
            strategy,
            runs_on,
            placement,
        });
    }
    Ok(out)
}

/// GitHub's default name for a job: the id, plus matrix values in parentheses.
pub fn default_name(job_id: &str, combo: Option<&Combination>) -> String {
    match combo {
        Some(c) if !c.is_empty() => {
            let values: Vec<String> = c
                .values()
                .map(|v| match v {
                    Value::Object(_) | Value::Array(_) => v.to_string(),
                    other => baste_expr::to_display_string(other),
                })
                .collect();
            format!("{job_id} ({})", values.join(", "))
        }
        _ => job_id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wf(src: &str) -> Workflow {
        Workflow::parse(".github/workflows/ci.yml", src).unwrap()
    }

    fn ctx() -> Map<String, Value> {
        json!({"github": {"event_name": "push"}, "needs": {}, "vars": {}, "inputs": {}})
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn classifies_runners() {
        let l = |v: Value| classify_labels(&runs_on_labels(&v));
        assert_eq!(l(json!("ubuntu-latest")), Placement::Local);
        assert_eq!(l(json!("ubuntu-24.04")), Placement::Local);
        assert_eq!(l(json!(["ubuntu-22.04"])), Placement::Local);
        assert!(
            matches!(l(json!("windows-latest")), Placement::GitHub(r) if r.contains("windows-latest"))
        );
        assert!(matches!(l(json!("macos-14")), Placement::GitHub(_)));
        assert!(matches!(
            l(json!(["self-hosted", "linux"])),
            Placement::GitHub(_)
        ));
        assert!(matches!(
            l(json!({"group": "big", "labels": ["ubuntu-latest"]})),
            Placement::GitHub(_)
        ));
        assert!(matches!(l(json!(null)), Placement::GitHub(_)));
    }

    #[test]
    fn expands_matrix_with_mixed_os() {
        let w = wf(r#"
on: push
jobs:
  test:
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: [ubuntu-latest, windows-latest]
        node: [20]
    steps: [{run: npm test}]
"#);
        let inst = expand_job(&w, &w.jobs[0], &ctx()).unwrap();
        assert_eq!(inst.len(), 2);
        assert_eq!(inst[0].name, "test (ubuntu-latest, 20)");
        assert_eq!(inst[0].placement, Placement::Local);
        assert!(matches!(inst[1].placement, Placement::GitHub(_)));
        assert_eq!(inst[1].strategy["job-index"], json!(1));
        assert_eq!(inst[1].strategy["job-total"], json!(2));
    }

    #[test]
    fn dynamic_matrix_from_needs() {
        let w = wf(r#"
on: push
jobs:
  setup:
    runs-on: ubuntu-latest
    outputs:
      versions: ${{ steps.v.outputs.list }}
    steps: [{id: v, run: echo}]
  test:
    needs: setup
    name: Test on ${{ matrix.version }}
    runs-on: ubuntu-latest
    strategy:
      matrix:
        version: ${{ fromJSON(needs.setup.outputs.versions) }}
    steps: [{run: x}]
"#);
        let test = w.job("test").unwrap();
        assert!(depends_on_needs(test));
        assert!(!depends_on_needs(w.job("setup").unwrap()));
        let mut c = ctx();
        c.insert(
            "needs".into(),
            json!({"setup": {"result": "success", "outputs": {"versions": "[18, 20]"}}}),
        );
        let inst = expand_job(&w, test, &c).unwrap();
        assert_eq!(
            inst.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            vec!["Test on 18", "Test on 20"]
        );
    }

    #[test]
    fn unsupported_features_hand_off() {
        let w = wf(r#"
on: push
jobs:
  db:
    runs-on: ubuntu-latest
    services:
      postgres:
        image: postgres
    steps: [{run: x}]
  deploy:
    runs-on: ubuntu-latest
    environment: production
    steps: [{run: x}]
  plain:
    runs-on: ubuntu-latest
    steps: [{run: x}]
"#);
        for (id, local) in [("db", false), ("deploy", false), ("plain", true)] {
            let inst = expand_job(&w, w.job(id).unwrap(), &ctx()).unwrap();
            assert_eq!(inst[0].placement == Placement::Local, local, "{id}");
        }
    }
}
