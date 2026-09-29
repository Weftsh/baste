//! Deciding what a push runs: which workflows trigger, for which event, and
//! which of their jobs run locally or are handed to GitHub.

use crate::git::Git;
use crate::github::GitHub;
use crate::store::{JobRecord, JobState, PullRequestInfo, Run, Store};
use anyhow::{bail, Result};
use baste_workflow::{depends_on_needs, expand_job, Job, JobInstance, Placement, Workflow};
use serde_json::{json, Map, Value};

pub const WORKFLOW_DIR: &str = ".github/workflows";
pub const CONTEXT_PREFIX: &str = "baste";
pub const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

/// A workflow that will run, with the event it runs for.
#[derive(Debug, Clone)]
pub struct PlannedWorkflow {
    pub workflow: Workflow,
    /// `push` or `pull_request`.
    pub event: String,
    /// The commit the VM checks out and the ref it is checked out as.
    pub checkout_sha: String,
    pub checkout_ref: String,
    /// The `github` context shared by every job of this workflow.
    pub github: Value,
    pub payload: Value,
    pub vars: Value,
}

/// An open pull request for the pushed branch.
struct PrPlan {
    raw: Value,
    info: PullRequestInfo,
    /// The local test merge commit, if the merge is clean.
    merge: Option<String>,
    changed: Option<Vec<String>>,
}

/// Facts gathered once per run.
pub struct RunFacts {
    pub actor: String,
    pub repository: Value,
    pub default_branch: Option<String>,
    pub vars: Map<String, Value>,
}

/// The status context for a job.
pub fn status_context(workflow: &str, job_name: &str) -> String {
    format!("{CONTEXT_PREFIX}/{workflow}/{job_name}")
}

pub fn gather_facts(run: &mut Run, git: &Git, api: Option<&GitHub>) -> RunFacts {
    let mut facts = RunFacts {
        actor: git
            .try_run(&["config", "user.name"])
            .unwrap_or_else(|| "baste".into()),
        repository: json!({
            "full_name": run.repo.full_name(),
            "name": run.repo.name,
            "owner": {"login": run.repo.owner},
            "html_url": format!("{}/{}", run.repo.server_url(), run.repo.full_name()),
            "clone_url": format!("{}/{}.git", run.repo.server_url(), run.repo.full_name()),
        }),
        default_branch: None,
        vars: Map::new(),
    };
    if let Some(api) = api {
        if let Ok(user) = api.current_user() {
            if let Some(login) = user.get("login").and_then(Value::as_str) {
                facts.actor = login.to_string();
            }
        }
        match api.repo_info() {
            Ok(info) => {
                facts.default_branch = Some(info.default_branch.clone());
                facts.repository = info.raw;
            }
            Err(e) => run
                .notes
                .push(format!("Couldn't read repository details: {e:#}")),
        }
        match api.variables() {
            Ok(v) => facts.vars = v,
            Err(_) => {
                // Reading variables needs admin-level access; jobs just see an empty `vars`.
            }
        }
    }
    facts
}

/// Plan `run`: pick workflows and events and create its job records.
pub fn plan(
    run: &mut Run,
    git: &Git,
    api: Option<&GitHub>,
    store: &Store,
    facts: &RunFacts,
) -> Result<Vec<PlannedWorkflow>> {
    let sha = run.sha.clone();
    let mut files: Vec<String> = git
        .ls_tree(&sha, WORKFLOW_DIR)
        .unwrap_or_default()
        .into_iter()
        .filter(|f| f.ends_with(".yml") || f.ends_with(".yaml"))
        .collect();
    files.sort();
    if !run.only_workflows.is_empty() {
        files.retain(|f| {
            run.only_workflows.iter().any(|w| {
                f == w || f.ends_with(&format!("/{w}")) || f.ends_with(&format!("/{w}.yml"))
            })
        });
    }
    if files.is_empty() && !run.only_workflows.is_empty() {
        bail!(
            "no workflow in {WORKFLOW_DIR} matches --workflow {}",
            run.only_workflows.join(", ")
        );
    }
    if files.is_empty() {
        run.notes.push(format!(
            "No workflows in {WORKFLOW_DIR} at {}",
            run.short_sha()
        ));
        return Ok(vec![]);
    }

    let is_tag = run.git_ref.starts_with("refs/tags/");
    let branch = run.git_ref.strip_prefix("refs/heads/").map(str::to_string);
    let push_changed = push_changed_files(run, git, facts.default_branch.as_deref());

    // Is there an open pull request for this branch? Then pull_request
    // workflows run too, on GitHub's test merge commit.
    let mut pr: Option<PrPlan> = None;
    let wants_pr = run
        .event_override
        .as_deref()
        .is_none_or(|e| e == "pull_request");
    if let (Some(api), Some(b), false, true) = (api, &branch, is_tag, wants_pr) {
        match api.open_pull_request(b) {
            Ok(Some(raw)) => {
                let base_ref = raw["base"]["ref"].as_str().unwrap_or_default().to_string();
                let remote = run.remote.clone().unwrap_or_else(|| "origin".into());
                let base_sha = git
                    .try_run(&[
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        &format!("refs/remotes/{remote}/{base_ref}"),
                    ])
                    .or_else(|| {
                        raw["base"]["sha"]
                            .as_str()
                            .filter(|s| git.object_exists(s))
                            .map(str::to_string)
                    });
                let number = raw["number"].as_u64().unwrap_or_default();
                let info = PullRequestInfo {
                    number,
                    base_ref: base_ref.clone(),
                    base_sha: base_sha.clone().unwrap_or_default(),
                    head_ref: b.clone(),
                };
                let (merge, changed) = match &base_sha {
                    Some(base) => {
                        let msg = format!("Merge {sha} into {base}");
                        let merge = match git.test_merge(base, &sha, &msg) {
                            Ok(Some(m)) => Some(m),
                            Ok(None) => {
                                run.notes.push(format!(
                                    "Pull request #{number} has merge conflicts with {base_ref}; its pull_request workflows run only on GitHub."
                                ));
                                None
                            }
                            Err(e) => {
                                run.notes.push(format!(
                                    "Couldn't create the test merge for #{number}: {e:#}"
                                ));
                                None
                            }
                        };
                        let changed = git
                            .merge_base(base, &sha)
                            .and_then(|mb| git.changed_files(&mb, &sha).ok());
                        (merge, changed)
                    }
                    None => {
                        run.notes.push(format!(
                            "The base branch '{base_ref}' of #{number} isn't available locally (git fetch); pull_request workflows run only on GitHub."
                        ));
                        (None, None)
                    }
                };
                run.pull_request = Some(info.clone());
                pr = Some(PrPlan {
                    raw,
                    info,
                    merge,
                    changed,
                });
            }
            Ok(None) => {}
            Err(e) => run
                .notes
                .push(format!("Couldn't look up pull requests: {e:#}")),
        }
    }

    let run_number = store.list().map(|r| r.len()).unwrap_or(1).max(1);
    let mut planned = Vec::new();
    for file in files {
        let Some(src) = git.show(&sha, &file)? else {
            continue;
        };
        let wf = match Workflow::parse(&file, &src) {
            Ok(w) => w,
            Err(e) => {
                run.notes.push(format!("Skipped {file}: {}", e.message));
                continue;
            }
        };
        let push_ok = run.event_override.as_deref().is_none_or(|e| e == "push")
            && wf
                .on
                .matches_push(&run.git_ref, push_changed.as_deref())
                .unwrap_or_else(|e| {
                    run.notes.push(format!("{file}: {e}"));
                    false
                });
        let (event, checkout_sha, checkout_ref, payload) = if push_ok {
            (
                "push",
                sha.clone(),
                run.git_ref.clone(),
                push_payload(run, git, facts),
            )
        } else if let Some(PrPlan {
            raw,
            info,
            merge: Some(merge),
            changed,
        }) = &pr
        {
            let matches = wf
                .on
                .matches_pull_request(&info.base_ref, "synchronize", changed.as_deref())
                .unwrap_or(false);
            if !matches {
                continue;
            }
            (
                "pull_request",
                merge.clone(),
                format!("refs/pull/{}/merge", info.number),
                pr_payload(run, raw, facts),
            )
        } else {
            continue;
        };
        let github = github_context(
            run,
            facts,
            &wf,
            event,
            &checkout_sha,
            &checkout_ref,
            &payload,
            run_number,
        );
        planned.push(PlannedWorkflow {
            workflow: wf,
            event: event.to_string(),
            checkout_sha,
            checkout_ref,
            github,
            payload,
            vars: Value::Object(facts.vars.clone()),
        });
    }

    for pw in &planned {
        add_job_records(run, pw)?;
    }
    if !run.only_workflows.is_empty() && run.only_jobs.is_empty() && run.jobs.is_empty() {
        let event = run.event_override.clone().unwrap_or_else(|| "push".into());
        bail!(
            "--workflow {} has no jobs to run for a {event} to {}: check the workflow's `on:` triggers",
            run.only_workflows.join(", "),
            run.git_ref
        );
    }
    if !run.only_jobs.is_empty() && run.jobs.is_empty() {
        let mut known: Vec<String> = planned
            .iter()
            .flat_map(|pw| pw.workflow.jobs.iter())
            .map(|j| match &j.name {
                Some(name) if name != &j.id && !name.contains("${{") => {
                    format!("{} ({name})", j.id)
                }
                _ => j.id.clone(),
            })
            .collect();
        known.dedup();
        bail!(
            "no job matches --job {}. Jobs that run for this commit: {}",
            run.only_jobs.join(", "),
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        );
    }
    if let Some(p) = planned.iter().find(|p| p.event == "pull_request") {
        run.checkout_sha = p.checkout_sha.clone();
    }
    Ok(planned)
}

/// Files changed by the push, if they can be worked out locally.
fn push_changed_files(run: &Run, git: &Git, default_branch: Option<&str>) -> Option<Vec<String>> {
    if let Some(before) = run.before.as_deref().filter(|b| *b != ZERO_SHA) {
        if git.object_exists(before) {
            return git.changed_files(before, &run.sha).ok();
        }
    }
    let remote = run.remote.clone().unwrap_or_else(|| "origin".into());
    let base = git.try_run(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("refs/remotes/{remote}/{}", default_branch?),
    ])?;
    let mb = git.merge_base(&base, &run.sha)?;
    if mb == run.sha {
        return None;
    }
    git.changed_files(&mb, &run.sha).ok()
}

/// Expression contexts available to job-level keys.
pub fn job_level_contexts(pw: &PlannedWorkflow, needs: Value) -> Map<String, Value> {
    let mut c = Map::new();
    c.insert("github".into(), pw.github.clone());
    c.insert("needs".into(), needs);
    c.insert("vars".into(), pw.vars.clone());
    c.insert("inputs".into(), json!({}));
    c
}

/// Job records for one workflow. Jobs whose matrix depends on other jobs'
/// outputs get a placeholder that the orchestrator expands later.
fn add_job_records(run: &mut Run, pw: &PlannedWorkflow) -> Result<()> {
    let wf = &pw.workflow;
    let wf_name = wf.display_name();
    let wanted = wanted_jobs(wf, &run.only_jobs);
    for id in wf.job_order().map_err(anyhow::Error::msg)? {
        let job = wf.job(&id).expect("ordered ids exist");
        if !wanted.contains(&id) {
            continue;
        }
        if depends_on_needs(job) {
            run.jobs.push(JobRecord {
                key: unique_key(run, &wf.file, &id, None),
                workflow: wf_name.clone(),
                workflow_file: wf.file.clone(),
                event: pw.event.clone(),
                job_id: id.clone(),
                name: job.name.clone().unwrap_or_else(|| id.clone()),
                matrix: None,
                context: None,
                state: JobState::Queued,
                reason: None,
                needs: job.needs.clone(),
                started_at: None,
                finished_at: None,
                steps: vec![],
                outputs: Map::new(),
                artifacts: vec![],
                error: None,
                continue_on_error: false,
                pending_expansion: true,
            });
            continue;
        }
        let ctx = job_level_contexts(pw, json!({}));
        match expand_job(wf, job, &ctx) {
            Ok(instances) => {
                let records = instance_records(run, pw, job, &instances);
                run.jobs.extend(records);
            }
            Err(e) => {
                let key = unique_key(run, &wf.file, &id, None);
                run.jobs.push(error_record(pw, job, key, e));
            }
        }
    }
    Ok(())
}

/// The selected jobs plus everything they need, transitively.
fn wanted_jobs(wf: &Workflow, only: &[String]) -> Vec<String> {
    if only.is_empty() {
        return wf.jobs.iter().map(|j| j.id.clone()).collect();
    }
    let mut out: Vec<String> = Vec::new();
    // A job's id, or the name GitHub and `baste status` show for it.
    let mut todo: Vec<String> = wf
        .jobs
        .iter()
        .filter(|j| {
            only.iter()
                .any(|o| o == &j.id || j.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(o)))
        })
        .map(|j| j.id.clone())
        .collect();
    while let Some(id) = todo.pop() {
        if out.contains(&id) {
            continue;
        }
        if let Some(j) = wf.job(&id) {
            todo.extend(j.needs.iter().cloned());
        }
        out.push(id);
    }
    out
}

pub fn instance_records(
    run: &Run,
    pw: &PlannedWorkflow,
    job: &Job,
    instances: &[JobInstance],
) -> Vec<JobRecord> {
    let wf_name = pw.workflow.display_name();
    let mut out: Vec<JobRecord> = Vec::new();
    for (i, inst) in instances.iter().enumerate() {
        let index = (instances.len() > 1).then_some(i);
        let mut key = unique_key(run, &pw.workflow.file, &job.id, index);
        while out.iter().any(|r| r.key == key) {
            key.push('x');
        }
        let (state, reason, context) = match &inst.placement {
            Placement::Local => (
                JobState::Queued,
                None,
                Some(status_context(&wf_name, &inst.name)),
            ),
            Placement::GitHub(r) => (JobState::HandedToGithub, Some(r.clone()), None),
        };
        out.push(JobRecord {
            key,
            workflow: wf_name.clone(),
            workflow_file: pw.workflow.file.clone(),
            event: pw.event.clone(),
            job_id: job.id.clone(),
            name: inst.name.clone(),
            matrix: inst.matrix.clone().map(Value::Object),
            context,
            state,
            reason,
            needs: job.needs.clone(),
            started_at: None,
            finished_at: None,
            steps: vec![],
            outputs: Map::new(),
            artifacts: vec![],
            error: None,
            continue_on_error: false,
            pending_expansion: false,
        });
    }
    out
}

pub fn error_record(pw: &PlannedWorkflow, job: &Job, key: String, error: String) -> JobRecord {
    let wf_name = pw.workflow.display_name();
    let name = job.name.clone().unwrap_or_else(|| job.id.clone());
    JobRecord {
        key,
        workflow: wf_name.clone(),
        workflow_file: pw.workflow.file.clone(),
        event: pw.event.clone(),
        job_id: job.id.clone(),
        context: Some(status_context(&wf_name, &name)),
        name,
        matrix: None,
        state: JobState::Failed,
        reason: None,
        needs: job.needs.clone(),
        started_at: None,
        finished_at: None,
        steps: vec![],
        outputs: Map::new(),
        artifacts: vec![],
        error: Some(error),
        continue_on_error: false,
        pending_expansion: false,
    }
}

fn unique_key(run: &Run, file: &str, job_id: &str, index: Option<usize>) -> String {
    let stem = std::path::Path::new(file)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let base = match index {
        Some(i) => crate::store::slug(&format!("{stem}-{job_id}-{}", i + 1)),
        None => crate::store::slug(&format!("{stem}-{job_id}")),
    };
    let mut key = base.clone();
    let mut n = 2;
    while run.jobs.iter().any(|j| j.key == key) {
        key = format!("{base}-{n}");
        n += 1;
    }
    key
}

fn commit_json(git: &Git, sha: &str, server: &str, full_name: &str) -> Value {
    let c = git.commit_info(sha).unwrap_or_default();
    json!({
        "id": sha,
        "tree_id": git.try_run(&["rev-parse", &format!("{sha}^{{tree}}")]).unwrap_or_default(),
        "message": c.message,
        "timestamp": c.timestamp,
        "url": format!("{server}/{full_name}/commit/{sha}"),
        "author": {"name": c.author_name, "email": c.author_email},
        "committer": {"name": c.committer_name, "email": c.committer_email},
        "distinct": true,
    })
}

fn push_payload(run: &Run, git: &Git, facts: &RunFacts) -> Value {
    let server = run.repo.server_url();
    let full = run.repo.full_name();
    let head = commit_json(git, &run.sha, &server, &full);
    let before = run.before.clone().unwrap_or_else(|| ZERO_SHA.into());
    json!({
        "ref": run.git_ref,
        "before": before,
        "after": run.sha,
        "created": before == ZERO_SHA,
        "deleted": false,
        "forced": false,
        "base_ref": null,
        "compare": format!("{server}/{full}/compare/{}...{}", &before[..12.min(before.len())], &run.sha[..12.min(run.sha.len())]),
        "commits": [head.clone()],
        "head_commit": head,
        "pusher": {"name": facts.actor},
        "repository": facts.repository,
        "sender": {"login": facts.actor},
    })
}

fn pr_payload(run: &Run, raw: &Value, facts: &RunFacts) -> Value {
    let mut pr = raw.clone();
    pr["head"]["sha"] = json!(run.sha);
    json!({
        "action": "synchronize",
        "number": raw["number"],
        "before": run.before.clone().unwrap_or_else(|| ZERO_SHA.into()),
        "after": run.sha,
        "pull_request": pr,
        "repository": facts.repository,
        "sender": {"login": facts.actor},
    })
}

#[allow(clippy::too_many_arguments)]
fn github_context(
    run: &Run,
    facts: &RunFacts,
    wf: &Workflow,
    event: &str,
    checkout_sha: &str,
    checkout_ref: &str,
    payload: &Value,
    run_number: usize,
) -> Value {
    let (ref_name, ref_type) = if let Some(b) = checkout_ref.strip_prefix("refs/heads/") {
        (b.to_string(), "branch")
    } else if let Some(t) = checkout_ref.strip_prefix("refs/tags/") {
        (t.to_string(), "tag")
    } else {
        (
            checkout_ref.trim_start_matches("refs/pull/").to_string(),
            "branch",
        )
    };
    let (head_ref, base_ref) = match (&run.pull_request, event) {
        (Some(pr), "pull_request") => (pr.head_ref.clone(), pr.base_ref.clone()),
        _ => (String::new(), String::new()),
    };
    let server = run.repo.server_url();
    let api = if run.repo.host == "github.com" {
        "https://api.github.com".to_string()
    } else {
        format!("{server}/api/v3")
    };
    let graphql = if run.repo.host == "github.com" {
        "https://api.github.com/graphql".to_string()
    } else {
        format!("{server}/api/graphql")
    };
    json!({
        "event_name": event,
        "event": payload,
        "sha": checkout_sha,
        "ref": checkout_ref,
        "ref_name": ref_name,
        "ref_type": ref_type,
        "ref_protected": false,
        "head_ref": head_ref,
        "base_ref": base_ref,
        "repository": run.repo.full_name(),
        "repository_owner": run.repo.owner,
        "repository_id": facts.repository.get("id").map(|v| v.to_string()).unwrap_or_default(),
        "repository_owner_id": facts.repository.pointer("/owner/id").map(|v| v.to_string()).unwrap_or_default(),
        "repositoryUrl": format!("git://{}/{}.git", run.repo.host, run.repo.full_name()),
        "actor": facts.actor,
        "actor_id": "",
        "triggering_actor": facts.actor,
        "workflow": wf.display_name(),
        "workflow_ref": format!("{}/{}@{}", run.repo.full_name(), wf.file, run.git_ref),
        "workflow_sha": run.sha,
        "run_id": run.created_at.timestamp_millis().to_string(),
        "run_number": run_number.to_string(),
        "run_attempt": run.run_attempt.to_string(),
        "retention_days": "90",
        "server_url": server,
        "api_url": api,
        "graphql_url": graphql,
        "secret_source": "None",
    })
}
