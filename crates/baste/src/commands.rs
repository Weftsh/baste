//! User-facing commands that read or start runs.

use crate::git::Git;
use crate::store::{JobRecord, JobState, Run, RunState, StepRecord, StepState, Store, Trigger};
use crate::ui::{self, bold, cyan, dim, duration, green, red, yellow};
use anyhow::{bail, Context, Result};
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::time::Duration;

pub fn run_state_label(r: &Run) -> String {
    let dead = !r.state.is_done() && r.worker_pid.is_some() && !r.worker_alive();
    match r.state {
        _ if dead => red("✗ stopped"),
        RunState::Queued => yellow("• queued"),
        RunState::Running => yellow("● running"),
        RunState::Passed => green("✓ passed"),
        RunState::Failed => red("✗ failed"),
        RunState::Cancelled => dim("⊘ cancelled"),
        RunState::Error => red("✗ error"),
    }
}

fn job_mark(j: &JobRecord) -> String {
    match j.state {
        JobState::Queued => yellow("•"),
        JobState::Running => yellow("●"),
        JobState::Passed => green("✓"),
        JobState::Failed if j.continue_on_error => yellow("✗"),
        JobState::Failed => red("✗"),
        JobState::Skipped => dim("-"),
        JobState::Cancelled => dim("⊘"),
        JobState::HandedToGithub => cyan("→"),
        JobState::NotRun => red("-"),
    }
}

fn job_detail(j: &JobRecord) -> String {
    match j.state {
        JobState::HandedToGithub => {
            format!("handed to GitHub: {}", j.reason.clone().unwrap_or_default())
        }
        JobState::Failed => match (j.failed_step(), &j.error) {
            (Some(s), _) => format!(
                "failed at '{}'{}",
                s.name,
                s.exit_code
                    .map(|c| format!(" (exit code {c})"))
                    .unwrap_or_default()
            ),
            (None, Some(e)) => format!("failed: {}", e.lines().next().unwrap_or_default()),
            _ => "failed".into(),
        },
        JobState::Running => match j.steps.iter().rev().find(|s| s.state == StepState::Running) {
            Some(s) => format!("running '{}'", s.name),
            None => "running".into(),
        },
        other => {
            let mut s = other.label().to_string();
            if let Some(r) = &j.reason {
                s.push_str(&format!(": {r}"));
            }
            s
        }
    }
}

fn print_run(r: &Run, verbose: bool) {
    let dur = r.duration_ms().map(duration).unwrap_or_default();
    let trigger = match &r.trigger {
        Trigger::Push => "push".to_string(),
        Trigger::Manual => "baste run".to_string(),
        Trigger::Rerun { of } => format!("rerun of {of}"),
    };
    println!(
        "{}  {}  {}  {}  {}  {}  {}",
        bold(&r.id),
        ui::pad(&run_state_label(r), 11),
        r.short_sha(),
        r.branch(),
        dim(&ui::ago(r.created_at)),
        dur,
        dim(&trigger)
    );
    let width = r
        .jobs
        .iter()
        .map(|j| j.title().chars().count())
        .max()
        .unwrap_or(0)
        .min(48);
    for j in &r.jobs {
        let d = j.duration_ms().map(duration).unwrap_or_default();
        println!(
            "   {} {}  {:>7}  {}",
            job_mark(j),
            ui::pad(&j.title(), width),
            d,
            dim(&job_detail(j))
        );
        if verbose {
            for s in &j.steps {
                let mark = match s.state {
                    StepState::Running => yellow("●"),
                    StepState::Success => green("✓"),
                    StepState::Failure if s.continued => yellow("✗"),
                    StepState::Failure => red("✗"),
                    StepState::Skipped => dim("-"),
                    StepState::Cancelled => dim("⊘"),
                };
                let d = s
                    .duration_ms
                    .map(|d| duration(d as i64))
                    .unwrap_or_default();
                println!("       {mark} {}  {}", ui::pad(&s.name, 40), dim(&d));
            }
        }
    }
    if let Some(e) = &r.error {
        println!("   {} {e}", red("error:"));
    }
    if verbose {
        for n in &r.notes {
            println!("   {} {n}", dim("note:"));
        }
        for line in crate::insights::summary_lines(r) {
            println!("   {line}");
        }
        let p = &r.provenance;
        if !p.backend.is_empty() {
            println!(
                "   {}",
                dim(&format!(
                    "provenance: {} executor on {}, {} backend{}, routing policy {}",
                    p.executor,
                    p.host,
                    p.backend,
                    p.image
                        .as_deref()
                        .map(|i| format!(", image {i}"))
                        .unwrap_or_default(),
                    p.policy
                ))
            );
        }
    }
}

pub fn status(
    store: &Store,
    run: Option<&str>,
    commit: Option<&str>,
    limit: usize,
    json: bool,
) -> Result<()> {
    let runs: Vec<Run> = if let Some(c) = commit {
        let runs: Vec<Run> = store
            .list()?
            .into_iter()
            .filter(|r| r.sha.starts_with(c))
            .collect();
        if runs.is_empty() {
            bail!("no local runs for commit {c}");
        }
        runs
    } else if let Some(q) = run {
        vec![store.resolve(q)?]
    } else {
        store.list()?.into_iter().take(limit).collect()
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&runs)?);
        return Ok(());
    }
    if runs.is_empty() {
        println!("No runs yet. Push a commit, or start one with `baste run`.");
        return Ok(());
    }
    let verbose = run.is_some() || commit.is_some();
    for (i, r) in runs.iter().enumerate() {
        if i > 0 {
            println!();
        }
        print_run(r, verbose);
    }
    Ok(())
}

/// Pick the jobs `logs` shows.
fn select_jobs<'a>(run: &'a Run, query: Option<&str>) -> Result<Vec<&'a JobRecord>> {
    let local: Vec<&JobRecord> = run
        .jobs
        .iter()
        .filter(|j| j.state != JobState::HandedToGithub)
        .collect();
    let Some(q) = query else {
        return Ok(local);
    };
    let ql = q.to_ascii_lowercase();
    let exact: Vec<&JobRecord> = local
        .iter()
        .copied()
        .filter(|j| {
            j.key == q
                || j.name.eq_ignore_ascii_case(q)
                || j.job_id.eq_ignore_ascii_case(q)
                || j.title().eq_ignore_ascii_case(q)
                || j.context.as_deref() == Some(q)
        })
        .collect();
    if !exact.is_empty() {
        return Ok(exact);
    }
    let partial: Vec<&JobRecord> = local
        .iter()
        .copied()
        .filter(|j| j.key.starts_with(&ql) || j.title().to_ascii_lowercase().contains(&ql))
        .collect();
    if partial.is_empty() {
        let names: Vec<String> = local.iter().map(|j| j.title()).collect();
        bail!("no job '{q}' in run {}. Jobs: {}", run.id, names.join(", "));
    }
    Ok(partial)
}

fn render_line(line: &str) -> String {
    if let Some(rest) = line.strip_prefix("##[error]") {
        red(&format!("Error: {rest}"))
    } else if let Some(rest) = line.strip_prefix("##[warning]") {
        yellow(&format!("Warning: {rest}"))
    } else if let Some(rest) = line.strip_prefix("##[notice]") {
        cyan(&format!("Notice: {rest}"))
    } else if let Some(rest) = line.strip_prefix("##[group]") {
        bold(&format!("▸ {rest}"))
    } else if line == "##[endgroup]" {
        String::new()
    } else if let Some(rest) = line.strip_prefix("##[command]") {
        dim(&format!("$ {rest}"))
    } else if let Some(rest) = line.strip_prefix("##[debug]") {
        dim(&format!("debug: {rest}"))
    } else {
        line.to_string()
    }
}

fn step_header(s: &StepRecord) -> String {
    format!("{} {}", cyan("▶"), bold(&s.name))
}

fn step_footer(s: &StepRecord) -> Option<String> {
    let d = s
        .duration_ms
        .map(|d| duration(d as i64))
        .unwrap_or_default();
    match s.state {
        StepState::Failure => Some(red(&format!(
            "✗ {} failed{} after {d}{}",
            s.name,
            s.exit_code
                .map(|c| format!(" with exit code {c}"))
                .unwrap_or_default(),
            if s.continued {
                " (continue-on-error)"
            } else {
                ""
            }
        ))),
        StepState::Skipped => Some(dim("  skipped")),
        StepState::Cancelled => Some(dim("  cancelled")),
        _ => None,
    }
}

/// Print a job's logs once, from the files.
fn print_job(store: &Store, run: &Run, j: &JobRecord, failed_only: bool) -> Result<()> {
    let dur = j.duration_ms().map(duration).unwrap_or_default();
    println!(
        "{} {} {}  {}",
        job_mark(j),
        bold(&j.title()),
        dim(&dur),
        dim(&job_detail(j))
    );
    let dir = store.job_dir(&run.id, &j.key);
    for s in &j.steps {
        if failed_only && s.state != StepState::Failure && s.index != 0 {
            continue;
        }
        println!("{}", step_header(s));
        if let Ok(text) = std::fs::read_to_string(dir.join(&s.log)) {
            for line in text.lines() {
                let r = render_line(line);
                if !r.is_empty() || !line.starts_with("##[") {
                    println!("  {r}");
                }
            }
        }
        if let Some(f) = step_footer(s) {
            println!("{f}");
        }
    }
    if let Some(e) = &j.error {
        if j.failed_step().is_none() {
            println!("{}", red(&format!("✗ {e}")));
        }
    }
    if let Ok(summary) = std::fs::read_to_string(dir.join("summary.md")) {
        println!("{}", bold("Job summary"));
        for line in summary.lines() {
            println!("  {line}");
        }
    }
    println!();
    Ok(())
}

pub fn logs(
    store: &Store,
    run_query: &str,
    job: Option<&str>,
    follow: Option<bool>,
    failed_only: bool,
) -> Result<bool> {
    let run = store.resolve(run_query)?;
    let follow = follow.unwrap_or(!run.state.is_done());
    if follow && !run.state.is_done() {
        return follow_run(store, &run.id, job);
    }
    let jobs = select_jobs(&run, job)?;
    if jobs.is_empty() {
        println!("Run {} has no local jobs.", run.id);
    }
    let failed = jobs
        .iter()
        .any(|j| matches!(j.state, JobState::Failed | JobState::NotRun));
    for j in jobs
        .iter()
        .filter(|j| !failed_only || matches!(j.state, JobState::Failed))
    {
        print_job(store, &run, j, failed_only)?;
    }
    print_footer(&run);
    Ok(!failed && run.state != RunState::Failed)
}

fn print_footer(run: &Run) {
    let handed: Vec<&JobRecord> = run
        .jobs
        .iter()
        .filter(|j| j.state == JobState::HandedToGithub)
        .collect();
    for h in &handed {
        println!(
            "{} {} {}",
            cyan("→"),
            h.title(),
            dim(&format!(
                "handed to GitHub: {}",
                h.reason.clone().unwrap_or_default()
            ))
        );
    }
    for n in &run.notes {
        println!("{} {n}", dim("note:"));
    }
    println!("{} {}  {}", bold("Run"), run.id, run_state_label(run));
    for line in crate::insights::summary_lines(run) {
        println!("  {line}");
    }
}

/// Stream logs of a run in progress until it finishes. Returns whether it passed.
pub fn follow_run(store: &Store, run_id: &str, job: Option<&str>) -> Result<bool> {
    use std::collections::HashMap;
    let mut offsets: HashMap<(String, usize), u64> = HashMap::new();
    let mut announced_jobs: Vec<String> = Vec::new();
    let mut headers: Vec<(String, usize)> = Vec::new();
    let mut footers: Vec<(String, usize)> = Vec::new();
    let mut done_jobs: Vec<String> = Vec::new();
    let mut last_planning_note = false;
    let started = std::time::Instant::now();
    loop {
        // A reader can race a save on some filesystems; retry briefly.
        let mut attempt = 0;
        let run = loop {
            match store.load(run_id) {
                Ok(r) => break r,
                Err(_) if attempt < 20 => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e),
            }
        };
        let jobs = if run.jobs.is_empty() {
            vec![]
        } else {
            select_jobs(&run, job).unwrap_or_default()
        };
        let multi = jobs.iter().filter(|j| j.state != JobState::Queued).count() > 1;
        if run.jobs.is_empty() && !run.state.is_done() && !last_planning_note {
            println!("{}", dim(&format!("Run {} is starting…", run.id)));
            last_planning_note = true;
        }
        for j in &jobs {
            if j.state == JobState::Queued && j.steps.is_empty() {
                continue;
            }
            if !announced_jobs.contains(&j.key) {
                println!("{} {}", job_mark(j), bold(&j.title()));
                announced_jobs.push(j.key.clone());
            }
            let prefix = if multi {
                dim(&format!("[{}] ", j.name))
            } else {
                String::new()
            };
            let dir = store.job_dir(&run.id, &j.key);
            for s in &j.steps {
                let k = (j.key.clone(), s.index);
                if !headers.contains(&k) {
                    println!("{prefix}{}", step_header(s));
                    headers.push(k.clone());
                }
                let off = offsets.entry(k.clone()).or_insert(0);
                if let Ok(mut f) = std::fs::File::open(dir.join(&s.log)) {
                    if f.seek(SeekFrom::Start(*off)).is_ok() {
                        let mut buf = String::new();
                        let _ = f.read_to_string(&mut buf);
                        // Only print complete lines.
                        let complete = buf.rfind('\n').map(|i| i + 1).unwrap_or(0);
                        for line in std::io::BufReader::new(&buf.as_bytes()[..complete])
                            .lines()
                            .map_while(Result::ok)
                        {
                            let r = render_line(&line);
                            if !r.is_empty() || !line.starts_with("##[") {
                                println!("{prefix}  {r}");
                            }
                        }
                        *off += complete as u64;
                    }
                }
                if s.state != StepState::Running && !footers.contains(&k) && s.duration_ms.is_some()
                {
                    if let Some(f) = step_footer(s) {
                        println!("{prefix}{f}");
                    }
                    footers.push(k);
                }
            }
            if j.state.is_done() && !done_jobs.contains(&j.key) {
                let d = j.duration_ms().map(duration).unwrap_or_default();
                println!(
                    "{} {} {}  {}",
                    job_mark(j),
                    bold(&j.title()),
                    dim(&d),
                    dim(&job_detail(j))
                );
                if let Some(e) = &j.error {
                    if j.failed_step().is_none() {
                        println!("{}", red(&format!("  {e}")));
                    }
                }
                done_jobs.push(j.key.clone());
            }
        }
        let _ = std::io::stdout().flush();
        let dead = run.worker_pid.is_some()
            && !run.worker_alive()
            && started.elapsed() > Duration::from_secs(2);
        if run.state.is_done() || (dead && !run.state.is_done()) {
            if dead && !run.state.is_done() {
                println!(
                    "{}",
                    red("The worker stopped unexpectedly; see worker.log in the run directory.")
                );
            }
            if let Some(e) = &run.error {
                println!("{} {e}", red("error:"));
            }
            println!();
            print_footer(&run);
            return Ok(run.state == RunState::Passed);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

pub struct RunArgs {
    pub git_ref: Option<String>,
    pub workflows: Vec<String>,
    pub jobs: Vec<String>,
    pub event: Option<String>,
    pub no_status: bool,
    pub backend: Option<String>,
    pub detach: bool,
}

/// `baste run`: run workflows for a commit now, without pushing.
pub fn run_now(git: &Git, store: &Store, args: RunArgs) -> Result<bool> {
    let rev = args.git_ref.clone().unwrap_or_else(|| "HEAD".into());
    let sha = git.rev_parse(&rev)?;
    let branch = if rev == "HEAD" {
        git.current_branch()
    } else if git
        .try_run(&[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{rev}"),
        ])
        .is_some()
    {
        Some(rev.clone())
    } else {
        None
    };
    let git_ref = match (
        &branch,
        git.try_run(&[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/tags/{rev}"),
        ]),
    ) {
        (Some(b), _) => format!("refs/heads/{b}"),
        (None, Some(_)) => format!("refs/tags/{rev}"),
        (None, None) => "refs/heads/HEAD".to_string(),
    };
    let remote = git.remote_for(branch.as_deref());
    let repo = git.repo(&remote)?;
    if let Some(e) = &args.event {
        if e != "push" && e != "pull_request" {
            bail!("--event must be push or pull_request");
        }
    }
    let mut run = Run::new(store.new_id(), repo, sha, git_ref.clone(), Trigger::Manual);
    run.remote = Some(remote.clone());
    run.post_statuses = !args.no_status;
    run.only_workflows = args.workflows;
    run.only_jobs = args.jobs;
    run.event_override = args.event;
    run.backend_override = args.backend;
    if let Some(b) = &branch {
        run.before = git
            .try_run(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/remotes/{remote}/{b}"),
            ])
            .filter(|s| s != &run.sha);
    }
    store.save(&run)?;
    crate::hook::spawn_worker(git, store, &run.id)?;
    println!(
        "Started run {} for {} ({})",
        bold(&run.id),
        run.short_sha(),
        run.branch()
    );
    if args.detach {
        println!("Follow it with `baste logs {}`.", run.id);
        return Ok(true);
    }
    println!("{}", dim("Ctrl-C stops following; the run keeps going."));
    follow_run(store, &run.id, None)
}

pub fn rerun(git: &Git, store: &Store, query: &str, detach: bool) -> Result<bool> {
    let old = store.resolve(query)?;
    if !old.state.is_done() && old.worker_alive() {
        bail!(
            "run {} is still running; cancel it first with `baste cancel {}`",
            old.id,
            old.id
        );
    }
    let mut run = Run::new(
        store.new_id(),
        old.repo.clone(),
        old.sha.clone(),
        old.git_ref.clone(),
        Trigger::Rerun { of: old.id.clone() },
    );
    run.before = old.before.clone();
    run.remote = old.remote.clone();
    run.post_statuses = old.post_statuses;
    run.only_workflows = old.only_workflows.clone();
    run.only_jobs = old.only_jobs.clone();
    run.event_override = old.event_override.clone();
    run.backend_override = old.backend_override.clone();
    run.run_attempt = old.run_attempt + 1;
    store.save(&run)?;
    crate::hook::spawn_worker(git, store, &run.id)?;
    println!(
        "Rerunning {} on {} as run {}",
        old.id,
        old.short_sha(),
        bold(&run.id)
    );
    if detach {
        return Ok(true);
    }
    follow_run(store, &run.id, None)
}

pub fn cancel(store: &Store, query: &str) -> Result<()> {
    let run = store.resolve(query)?;
    if run.state.is_done() {
        println!(
            "Run {} already finished ({}).",
            run.id,
            run_state_label(&run)
        );
        return Ok(());
    }
    match run.worker_pid {
        Some(pid) if run.worker_alive() => {
            // SAFETY: plain syscall.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            println!("Cancelling run {}…", run.id);
        }
        _ => {
            let mut run = run;
            run.state = RunState::Cancelled;
            run.finished_at = Some(chrono::Utc::now());
            store.save(&run)?;
            println!("Run {} had no live worker; marked cancelled.", run.id);
        }
    }
    Ok(())
}

pub fn secrets_set(secrets: &crate::secrets::Secrets, name: &str, global: bool) -> Result<()> {
    use std::io::IsTerminal;
    let value = if std::io::stdin().is_terminal() {
        eprint!("Value for {name}: ");
        let _ = std::io::stderr().flush();
        let v = read_hidden_line()?;
        eprintln!();
        v
    } else {
        let mut v = String::new();
        std::io::stdin().read_to_string(&mut v)?;
        v.strip_suffix('\n')
            .map(|s| s.strip_suffix('\r').unwrap_or(s).to_string())
            .unwrap_or(v)
    };
    if value.is_empty() {
        bail!("empty value; nothing stored");
    }
    secrets.set(name, &value, global)?;
    println!(
        "Stored {name} {} in the {}.",
        if global {
            "for all repositories"
        } else {
            "for this repository"
        },
        secrets.describe()
    );
    Ok(())
}

fn read_hidden_line() -> Result<String> {
    // SAFETY: termios calls on stdin.
    unsafe {
        let mut term: libc::termios = std::mem::zeroed();
        let fd = libc::STDIN_FILENO;
        let have = libc::tcgetattr(fd, &mut term) == 0;
        let saved = term;
        if have {
            term.c_lflag &= !libc::ECHO;
            libc::tcsetattr(fd, libc::TCSANOW, &term);
        }
        let mut line = String::new();
        let r = std::io::stdin().read_line(&mut line);
        if have {
            libc::tcsetattr(fd, libc::TCSANOW, &saved);
        }
        r.context("reading the value")?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }
}

pub fn gate_snippet() {
    println!(
        r#"# Opt-in merge gate: add this job to a workflow and make its other jobs
# depend on it. If Baste already reported success for this commit, the
# GitHub-hosted jobs are skipped and the workflow passes in seconds.
# Only gate jobs that Baste runs locally (ubuntu-*), not Windows/macOS ones.

permissions:
  contents: read
  statuses: read

jobs:
  baste-gate:
    runs-on: ubuntu-latest
    outputs:
      skip: ${{{{ steps.gate.outputs.skip }}}}
    steps:
      - id: gate
        uses: weftsh/baste/gate@v1

  build:
    needs: baste-gate
    if: needs.baste-gate.outputs.skip != 'true'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v5
      - run: make test"#
    );
}
