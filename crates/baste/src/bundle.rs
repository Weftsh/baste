//! Building a job bundle: the job spec plus every file the runner needs
//! (actions, checkout packs, artifacts), so the job runs without talking to
//! GitHub for anything but what the steps themselves do.

use crate::git::Git;
use crate::github::GitHub;
use crate::plan::PlannedWorkflow;
use crate::secrets::Secrets;
use crate::store::JobRecord;
use anyhow::{anyhow, Context, Result};
use baste_protocol::{
    ActionSource, ArtifactSource, CacheSource, CheckoutPack, CheckoutSource, JobSpec, RunDefaults,
    RunnerInfo, PROTOCOL_VERSION,
};
use baste_workflow::{scalar_string, ActionMeta, Job, Uses};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Downloads actions once and keeps them in the cache, keyed by commit.
pub struct ActionCache {
    dir: PathBuf,
    api: Option<GitHub>,
    resolved: Mutex<HashMap<String, PathBuf>>,
    /// Held while downloading, so jobs starting together fetch an action once.
    downloading: Mutex<()>,
}

impl ActionCache {
    pub fn new(api: Option<GitHub>) -> ActionCache {
        ActionCache {
            dir: crate::config::cache_dir().join("actions"),
            api,
            resolved: Mutex::new(HashMap::new()),
            downloading: Mutex::new(()),
        }
    }

    /// Directory holding `owner/repo` at `git_ref`, downloading it if needed.
    pub fn ensure(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
        log: &mut dyn FnMut(&str),
    ) -> Result<PathBuf> {
        let key = format!("{owner}/{repo}@{git_ref}").to_ascii_lowercase();
        if let Some(p) = self.resolved.lock().unwrap().get(&key) {
            return Ok(p.clone());
        }
        let base = self
            .dir
            .join(owner.to_ascii_lowercase())
            .join(repo.to_ascii_lowercase());
        let ref_file = base.join("refs").join(crate::store::slug(git_ref));
        let is_sha = git_ref.len() == 40 && git_ref.chars().all(|c| c.is_ascii_hexdigit());
        let sha = if is_sha {
            git_ref.to_string()
        } else {
            let resolved = self
                .api
                .as_ref()
                .ok_or_else(|| anyhow!("no GitHub access to resolve {owner}/{repo}@{git_ref}"))
                .and_then(|api| api.resolve_ref(owner, repo, git_ref));
            match resolved {
                Ok(s) => {
                    let _ = std::fs::create_dir_all(ref_file.parent().unwrap());
                    let _ = std::fs::write(&ref_file, &s);
                    s
                }
                Err(e) => match std::fs::read_to_string(&ref_file) {
                    Ok(s) => {
                        log(&format!("Using cached {owner}/{repo}@{git_ref} ({e})"));
                        s.trim().to_string()
                    }
                    Err(_) => {
                        return Err(e.context(format!("resolving action {owner}/{repo}@{git_ref}")))
                    }
                },
            }
        };
        let dir = base.join(&sha);
        let _one_at_a_time = self.downloading.lock().unwrap();
        if !dir.join(crate::github::COMPLETE_MARKER).exists() {
            log(&format!(
                "Downloading action {owner}/{repo}@{git_ref} ({})",
                &sha[..12]
            ));
            let api = self
                .api
                .as_ref()
                .ok_or_else(|| anyhow!("no GitHub access to download {owner}/{repo}"))?;
            std::fs::create_dir_all(&base)?;
            api.download_repo(owner, repo, &sha, &dir)?;
        }
        self.resolved.lock().unwrap().insert(key, dir.clone());
        Ok(dir)
    }
}

/// Every remote action a job uses, including those used by composite actions
/// (remote or local), as `(owner, repo, ref)`.
/// What the gate action reports in a local run.
const LOCAL_GATE_SCRIPT: &str =
    "echo \"This is Baste's local run, so the gate lets every job run.\"\n\
echo skip=false >> \"$GITHUB_OUTPUT\"\n\
echo result=local >> \"$GITHUB_OUTPUT\"\n";

fn is_gate_action(uses: &str) -> bool {
    matches!(
        Uses::parse(uses),
        Ok(Uses::Remote { owner, repo, path, .. })
            if owner.eq_ignore_ascii_case("weftsh")
                && repo.eq_ignore_ascii_case("baste")
                && path.as_deref() == Some("gate")
    )
}

/// The job as a local run executes it. A step using Baste's gate action
/// (`weftsh/baste/gate`) becomes one that lets every job run: the gate skips
/// GitHub's copy of jobs that already passed locally, and asked from inside
/// a local run it would wait on that run's own pending checks. The step keeps
/// its `id`, `name` and `if`, so `steps.<id>.outputs.skip` works as usual.
pub fn for_local_run(job: &Job) -> Job {
    let mut job = job.clone();
    for step in &mut job.steps {
        let gate = step
            .get("uses")
            .and_then(scalar_string)
            .is_some_and(|u| is_gate_action(&u));
        let Some(fields) = step.as_object_mut().filter(|_| gate) else {
            continue;
        };
        fields.remove("uses");
        fields.remove("with");
        fields.entry("name").or_insert_with(|| json!("Baste gate"));
        fields.insert("shell".into(), json!("bash"));
        fields.insert("run".into(), json!(LOCAL_GATE_SCRIPT));
    }
    job
}

pub fn collect_actions(
    job: &Job,
    git: &Git,
    checkout_sha: &str,
    cache: &ActionCache,
    log: &mut dyn FnMut(&str),
) -> Result<Vec<(String, String, String, PathBuf)>> {
    let mut out: Vec<(String, String, String, PathBuf)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut todo: Vec<String> = job
        .steps
        .iter()
        .filter_map(|s| s.get("uses").and_then(scalar_string))
        .collect();
    let mut local_seen: BTreeSet<String> = BTreeSet::new();
    while let Some(uses) = todo.pop() {
        let Ok(parsed) = Uses::parse(&uses) else {
            continue;
        };
        match &parsed {
            Uses::Remote {
                owner,
                repo,
                path,
                git_ref,
            } => {
                // The agent always handles actions/cache itself (Baste's own
                // cache store), so it needs no download.
                if owner.eq_ignore_ascii_case("actions")
                    && repo.eq_ignore_ascii_case("cache")
                    && matches!(path.as_deref(), None | Some("restore" | "save"))
                {
                    continue;
                }
                let key = parsed.repo_ref().unwrap().to_ascii_lowercase();
                if !seen.insert(format!("{key}/{}", path.clone().unwrap_or_default())) {
                    continue;
                }
                let dir = cache.ensure(owner, repo, git_ref, log)?;
                if !out
                    .iter()
                    .any(|(o, r, g, _)| format!("{o}/{r}@{g}").to_ascii_lowercase() == key)
                {
                    out.push((owner.clone(), repo.clone(), git_ref.clone(), dir.clone()));
                }
                let action_dir = match path {
                    Some(p) => dir.join(p),
                    None => dir,
                };
                if let Ok(meta) = ActionMeta::load(&action_dir) {
                    todo.extend(meta.nested_uses());
                }
            }
            Uses::Local { path } => {
                if !local_seen.insert(path.clone()) {
                    continue;
                }
                let rel = path.trim_start_matches("./").trim_end_matches('/');
                let meta_src = ["action.yml", "action.yaml"].iter().find_map(|f| {
                    let p = if rel.is_empty() {
                        f.to_string()
                    } else {
                        format!("{rel}/{f}")
                    };
                    git.show(checkout_sha, &p).ok().flatten()
                });
                if let Some(src) = meta_src {
                    if let Ok(meta) = ActionMeta::parse(&src) {
                        todo.extend(meta.nested_uses());
                    }
                }
            }
            Uses::Docker { .. } => {}
        }
    }
    Ok(out)
}

/// Checkout fetch depths a job asks for (0 = full history).
pub fn checkout_depths(job: &Job) -> BTreeSet<u32> {
    let mut depths = BTreeSet::new();
    depths.insert(1);
    for s in &job.steps {
        let uses = s.get("uses").and_then(scalar_string).unwrap_or_default();
        if !uses.to_ascii_lowercase().starts_with("actions/checkout@") {
            continue;
        }
        match s.pointer("/with/fetch-depth") {
            None => {}
            Some(v) => match scalar_string(v).and_then(|s| s.trim().parse::<u32>().ok()) {
                Some(d) => {
                    depths.insert(d);
                }
                None => {
                    depths.insert(0);
                }
            },
        }
    }
    depths
}

/// Builds and caches checkout packs per (commit, depth) for a run.
/// A pack file and its shallow boundary commits.
pub type Pack = (PathBuf, Vec<String>);

pub struct PackCache {
    dir: PathBuf,
    built: Mutex<HashMap<(String, u32), Pack>>,
}

impl PackCache {
    pub fn new(dir: PathBuf) -> PackCache {
        PackCache {
            dir,
            built: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, git: &Git, sha: &str, depth: u32) -> Result<Pack> {
        let mut built = self.built.lock().unwrap();
        if let Some(p) = built.get(&(sha.to_string(), depth)) {
            return Ok(p.clone());
        }
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("{sha}-{depth}.pack"));
        let shallow = git
            .write_pack(sha, depth, &path)
            .with_context(|| format!("packing {sha} (depth {depth})"))?;
        built.insert((sha.to_string(), depth), (path.clone(), shallow.clone()));
        Ok((path, shallow))
    }
}

/// Every secret name a job references (`secrets.X`), in first-seen order.
pub fn referenced_secrets(wf_env: &Map<String, Value>, job: &Job) -> Vec<String> {
    fn walk(v: &Value, key: Option<&str>, out: &mut Vec<String>) {
        match v {
            Value::String(s) => {
                let bare = key == Some("if") && !s.contains("${{");
                for name in baste_expr::referenced_secrets(s, bare) {
                    if !out.contains(&name) {
                        out.push(name);
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, None, out)),
            Value::Object(o) => o.iter().for_each(|(k, x)| walk(x, Some(k), out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(&Value::Object(wf_env.clone()), None, &mut out);
    walk(&Value::Object(job.env.clone()), None, &mut out);
    for s in &job.steps {
        walk(s, None, &mut out);
    }
    out
}

pub struct SecretValues {
    pub values: Map<String, Value>,
    pub missing: Vec<String>,
}

/// Look up referenced secrets in the keychain. `GITHUB_TOKEN` is the gh token.
pub fn resolve_secrets(
    names: &[String],
    secrets: Option<&Secrets>,
    token: &str,
) -> Result<SecretValues> {
    let mut values = Map::new();
    let mut missing = Vec::new();
    values.insert("GITHUB_TOKEN".into(), Value::String(token.to_string()));
    for name in names {
        if name.eq_ignore_ascii_case("GITHUB_TOKEN") {
            continue;
        }
        let found = match secrets {
            Some(s) => s.get(name)?,
            None => None,
        };
        match found {
            Some(v) => {
                values.insert(name.clone(), Value::String(v));
            }
            None => missing.push(name.clone()),
        }
    }
    Ok(SecretValues { values, missing })
}

/// Hard-link a file, or copy it when linking isn't possible.
pub fn link_or_copy(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(to);
    if std::fs::hard_link(from, to).is_err() {
        std::fs::copy(from, to).with_context(|| format!("copying {}", from.display()))?;
    }
    Ok(())
}

pub fn link_or_copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let dest = to.join(entry.file_name());
        if ft.is_dir() {
            link_or_copy_dir(&entry.path(), &dest)?;
        } else if ft.is_symlink() {
            let target = std::fs::read_link(entry.path())?;
            let _ = std::fs::remove_file(&dest);
            std::os::unix::fs::symlink(target, &dest)?;
        } else {
            link_or_copy(&entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// Everything needed to write one job's bundle.
pub struct BundleInput<'a> {
    pub run_id: &'a str,
    pub record: &'a JobRecord,
    pub planned: &'a PlannedWorkflow,
    pub job: &'a Job,
    pub strategy: Value,
    pub needs: Value,
    pub runner: RunnerInfo,
    pub secrets: SecretValues,
    pub actions: Vec<(String, String, String, PathBuf)>,
    pub packs: Vec<(u32, PathBuf, Vec<String>)>,
    pub artifacts: Vec<(String, PathBuf)>,
    /// Saved `actions/cache` entries the job may restore, newest first.
    pub caches: Vec<(crate::actions_cache::Entry, PathBuf)>,
    pub repository: String,
    pub server_url: String,
}

/// Write the bundle directory and return the spec that was written.
pub fn write_bundle(dir: &Path, input: BundleInput) -> Result<JobSpec> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir)?;
    let pw = input.planned;
    let job = input.job;

    let mut actions = Vec::new();
    for (owner, repo, git_ref, src) in &input.actions {
        let rel = format!("actions/{owner}/{repo}/{}", crate::store::slug(git_ref));
        link_or_copy_dir(src, &dir.join(&rel))?;
        actions.push(ActionSource {
            repo_ref: format!("{owner}/{repo}@{git_ref}"),
            path: rel,
        });
    }
    let mut packs = Vec::new();
    for (depth, path, shallow) in &input.packs {
        let rel = format!("checkout/depth-{depth}.pack");
        link_or_copy(path, &dir.join(&rel))?;
        packs.push(CheckoutPack {
            depth: *depth,
            file: rel,
            shallow: shallow.clone(),
        });
    }
    let mut artifacts = Vec::new();
    for (name, path) in &input.artifacts {
        let rel = format!("artifacts/{}.tar", crate::store::slug(name));
        link_or_copy(path, &dir.join(&rel))?;
        artifacts.push(ArtifactSource {
            name: name.clone(),
            file: rel,
        });
    }
    let mut caches = Vec::new();
    for (i, (entry, path)) in input.caches.iter().enumerate() {
        let rel = format!("caches/{i}.tgz");
        link_or_copy(path, &dir.join(&rel))?;
        caches.push(CacheSource {
            key: entry.key.clone(),
            version: entry.version.clone(),
            file: rel,
        });
    }
    std::fs::write(
        dir.join("event.json"),
        serde_json::to_vec_pretty(&pw.payload)?,
    )?;

    let mut contexts = Map::new();
    let mut github = pw.github.clone();
    github["job"] = json!(job.id);
    contexts.insert("github".into(), github);
    contexts.insert(
        "matrix".into(),
        input.record.matrix.clone().unwrap_or_else(|| json!({})),
    );
    contexts.insert("strategy".into(), input.strategy.clone());
    contexts.insert("needs".into(), input.needs.clone());
    contexts.insert("vars".into(), pw.vars.clone());
    contexts.insert("inputs".into(), json!({}));

    let timeout_minutes = match &job.timeout_minutes {
        None | Some(Value::Null) => None,
        Some(v) => {
            let env_ctx = contexts.clone();
            let env = baste_expr::Env {
                contexts: &env_ctx,
                functions: &baste_expr::NoFunctions,
            };
            let s = match v {
                Value::String(s) => {
                    baste_expr::interpolate(s, &env).map_err(|e| anyhow!("timeout-minutes: {e}"))?
                }
                other => scalar_string(other).unwrap_or_default(),
            };
            Some(
                s.trim()
                    .parse::<f64>()
                    .map_err(|_| anyhow!("timeout-minutes must be a number, got '{s}'"))?,
            )
        }
    };
    let defaults = job.defaults.or(&pw.workflow.defaults);
    let spec = JobSpec {
        protocol: PROTOCOL_VERSION,
        run_id: input.run_id.to_string(),
        job_key: input.record.key.clone(),
        job_id: job.id.clone(),
        job_name: input.record.name.clone(),
        workflow: baste_protocol::WorkflowInfo {
            name: pw.workflow.display_name(),
            file: pw.workflow.file.clone(),
        },
        workflow_env: pw.workflow.env.clone(),
        job_env: job.env.clone(),
        defaults: RunDefaults {
            shell: defaults.shell,
            working_directory: defaults.working_directory,
        },
        steps: job.steps.clone(),
        outputs: job.outputs.clone(),
        timeout_minutes,
        contexts,
        secrets: input.secrets.values,
        missing_secrets: input.secrets.missing,
        masks: vec![],
        actions,
        checkout: CheckoutSource {
            repository: input.repository,
            sha: pw.checkout_sha.clone(),
            git_ref: pw.checkout_ref.clone(),
            packs,
            server_url: input.server_url,
        },
        artifacts,
        caches,
        event_file: "event.json".into(),
        runner: input.runner,
    };
    write_spec(dir, &spec)?;
    Ok(spec)
}

fn write_spec(dir: &Path, spec: &JobSpec) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // The spec holds secret values: keep it private to the user.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(dir.join(baste_protocol::JOB_SPEC_FILE))?;
    f.write_all(&serde_json::to_vec(spec)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use baste_workflow::Workflow;

    #[test]
    fn finds_secrets_and_depths() {
        let wf = Workflow::parse(
            "ci.yml",
            r#"
on: push
env:
  A: ${{ secrets.WF_SECRET }}
jobs:
  build:
    runs-on: ubuntu-latest
    env:
      B: ${{ secrets.JOB_SECRET }}
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - run: echo ${{ secrets['STEP_SECRET'] }}
        if: secrets.COND_SECRET != ''
      - uses: some/action@v1
        with:
          token: ${{ secrets.GITHUB_TOKEN }}
"#,
        )
        .unwrap();
        let job = wf.job("build").unwrap();
        assert_eq!(
            referenced_secrets(&wf.env, job),
            vec![
                "WF_SECRET",
                "JOB_SECRET",
                "STEP_SECRET",
                "COND_SECRET",
                "GITHUB_TOKEN"
            ]
        );
        assert_eq!(
            checkout_depths(job).into_iter().collect::<Vec<_>>(),
            vec![0, 1]
        );
        let r = resolve_secrets(&referenced_secrets(&wf.env, job), None, "tok").unwrap();
        assert_eq!(
            r.missing,
            vec!["WF_SECRET", "JOB_SECRET", "STEP_SECRET", "COND_SECRET"]
        );
        assert_eq!(r.values["GITHUB_TOKEN"], json!("tok"));
    }
}
