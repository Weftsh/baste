//! Running `uses:` steps: JavaScript, composite and Docker actions, and the
//! pre/post steps they register.

use crate::job::{step_context, Job, PostKind, PostStep, ProcResult, Scope, StepResult};
use crate::process;
use baste_expr::{Env, Map, Value};
use baste_protocol::Outcome;
use baste_workflow::{scalar_string, ActionMeta, Runs, Step, Uses};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Maximum depth of composite actions calling composite actions.
const MAX_DEPTH: usize = 10;

/// Evaluate a boolean-ish value (`true`, `${{ matrix.experimental }}`).
pub(crate) fn eval_bool(v: &Value, env: &Env) -> Result<bool, baste_expr::Error> {
    Ok(match v {
        Value::Bool(b) => *b,
        Value::String(s) => {
            baste_expr::is_truthy(&baste_expr::interpolate_value(s, env)?)
                && !s.trim().eq_ignore_ascii_case("false")
        }
        other => baste_expr::is_truthy(other),
    })
}

/// A resolved action: where it lives and what it is.
pub(crate) struct Resolved {
    pub dir: PathBuf,
    pub meta: ActionMeta,
    pub repository: String,
    pub git_ref: String,
    pub display: String,
}

/// Find the directory of an action reference and load its metadata.
pub(crate) fn resolve(job: &Job, uses: &Uses) -> Result<Resolved, String> {
    let (dir, repository, git_ref) = match uses {
        Uses::Remote {
            owner,
            repo,
            path,
            git_ref,
        } => {
            let key = format!("{owner}/{repo}@{git_ref}").to_ascii_lowercase();
            let base = job.actions.get(&key).ok_or_else(|| {
                format!(
                    "Action '{owner}/{repo}@{git_ref}' was not downloaded before the job started"
                )
            })?;
            let dir = match path {
                Some(p) => base.join(p),
                None => base.clone(),
            };
            (dir, format!("{owner}/{repo}"), git_ref.clone())
        }
        Uses::Local { path } => (
            job.dirs.workspace.join(path.trim_start_matches("./")),
            String::new(),
            String::new(),
        ),
        Uses::Docker { .. } => return Err("docker:// references have no metadata".into()),
    };
    let meta = ActionMeta::load(&dir).map_err(|e| {
        if matches!(uses, Uses::Local { .. }) && !job.dirs.workspace.join(".git").exists() {
            format!("{e}. Did you forget to run actions/checkout before running a local action?")
        } else {
            e
        }
    })?;
    let display = match uses {
        Uses::Local { path } => path.clone(),
        _ => format!("{repository}@{git_ref}"),
    };
    Ok(Resolved {
        dir,
        meta,
        repository,
        git_ref,
        display,
    })
}

pub(crate) fn run_uses(
    job: &mut Job,
    step: &Step,
    uses: &str,
    scope: &mut Scope,
    step_env: &Map<String, Value>,
    position: Option<usize>,
) -> StepResult {
    let index = scope.step_index;
    let parsed = match Uses::parse(uses) {
        Ok(u) => u,
        Err(e) => {
            job.error(index, &e);
            return StepResult::failed(None);
        }
    };
    if let Uses::Docker { image } = &parsed {
        return run_docker_uri(job, step, image, scope, step_env);
    }
    if let Some(result) = crate::shims::try_shim(job, step, &parsed, scope, step_env) {
        return result;
    }
    let resolved = match resolve(job, &parsed) {
        Ok(r) => r,
        Err(e) => {
            job.error(index, &e);
            return StepResult::failed(None);
        }
    };
    let inputs = match action_inputs(job, &resolved.meta, step, scope, step_env) {
        Ok(i) => i,
        Err(e) => {
            job.error(index, &e);
            return StepResult::failed(None);
        }
    };
    let pre_state = position
        .and_then(|p| job.pre_state.remove(&p))
        .unwrap_or_default();
    let mut action_scope = scope.clone();
    action_scope.action_path = Some(resolved.dir.clone());
    action_scope.action_repository = resolved.repository.clone();
    action_scope.action_ref = resolved.git_ref.clone();
    let name = step.name.clone().unwrap_or_else(|| step.default_name());

    match resolved.meta.runs.clone() {
        Runs::Node {
            using,
            main,
            post,
            post_if,
            ..
        } => {
            let node = match node_binary(job, &using) {
                Ok(n) => n,
                Err(e) => {
                    job.error(index, &e);
                    return StepResult::failed(None);
                }
            };
            let env = action_env(job, &action_scope, step, step_env, &inputs, &pre_state);
            let timeout = match job.step_timeout(step, scope) {
                Ok(t) => t,
                Err(e) => {
                    job.error(index, &e);
                    return StepResult::failed(None);
                }
            };
            let script = resolved.dir.join(&main);
            let r = job.exec(
                index,
                &node.display().to_string(),
                &[script.display().to_string()],
                env.clone(),
                &job.dirs.workspace.clone(),
                timeout,
            );
            let result = finish(job, index, r, timeout);
            if let Some(post) = post {
                let mut state = pre_state;
                state.extend(result.1);
                job.posts.push(PostStep {
                    name: format!("Post {name}"),
                    condition: post_if.unwrap_or_else(|| "always()".into()),
                    scope: action_scope,
                    kind: PostKind::Node {
                        node,
                        script: resolved.dir.join(post),
                        env,
                        state,
                    },
                });
            }
            result.0
        }
        Runs::Composite { steps } => run_composite(job, &resolved, &steps, inputs, scope, step_env),
        Runs::Docker {
            image,
            args,
            entrypoint,
            post_entrypoint,
            env: action_env_map,
            post_if,
            ..
        } => {
            let prepared = match prepare_docker_image(job, index, &image, Some(&resolved.dir)) {
                Ok(i) => i,
                Err(e) => {
                    job.error(index, &e);
                    return StepResult::failed(None);
                }
            };
            let mut ctx_scope = action_scope.clone();
            ctx_scope.inputs = Some(inputs.clone());
            let ctx = job.contexts(&ctx_scope, Some(step_env));
            let f = job.functions(&ctx_scope);
            let expr_env = Env {
                contexts: &ctx,
                functions: &f,
            };
            let mut docker_args = Vec::new();
            for a in &args {
                match crate::job::eval_to_string(a, &expr_env) {
                    Ok(s) => docker_args.push(s),
                    Err(e) => {
                        job.error(index, &format!("evaluating docker args: {e}"));
                        return StepResult::failed(None);
                    }
                }
            }
            let mut extra = BTreeMap::new();
            for (k, v) in &action_env_map {
                match crate::job::eval_to_string(v, &expr_env) {
                    Ok(s) => {
                        extra.insert(k.clone(), s);
                    }
                    Err(e) => {
                        job.error(index, &format!("evaluating docker env {k}: {e}"));
                        return StepResult::failed(None);
                    }
                }
            }
            let mut env = action_env(job, &action_scope, step, step_env, &inputs, &pre_state);
            env.extend(extra);
            let timeout = job.step_timeout(step, scope).ok().flatten();
            let r = run_container(
                job,
                index,
                &prepared,
                entrypoint.as_deref(),
                &docker_args,
                env.clone(),
                timeout,
            );
            let result = finish(job, index, r, timeout);
            if let Some(post_ep) = post_entrypoint {
                let mut state = pre_state;
                state.extend(result.1);
                job.posts.push(PostStep {
                    name: format!("Post {name}"),
                    condition: post_if.unwrap_or_else(|| "always()".into()),
                    scope: action_scope,
                    kind: PostKind::Docker {
                        image: prepared,
                        entrypoint: post_ep,
                        env,
                        state,
                    },
                });
            }
            result.0
        }
    }
}

/// Turn a process result into a step result plus the state it saved.
fn finish(
    job: &mut Job,
    index: usize,
    r: Result<ProcResult, String>,
    timeout: Option<std::time::Duration>,
) -> (StepResult, Vec<(String, String)>) {
    match r {
        Ok(p) => {
            let exit = job.note_exit(index, p.exit, timeout);
            (StepResult::from_exit(exit, p.outputs), p.state)
        }
        Err(e) => {
            job.error(index, &e);
            (StepResult::failed(None), vec![])
        }
    }
}

/// Resolve action inputs: `with:` values (evaluated), else defaults.
pub(crate) fn action_inputs(
    job: &Job,
    meta: &ActionMeta,
    step: &Step,
    scope: &Scope,
    step_env: &Map<String, Value>,
) -> Result<Map<String, Value>, String> {
    let ctx = job.contexts(scope, Some(step_env));
    let f = job.functions(scope);
    let env = Env {
        contexts: &ctx,
        functions: &f,
    };
    let mut out = Map::new();
    let mut unexpected = Vec::new();
    for (k, v) in &step.with {
        let value = crate::job::eval_to_string(v, &env)
            .map_err(|e| format!("evaluating input '{k}': {e}"))?;
        match meta.inputs.iter().find(|i| i.name.eq_ignore_ascii_case(k)) {
            Some(i) => out.insert(i.name.clone(), Value::String(value)),
            None => {
                unexpected.push(k.clone());
                out.insert(k.clone(), Value::String(value))
            }
        };
    }
    for input in &meta.inputs {
        if out.contains_key(&input.name) {
            continue;
        }
        let value = match &input.default {
            Some(d) => crate::job::eval_to_string(d, &env)
                .map_err(|e| format!("evaluating default of input '{}': {e}", input.name))?,
            None => {
                if input.required {
                    job.warning(
                        scope.step_index,
                        &format!("Input required and not supplied: {}", input.name),
                    );
                }
                String::new()
            }
        };
        out.insert(input.name.clone(), Value::String(value));
    }
    if !unexpected.is_empty() && !meta.inputs.is_empty() {
        let valid: Vec<_> = meta
            .inputs
            .iter()
            .map(|i| format!("'{}'", i.name))
            .collect();
        job.warning(
            scope.step_index,
            &format!(
                "Unexpected input(s) '{}', valid inputs are [{}]",
                unexpected.join("', '"),
                valid.join(", ")
            ),
        );
    }
    Ok(out)
}

fn input_var(name: &str) -> String {
    format!("INPUT_{}", name.replace(' ', "_").to_ascii_uppercase())
}

/// Process environment for an action's process.
fn action_env(
    job: &Job,
    scope: &Scope,
    step: &Step,
    step_env: &Map<String, Value>,
    inputs: &Map<String, Value>,
    state: &[(String, String)],
) -> BTreeMap<String, String> {
    let mut env = job.process_env(scope, step_env);
    for (k, v) in inputs {
        env.insert(input_var(k), baste_expr::to_display_string(v));
    }
    for (k, v) in state {
        env.insert(format!("STATE_{k}"), v.clone());
    }
    env.insert(
        "GITHUB_ACTION".into(),
        step.id.clone().unwrap_or_else(|| "__action".into()),
    );
    env
}

fn node_binary(job: &Job, using: &str) -> Result<PathBuf, String> {
    // GitHub runs Node 20 (and older) actions on Node 24 unless a job opts out.
    let allow_old = job
        .env
        .get("ACTIONS_ALLOW_USE_UNSECURE_NODE_VERSION")
        .map(baste_expr::to_display_string)
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let preferred = if using == "node20" && !allow_old {
        ["node24", "node20"]
    } else {
        [using, "node24"]
    };
    for runtime in preferred {
        if let Some(p) = job.spec.runner.node.get(runtime).and_then(Value::as_str) {
            let p = PathBuf::from(p);
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    let path = std::env::var("PATH").unwrap_or_default();
    process::which("node", &path).ok_or_else(|| {
        format!("JavaScript actions need Node.js ({using}), but 'node' was not found")
    })
}

/// The `pre` part of a step's action, if it has one. Local actions never run
/// `pre` (the repository isn't checked out yet), matching GitHub.
pub(crate) enum Pre {
    Node {
        node: PathBuf,
        script: PathBuf,
        condition: String,
    },
    Docker {
        image: String,
        entrypoint: String,
        condition: String,
    },
}

pub(crate) fn pre_step(job: &Job, step: &Step) -> Option<Pre> {
    let uses = Uses::parse(step.uses.as_deref()?).ok()?;
    if !matches!(uses, Uses::Remote { .. }) || crate::shims::is_shimmed(job, &uses) {
        return None;
    }
    let r = resolve(job, &uses).ok()?;
    match r.meta.runs {
        Runs::Node {
            using,
            pre: Some(pre),
            pre_if,
            ..
        } => Some(Pre::Node {
            node: node_binary(job, &using).ok()?,
            script: r.dir.join(pre),
            condition: pre_if.unwrap_or_else(|| "always()".into()),
        }),
        Runs::Docker {
            image,
            pre_entrypoint: Some(ep),
            pre_if,
            ..
        } => Some(Pre::Docker {
            image,
            entrypoint: ep,
            condition: pre_if.unwrap_or_else(|| "always()".into()),
        }),
        _ => None,
    }
}

pub(crate) fn run_pre(
    job: &mut Job,
    step: &Step,
    pre: Pre,
    scope: &mut Scope,
    position: usize,
) -> StepResult {
    let index = scope.step_index;
    let uses = Uses::parse(step.uses.as_deref().unwrap_or_default()).expect("checked in pre_step");
    let resolved = match resolve(job, &uses) {
        Ok(r) => r,
        Err(e) => {
            job.error(index, &e);
            return StepResult::failed(None);
        }
    };
    let mut action_scope = scope.clone();
    action_scope.action_path = Some(resolved.dir.clone());
    action_scope.action_repository = resolved.repository.clone();
    action_scope.action_ref = resolved.git_ref.clone();
    let condition = match &pre {
        Pre::Node { condition, .. } | Pre::Docker { condition, .. } => condition.clone(),
    };
    let ctx = job.contexts(&action_scope, None);
    let f = job.functions(&action_scope);
    if !baste_expr::evaluate_condition(
        &condition,
        &Env {
            contexts: &ctx,
            functions: &f,
        },
    )
    .unwrap_or(false)
    {
        return StepResult {
            outcome: Outcome::Skipped,
            conclusion: Outcome::Skipped,
            exit_code: None,
            outputs: Map::new(),
        };
    }
    let step_env = job.evaluate_map(&step.env, scope, None).unwrap_or_default();
    let inputs = match action_inputs(job, &resolved.meta, step, scope, &step_env) {
        Ok(i) => i,
        Err(e) => {
            job.error(index, &e);
            return StepResult::failed(None);
        }
    };
    let env = action_env(job, &action_scope, step, &step_env, &inputs, &[]);
    let timeout = job.step_timeout(step, scope).ok().flatten();
    let r = match pre {
        Pre::Node { node, script, .. } => job.exec(
            index,
            &node.display().to_string(),
            &[script.display().to_string()],
            env,
            &job.dirs.workspace.clone(),
            timeout,
        ),
        Pre::Docker {
            image, entrypoint, ..
        } => match prepare_docker_image(job, index, &image, Some(&resolved.dir)) {
            Ok(img) => run_container(job, index, &img, Some(&entrypoint), &[], env, timeout),
            Err(e) => Err(e),
        },
    };
    let (result, state) = finish(job, index, r, timeout);
    job.pre_state.insert(position, state);
    result
}

pub(crate) fn run_post(job: &mut Job, post: PostStep, index: usize) -> StepResult {
    let timeout = Some(
        job.deadline
            .saturating_duration_since(std::time::Instant::now()),
    );
    let r = match post.kind {
        PostKind::Node {
            node,
            script,
            mut env,
            state,
        } => {
            for (k, v) in state {
                env.insert(format!("STATE_{k}"), v);
            }
            job.exec(
                index,
                &node.display().to_string(),
                &[script.display().to_string()],
                env,
                &job.dirs.workspace.clone(),
                timeout,
            )
        }
        PostKind::Docker {
            image,
            entrypoint,
            mut env,
            state,
            ..
        } => {
            for (k, v) in state {
                env.insert(format!("STATE_{k}"), v);
            }
            run_container(job, index, &image, Some(&entrypoint), &[], env, timeout)
        }
    };
    finish(job, index, r, timeout).0
}

fn run_composite(
    job: &mut Job,
    resolved: &Resolved,
    steps: &[Value],
    inputs: Map<String, Value>,
    parent: &Scope,
    step_env: &Map<String, Value>,
) -> StepResult {
    let index = parent.step_index;
    if parent.depth >= MAX_DEPTH {
        job.error(
            index,
            &format!("Composite actions are nested more than {MAX_DEPTH} levels deep"),
        );
        return StepResult::failed(None);
    }
    let mut env = parent.env.clone();
    env.extend(step_env.clone());
    let mut scope = Scope {
        step_index: index,
        steps: Map::new(),
        inputs: Some(inputs),
        action_path: Some(resolved.dir.clone()),
        action_repository: resolved.repository.clone(),
        action_ref: resolved.git_ref.clone(),
        env,
        status: parent.status,
        depth: parent.depth + 1,
        defaults: Default::default(),
    };
    for (i, raw) in steps.iter().enumerate() {
        let step = match Step::parse(raw) {
            Ok(s) => s,
            Err(e) => {
                job.error(index, &format!("{}: step {}: {e}", resolved.display, i + 1));
                return StepResult::failed(None);
            }
        };
        let name = step.name.clone().unwrap_or_else(|| step.default_name());
        job.log(index, &format!("##[group]{name}"));
        let result = job.execute_step(&step, &mut scope, None);
        job.log(index, "##[endgroup]");
        if let Some(id) = &step.id {
            scope.steps.insert(id.clone(), step_context(&result));
        }
    }
    let ctx = job.contexts(&scope, None);
    let f = job.functions(&scope);
    let env = Env {
        contexts: &ctx,
        functions: &f,
    };
    let mut outputs = Map::new();
    for (name, def) in &resolved.meta.outputs {
        if let Some(v) = def.get("value") {
            match crate::job::eval_to_string(v, &env) {
                Ok(s) => {
                    outputs.insert(name.clone(), Value::String(s));
                }
                Err(e) => job.warning(index, &format!("evaluating output '{name}': {e}")),
            }
        }
    }
    let outcome = match scope.status {
        crate::job::Status::Success => Outcome::Success,
        crate::job::Status::Failure => Outcome::Failure,
        crate::job::Status::Cancelled => Outcome::Cancelled,
    };
    StepResult {
        outcome,
        conclusion: outcome,
        exit_code: None,
        outputs,
    }
}

// ----- Docker ---------------------------------------------------------------

fn docker(job: &Job) -> Result<String, String> {
    let path = job
        .process_env(&job_scope_stub(), &Map::new())
        .get("PATH")
        .cloned()
        .unwrap_or_default();
    process::which("docker", &path)
        .map(|p| p.display().to_string())
        .ok_or_else(|| "Docker actions need Docker, but 'docker' was not found".to_string())
}

fn job_scope_stub() -> Scope {
    Scope {
        step_index: 0,
        steps: Map::new(),
        inputs: None,
        action_path: None,
        action_repository: String::new(),
        action_ref: String::new(),
        env: Map::new(),
        status: crate::job::Status::Success,
        depth: 0,
        defaults: Default::default(),
    }
}

/// Pull or build the image for a Docker action; returns an image reference.
fn prepare_docker_image(
    job: &mut Job,
    index: usize,
    image: &str,
    dir: Option<&Path>,
) -> Result<String, String> {
    let docker = docker(job)?;
    let env = job.process_env(&job_scope_stub(), &Map::new());
    let cwd = job.dirs.workspace.clone();
    if let Some(reference) = image.strip_prefix("docker://") {
        let r = job.exec(
            index,
            &docker,
            &["pull".into(), reference.into()],
            env,
            &cwd,
            None,
        )?;
        return if r.exit.success() {
            Ok(reference.to_string())
        } else {
            Err(format!("docker pull {reference} failed"))
        };
    }
    let dir = dir.ok_or("a Dockerfile action needs an action directory")?;
    let dockerfile = dir.join(image);
    let context = dockerfile.parent().unwrap_or(dir).to_path_buf();
    let tag = format!(
        "baste-action:{}",
        &hex::encode(Sha256::digest(dockerfile.display().to_string().as_bytes()))[..12]
    );
    let mut args = vec![
        "build".to_string(),
        "-t".into(),
        tag.clone(),
        "-f".into(),
        dockerfile.display().to_string(),
    ];
    if let Some(platform) = &job.spec.runner.docker_platform {
        args.push("--platform".into());
        args.push(platform.clone());
    }
    args.push(context.display().to_string());
    let r = job.exec(index, &docker, &args, env, &cwd, None)?;
    if r.exit.success() {
        Ok(tag)
    } else {
        Err(format!("docker build of {} failed", dockerfile.display()))
    }
}

/// Run a container the way the GitHub runner does, mounting the workspace and
/// file-command files at `/github/...`.
fn run_container(
    job: &mut Job,
    index: usize,
    image: &str,
    entrypoint: Option<&str>,
    args: &[String],
    mut env: BTreeMap<String, String>,
    timeout: Option<std::time::Duration>,
) -> Result<ProcResult, String> {
    let docker = docker(job)?;
    let fc = job.new_file_commands()?;
    let name = |p: &Path| {
        format!(
            "/github/file_commands/{}",
            p.file_name().unwrap_or_default().to_string_lossy()
        )
    };
    let mut container: BTreeMap<String, String> = env
        .iter()
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "PATH" | "HOME" | "USER" | "LOGNAME" | "SHELL" | "LANG"
            )
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    container.insert("GITHUB_WORKSPACE".into(), "/github/workspace".into());
    container.insert("HOME".into(), "/github/home".into());
    container.insert(
        "GITHUB_EVENT_PATH".into(),
        "/github/workflow/event.json".into(),
    );
    container.insert("GITHUB_ENV".into(), name(&fc.env));
    container.insert("GITHUB_OUTPUT".into(), name(&fc.output));
    container.insert("GITHUB_PATH".into(), name(&fc.path));
    container.insert("GITHUB_STATE".into(), name(&fc.state));
    container.insert("GITHUB_STEP_SUMMARY".into(), name(&fc.summary));
    container.insert("RUNNER_TEMP".into(), "/github/runner_temp".into());

    let mut cmd = vec![
        "run".to_string(),
        "--rm".into(),
        "--label".into(),
        "baste".into(),
        "--workdir".into(),
        "/github/workspace".into(),
    ];
    if let Some(platform) = &job.spec.runner.docker_platform {
        cmd.push("--platform".into());
        cmd.push(platform.clone());
    }
    for (k, v) in &container {
        // Pass values through the environment of the docker CLI so secrets
        // never appear on a command line.
        cmd.push("-e".into());
        cmd.push(k.clone());
        env.insert(k.clone(), v.clone());
    }
    let mount =
        |host: &Path, guest: &str| vec!["-v".to_string(), format!("{}:{guest}", host.display())];
    cmd.extend(mount(
        Path::new("/var/run/docker.sock"),
        "/var/run/docker.sock",
    ));
    cmd.extend(mount(&job.dirs.github_home, "/github/home"));
    cmd.extend(mount(&job.dirs.workflow, "/github/workflow"));
    cmd.extend(mount(&job.dirs.file_commands, "/github/file_commands"));
    cmd.extend(mount(&job.dirs.workspace, "/github/workspace"));
    cmd.extend(mount(&job.dirs.temp, "/github/runner_temp"));
    if let Some(ep) = entrypoint {
        cmd.push("--entrypoint".into());
        cmd.push(ep.into());
    }
    cmd.push(image.into());
    cmd.extend(args.iter().cloned());
    // The docker CLI itself needs the host-side paths for GITHUB_ENV etc. only
    // for our own bookkeeping, so hand exec_with the host FileCommands.
    let cwd = job.dirs.workspace.clone();
    job.exec_with(index, &docker, &cmd, env, &cwd, timeout, &fc)
}

fn run_docker_uri(
    job: &mut Job,
    step: &Step,
    image: &str,
    scope: &mut Scope,
    step_env: &Map<String, Value>,
) -> StepResult {
    let index = scope.step_index;
    let ctx = job.contexts(scope, Some(step_env));
    let f = job.functions(scope);
    let env = Env {
        contexts: &ctx,
        functions: &f,
    };
    let get = |k: &str| -> Result<Option<String>, String> {
        match step.with.get(k) {
            Some(v) => crate::job::eval_to_string(v, &env)
                .map(Some)
                .map_err(|e| e.to_string()),
            None => Ok(None),
        }
    };
    let (args, entrypoint) = match (get("args"), get("entrypoint")) {
        (Ok(a), Ok(e)) => (a.map(|a| process::split_command(&a)).unwrap_or_default(), e),
        (Err(e), _) | (_, Err(e)) => {
            job.error(index, &e);
            return StepResult::failed(None);
        }
    };
    let mut inputs = Map::new();
    for (k, v) in &step.with {
        if let Some(s) = scalar_string(v) {
            inputs.insert(
                k.clone(),
                Value::String(baste_expr::interpolate(&s, &env).unwrap_or(s)),
            );
        }
    }
    let prepared = match prepare_docker_image(job, index, &format!("docker://{image}"), None) {
        Ok(i) => i,
        Err(e) => {
            job.error(index, &e);
            return StepResult::failed(None);
        }
    };
    let penv = action_env(job, scope, step, step_env, &inputs, &[]);
    let timeout = job.step_timeout(step, scope).ok().flatten();
    let r = run_container(
        job,
        index,
        &prepared,
        entrypoint.as_deref(),
        &args,
        penv,
        timeout,
    );
    finish(job, index, r, timeout).0
}
