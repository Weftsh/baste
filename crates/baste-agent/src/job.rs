//! Executes one job: set up the work tree, run each step with GitHub's
//! semantics, run post steps, compute outputs, and report everything as
//! protocol events.

use crate::commands::{parse_command, parse_env_file};
use crate::mask::Masker;
use crate::process::{self, Exit, Spawn, UserIds};
use crate::sink::EventSink;
use baste_expr::{Env, Error as ExprError, Functions, Map, Value};
use baste_protocol::{AnnotationLevel, Event, JobSpec, Outcome, StepPhase, PROTOCOL_VERSION};
use baste_workflow::{scalar_string, RunDefaults, Step};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Environment variables passed from the agent's own environment to steps.
const PASSTHROUGH_ENV: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
    "GIT_SSL_CAINFO",
    "DOCKER_HOST",
];

/// Default job timeout on GitHub: 6 hours.
const DEFAULT_JOB_TIMEOUT_MINUTES: f64 = 360.0;

pub struct AgentOptions {
    /// Directory holding `job.json` and everything it references.
    pub bundle: PathBuf,
    /// Set to request cancellation (e.g. from a SIGTERM handler).
    pub cancel: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Success,
    Failure,
    Cancelled,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Success => "success",
            Status::Failure => "failure",
            Status::Cancelled => "cancelled",
        }
    }
}

/// Status functions and `hashFiles` for expression evaluation.
pub(crate) struct StepFunctions {
    pub status: Status,
    pub workspace: PathBuf,
}

impl Functions for StepFunctions {
    fn call(&self, name: &str, args: &[Value]) -> Option<Result<Value, ExprError>> {
        Some(Ok(Value::Bool(match name {
            "success" => self.status == Status::Success,
            "failure" => self.status == Status::Failure,
            "cancelled" => self.status == Status::Cancelled,
            "always" => true,
            "hashfiles" => {
                let patterns: Vec<String> =
                    args.iter().map(baste_expr::to_display_string).collect();
                return Some(
                    crate::hashfiles::hash_files(&self.workspace, &patterns)
                        .map(Value::String)
                        .map_err(ExprError::Eval),
                );
            }
            _ => return None,
        })))
    }
}

pub(crate) struct Dirs {
    pub work_root: PathBuf,
    pub workspace: PathBuf,
    pub temp: PathBuf,
    pub actions: PathBuf,
    pub tool_cache: PathBuf,
    pub file_commands: PathBuf,
    pub workflow: PathBuf,
    pub github_home: PathBuf,
    pub event_path: PathBuf,
}

/// Evaluation scope: the top level of the job, or the inside of a composite action.
#[derive(Clone)]
pub(crate) struct Scope {
    /// Index of the top-level step that output is attributed to.
    pub step_index: usize,
    pub steps: Map<String, Value>,
    pub inputs: Option<Map<String, Value>>,
    pub action_path: Option<PathBuf>,
    pub action_repository: String,
    pub action_ref: String,
    /// Env layered over the job env (a composite step's `env`).
    pub env: Map<String, Value>,
    pub status: Status,
    pub depth: usize,
    pub defaults: RunDefaults,
}

/// Result of running one step.
pub(crate) struct StepResult {
    pub outcome: Outcome,
    pub conclusion: Outcome,
    pub exit_code: Option<i32>,
    pub outputs: Map<String, Value>,
}

impl StepResult {
    pub fn failed(exit_code: Option<i32>) -> StepResult {
        StepResult {
            outcome: Outcome::Failure,
            conclusion: Outcome::Failure,
            exit_code,
            outputs: Map::new(),
        }
    }

    fn skipped() -> StepResult {
        StepResult {
            outcome: Outcome::Skipped,
            conclusion: Outcome::Skipped,
            exit_code: None,
            outputs: Map::new(),
        }
    }

    pub fn from_exit(exit: Exit, outputs: Map<String, Value>) -> StepResult {
        let outcome = match exit {
            Exit::Code(0) => Outcome::Success,
            Exit::Cancelled => Outcome::Cancelled,
            _ => Outcome::Failure,
        };
        StepResult {
            outcome,
            conclusion: outcome,
            exit_code: exit.code(),
            outputs,
        }
    }
}

/// Paths of the per-step environment files.
pub(crate) struct FileCommands {
    pub env: PathBuf,
    pub output: PathBuf,
    pub path: PathBuf,
    pub state: PathBuf,
    pub summary: PathBuf,
}

/// What a finished process left behind.
pub(crate) struct ProcResult {
    pub exit: Exit,
    pub outputs: Map<String, Value>,
    pub state: Vec<(String, String)>,
}

/// A post step registered by an action's main step.
pub(crate) struct PostStep {
    pub name: String,
    pub condition: String,
    pub scope: Scope,
    pub kind: PostKind,
}

pub(crate) enum PostKind {
    Node {
        node: PathBuf,
        script: PathBuf,
        env: BTreeMap<String, String>,
        state: Vec<(String, String)>,
    },
    Docker {
        image: String,
        entrypoint: String,
        env: BTreeMap<String, String>,
        state: Vec<(String, String)>,
    },
}

pub(crate) struct Job<'a> {
    pub spec: &'a JobSpec,
    pub opts: &'a AgentOptions,
    pub sink: &'a dyn EventSink,
    pub masker: Masker,
    pub dirs: Dirs,
    pub user: Option<UserIds>,
    /// The `env` context: workflow env, job env, and `GITHUB_ENV` exports.
    pub env: Map<String, Value>,
    /// Directories added with `GITHUB_PATH`, most recent first.
    pub path_add: Vec<String>,
    pub github: Value,
    pub posts: Vec<PostStep>,
    pub next_index: usize,
    pub pgids: Vec<i32>,
    /// `owner/repo@ref` (lowercase) to directory.
    pub actions: HashMap<String, PathBuf>,
    /// State saved by `pre` steps, keyed by main step position.
    pub pre_state: HashMap<usize, Vec<(String, String)>>,
    pub deadline: Instant,
    pub timed_out: bool,
    /// Set once cancellation has been observed; `always()` steps still run.
    pub cancelled: bool,
    /// The top-level `steps` context once all steps ran (for job outputs).
    pub collected_steps: Map<String, Value>,
    base_path: String,
    home: PathBuf,
    counter: AtomicU64,
}

/// Run a job and report it on `sink`. Returns the job result.
pub fn run_job(spec: &JobSpec, opts: &AgentOptions, sink: &dyn EventSink) -> Outcome {
    sink.send(Event::Hello {
        protocol: PROTOCOL_VERSION,
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
    });
    let mut job = Job::new(spec, opts, sink);
    let result = job.run();
    let outputs = if result == Outcome::Success || result == Outcome::Failure {
        job.job_outputs()
    } else {
        Map::new()
    };
    job.cleanup_processes();
    let error = job.timed_out.then(|| {
        format!(
            "The job exceeded its maximum execution time of {} minutes.",
            spec.timeout_minutes.unwrap_or(DEFAULT_JOB_TIMEOUT_MINUTES)
        )
    });
    sink.send(Event::JobFinished {
        result,
        outputs,
        error,
        at: now(),
    });
    result
}

pub(crate) fn now() -> String {
    // RFC 3339 in UTC without pulling in a date crate.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let (s, ms) = (secs.as_secs() as i64, secs.subsec_millis());
    let days = s.div_euclid(86_400);
    let rem = s.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    // Howard Hinnant's algorithm.
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

impl<'a> Job<'a> {
    fn new(spec: &'a JobSpec, opts: &'a AgentOptions, sink: &'a dyn EventSink) -> Job<'a> {
        let work_root = PathBuf::from(&spec.runner.work_root);
        let repo_name = spec
            .checkout
            .repository
            .rsplit('/')
            .next()
            .unwrap_or("repo")
            .to_string();
        let temp = work_root.join("_temp");
        let dirs = Dirs {
            workspace: work_root.join(&repo_name).join(&repo_name),
            actions: work_root.join("_actions"),
            tool_cache: PathBuf::from(&spec.runner.tool_cache),
            file_commands: temp.join("_runner_file_commands"),
            workflow: temp.join("_github_workflow"),
            github_home: temp.join("_github_home"),
            event_path: temp.join("_github_workflow").join("event.json"),
            temp,
            work_root,
        };
        let timeout = spec.timeout_minutes.unwrap_or(DEFAULT_JOB_TIMEOUT_MINUTES);
        Job {
            spec,
            opts,
            sink,
            masker: Masker::new(),
            dirs,
            user: None,
            env: Map::new(),
            path_add: vec![],
            github: Value::Null,
            posts: vec![],
            next_index: 1,
            pgids: vec![],
            actions: HashMap::new(),
            pre_state: HashMap::new(),
            deadline: Instant::now() + Duration::from_secs_f64(timeout.max(0.0) * 60.0),
            timed_out: false,
            cancelled: false,
            collected_steps: Map::new(),
            base_path: std::env::var("PATH").unwrap_or_else(|_| {
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()
            }),
            home: std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| "/root".into()),
            counter: AtomicU64::new(0),
        }
    }

    // ----- reporting -------------------------------------------------------

    pub fn log(&self, step: usize, line: &str) {
        self.sink.send(Event::Log {
            step,
            line: self.masker.mask(line),
        });
    }

    pub fn error(&self, step: usize, message: &str) {
        let message = self.masker.mask(message);
        self.sink.send(Event::Log {
            step,
            line: format!("##[error]{message}"),
        });
        self.sink.send(Event::Annotation {
            step,
            level: AnnotationLevel::Error,
            message,
            file: None,
            line: None,
            title: None,
        });
    }

    pub fn warning(&self, step: usize, message: &str) {
        let message = self.masker.mask(message);
        self.sink.send(Event::Log {
            step,
            line: format!("##[warning]{message}"),
        });
        self.sink.send(Event::Annotation {
            step,
            level: AnnotationLevel::Warning,
            message,
            file: None,
            line: None,
            title: None,
        });
    }

    fn start_step(&mut self, name: &str, phase: StepPhase) -> usize {
        let index = if phase == StepPhase::Setup {
            0
        } else {
            let i = self.next_index;
            self.next_index += 1;
            i
        };
        self.sink.send(Event::StepStarted {
            index,
            name: self.masker.mask(name),
            phase,
            at: now(),
        });
        index
    }

    fn finish_step(&self, index: usize, r: &StepResult, started: Instant) {
        self.sink.send(Event::StepFinished {
            index,
            outcome: r.outcome,
            conclusion: r.conclusion,
            exit_code: r.exit_code,
            duration_ms: started.elapsed().as_millis() as u64,
            at: now(),
        });
    }

    pub fn unique(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{:x}{:x}{:x}", t & 0xffff_ffff, std::process::id(), n)
    }

    // ----- the job ---------------------------------------------------------

    fn run(&mut self) -> Outcome {
        let started = Instant::now();
        let setup = self.start_step("Set up job", StepPhase::Setup);
        let steps = match self.setup(setup) {
            Ok(steps) => {
                self.finish_step(
                    setup,
                    &StepResult {
                        outcome: Outcome::Success,
                        conclusion: Outcome::Success,
                        exit_code: None,
                        outputs: Map::new(),
                    },
                    started,
                );
                steps
            }
            Err(e) => {
                self.error(setup, &e);
                self.finish_step(setup, &StepResult::failed(None), started);
                return Outcome::Failure;
            }
        };

        let mut scope = Scope {
            step_index: 0,
            steps: Map::new(),
            inputs: None,
            action_path: None,
            action_repository: String::new(),
            action_ref: String::new(),
            env: Map::new(),
            status: Status::Success,
            depth: 0,
            defaults: self.spec.defaults_as_run_defaults(),
        };

        self.run_pre_steps(&steps, &mut scope);
        for (position, step) in steps.iter().enumerate() {
            self.check_cancel(&mut scope);
            let name = self.step_display_name(step, &scope);
            let index = self.start_step(&name, StepPhase::Main);
            scope.step_index = index;
            let started = Instant::now();
            let result = self.execute_step(step, &mut scope, Some(position));
            self.finish_step(index, &result, started);
            if let Some(id) = &step.id {
                scope.steps.insert(id.clone(), step_context(&result));
            }
        }

        while let Some(post) = self.posts.pop() {
            self.check_cancel(&mut scope);
            self.run_post(post, &mut scope);
        }
        self.collected_steps = scope.steps.clone();

        match scope.status {
            Status::Success => Outcome::Success,
            Status::Failure => Outcome::Failure,
            Status::Cancelled if self.timed_out => Outcome::Failure,
            Status::Cancelled => Outcome::Cancelled,
        }
    }

    /// Notice external cancellation or the job timeout. After this the job is
    /// cancelled: only steps with `always()`/`cancelled()` still run.
    fn check_cancel(&mut self, scope: &mut Scope) {
        let external = self.opts.cancel.swap(false, Ordering::SeqCst);
        if external || (!self.timed_out && Instant::now() >= self.deadline) {
            if !external {
                self.timed_out = true;
            }
            self.cancelled = true;
        }
        if self.cancelled {
            scope.status = Status::Cancelled;
        }
    }

    fn setup(&mut self, index: usize) -> Result<Vec<Step>, String> {
        let spec = self.spec;
        if spec.protocol != PROTOCOL_VERSION {
            return Err(format!(
                "job spec uses protocol {} but this agent speaks {PROTOCOL_VERSION}; update baste",
                spec.protocol
            ));
        }
        self.log(
            index,
            &format!(
                "Baste agent {} on {} {}",
                env!("CARGO_PKG_VERSION"),
                spec.runner.os,
                spec.runner.arch
            ),
        );

        for (name, value) in &spec.secrets {
            self.masker.add(&baste_expr::to_display_string(value));
            let _ = name;
        }
        for m in &spec.masks {
            self.masker.add(m);
        }

        if !spec.missing_secrets.is_empty() {
            let names = spec.missing_secrets.join(", ");
            let plural = if spec.missing_secrets.len() == 1 {
                ""
            } else {
                "s"
            };
            return Err(format!(
                "Secret{plural} {names} {} not set locally. Set {} with `baste secrets set {}` and push again (or `baste rerun`).",
                if plural.is_empty() { "is" } else { "are" },
                if plural.is_empty() { "it" } else { "them" },
                spec.missing_secrets[0],
            ));
        }

        if let Some(name) = &spec.runner.user {
            // SAFETY: plain syscall.
            if unsafe { libc::geteuid() } == 0 {
                let user = process::lookup_user(name)
                    .ok_or_else(|| format!("runner user '{name}' does not exist"))?;
                self.home = user.home.clone();
                self.user = Some(user);
            } else {
                self.log(
                    index,
                    &format!(
                        "Not running as root; steps run as the current user instead of '{name}'"
                    ),
                );
            }
        }

        for d in [
            &self.dirs.work_root,
            &self.dirs.workspace,
            &self.dirs.temp,
            &self.dirs.actions,
            &self.dirs.file_commands,
            &self.dirs.workflow,
            &self.dirs.github_home,
        ] {
            std::fs::create_dir_all(d).map_err(|e| format!("creating {}: {e}", d.display()))?;
        }
        let _ = std::fs::create_dir_all(&self.dirs.tool_cache);

        let event_src = self.opts.bundle.join(&spec.event_file);
        std::fs::copy(&event_src, &self.dirs.event_path)
            .map_err(|e| format!("copying event payload {}: {e}", event_src.display()))?;

        for action in &spec.actions {
            let src = self.opts.bundle.join(&action.path);
            let (repo, git_ref) = action
                .repo_ref
                .rsplit_once('@')
                .ok_or_else(|| format!("bad action reference {}", action.repo_ref))?;
            let dst = self.dirs.actions.join(repo).join(git_ref);
            if !dst.exists() {
                copy_dir(&src, &dst)
                    .map_err(|e| format!("preparing action {}: {e}", action.repo_ref))?;
            }
            self.log(index, &format!("Using action '{}'", action.repo_ref));
            self.actions
                .insert(action.repo_ref.to_ascii_lowercase(), dst);
        }

        if let Some(user) = &self.user {
            chown_tree(&self.dirs.work_root, user.uid, user.gid);
            if self.dirs.tool_cache.exists() {
                chown_tree(&self.dirs.tool_cache, user.uid, user.gid);
            }
        }

        self.github = self.github_context();
        self.env = Map::new();
        let layers = [spec.workflow_env.clone(), spec.job_env.clone()];
        for layer in layers {
            let scope = self.dummy_scope();
            let ctx = self.contexts(&scope, None);
            let env = Env {
                contexts: &ctx,
                functions: &baste_expr::NoFunctions,
            };
            for (k, v) in layer {
                let value =
                    eval_to_string(&v, &env).map_err(|e| format!("evaluating env {k}: {e}"))?;
                self.env.insert(k, Value::String(value));
            }
        }

        let steps: Vec<Step> = spec
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| Step::parse(s).map_err(|e| format!("step {}: {e}", i + 1)))
            .collect::<Result<_, _>>()?;
        self.log(
            index,
            &format!("Workspace: {}", self.dirs.workspace.display()),
        );
        Ok(steps)
    }

    fn dummy_scope(&self) -> Scope {
        Scope {
            step_index: 0,
            steps: Map::new(),
            inputs: None,
            action_path: None,
            action_repository: String::new(),
            action_ref: String::new(),
            env: Map::new(),
            status: Status::Success,
            depth: 0,
            defaults: RunDefaults::default(),
        }
    }

    fn github_context(&self) -> Value {
        let mut g = self
            .spec
            .contexts
            .get("github")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let token = self
            .spec
            .secrets
            .get("GITHUB_TOKEN")
            .cloned()
            .unwrap_or(Value::String(String::new()));
        g.insert("token".into(), token);
        g.insert(
            "workspace".into(),
            Value::String(self.dirs.workspace.display().to_string()),
        );
        g.insert(
            "event_path".into(),
            Value::String(self.dirs.event_path.display().to_string()),
        );
        if let Ok(text) = std::fs::read_to_string(&self.dirs.event_path) {
            if let Ok(event) = serde_json::from_str::<Value>(&text) {
                g.insert("event".into(), event);
            }
        }
        g.insert("job".into(), Value::String(self.spec.job_id.clone()));
        Value::Object(g)
    }

    /// Build the expression contexts for a scope.
    pub fn contexts(
        &self,
        scope: &Scope,
        step_env: Option<&Map<String, Value>>,
    ) -> Map<String, Value> {
        let spec = self.spec;
        let mut c = Map::new();
        let mut github = self.github.clone();
        if let Some(obj) = github.as_object_mut() {
            if let Some(p) = &scope.action_path {
                obj.insert("action_path".into(), Value::String(p.display().to_string()));
            }
            if !scope.action_repository.is_empty() {
                obj.insert(
                    "action_repository".into(),
                    Value::String(scope.action_repository.clone()),
                );
                obj.insert("action_ref".into(), Value::String(scope.action_ref.clone()));
            }
        }
        c.insert("github".into(), github);
        let mut env = self.env.clone();
        for (k, v) in &scope.env {
            env.insert(k.clone(), v.clone());
        }
        if let Some(extra) = step_env {
            for (k, v) in extra {
                env.insert(k.clone(), v.clone());
            }
        }
        c.insert("env".into(), Value::Object(env));
        c.insert(
            "job".into(),
            serde_json::json!({"status": scope.status.as_str(), "container": {}, "services": {}}),
        );
        c.insert("steps".into(), Value::Object(scope.steps.clone()));
        c.insert("runner".into(), self.runner_context());
        c.insert("secrets".into(), Value::Object(spec.secrets.clone()));
        for name in ["strategy", "matrix", "needs", "vars", "inputs"] {
            c.insert(
                name.into(),
                spec.contexts
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Map::new())),
            );
        }
        if let Some(inputs) = &scope.inputs {
            c.insert("inputs".into(), Value::Object(inputs.clone()));
        }
        c
    }

    fn runner_context(&self) -> Value {
        serde_json::json!({
            "name": self.spec.runner.name,
            "os": self.spec.runner.os,
            "arch": self.spec.runner.arch,
            "temp": self.dirs.temp.display().to_string(),
            "tool_cache": self.dirs.tool_cache.display().to_string(),
            "workspace": self.dirs.work_root.join(self.repo_name()).display().to_string(),
            "debug": if self.debug() { "1" } else { "" },
            "environment": "github-hosted",
        })
    }

    fn repo_name(&self) -> String {
        self.spec
            .checkout
            .repository
            .rsplit('/')
            .next()
            .unwrap_or("repo")
            .to_string()
    }

    fn debug(&self) -> bool {
        let truthy = |v: Option<&Value>| {
            v.map(baste_expr::to_display_string)
                .is_some_and(|s| s.eq_ignore_ascii_case("true") || s == "1")
        };
        truthy(self.spec.secrets.get("ACTIONS_STEP_DEBUG"))
            || truthy(
                self.spec
                    .contexts
                    .get("vars")
                    .and_then(|v| v.get("ACTIONS_STEP_DEBUG")),
            )
    }

    pub fn functions(&self, scope: &Scope) -> StepFunctions {
        StepFunctions {
            status: scope.status,
            workspace: self.dirs.workspace.clone(),
        }
    }

    fn step_display_name(&self, step: &Step, scope: &Scope) -> String {
        match &step.name {
            Some(n) => {
                let ctx = self.contexts(scope, None);
                let f = self.functions(scope);
                baste_expr::interpolate(
                    n,
                    &Env {
                        contexts: &ctx,
                        functions: &f,
                    },
                )
                .unwrap_or_else(|_| n.clone())
            }
            None => step.default_name(),
        }
    }

    // ----- steps -----------------------------------------------------------

    /// Evaluate a step's condition and run it. `position` is the index of a
    /// top-level step in the job (for `pre` state), `None` inside composites.
    pub fn execute_step(
        &mut self,
        step: &Step,
        scope: &mut Scope,
        position: Option<usize>,
    ) -> StepResult {
        let index = scope.step_index;
        let ctx = self.contexts(scope, None);
        let funcs = self.functions(scope);
        let env = Env {
            contexts: &ctx,
            functions: &funcs,
        };
        let cond = step.condition.clone().unwrap_or_default();
        match baste_expr::evaluate_condition(&cond, &env) {
            Ok(true) => {}
            Ok(false) => {
                if scope.depth > 0 {
                    self.log(
                        index,
                        &format!(
                            "Skipping '{}': condition is false",
                            step.name.clone().unwrap_or_else(|| step.default_name())
                        ),
                    );
                }
                return StepResult::skipped();
            }
            Err(e) => {
                self.error(
                    index,
                    &format!("Error evaluating 'if' condition '{cond}': {e}"),
                );
                return self.conclude(step, scope, StepResult::failed(None));
            }
        }

        let step_env = match self.evaluate_map(&step.env, scope, None) {
            Ok(e) => e,
            Err(e) => {
                self.error(index, &format!("Error evaluating step env: {e}"));
                return self.conclude(step, scope, StepResult::failed(None));
            }
        };

        let result = if let Some(run) = &step.run {
            self.run_script(step, run, scope, &step_env)
        } else {
            let uses = step.uses.clone().unwrap_or_default();
            crate::actions::run_uses(self, step, &uses, scope, &step_env, position)
        };
        self.conclude(step, scope, result)
    }

    /// Apply `continue-on-error` and fold the result into the scope status.
    fn conclude(&mut self, step: &Step, scope: &mut Scope, mut r: StepResult) -> StepResult {
        if r.outcome == Outcome::Failure {
            let continue_on_error = match &step.continue_on_error {
                Some(v) => {
                    let ctx = self.contexts(scope, None);
                    let f = self.functions(scope);
                    let env = Env {
                        contexts: &ctx,
                        functions: &f,
                    };
                    crate::actions::eval_bool(v, &env).unwrap_or(false)
                }
                None => false,
            };
            if continue_on_error {
                r.conclusion = Outcome::Success;
            } else if scope.status == Status::Success {
                scope.status = Status::Failure;
            }
        }
        if r.outcome == Outcome::Cancelled {
            scope.status = Status::Cancelled;
            self.cancelled = true;
        }
        r
    }

    /// Evaluate every value of a map to a string.
    pub fn evaluate_map(
        &self,
        map: &Map<String, Value>,
        scope: &Scope,
        step_env: Option<&Map<String, Value>>,
    ) -> Result<Map<String, Value>, String> {
        let mut out = Map::new();
        for (k, v) in map {
            // Later entries can't see earlier ones (GitHub evaluates them together).
            let ctx = self.contexts(scope, step_env);
            let f = self.functions(scope);
            let env = Env {
                contexts: &ctx,
                functions: &f,
            };
            let s = eval_to_string(v, &env).map_err(|e| format!("{k}: {e}"))?;
            out.insert(k.clone(), Value::String(s));
        }
        Ok(out)
    }

    fn run_script(
        &mut self,
        step: &Step,
        run: &str,
        scope: &mut Scope,
        step_env: &Map<String, Value>,
    ) -> StepResult {
        let index = scope.step_index;
        let ctx = self.contexts(scope, Some(step_env));
        let funcs = self.functions(scope);
        let env = Env {
            contexts: &ctx,
            functions: &funcs,
        };
        let script = match baste_expr::interpolate(run, &env) {
            Ok(s) => s,
            Err(e) => {
                self.error(index, &format!("Error evaluating 'run': {e}"));
                return StepResult::failed(None);
            }
        };
        let eval = |s: &Option<String>| -> Result<Option<String>, String> {
            match s {
                Some(s) => baste_expr::interpolate(s, &env)
                    .map(Some)
                    .map_err(|e| e.to_string()),
                None => Ok(None),
            }
        };
        let (shell, workdir) = match (eval(&step.shell), eval(&step.working_directory)) {
            (Ok(s), Ok(w)) => (
                s.or_else(|| scope.defaults.shell.clone()),
                w.or_else(|| scope.defaults.working_directory.clone()),
            ),
            (Err(e), _) | (_, Err(e)) => {
                self.error(index, &e);
                return StepResult::failed(None);
            }
        };
        if scope.depth > 0 && shell.is_none() {
            self.error(
                index,
                "Required property is missing: shell (composite action steps must set 'shell')",
            );
            return StepResult::failed(None);
        }
        let timeout = match self.step_timeout(step, scope) {
            Ok(t) => t,
            Err(e) => {
                self.error(index, &e);
                return StepResult::failed(None);
            }
        };

        let mut process_env = self.process_env(scope, step_env);
        let path = process_env.get("PATH").cloned().unwrap_or_default();
        let (template, ext) = match shell.as_deref() {
            None => {
                if process::which("bash", &path).is_some() {
                    ("bash -e {0}".to_string(), ".sh")
                } else {
                    ("sh -e {0}".to_string(), ".sh")
                }
            }
            Some("bash") => (
                "bash --noprofile --norc -eo pipefail {0}".to_string(),
                ".sh",
            ),
            Some("sh") => ("sh -e {0}".to_string(), ".sh"),
            Some("python") => {
                let py = if process::which("python", &path).is_some() {
                    "python"
                } else {
                    "python3"
                };
                (format!("{py} {{0}}"), ".py")
            }
            Some("pwsh") => ("pwsh -command \". '{0}'\"".to_string(), ".ps1"),
            Some(custom) if custom.contains("{0}") => (custom.to_string(), ""),
            Some(other) => {
                self.error(index, &format!("Invalid shell option '{other}'. Shell must be a valid built-in (bash, sh, python, pwsh) or a format string containing '{{0}}'"));
                return StepResult::failed(None);
            }
        };
        let script_path = self.dirs.temp.join(format!("{}{ext}", self.unique()));
        if let Err(e) = std::fs::write(&script_path, &script) {
            self.error(index, &format!("writing script: {e}"));
            return StepResult::failed(None);
        }
        if let Some(u) = &self.user {
            chown_path(&script_path, u.uid, u.gid);
        }
        let mut parts = process::split_command(&template);
        for p in parts.iter_mut() {
            *p = p.replace("{0}", &script_path.display().to_string());
        }
        let program = parts.remove(0);

        let cwd = match workdir {
            Some(w) => self.dirs.workspace.join(w),
            None => self.dirs.workspace.clone(),
        };
        if !cwd.is_dir() {
            self.error(index, &format!("An error occurred trying to start process '{program}' with working directory '{}'. No such file or directory", cwd.display()));
            return StepResult::failed(None);
        }

        for line in script.lines() {
            self.log(index, &format!("##[command]{line}"));
        }
        process_env.insert(
            "GITHUB_ACTION".into(),
            step.id.clone().unwrap_or_else(|| "__run".into()),
        );
        let r = self.exec(index, &program, &parts, process_env, &cwd, timeout);
        let _ = std::fs::remove_file(&script_path);
        match r {
            Ok(p) => StepResult::from_exit(self.note_exit(index, p.exit, timeout), p.outputs),
            Err(e) => {
                self.error(index, &e);
                StepResult::failed(None)
            }
        }
    }

    /// Log why a process ended badly and return its exit.
    pub fn note_exit(&mut self, index: usize, exit: Exit, timeout: Option<Duration>) -> Exit {
        match exit {
            Exit::Code(0) => {}
            Exit::Code(c) => self.error(index, &format!("Process completed with exit code {c}.")),
            Exit::Signal(s) => self.error(index, &format!("Process was terminated by signal {s}.")),
            Exit::TimedOut => {
                if Instant::now() >= self.deadline {
                    self.timed_out = true;
                    self.cancelled = true;
                    self.error(index, "The job has exceeded its maximum execution time.");
                    return Exit::Cancelled;
                }
                let mins = timeout.map(|t| t.as_secs_f64() / 60.0).unwrap_or(0.0);
                self.error(
                    index,
                    &format!("The action has timed out after {mins} minutes."),
                );
            }
            Exit::Cancelled => {
                self.cancelled = true;
                self.error(index, "The operation was canceled.");
            }
        }
        exit
    }

    /// The step timeout, capped by the time left for the job.
    pub fn step_timeout(&self, step: &Step, scope: &Scope) -> Result<Option<Duration>, String> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        let own = match &step.timeout_minutes {
            None | Some(Value::Null) => None,
            Some(v) => {
                let ctx = self.contexts(scope, None);
                let f = self.functions(scope);
                let env = Env {
                    contexts: &ctx,
                    functions: &f,
                };
                let s = eval_to_string(v, &env).map_err(|e| format!("timeout-minutes: {e}"))?;
                let mins: f64 = s
                    .trim()
                    .parse()
                    .map_err(|_| format!("timeout-minutes must be a number, got '{s}'"))?;
                Some(Duration::from_secs_f64(mins.max(0.0) * 60.0))
            }
        };
        Ok(Some(own.map_or(left, |o| o.min(left))))
    }

    /// Environment for a process run by a step.
    pub fn process_env(
        &self,
        scope: &Scope,
        step_env: &Map<String, Value>,
    ) -> BTreeMap<String, String> {
        let spec = self.spec;
        let mut e: BTreeMap<String, String> = BTreeMap::new();
        for k in PASSTHROUGH_ENV {
            if let Ok(v) = std::env::var(k) {
                e.insert(k.to_string(), v);
            }
        }
        let mut path = self.path_add.clone();
        path.push(self.base_path.clone());
        e.insert("PATH".into(), path.join(":"));
        e.insert("HOME".into(), self.home.display().to_string());
        let user = self
            .user
            .as_ref()
            .map(|u| u.name.clone())
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "runner".into());
        e.insert("USER".into(), user.clone());
        e.insert("LOGNAME".into(), user);
        e.insert("LANG".into(), "C.UTF-8".into());
        e.insert("CI".into(), "true".into());
        e.insert("GITHUB_ACTIONS".into(), "true".into());
        let github = self.github.as_object().cloned().unwrap_or_default();
        let g = |k: &str| {
            github
                .get(k)
                .map(baste_expr::to_display_string)
                .unwrap_or_default()
        };
        for (var, key) in [
            ("GITHUB_ACTOR", "actor"),
            ("GITHUB_ACTOR_ID", "actor_id"),
            ("GITHUB_API_URL", "api_url"),
            ("GITHUB_BASE_REF", "base_ref"),
            ("GITHUB_EVENT_NAME", "event_name"),
            ("GITHUB_GRAPHQL_URL", "graphql_url"),
            ("GITHUB_HEAD_REF", "head_ref"),
            ("GITHUB_JOB", "job"),
            ("GITHUB_REF", "ref"),
            ("GITHUB_REF_NAME", "ref_name"),
            ("GITHUB_REF_PROTECTED", "ref_protected"),
            ("GITHUB_REF_TYPE", "ref_type"),
            ("GITHUB_REPOSITORY", "repository"),
            ("GITHUB_REPOSITORY_ID", "repository_id"),
            ("GITHUB_REPOSITORY_OWNER", "repository_owner"),
            ("GITHUB_REPOSITORY_OWNER_ID", "repository_owner_id"),
            ("GITHUB_RETENTION_DAYS", "retention_days"),
            ("GITHUB_RUN_ATTEMPT", "run_attempt"),
            ("GITHUB_RUN_ID", "run_id"),
            ("GITHUB_RUN_NUMBER", "run_number"),
            ("GITHUB_SERVER_URL", "server_url"),
            ("GITHUB_SHA", "sha"),
            ("GITHUB_TRIGGERING_ACTOR", "triggering_actor"),
            ("GITHUB_WORKFLOW", "workflow"),
            ("GITHUB_WORKFLOW_REF", "workflow_ref"),
            ("GITHUB_WORKFLOW_SHA", "workflow_sha"),
        ] {
            e.insert(var.into(), g(key));
        }
        e.insert(
            "GITHUB_WORKSPACE".into(),
            self.dirs.workspace.display().to_string(),
        );
        e.insert(
            "GITHUB_EVENT_PATH".into(),
            self.dirs.event_path.display().to_string(),
        );
        e.insert("RUNNER_OS".into(), spec.runner.os.clone());
        e.insert("RUNNER_ARCH".into(), spec.runner.arch.clone());
        e.insert("RUNNER_NAME".into(), spec.runner.name.clone());
        e.insert("RUNNER_TEMP".into(), self.dirs.temp.display().to_string());
        e.insert(
            "RUNNER_TOOL_CACHE".into(),
            self.dirs.tool_cache.display().to_string(),
        );
        e.insert(
            "AGENT_TOOLSDIRECTORY".into(),
            self.dirs.tool_cache.display().to_string(),
        );
        e.insert(
            "RUNNER_WORKSPACE".into(),
            self.dirs
                .work_root
                .join(self.repo_name())
                .display()
                .to_string(),
        );
        e.insert("RUNNER_ENVIRONMENT".into(), "github-hosted".into());
        e.insert("ImageOS".into(), "ubuntu24".into());
        if self.debug() {
            e.insert("RUNNER_DEBUG".into(), "1".into());
        }
        for (k, v) in &spec.runner.env {
            e.insert(k.clone(), baste_expr::to_display_string(v));
        }
        for layer in [&self.env, &scope.env, step_env] {
            for (k, v) in layer {
                e.insert(k.clone(), baste_expr::to_display_string(v));
            }
        }
        if let Some(p) = &scope.action_path {
            e.insert("GITHUB_ACTION_PATH".into(), p.display().to_string());
        }
        if !scope.action_repository.is_empty() {
            e.insert(
                "GITHUB_ACTION_REPOSITORY".into(),
                scope.action_repository.clone(),
            );
            e.insert("GITHUB_ACTION_REF".into(), scope.action_ref.clone());
        }
        e
    }

    pub fn new_file_commands(&self) -> Result<FileCommands, String> {
        let id = self.unique();
        let d = &self.dirs.file_commands;
        let fc = FileCommands {
            env: d.join(format!("set_env_{id}")),
            output: d.join(format!("set_output_{id}")),
            path: d.join(format!("add_path_{id}")),
            state: d.join(format!("save_state_{id}")),
            summary: d.join(format!("step_summary_{id}")),
        };
        for p in [&fc.env, &fc.output, &fc.path, &fc.state, &fc.summary] {
            std::fs::write(p, "").map_err(|e| format!("creating {}: {e}", p.display()))?;
            if let Some(u) = &self.user {
                chown_path(p, u.uid, u.gid);
            }
        }
        Ok(fc)
    }

    /// Run a process for step `index`, handling workflow commands in its output
    /// and the environment files it writes.
    pub fn exec(
        &mut self,
        index: usize,
        program: &str,
        args: &[String],
        mut env: BTreeMap<String, String>,
        cwd: &Path,
        timeout: Option<Duration>,
    ) -> Result<ProcResult, String> {
        let fc = self.new_file_commands()?;
        env.insert("GITHUB_ENV".into(), fc.env.display().to_string());
        env.insert("GITHUB_OUTPUT".into(), fc.output.display().to_string());
        env.insert("GITHUB_PATH".into(), fc.path.display().to_string());
        env.insert("GITHUB_STATE".into(), fc.state.display().to_string());
        env.insert(
            "GITHUB_STEP_SUMMARY".into(),
            fc.summary.display().to_string(),
        );
        self.exec_with(index, program, args, env, cwd, timeout, &fc)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn exec_with(
        &mut self,
        index: usize,
        program: &str,
        args: &[String],
        env: BTreeMap<String, String>,
        cwd: &Path,
        timeout: Option<Duration>,
        fc: &FileCommands,
    ) -> Result<ProcResult, String> {
        let spawn = Spawn {
            program: program.to_string(),
            args: args.to_vec(),
            env,
            cwd: cwd.to_path_buf(),
            user: self.user.clone(),
            timeout,
        };
        let mut lp = LineProcessor::new(index, self.debug());
        let stop = self.opts.cancel.clone();
        let result = {
            let this: &Job = self;
            process::run(&spawn, &stop, &mut |line| lp.process(this, line))
        };
        let (exit, pgid) = result.map_err(|e| e.to_string())?;
        if process::group_alive(pgid) {
            self.pgids.push(pgid);
        }
        let mut exit = exit;
        if lp.failed && exit.success() {
            exit = Exit::Code(1);
        }
        let mut outputs: Map<String, Value> = lp
            .outputs
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect();
        let mut state = lp.state;
        match self.apply_file_commands(index, fc) {
            Ok((o, s)) => {
                outputs.extend(o);
                state.extend(s);
            }
            Err(e) => {
                self.error(index, &e);
                if exit.success() {
                    exit = Exit::Code(1);
                }
            }
        }
        Ok(ProcResult {
            exit,
            outputs,
            state,
        })
    }

    fn apply_file_commands(
        &mut self,
        index: usize,
        fc: &FileCommands,
    ) -> Result<(Map<String, Value>, Vec<(String, String)>), String> {
        let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
        let cleanup = || {
            for p in [&fc.env, &fc.output, &fc.path, &fc.state, &fc.summary] {
                let _ = std::fs::remove_file(p);
            }
        };
        let result = (|| {
            for (k, v) in parse_env_file(&read(&fc.env))
                .map_err(|e| format!("Unable to process file command 'env': {e}"))?
            {
                if k == "NODE_OPTIONS" && v.contains("--require") {
                    // Mirrors GitHub's block on code injection via NODE_OPTIONS.
                    self.warning(
                        index,
                        "Can't store NODE_OPTIONS output parameter using '$GITHUB_ENV' command.",
                    );
                    continue;
                }
                self.env.insert(k, Value::String(v));
            }
            for line in read(&fc.path)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
            {
                self.path_add.insert(0, line.to_string());
            }
            let outputs: Map<String, Value> = parse_env_file(&read(&fc.output))
                .map_err(|e| format!("Unable to process file command 'output': {e}"))?
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect();
            let state = parse_env_file(&read(&fc.state))
                .map_err(|e| format!("Unable to process file command 'state': {e}"))?;
            let summary = read(&fc.summary);
            if !summary.trim().is_empty() {
                let summary = if summary.len() > 1024 * 1024 {
                    self.warning(index, "Step summary exceeds 1MiB and was truncated.");
                    summary.chars().take(1024 * 1024).collect()
                } else {
                    summary
                };
                self.sink.send(Event::Summary {
                    step: index,
                    markdown: self.masker.mask(&summary),
                });
            }
            Ok((outputs, state))
        })();
        cleanup();
        result
    }

    fn run_pre_steps(&mut self, steps: &[Step], scope: &mut Scope) {
        for (position, step) in steps.iter().enumerate() {
            let Some(pre) = crate::actions::pre_step(self, step) else {
                continue;
            };
            self.check_cancel(scope);
            let name = format!("Pre {}", self.step_display_name(step, scope));
            let index = self.start_step(&name, StepPhase::Pre);
            scope.step_index = index;
            let started = Instant::now();
            let r = crate::actions::run_pre(self, step, pre, scope, position);
            let r = self.conclude(step, scope, r);
            self.finish_step(index, &r, started);
        }
    }

    fn run_post(&mut self, post: PostStep, top: &mut Scope) {
        let mut scope = post.scope.clone();
        scope.status = top.status;
        let ctx = self.contexts(&scope, None);
        let f = self.functions(&scope);
        let env = Env {
            contexts: &ctx,
            functions: &f,
        };
        let run = match baste_expr::evaluate_condition(&post.condition, &env) {
            Ok(b) => b,
            Err(e) => {
                let index = self.start_step(&post.name, StepPhase::Post);
                self.error(index, &format!("Error evaluating post condition: {e}"));
                self.finish_step(index, &StepResult::failed(None), Instant::now());
                return;
            }
        };
        if !run {
            return;
        }
        let index = self.start_step(&post.name, StepPhase::Post);
        let started = Instant::now();
        let r = crate::actions::run_post(self, post, index);
        // A failing post step fails the job, as on GitHub.
        if r.outcome == Outcome::Failure && top.status == Status::Success {
            top.status = Status::Failure;
        }
        self.finish_step(index, &r, started);
    }

    fn job_outputs(&mut self) -> Map<String, Value> {
        let mut out = Map::new();
        if self.spec.outputs.is_empty() {
            return out;
        }
        let mut scope = self.dummy_scope();
        scope.steps = self.collected_steps.clone();
        for (name, template) in &self.spec.outputs {
            let ctx = self.contexts(&scope, None);
            let f = self.functions(&scope);
            let env = Env {
                contexts: &ctx,
                functions: &f,
            };
            match eval_to_string(template, &env) {
                Ok(v) if self.masker.contains_secret(&v) => {
                    self.warning(
                        0,
                        &format!("Skip output '{name}' since it may contain secret."),
                    );
                }
                Ok(v) => {
                    out.insert(name.clone(), Value::String(v));
                }
                Err(e) => self.warning(0, &format!("Error evaluating output '{name}': {e}")),
            }
        }
        out
    }

    fn cleanup_processes(&mut self) {
        for pgid in std::mem::take(&mut self.pgids) {
            if process::group_alive(pgid) {
                process::kill_group(pgid, libc::SIGTERM);
            }
        }
    }
}

/// Handles one line of step output: workflow commands, masking, logging.
pub(crate) struct LineProcessor {
    step: usize,
    debug: bool,
    stop_token: Option<String>,
    echo: bool,
    pub outputs: Vec<(String, String)>,
    pub state: Vec<(String, String)>,
    pub failed: bool,
}

impl LineProcessor {
    pub fn new(step: usize, debug: bool) -> Self {
        LineProcessor {
            step,
            debug,
            stop_token: None,
            echo: false,
            outputs: vec![],
            state: vec![],
            failed: false,
        }
    }

    pub fn process(&mut self, job: &Job, line: String) {
        if let Some(token) = &self.stop_token {
            if line.trim() == format!("::{token}::") {
                self.stop_token = None;
            } else {
                job.log(self.step, &line);
            }
            return;
        }
        let Some(cmd) = parse_command(&line) else {
            job.log(self.step, &line);
            return;
        };
        if self.echo {
            job.log(self.step, &line);
        }
        let step = self.step;
        let annotation = |level: AnnotationLevel, prefix: &str| {
            let message = job.masker.mask(&cmd.message);
            let file = cmd.properties.get("file").cloned();
            let line_no = cmd.properties.get("line").and_then(|l| l.parse().ok());
            let location = match (&file, line_no) {
                (Some(f), Some(l)) => format!(" ({f}:{l})"),
                (Some(f), None) => format!(" ({f})"),
                _ => String::new(),
            };
            job.sink.send(Event::Log {
                step,
                line: format!("##[{prefix}]{message}{location}"),
            });
            job.sink.send(Event::Annotation {
                step,
                level,
                message,
                file,
                line: line_no,
                title: cmd.properties.get("title").map(|t| job.masker.mask(t)),
            });
        };
        match cmd.name.as_str() {
            "add-mask" => job.masker.add(&cmd.message),
            "set-output" => match cmd.properties.get("name") {
                Some(n) => self.outputs.push((n.clone(), cmd.message.clone())),
                None => job.warning(step, "set-output command is missing 'name'"),
            },
            "save-state" => {
                if let Some(n) = cmd.properties.get("name") {
                    self.state.push((n.clone(), cmd.message.clone()));
                }
            }
            "error" => annotation(AnnotationLevel::Error, "error"),
            "warning" => annotation(AnnotationLevel::Warning, "warning"),
            "notice" => annotation(AnnotationLevel::Notice, "notice"),
            "debug" => {
                if self.debug {
                    job.log(step, &format!("##[debug]{}", cmd.message));
                }
            }
            "group" => job.log(step, &format!("##[group]{}", cmd.message)),
            "endgroup" => job.log(step, "##[endgroup]"),
            "stop-commands" => self.stop_token = Some(cmd.message.clone()),
            "echo" => self.echo = cmd.message.trim().eq_ignore_ascii_case("on"),
            "set-env" | "add-path" => {
                job.error(step, &format!("Unable to process command '::{}' successfully. The `{}` command is disabled. Please upgrade to using Environment Files.", cmd.name, cmd.name));
                self.failed = true;
            }
            _ => job.log(step, &line),
        }
    }
}

/// The `steps.<id>` context entry for a result.
pub(crate) fn step_context(r: &StepResult) -> Value {
    serde_json::json!({
        "outputs": r.outputs,
        "outcome": r.outcome.as_str(),
        "conclusion": r.conclusion.as_str(),
    })
}

/// Evaluate a scalar or template to a string.
pub(crate) fn eval_to_string(v: &Value, env: &Env) -> Result<String, ExprError> {
    match v {
        Value::String(s) => baste_expr::interpolate(s, env),
        other => Ok(scalar_string(other).unwrap_or_default()),
    }
}

pub(crate) fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else if ft.is_symlink() {
            let target = std::fs::read_link(entry.path())?;
            let _ = std::fs::remove_file(&to);
            std::os::unix::fs::symlink(target, &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

pub(crate) fn chown_path(p: &Path, uid: u32, gid: u32) {
    let _ = std::os::unix::fs::lchown(p, Some(uid), Some(gid));
}

pub(crate) fn chown_tree(root: &Path, uid: u32, gid: u32) {
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
    {
        chown_path(entry.path(), uid, gid);
    }
}

trait SpecExt {
    fn defaults_as_run_defaults(&self) -> RunDefaults;
}

impl SpecExt for JobSpec {
    fn defaults_as_run_defaults(&self) -> RunDefaults {
        RunDefaults {
            shell: self.defaults.shell.clone(),
            working_directory: self.defaults.working_directory.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_timestamps() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_723), (2026, 9, 27));
        assert!(now().ends_with('Z'));
    }
}
