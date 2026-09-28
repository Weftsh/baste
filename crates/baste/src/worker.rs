//! The background worker that owns one run: plan it, post statuses, run each
//! local job in a fresh VM in dependency order, and record everything.

use crate::backend::{self, Backend, JobLaunch};
use crate::bundle::{self, ActionCache, BundleInput, PackCache};
use crate::config::Config;
use crate::git::Git;
use crate::github::GitHub;
use crate::plan::{self, PlannedWorkflow};
use crate::poster::{StatusPoster, StatusUpdate};
use crate::secrets::Secrets;
use crate::store::{
    JobRecord, JobState, Provenance, Run, RunState, StepRecord, StepState, Store, Trigger,
};
use crate::ui::duration;
use anyhow::{anyhow, Result};
use base64::Engine;
use baste_expr::{Env, Error as ExprError, Functions, Map, Value};
use baste_protocol::{Event, Outcome, StepPhase};
use baste_workflow::{expand_job, Placement};
use chrono::Utc;
use serde_json::json;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Set by SIGTERM/SIGINT: cancel the run.
static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    CANCEL_REQUESTED.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
}

/// Run a queued run to completion. Called in the detached worker process.
pub fn work(store: Store, git: Git, run_id: &str) -> Result<()> {
    install_signal_handlers();
    let config = Config::load().unwrap_or_default();
    let mut run = store.load(run_id)?;
    run.worker_pid = Some(std::process::id());
    run.state = RunState::Running;
    run.started_at = Some(Utc::now());
    store.save(&run)?;

    let api = match GitHub::from_gh(run.repo.clone()) {
        Ok(a) => Some(a),
        Err(e) => {
            run.notes
                .push(format!("No GitHub access ({e}); statuses won't be posted."));
            None
        }
    };
    let wait = Duration::from_secs(if run.post_statuses {
        match run.trigger {
            Trigger::Push | Trigger::Rerun { .. } => config.status_wait_minutes * 60,
            Trigger::Manual => 20,
        }
    } else {
        0
    });
    let poster_api = api.clone().filter(|_| run.post_statuses);
    let poster = StatusPoster::start(poster_api, run.sha.clone(), wait);

    let mut w = Worker {
        shared: Arc::new(Shared {
            store: store.clone(),
            git,
            run: Mutex::new(run),
            planned: vec![],
            backend: None,
            poster,
            api,
            secrets: None,
            secrets_error: None,
            actions: ActionCache::new(None),
            packs: PackCache::new(store.run_dir(run_id).join("checkout")),
            config,
            last_save: Mutex::new(Instant::now()),
        }),
    };
    let result = w.run();
    let shared = Arc::get_mut(&mut w.shared).ok_or_else(|| anyhow!("job threads still running"))?;
    if let Err(e) = &result {
        let run = shared.run.get_mut().unwrap();
        run.error = Some(format!("{e:#}"));
        run.state = RunState::Error;
    }
    // Flush statuses (waiting for the push to land if needed).
    shared.poster.finish();
    let notes = shared.poster.notes.lock().unwrap().clone();
    let run = shared.run.get_mut().unwrap();
    run.notes.extend(notes);
    run.finished_at.get_or_insert_with(Utc::now);
    run.worker_pid = None;
    store.save(run)?;
    let _ = std::fs::remove_dir_all(store.run_dir(run_id).join("checkout"));
    let _ = store.prune(50);
    result
}

struct Shared {
    store: Store,
    git: Git,
    run: Mutex<Run>,
    planned: Vec<PlannedWorkflow>,
    backend: Option<Arc<dyn Backend>>,
    poster: StatusPoster,
    api: Option<GitHub>,
    secrets: Option<Secrets>,
    secrets_error: Option<String>,
    actions: ActionCache,
    packs: PackCache,
    config: Config,
    last_save: Mutex<Instant>,
}

impl Shared {
    fn run_id(&self) -> String {
        self.run.lock().unwrap().id.clone()
    }

    fn save(&self) {
        // Serialize saves (snapshot and write together) so job threads never
        // interleave writes or replace a newer snapshot with an older one.
        let mut last = self.last_save.lock().unwrap();
        let run = self.run.lock().unwrap().clone();
        let _ = self.store.save(&run);
        *last = Instant::now();
    }

    /// Save at most every 300ms (used for high-frequency step updates).
    fn save_soon(&self) {
        if self.last_save.lock().unwrap().elapsed() > Duration::from_millis(300) {
            self.save();
        }
    }

    fn with_job<R>(&self, key: &str, f: impl FnOnce(&mut JobRecord) -> R) -> Option<R> {
        let mut run = self.run.lock().unwrap();
        run.job_mut(key).map(f)
    }

    fn details_url(&self) -> String {
        let run = self.run.lock().unwrap();
        self.config
            .details_url
            .replace("{run}", &run.id)
            .replace("{repo}", &run.repo.full_name())
            .replace("{sha}", &run.sha)
    }

    fn post(&self, record: &JobRecord, state: &'static str, description: String) {
        if let Some(context) = &record.context {
            self.poster.update(StatusUpdate {
                context: context.clone(),
                state,
                description,
                target_url: Some(self.details_url()),
            });
        }
    }

    /// Post the final status for a finished job.
    fn post_final(&self, record: &JobRecord) {
        let id = self.run_id();
        let took = record.duration_ms().map(duration).unwrap_or_default();
        let (state, description) = match record.state {
            JobState::Passed => ("success", format!("Passed in {took} · baste logs {id}")),
            JobState::Failed => match (record.failed_step(), &record.error) {
                (Some(step), _) => (
                    "failure",
                    // Keep room for the duration and run id: a run step's
                    // default name is its whole command.
                    format!(
                        "Failed at '{}' after {took} · baste logs {id}",
                        crate::poster::shorten(&step.name, 60)
                    ),
                ),
                (None, Some(e)) if record.steps.len() <= 1 => (
                    "failure",
                    format!("Failed: {} · baste logs {id}", first_line(e)),
                ),
                _ => ("failure", format!("Failed after {took} · baste logs {id}")),
            },
            JobState::Cancelled => (
                "error",
                format!(
                    "Cancelled{} · baste logs {id}",
                    record
                        .reason
                        .as_deref()
                        .map(|r| format!(": {r}"))
                        .unwrap_or_default()
                ),
            ),
            JobState::Skipped => (
                "success",
                format!(
                    "Skipped: {} · run {id}",
                    record
                        .reason
                        .clone()
                        .unwrap_or_else(|| "condition is false".into())
                ),
            ),
            JobState::NotRun => (
                "failure",
                format!(
                    "Not run: {} · run {id}",
                    record.reason.clone().unwrap_or_default()
                ),
            ),
            JobState::Queued | JobState::Running | JobState::HandedToGithub => return,
        };
        self.post(record, state, description);
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or_default().to_string()
}

struct Worker {
    shared: Arc<Shared>,
}

/// A job whose needs are met and that waits for a slot.
struct Ready {
    key: String,
    pw: usize,
    job_id: String,
    strategy: Value,
    needs: Value,
}

struct Done {
    key: String,
}

#[derive(Default)]
struct Group {
    released: bool,
    fail_fast: bool,
    max_parallel: usize,
    running: usize,
}

impl Worker {
    fn shared_mut(&mut self) -> &mut Shared {
        Arc::get_mut(&mut self.shared).expect("no job threads during setup")
    }

    fn run(&mut self) -> Result<()> {
        // ---- plan ------------------------------------------------------
        let (backend, facts_run) = {
            let s = self.shared_mut();
            let mut run = s.run.lock().unwrap().clone();
            let backend = backend::select(&s.config, run.backend_override.as_deref())?;
            run.provenance = Provenance {
                executor: "local".into(),
                host: crate::sys::hostname(),
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
                backend: backend.name().into(),
                image: backend.image(),
                baste_version: env!("CARGO_PKG_VERSION").into(),
                policy: crate::routing::POLICY.into(),
            };
            let facts = plan::gather_facts(&mut run, &s.git, s.api.as_ref());
            let planned = plan::plan(&mut run, &s.git, s.api.as_ref(), &s.store, &facts)?;
            s.planned = planned;
            s.secrets = match Secrets::open(&format!("{}/{}", run.repo.host, run.repo.full_name()))
            {
                Ok(sec) => Some(sec),
                Err(e) => {
                    // Only worth mentioning to jobs that use secrets.
                    s.secrets_error = Some(e.to_string());
                    None
                }
            };
            s.actions = ActionCache::new(s.api.clone());
            s.backend = Some(backend.clone());
            *s.run.lock().unwrap() = run.clone();
            (backend, run)
        };
        self.shared.save();

        for record in facts_run
            .jobs
            .iter()
            .filter(|j| j.state == JobState::Queued)
        {
            self.shared.post(
                record,
                "pending",
                format!("Queued locally · baste run {}", facts_run.id),
            );
        }
        if !facts_run.jobs.iter().any(|j| j.state == JobState::Queued) {
            self.finish_run(false);
            return Ok(());
        }

        self.cancel_superseded(&facts_run);
        self.wait_for_power(&facts_run);

        // ---- prepare the image once ------------------------------------
        let prepare_log = self.shared.store.run_dir(&facts_run.id).join("prepare.log");
        let mut log_file = std::fs::File::create(&prepare_log).ok();
        let prepared = backend.prepare(&mut |line: &str| {
            if let Some(f) = log_file.as_mut() {
                let _ = writeln!(f, "{line}");
            }
        });
        if let Err(e) = prepared {
            let msg = format!("Preparing the {} VM image failed: {e:#}", backend.name());
            self.fail_all_queued(&msg);
            return Err(anyhow!(msg));
        }

        self.schedule(backend);
        let cancelled = CANCEL_REQUESTED.load(Ordering::SeqCst);
        self.finish_run(cancelled);
        Ok(())
    }

    /// Cancel older runs of the same branch that are still going.
    fn cancel_superseded(&self, run: &Run) {
        if !self.shared.config.cancel_superseded || !matches!(run.trigger, Trigger::Push) {
            return;
        }
        for other in self.shared.store.list().unwrap_or_default() {
            if other.id != run.id
                && other.git_ref == run.git_ref
                && !other.state.is_done()
                && other.created_at < run.created_at
                && other.worker_alive()
            {
                if let Some(pid) = other.worker_pid {
                    // SAFETY: plain syscall.
                    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                    self.shared.run.lock().unwrap().notes.push(format!(
                        "Cancelled the older run {} of {}",
                        other.id,
                        run.branch()
                    ));
                }
            }
        }
    }

    fn wait_for_power(&self, run: &Run) {
        if !self.shared.config.pause_on_battery || crate::sys::on_battery() != Some(true) {
            return;
        }
        for record in run.jobs.iter().filter(|j| j.state == JobState::Queued) {
            self.shared.post(
                record,
                "pending",
                format!("Paused: on battery power · baste run {}", run.id),
            );
        }
        while crate::sys::on_battery() == Some(true) && !CANCEL_REQUESTED.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_secs(15));
        }
    }

    fn fail_all_queued(&self, msg: &str) {
        let records: Vec<JobRecord> = {
            let mut run = self.shared.run.lock().unwrap();
            for j in run.jobs.iter_mut().filter(|j| j.state == JobState::Queued) {
                j.state = JobState::Failed;
                j.error = Some(msg.to_string());
                j.finished_at = Some(Utc::now());
            }
            run.jobs.clone()
        };
        for r in records.iter().filter(|r| r.state == JobState::Failed) {
            self.shared.post_final(r);
        }
        self.shared.save();
    }

    fn finish_run(&self, cancelled: bool) {
        let insights = {
            let run = self.shared.run.lock().unwrap().clone();
            crate::insights::run_insights(&run, self.shared.api.as_ref())
        };
        let mut run = self.shared.run.lock().unwrap();
        run.insights = Some(insights);
        let local: Vec<&JobRecord> = run
            .jobs
            .iter()
            .filter(|j| j.state != JobState::HandedToGithub)
            .collect();
        let failed = local.iter().any(|j| {
            matches!(
                j.state,
                JobState::Failed | JobState::NotRun | JobState::Cancelled
            ) && !j.continue_on_error
        });
        run.state = if cancelled {
            RunState::Cancelled
        } else if failed {
            RunState::Failed
        } else {
            RunState::Passed
        };
        run.finished_at = Some(Utc::now());
        drop(run);
        self.shared.save();
        self.notify();
    }

    /// Tell the developer how a background run ended.
    fn notify(&self) {
        let run = self.shared.run.lock().unwrap().clone();
        let ran_locally = run.jobs.iter().any(|j| j.state != JobState::HandedToGithub);
        if !self.shared.config.notify || !matches!(run.trigger, Trigger::Push) || !ran_locally {
            return;
        }
        let verdict = match run.state {
            RunState::Passed => "passed",
            RunState::Failed => "failed",
            RunState::Cancelled => return,
            _ => "stopped",
        };
        let mut body = format!(
            "{} · {} · baste logs {}",
            run.short_sha(),
            run.duration_ms().map(duration).unwrap_or_default(),
            run.id
        );
        if let Some(saved) = run
            .insights
            .as_ref()
            .and_then(|i| i.time_saved_ms)
            .filter(|ms| *ms > 0)
        {
            body.push_str(&format!(" · saved {}", duration(saved)));
        }
        crate::sys::notify(&format!("CI {verdict} on {}", run.branch()), &body);
    }

    /// Run jobs in dependency order with at most `max_parallel_jobs` at once.
    fn schedule(&mut self, backend: Arc<dyn Backend>) {
        let (tx, rx) = mpsc::channel::<Done>();
        let mut groups: HashMap<(usize, String), Group> = HashMap::new();
        let mut ready: Vec<Ready> = Vec::new();
        let mut running: HashMap<String, (Arc<AtomicBool>, (usize, String))> = HashMap::new();
        let mut handles = Vec::new();
        let max = self.shared.config.max_parallel_jobs.max(1);
        let mut cancel_seen = false;

        loop {
            if CANCEL_REQUESTED.load(Ordering::SeqCst) && !cancel_seen {
                cancel_seen = true;
                for (flag, _) in running.values() {
                    flag.store(true, Ordering::SeqCst);
                }
                ready.clear();
                self.cancel_queued("the run was cancelled", None);
            }

            // Release jobs whose needs are done.
            if !cancel_seen {
                for (pw_index, pw) in self.shared.planned.iter().enumerate() {
                    for job in &pw.workflow.jobs {
                        let key = (pw_index, job.id.clone());
                        if groups.get(&key).is_some_and(|g| g.released) {
                            continue;
                        }
                        if let Some(group) = self.try_release(pw_index, pw, job, &mut ready) {
                            groups.insert(key, group);
                        }
                    }
                }
            }

            // Start what we can.
            let mut i = 0;
            while i < ready.len() && running.len() < max {
                let group_key = (ready[i].pw, ready[i].job_id.clone());
                let group = groups.entry(group_key.clone()).or_default();
                if group.running >= group.max_parallel.max(1) {
                    i += 1;
                    continue;
                }
                let task = ready.remove(i);
                group.running += 1;
                let flag = Arc::new(AtomicBool::new(false));
                running.insert(task.key.clone(), (flag.clone(), group_key));
                let shared = self.shared.clone();
                let backend = backend.clone();
                let tx = tx.clone();
                handles.push(std::thread::spawn(move || {
                    let key = task.key.clone();
                    run_job_thread(&shared, backend.as_ref(), task, flag);
                    let _ = tx.send(Done { key });
                }));
            }

            let unreleased = self.shared.planned.iter().enumerate().any(|(i, pw)| {
                pw.workflow
                    .jobs
                    .iter()
                    .any(|j| !groups.get(&(i, j.id.clone())).is_some_and(|g| g.released))
            });
            if running.is_empty() && ready.is_empty() && (!unreleased || cancel_seen) {
                break;
            }
            if running.is_empty() && ready.is_empty() && unreleased {
                // Nothing can make progress (shouldn't happen with a valid DAG).
                self.cancel_queued("its needs could not be met", None);
                break;
            }

            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(done) => {
                    if let Some((_, group_key)) = running.remove(&done.key) {
                        if let Some(g) = groups.get_mut(&group_key) {
                            g.running = g.running.saturating_sub(1);
                        }
                        let record = self.shared.run.lock().unwrap().job(&done.key).cloned();
                        if let Some(record) = record {
                            self.shared.post_final(&record);
                            let fail_fast = groups.get(&group_key).is_some_and(|g| g.fail_fast);
                            if record.state == JobState::Failed
                                && !record.continue_on_error
                                && fail_fast
                            {
                                let reason = format!("fail-fast after '{}' failed", record.name);
                                ready.retain(|r| (r.pw, r.job_id.clone()) != group_key);
                                self.cancel_queued(&reason, Some(&group_key));
                                for (flag, gk) in running.values() {
                                    if *gk == group_key {
                                        flag.store(true, Ordering::SeqCst);
                                    }
                                }
                            }
                        }
                    }
                    self.shared.save();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        for h in handles {
            let _ = h.join();
        }
    }

    /// Mark queued jobs cancelled (optionally only one job group).
    fn cancel_queued(&self, reason: &str, only: Option<&(usize, String)>) {
        let changed: Vec<JobRecord> = {
            let mut run = self.shared.run.lock().unwrap();
            let files: Vec<String> = self
                .shared
                .planned
                .iter()
                .map(|p| p.workflow.file.clone())
                .collect();
            let mut out = Vec::new();
            for j in run.jobs.iter_mut().filter(|j| j.state == JobState::Queued) {
                if let Some((pw, id)) = only {
                    if files.get(*pw) != Some(&j.workflow_file) || &j.job_id != id {
                        continue;
                    }
                }
                j.state = JobState::Cancelled;
                j.reason = Some(reason.to_string());
                j.finished_at = Some(Utc::now());
                out.push(j.clone());
            }
            out
        };
        for r in &changed {
            self.shared.post_final(r);
        }
        self.shared.save();
    }

    /// If all needs of `job` are done, decide its fate and queue its local
    /// instances. Returns the group state once released.
    fn try_release(
        &self,
        pw_index: usize,
        pw: &PlannedWorkflow,
        job: &baste_workflow::Job,
        ready: &mut Vec<Ready>,
    ) -> Option<Group> {
        let file = &pw.workflow.file;
        let snapshot: Vec<JobRecord> = self
            .shared
            .run
            .lock()
            .unwrap()
            .jobs
            .iter()
            .filter(|j| &j.workflow_file == file)
            .cloned()
            .collect();
        let mine: Vec<&JobRecord> = snapshot.iter().filter(|j| j.job_id == job.id).collect();
        if mine.is_empty() {
            // Filtered out (e.g. `baste run --job`), nothing to do.
            return Some(Group {
                released: true,
                ..Default::default()
            });
        }
        if mine.iter().all(|j| j.state.is_done()) {
            return Some(Group {
                released: true,
                ..Default::default()
            });
        }

        // Needs must be finished.
        let mut needs_ctx = Map::new();
        let mut needs_ok = true;
        let mut needs_failed = false;
        let mut failed_need: Option<String> = None;
        let mut github_need: Option<String> = None;
        for need in &job.needs {
            let recs: Vec<&JobRecord> = snapshot.iter().filter(|j| &j.job_id == need).collect();
            if recs.is_empty() {
                continue;
            }
            if recs.iter().any(|j| !j.state.is_done()) {
                return None;
            }
            if recs.iter().any(|j| j.state == JobState::HandedToGithub) {
                github_need = Some(need.clone());
            }
            let result = aggregate_result(&recs);
            match result {
                "success" => {}
                "failure" | "cancelled" => {
                    needs_ok = false;
                    needs_failed = needs_failed || result == "failure";
                    failed_need.get_or_insert(need.clone());
                }
                _ => needs_ok = false,
            }
            let mut outputs = Map::new();
            for r in &recs {
                for (k, v) in &r.outputs {
                    outputs.insert(k.clone(), v.clone());
                }
            }
            needs_ctx.insert(need.clone(), json!({"result": result, "outputs": outputs}));
        }
        let needs_value = Value::Object(needs_ctx);

        if let Some(need) = github_need {
            self.set_group_state(
                file,
                &job.id,
                JobState::HandedToGithub,
                format!("it needs '{need}', which runs on GitHub"),
            );
            return Some(Group {
                released: true,
                ..Default::default()
            });
        }

        // Job-level `if`.
        let ctx = plan::job_level_contexts(pw, needs_value.clone());
        let funcs = JobStatusFunctions {
            needs_ok,
            needs_failed,
            cancelled: CANCEL_REQUESTED.load(Ordering::SeqCst),
        };
        let env = Env {
            contexts: &ctx,
            functions: &funcs,
        };
        let condition = job.condition.clone().unwrap_or_default();
        match baste_expr::evaluate_condition(&condition, &env) {
            Ok(true) => {}
            Ok(false) => {
                let (state, reason) = match (&failed_need, condition.trim().is_empty()) {
                    (Some(n), _) => (JobState::NotRun, format!("needs '{n}' failed")),
                    (None, true) => (JobState::Skipped, "a job it needs was skipped".to_string()),
                    (None, false) => (JobState::Skipped, "condition is false".to_string()),
                };
                self.set_group_state(file, &job.id, state, reason);
                return Some(Group {
                    released: true,
                    ..Default::default()
                });
            }
            Err(e) => {
                self.fail_group(
                    file,
                    &job.id,
                    format!("Error evaluating 'if' of job '{}': {e}", job.id),
                );
                return Some(Group {
                    released: true,
                    ..Default::default()
                });
            }
        }

        // Expand (again) now that needs are known.
        let instances = match expand_job(&pw.workflow, job, &ctx) {
            Ok(i) => i,
            Err(e) => {
                self.fail_group(file, &job.id, e);
                return Some(Group {
                    released: true,
                    ..Default::default()
                });
            }
        };
        let fail_fast = instances
            .first()
            .and_then(|i| i.strategy.get("fail-fast"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let max_parallel = instances
            .first()
            .and_then(|i| i.strategy.get("max-parallel"))
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX) as usize;

        let records: Vec<JobRecord> = {
            let mut run = self.shared.run.lock().unwrap();
            let positions: Vec<usize> = run
                .jobs
                .iter()
                .enumerate()
                .filter(|(_, j)| &j.workflow_file == file && j.job_id == job.id)
                .map(|(i, _)| i)
                .collect();
            let existing: Vec<JobRecord> = positions.iter().map(|&i| run.jobs[i].clone()).collect();
            let placeholder = existing.iter().any(|j| j.pending_expansion);
            let mut records = if placeholder || existing.len() != instances.len() {
                let first = positions[0];
                for &i in positions.iter().rev() {
                    run.jobs.remove(i);
                }
                let fresh = plan::instance_records(&run, pw, job, &instances);
                for (n, r) in fresh.iter().enumerate() {
                    run.jobs.insert(first + n, r.clone());
                }
                fresh
            } else {
                existing
            };
            for (r, inst) in records.iter_mut().zip(&instances) {
                r.continue_on_error = job_continue_on_error(pw, job, inst, &needs_value);
                if let Some(rec) = run.job_mut(&r.key) {
                    rec.continue_on_error = r.continue_on_error;
                }
            }
            records
        };
        let run_id = self.shared.run_id();
        for (record, inst) in records.iter().zip(&instances) {
            if record.state != JobState::Queued {
                continue;
            }
            if matches!(inst.placement, Placement::Local)
                && crate::routing::route(record) == crate::routing::Route::Local
            {
                self.shared.post(
                    record,
                    "pending",
                    format!("Queued locally · baste run {run_id}"),
                );
                ready.push(Ready {
                    key: record.key.clone(),
                    pw: pw_index,
                    job_id: job.id.clone(),
                    strategy: inst.strategy.clone(),
                    needs: filtered_needs(&needs_value, &job.needs),
                });
            }
        }
        self.shared.save();
        Some(Group {
            released: true,
            fail_fast,
            max_parallel,
            running: 0,
        })
    }

    fn set_group_state(&self, file: &str, job_id: &str, state: JobState, reason: String) {
        let changed: Vec<JobRecord> = {
            let mut run = self.shared.run.lock().unwrap();
            let mut out = Vec::new();
            for j in run
                .jobs
                .iter_mut()
                .filter(|j| j.workflow_file == file && j.job_id == job_id && !j.state.is_done())
            {
                j.state = state;
                j.reason = Some(reason.clone());
                j.finished_at = Some(Utc::now());
                if j.pending_expansion && j.name.contains("${{") {
                    j.context = None;
                } else if j.context.is_none() && state != JobState::HandedToGithub {
                    j.context = Some(plan::status_context(&j.workflow, &j.name));
                }
                if state == JobState::HandedToGithub {
                    j.context = None;
                }
                out.push(j.clone());
            }
            out
        };
        for r in &changed {
            self.shared.post_final(r);
        }
        self.shared.save();
    }

    fn fail_group(&self, file: &str, job_id: &str, error: String) {
        let changed: Vec<JobRecord> = {
            let mut run = self.shared.run.lock().unwrap();
            let mut out = Vec::new();
            for j in run
                .jobs
                .iter_mut()
                .filter(|j| j.workflow_file == file && j.job_id == job_id && !j.state.is_done())
            {
                j.state = JobState::Failed;
                j.error = Some(error.clone());
                j.finished_at = Some(Utc::now());
                if j.context.is_none() && !j.name.contains("${{") {
                    j.context = Some(plan::status_context(&j.workflow, &j.name));
                }
                out.push(j.clone());
            }
            out
        };
        for r in &changed {
            self.shared.post_final(r);
        }
        self.shared.save();
    }
}

fn filtered_needs(all: &Value, needs: &[String]) -> Value {
    let mut out = Map::new();
    for n in needs {
        if let Some(v) = all.get(n) {
            out.insert(n.clone(), v.clone());
        }
    }
    Value::Object(out)
}

fn job_continue_on_error(
    pw: &PlannedWorkflow,
    job: &baste_workflow::Job,
    inst: &baste_workflow::JobInstance,
    needs: &Value,
) -> bool {
    let Some(v) = &job.continue_on_error else {
        return false;
    };
    let mut ctx = plan::job_level_contexts(pw, needs.clone());
    ctx.insert(
        "matrix".into(),
        Value::Object(inst.matrix.clone().unwrap_or_default()),
    );
    ctx.insert("strategy".into(), inst.strategy.clone());
    let env = Env {
        contexts: &ctx,
        functions: &baste_expr::NoFunctions,
    };
    match baste_workflow::evaluate_tree(v, &env) {
        Ok(Value::String(s)) => s.trim().eq_ignore_ascii_case("true"),
        Ok(other) => baste_expr::is_truthy(&other),
        Err(_) => false,
    }
}

/// `needs.<id>.result` for all instances of a job.
fn aggregate_result(records: &[&JobRecord]) -> &'static str {
    let result = |r: &JobRecord| match r.state {
        JobState::Passed => "success",
        JobState::Failed if r.continue_on_error => "success",
        JobState::Failed => "failure",
        JobState::Cancelled => "cancelled",
        _ => "skipped",
    };
    let all: Vec<&str> = records.iter().map(|r| result(r)).collect();
    if all.contains(&"failure") {
        "failure"
    } else if all.contains(&"cancelled") {
        "cancelled"
    } else if all.iter().all(|r| *r == "skipped") {
        "skipped"
    } else {
        "success"
    }
}

/// Status functions for job-level `if:`.
struct JobStatusFunctions {
    needs_ok: bool,
    needs_failed: bool,
    cancelled: bool,
}

impl Functions for JobStatusFunctions {
    fn call(&self, name: &str, _args: &[Value]) -> Option<Result<Value, ExprError>> {
        Some(Ok(Value::Bool(match name {
            "success" => self.needs_ok && !self.cancelled,
            "failure" => self.needs_failed,
            "cancelled" => self.cancelled,
            "always" => true,
            _ => return None,
        })))
    }
}

// ----- one job ----------------------------------------------------------------

/// Writes step logs and folds agent events into the job record.
struct JobRecorder<'a> {
    shared: &'a Shared,
    key: String,
    job_dir: PathBuf,
    logs: HashMap<usize, std::fs::File>,
    artifacts: HashMap<String, std::fs::File>,
    finished: Option<(Outcome, Map<String, Value>, Option<String>)>,
}

impl<'a> JobRecorder<'a> {
    fn new(shared: &'a Shared, key: &str, job_dir: PathBuf) -> Self {
        JobRecorder {
            shared,
            key: key.to_string(),
            job_dir,
            logs: HashMap::new(),
            artifacts: HashMap::new(),
            finished: None,
        }
    }

    fn start_step(&mut self, index: usize, name: &str, phase: &str) {
        let log = Store::step_log_name(index);
        let path = self.job_dir.join(&log);
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            self.logs.insert(index, f);
        }
        self.shared.with_job(&self.key, |j| {
            let record = StepRecord {
                index,
                name: name.to_string(),
                phase: phase.to_string(),
                state: StepState::Running,
                continued: false,
                exit_code: None,
                started_at: Utc::now(),
                duration_ms: None,
                log,
            };
            match j.steps.iter_mut().find(|s| s.index == index) {
                Some(s) => {
                    s.name = record.name;
                    s.phase = record.phase;
                }
                None => j.steps.push(record),
            }
        });
        self.shared.save_soon();
    }

    fn line(&mut self, index: usize, line: &str) {
        if !self.logs.contains_key(&index) {
            self.start_step(index, "Output", "main");
        }
        if let Some(f) = self.logs.get_mut(&index) {
            let _ = writeln!(f, "{line}");
        }
    }

    fn host_log(&mut self, line: &str) {
        self.line(0, line);
    }

    fn event(&mut self, e: Event) {
        match e {
            Event::Hello {
                agent_version,
                os,
                arch,
                ..
            } => self.host_log(&format!("Agent {agent_version} ready ({os}/{arch})")),
            Event::StepStarted {
                index, name, phase, ..
            } => {
                let phase = match phase {
                    StepPhase::Setup => "setup",
                    StepPhase::Pre => "pre",
                    StepPhase::Main => "main",
                    StepPhase::Post => "post",
                    StepPhase::Complete => "complete",
                };
                self.start_step(index, &name, phase);
            }
            Event::Log { step, line } => self.line(step, &line),
            Event::Annotation { .. } => {}
            Event::StepFinished {
                index,
                outcome,
                conclusion,
                exit_code,
                duration_ms,
                ..
            } => {
                self.shared.with_job(&self.key, |j| {
                    if let Some(s) = j.steps.iter_mut().find(|s| s.index == index) {
                        s.state = match outcome {
                            Outcome::Success => StepState::Success,
                            Outcome::Failure => StepState::Failure,
                            Outcome::Cancelled => StepState::Cancelled,
                            Outcome::Skipped => StepState::Skipped,
                        };
                        s.continued = outcome == Outcome::Failure && conclusion == Outcome::Success;
                        s.exit_code = exit_code;
                        // Setup includes host-side work (bundle, VM boot).
                        s.duration_ms = Some(if index == 0 {
                            (Utc::now() - s.started_at).num_milliseconds().max(0) as u64
                        } else {
                            duration_ms
                        });
                    }
                });
                self.shared.save_soon();
            }
            Event::ArtifactChunk { name, data } => {
                let dir = self
                    .shared
                    .store
                    .run_dir(&self.shared.run_id())
                    .join("artifacts");
                let _ = std::fs::create_dir_all(&dir);
                let file = self.artifacts.entry(name.clone()).or_insert_with(|| {
                    std::fs::File::create(
                        dir.join(format!("{}.tar.partial", crate::store::slug(&name))),
                    )
                    .expect("creating artifact file")
                });
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                    let _ = file.write_all(&bytes);
                }
            }
            Event::ArtifactEnd { name, .. } => {
                self.artifacts.remove(&name);
                let dir = self
                    .shared
                    .store
                    .run_dir(&self.shared.run_id())
                    .join("artifacts");
                let slug = crate::store::slug(&name);
                let _ = std::fs::rename(
                    dir.join(format!("{slug}.tar.partial")),
                    dir.join(format!("{slug}.tar")),
                );
                let _ = std::fs::write(dir.join(format!("{slug}.name")), &name);
                self.shared.with_job(&self.key, |j| j.artifacts.push(name));
            }
            Event::Summary { markdown, .. } => {
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.job_dir.join("summary.md"))
                {
                    let _ = writeln!(f, "{markdown}");
                }
            }
            Event::JobFinished {
                result,
                outputs,
                error,
                ..
            } => self.finished = Some((result, outputs, error)),
        }
    }
}

/// Artifacts uploaded so far in this run, by name.
fn run_artifacts(shared: &Shared) -> Vec<(String, PathBuf)> {
    let dir = shared.store.run_dir(&shared.run_id()).join("artifacts");
    let mut out = Vec::new();
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "tar") {
            let name = std::fs::read_to_string(p.with_extension("name")).unwrap_or_else(|_| {
                p.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
            out.push((name, p));
        }
    }
    out
}

fn run_job_thread(shared: &Shared, backend: &dyn Backend, task: Ready, cancel: Arc<AtomicBool>) {
    let run_id = shared.run_id();
    let job_dir = shared.store.job_dir(&run_id, &task.key);
    let _ = std::fs::create_dir_all(&job_dir);
    let mut rec = JobRecorder::new(shared, &task.key, job_dir.clone());

    let stop = || cancel.load(Ordering::SeqCst) || CANCEL_REQUESTED.load(Ordering::SeqCst);
    let Some(slot) = backend::acquire_slot(shared.config.max_parallel_jobs, &stop) else {
        shared.with_job(&task.key, |j| {
            j.state = JobState::Cancelled;
            j.finished_at = Some(Utc::now());
        });
        return;
    };

    shared.with_job(&task.key, |j| {
        j.state = JobState::Running;
        j.started_at = Some(Utc::now());
    });
    let record = shared.run.lock().unwrap().job(&task.key).cloned();
    if let Some(record) = record {
        shared.post(
            &record,
            "pending",
            format!("Running locally · baste logs {run_id}"),
        );
    }
    rec.start_step(0, "Set up job", "setup");
    shared.save();

    let timed_out = Arc::new(AtomicBool::new(false));
    let result = prepare_and_run(
        shared, backend, &task, &job_dir, slot.index, &cancel, &timed_out, &mut rec,
    );
    drop(slot);
    let _ = std::fs::remove_dir_all(job_dir.join("bundle"));

    let timed_out = timed_out.load(Ordering::SeqCst);
    let cancelled =
        !timed_out && (cancel.load(Ordering::SeqCst) || CANCEL_REQUESTED.load(Ordering::SeqCst));
    shared.with_job(&task.key, |j| {
        j.finished_at = Some(Utc::now());
        match (&result, rec.finished.take()) {
            (Ok(()), Some((outcome, outputs, error))) => {
                j.outputs = outputs;
                j.error = error;
                j.state = match outcome {
                    Outcome::Success => JobState::Passed,
                    Outcome::Cancelled => JobState::Cancelled,
                    _ if cancelled => JobState::Cancelled,
                    _ => JobState::Failed,
                };
            }
            (Ok(()), None) => {
                j.state = if cancelled {
                    JobState::Cancelled
                } else {
                    JobState::Failed
                };
                j.error.get_or_insert_with(|| {
                    if timed_out {
                        "The job ran past its timeout and its VM was stopped.".into()
                    } else {
                        "The runner stopped without reporting a result.".into()
                    }
                });
            }
            (Err(e), _) => {
                j.state = if cancelled {
                    JobState::Cancelled
                } else {
                    JobState::Failed
                };
                j.error = Some(format!("{e:#}"));
            }
        }
        // Mark steps that never finished.
        for s in j.steps.iter_mut().filter(|s| s.state == StepState::Running) {
            s.state = if cancelled {
                StepState::Cancelled
            } else {
                StepState::Failure
            };
        }
    });
    if let Err(e) = &result {
        rec.host_log(&format!("##[error]{e:#}"));
        shared.with_job(&task.key, |j| {
            if let Some(s) = j.steps.iter_mut().find(|s| s.index == 0) {
                if s.state == StepState::Running || s.duration_ms.is_none() {
                    s.state = StepState::Failure;
                }
            }
        });
    }
    shared.save();
}

#[allow(clippy::too_many_arguments)]
fn prepare_and_run(
    shared: &Shared,
    backend: &dyn Backend,
    task: &Ready,
    job_dir: &std::path::Path,
    slot: usize,
    cancel: &Arc<AtomicBool>,
    timed_out: &Arc<AtomicBool>,
    rec: &mut JobRecorder,
) -> Result<()> {
    let pw = &shared.planned[task.pw];
    let job = pw
        .workflow
        .job(&task.job_id)
        .ok_or_else(|| anyhow!("job {} disappeared", task.job_id))?;
    let record = shared
        .run
        .lock()
        .unwrap()
        .job(&task.key)
        .cloned()
        .ok_or_else(|| anyhow!("no record for {}", task.key))?;
    let (repository, server_url) = {
        let run = shared.run.lock().unwrap();
        (run.repo.full_name(), run.repo.server_url())
    };

    let mut log = |l: &str| rec.host_log(l);
    let actions = bundle::collect_actions(
        job,
        &shared.git,
        &pw.checkout_sha,
        &shared.actions,
        &mut log,
    )?;
    let mut packs = Vec::new();
    for depth in bundle::checkout_depths(job) {
        let (path, shallow) = shared.packs.get(&shared.git, &pw.checkout_sha, depth)?;
        packs.push((depth, path, shallow));
    }
    let names = bundle::referenced_secrets(&pw.workflow.env, job);
    let token = shared
        .api
        .as_ref()
        .map(|a| a.token().to_string())
        .unwrap_or_default();
    let secrets = bundle::resolve_secrets(&names, shared.secrets.as_ref(), &token)?;
    if !secrets.missing.is_empty() {
        rec.host_log(&format!(
            "Missing local secrets: {}",
            secrets.missing.join(", ")
        ));
        if let Some(e) = &shared.secrets_error {
            rec.host_log(&format!("Secrets unavailable: {e}"));
        }
    }
    let bundle_dir = job_dir.join("bundle");
    let spec = bundle::write_bundle(
        &bundle_dir,
        BundleInput {
            run_id: &shared.run_id(),
            record: &record,
            planned: pw,
            job,
            strategy: task.strategy.clone(),
            needs: task.needs.clone(),
            runner: backend.runner(job_dir),
            secrets,
            actions,
            packs,
            artifacts: run_artifacts(shared),
            repository,
            server_url,
        },
    )?;
    let launch = JobLaunch {
        run_id: &shared.run_id(),
        bundle: &bundle_dir,
        job_dir,
        slot,
        cancel: cancel.clone(),
    };
    // The agent enforces the job timeout inside the VM; this watchdog stops a
    // VM that stopped responding altogether, a little after that.
    let limit = Duration::from_secs_f64(spec.timeout_minutes.unwrap_or(360.0).max(0.0) * 60.0)
        + Duration::from_secs(300);
    let done = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let (done, cancel, timed_out) = (done.clone(), cancel.clone(), timed_out.clone());
        let started = Instant::now();
        std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                if started.elapsed() > limit {
                    timed_out.store(true, Ordering::SeqCst);
                    cancel.store(true, Ordering::SeqCst);
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        })
    };
    // Split the recorder between the event and log callbacks.
    let rec_cell = std::cell::RefCell::new(rec);
    let result = backend.run(&launch, &mut |e| rec_cell.borrow_mut().event(e), &mut |l| {
        rec_cell.borrow_mut().host_log(l)
    });
    done.store(true, Ordering::SeqCst);
    let _ = watchdog.join();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(state: JobState) -> JobRecord {
        JobRecord {
            key: "k".into(),
            workflow: "CI".into(),
            workflow_file: "ci.yml".into(),
            event: "push".into(),
            job_id: "a".into(),
            name: "a".into(),
            matrix: None,
            context: None,
            state,
            reason: None,
            needs: vec![],
            started_at: None,
            finished_at: None,
            steps: vec![],
            outputs: Map::new(),
            artifacts: vec![],
            error: None,
            continue_on_error: false,
            pending_expansion: false,
        }
    }

    #[test]
    fn aggregates_needs_results() {
        let p = rec(JobState::Passed);
        let f = rec(JobState::Failed);
        let s = rec(JobState::Skipped);
        assert_eq!(aggregate_result(&[&p, &p]), "success");
        assert_eq!(aggregate_result(&[&p, &f]), "failure");
        assert_eq!(aggregate_result(&[&s]), "skipped");
        let mut c = rec(JobState::Failed);
        c.continue_on_error = true;
        assert_eq!(aggregate_result(&[&c]), "success");
    }
}
