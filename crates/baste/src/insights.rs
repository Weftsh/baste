//! Insights: time per step, and local duration against the last GitHub
//! Actions run of the same workflow ("time saved").

use crate::github::GitHub;
use crate::store::{JobState, Run, RunInsights, RunState, StepState, WorkflowInsight};
use crate::ui::{bold, dim, duration, green, yellow};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Compute the end-of-run comparison with GitHub. Network errors just leave
/// the GitHub side empty.
pub fn run_insights(run: &Run, api: Option<&GitHub>) -> RunInsights {
    let finished = run.finished_at.unwrap_or_else(Utc::now);
    let mut out = RunInsights {
        local_total_ms: (finished - run.created_at).num_milliseconds(),
        ..Default::default()
    };
    let mut by_workflow: BTreeMap<String, Vec<&crate::store::JobRecord>> = BTreeMap::new();
    for j in run.jobs.iter().filter(|j| j.started_at.is_some()) {
        by_workflow
            .entry(j.workflow_file.clone())
            .or_default()
            .push(j);
    }
    let mut saved_total: Option<i64> = None;
    let mut minutes_total: Option<u64> = None;
    for (file, jobs) in by_workflow {
        let start = jobs
            .iter()
            .filter_map(|j| j.started_at)
            .min()
            .unwrap_or(run.created_at);
        let end = jobs
            .iter()
            .filter_map(|j| j.finished_at)
            .max()
            .unwrap_or(finished);
        let mut w = WorkflowInsight {
            workflow: jobs[0].workflow.clone(),
            workflow_file: file.clone(),
            local_ms: (end - start).num_milliseconds(),
            ..Default::default()
        };
        // Local minutes as GitHub would bill them: each job rounded up.
        let local_minutes: u64 = jobs
            .iter()
            .filter_map(|j| j.duration_ms())
            .map(|ms| (ms.max(0) as u64).div_ceil(60_000))
            .sum();
        if let Some(api) = api {
            if let Ok(runs) = api.workflow_runs(&file) {
                // Prefer a successful run on the same branch, then any successful run.
                let branch = run.branch();
                let ok = |r: &&crate::github::WorkflowRun| {
                    r.head_sha != run.sha && r.conclusion.as_deref() == Some("success")
                };
                let pick = runs
                    .iter()
                    .filter(ok)
                    .find(|r| r.head_branch.as_deref() == Some(branch.as_str()))
                    .or_else(|| runs.iter().find(ok));
                if let Some(gh) = pick {
                    w.github_run_id = Some(gh.id);
                    let created = parse_time(&gh.created_at);
                    let started = gh.run_started_at.as_deref().and_then(parse_time);
                    let updated = parse_time(&gh.updated_at);
                    if let (Some(c), Some(u)) = (created, updated) {
                        w.github_total_ms = Some((u - c).num_milliseconds());
                    }
                    if let (Some(s), Some(u)) = (started, updated) {
                        w.github_run_ms = Some((u - s).num_milliseconds());
                    }
                    let billed: Option<u64> = api.run_jobs(gh.id).ok().map(|gh_jobs| {
                        gh_jobs
                            .iter()
                            .filter(|g| jobs.iter().any(|j| j.name == g.name))
                            .filter_map(|g| {
                                let s = parse_time(g.started_at.as_deref()?)?;
                                let e = parse_time(g.completed_at.as_deref()?)?;
                                Some(((e - s).num_milliseconds().max(0) as u64).div_ceil(60_000))
                            })
                            .sum()
                    });
                    w.minutes_saved = billed.filter(|m| *m > 0).or(Some(local_minutes));
                }
            }
        }
        if let Some(gh_total) = w.github_total_ms {
            // Local time counts from the push, like GitHub's counts from its trigger.
            let local = (end - run.created_at).num_milliseconds();
            *saved_total.get_or_insert(0) += gh_total - local;
        }
        if let Some(m) = w.minutes_saved {
            *minutes_total.get_or_insert(0) += m;
        }
        out.workflows.push(w);
    }
    out.time_saved_ms = saved_total;
    out.minutes_saved = minutes_total;
    out
}

/// Lines for the end-of-run summary shown by `status` and `logs`.
pub fn summary_lines(run: &Run) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(ins) = &run.insights else {
        return lines;
    };
    for w in &ins.workflows {
        let gh = match w.github_total_ms {
            Some(t) => format!("GitHub took {} last time", duration(t)),
            None => "no finished GitHub run to compare".into(),
        };
        lines.push(format!(
            "{}: local {} · {gh}",
            w.workflow,
            duration(w.local_ms)
        ));
    }
    match ins.time_saved_ms {
        Some(ms) if ms > 0 => lines.push(green(&format!(
            "Saved about {} compared with waiting on GitHub",
            duration(ms)
        ))),
        Some(ms) => lines.push(yellow(&format!(
            "{} slower than GitHub's last run",
            duration(-ms)
        ))),
        None => {}
    }
    if let Some(m) = ins.minutes_saved.filter(|m| *m > 0) {
        lines.push(format!(
            "{m} Actions minute{} not spent",
            if m == 1 { "" } else { "s" }
        ));
    }
    lines
}

/// `baste insights`: aggregate over recent runs.
pub fn print_insights(runs: &[Run]) {
    let finished: Vec<&Run> = runs.iter().filter(|r| r.state.is_done()).collect();
    if finished.is_empty() {
        println!("No finished runs yet.");
        return;
    }
    let passed = finished
        .iter()
        .filter(|r| r.state == RunState::Passed)
        .count();
    println!(
        "{} over the last {} runs: {} passed, {} failed",
        bold("Runs"),
        finished.len(),
        passed,
        finished.len() - passed
    );
    let saved: i64 = finished
        .iter()
        .filter_map(|r| r.insights.as_ref()?.time_saved_ms)
        .filter(|ms| *ms > 0)
        .sum();
    let minutes: u64 = finished
        .iter()
        .filter_map(|r| r.insights.as_ref()?.minutes_saved)
        .sum();
    if saved > 0 {
        println!(
            "{} {} of waiting on GitHub",
            bold("Time saved"),
            green(&duration(saved))
        );
    }
    if minutes > 0 {
        println!("{} {minutes}", bold("Actions minutes not spent"));
    }

    // Local vs GitHub per workflow.
    let mut per_wf: BTreeMap<String, (Vec<i64>, Vec<i64>)> = BTreeMap::new();
    for r in &finished {
        for w in r.insights.iter().flat_map(|i| &i.workflows) {
            let e = per_wf.entry(w.workflow.clone()).or_default();
            e.0.push(w.local_ms);
            if let Some(g) = w.github_run_ms {
                e.1.push(g);
            }
        }
    }
    if !per_wf.is_empty() {
        println!("\n{}", bold("Workflow duration (average)"));
        for (wf, (local, gh)) in per_wf {
            let avg = |v: &[i64]| v.iter().sum::<i64>() / v.len().max(1) as i64;
            let gh = if gh.is_empty() {
                dim("no GitHub data")
            } else {
                format!("GitHub {}", duration(avg(&gh)))
            };
            println!("  {wf}: local {} · {gh}", duration(avg(&local)));
        }
    }

    // Slowest steps.
    let mut steps: BTreeMap<(String, String), Vec<u64>> = BTreeMap::new();
    for r in &finished {
        for j in &r.jobs {
            for s in &j.steps {
                if let Some(d) = s.duration_ms {
                    if s.state != StepState::Skipped {
                        steps
                            .entry((j.title(), s.name.clone()))
                            .or_default()
                            .push(d);
                    }
                }
            }
        }
    }
    let mut ranked: Vec<((String, String), u64, usize)> = steps
        .into_iter()
        .map(|(k, v)| {
            let avg = v.iter().sum::<u64>() / v.len() as u64;
            (k, avg, v.len())
        })
        .collect();
    ranked.sort_by_key(|r| std::cmp::Reverse(r.1));
    if !ranked.is_empty() {
        println!("\n{}", bold("Slowest steps (average)"));
        for ((job, step), avg, n) in ranked.iter().take(10) {
            println!(
                "  {:>8}  {job} › {step} {}",
                duration(*avg as i64),
                dim(&format!("({n} runs)"))
            );
        }
    }

    // Flaky jobs: passed and failed on the same commit.
    let mut outcomes: BTreeMap<(String, String), (bool, bool)> = BTreeMap::new();
    for r in &finished {
        for j in &r.jobs {
            let e = outcomes.entry((r.sha.clone(), j.title())).or_default();
            match j.state {
                JobState::Passed => e.0 = true,
                JobState::Failed => e.1 = true,
                _ => {}
            }
        }
    }
    let flaky: Vec<_> = outcomes
        .iter()
        .filter(|(_, (p, f))| *p && *f)
        .map(|((sha, job), _)| {
            format!(
                "  {job} {}",
                dim(&format!(
                    "(passed and failed on {})",
                    &sha[..7.min(sha.len())]
                ))
            )
        })
        .collect();
    if !flaky.is_empty() {
        println!("\n{}", bold("Flaky jobs"));
        for f in flaky {
            println!("{f}");
        }
    }
}
