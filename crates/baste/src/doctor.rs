//! `baste doctor` and `baste init`: check that this machine and repository can
//! run CI locally, and (for init) install the hook only if everything passes.

use crate::backend::{self, Check};
use crate::config::Config;
use crate::git::Git;
use crate::github::{gh_token, GitHub};
use crate::plan;
use crate::sys::Platform;
use crate::ui::{bold, dim, green, red, yellow};
use anyhow::Result;
use baste_workflow::{depends_on_needs, expand_job, Placement, Workflow};
use serde_json::json;

pub struct Report {
    pub checks: Vec<Check>,
    /// Status contexts local jobs will post, for branch protection.
    pub contexts: Vec<String>,
    pub handed: Vec<String>,
}

impl Report {
    pub fn blocking(&self) -> Vec<&Check> {
        self.checks.iter().filter(|c| !c.ok && c.blocking).collect()
    }

    pub fn print(&self) {
        for c in &self.checks {
            let mark = if c.ok {
                green("✓")
            } else if c.blocking {
                red("✗")
            } else {
                yellow("!")
            };
            println!("{mark} {} {}", bold(&c.name), c.detail);
        }
    }
}

fn platform_check() -> Check {
    match crate::sys::platform() {
        Platform::MacAppleSilicon => Check::pass("Platform", "macOS on Apple Silicon (Tart backend)"),
        Platform::MacIntel => Check::fail(
            "Platform",
            "Intel Macs are not supported: the Tart backend needs Apple Silicon. Nothing was changed.",
        ),
        Platform::Linux => Check::pass("Platform", "Linux (Firecracker backend)"),
        Platform::Wsl2 => Check::pass("Platform", "Windows via WSL2 (Firecracker backend with nested virtualization)"),
        Platform::Other => Check::fail("Platform", format!("{} is not supported", std::env::consts::OS)),
    }
}

pub fn run_checks(git: Option<&Git>, config: &Config, backend_override: Option<&str>) -> Report {
    let mut checks = Vec::new();
    let mut contexts = Vec::new();
    let mut handed = Vec::new();

    let backend = backend::select(config, backend_override);
    let is_host = backend.as_ref().is_ok_and(|b| b.name() == "host");
    if !is_host {
        checks.push(platform_check());
    }
    match &backend {
        Ok(b) => checks.extend(b.doctor()),
        Err(e) => checks.push(Check::fail("Backend", e.to_string())),
    }

    // git
    match std::process::Command::new("git").arg("--version").output() {
        Ok(out) => {
            let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let modern = v
                .split_whitespace()
                .nth(2)
                .and_then(|n| {
                    let mut p = n.split('.').map(|x| x.parse::<u32>().unwrap_or(0));
                    Some((p.next()?, p.next()?))
                })
                .is_some_and(|(a, b)| (a, b) >= (2, 38));
            checks.push(if modern {
                Check::pass("git", v)
            } else {
                Check::warn(
                    "git",
                    format!("{v}: git 2.38+ is needed to test pull_request merges locally"),
                )
            });
        }
        Err(_) => checks.push(Check::fail("git", "git is not installed")),
    }

    let Some(git) = git else {
        checks.push(Check::fail("Repository", "not inside a git repository"));
        return Report {
            checks,
            contexts,
            handed,
        };
    };
    let remote = git.remote_for(git.current_branch().as_deref());
    let repo = match git.repo(&remote) {
        Ok(r) => {
            checks.push(Check::pass(
                "Repository",
                format!("{} (remote '{remote}')", r.full_name()),
            ));
            r
        }
        Err(e) => {
            checks.push(Check::fail("Repository", format!("{e}")));
            return Report {
                checks,
                contexts,
                handed,
            };
        }
    };

    // GitHub auth via gh
    match gh_token(&repo.host) {
        Err(e) => checks.push(Check::fail("GitHub auth", e.to_string())),
        Ok(token) => {
            checks.push(Check::pass(
                "GitHub auth",
                format!("using the gh CLI token for {}", repo.host),
            ));
            let api = GitHub::new(repo.clone(), token);
            match api.repo_info() {
                Err(e) => checks.push(Check::fail(
                    "Commit statuses",
                    format!("can't read {} with the gh token: {e}", repo.full_name()),
                )),
                Ok(info) => {
                    let can_write = info
                        .permissions
                        .as_ref()
                        .is_some_and(|p| p.push || p.maintain || p.admin);
                    let scope_ok = match &info.scopes {
                        None => true, // fine-grained token: no scope header
                        Some(s) => s.iter().any(|x| {
                            x == "repo"
                                || x == "repo:status"
                                || (!info.private && x == "public_repo")
                        }),
                    };
                    if !can_write {
                        checks.push(Check::fail(
                            "Commit statuses",
                            format!(
                                "your GitHub account can't write to {} (missing permission: write access, needed to create commit statuses). Nothing was changed.",
                                repo.full_name()
                            ),
                        ));
                    } else if !scope_ok {
                        checks.push(Check::fail(
                            "Commit statuses",
                            "the gh token is missing the 'repo:status' scope (run `gh auth refresh --scopes repo:status`). Nothing was changed.",
                        ));
                    } else {
                        checks.push(Check::pass(
                            "Commit statuses",
                            "the gh token can write commit statuses",
                        ));
                    }
                }
            }
            checks.push(Check::warn(
                "GITHUB_TOKEN",
                "local jobs get your gh token as GITHUB_TOKEN. It has broader scopes than GitHub's per-job token and doesn't expire with the job; Baste masks it in logs.",
            ));
        }
    }

    // Workflows at HEAD: what runs locally, what GitHub keeps, what's missing.
    if let Ok(head) = git.rev_parse("HEAD") {
        let files = git.ls_tree(&head, plan::WORKFLOW_DIR).unwrap_or_default();
        let secrets = crate::secrets::Secrets::open(&format!("{}/{}", repo.host, repo.full_name()));
        let mut missing = Vec::new();
        let mut local_count = 0;
        for f in files
            .iter()
            .filter(|f| f.ends_with(".yml") || f.ends_with(".yaml"))
        {
            let Ok(Some(src)) = git.show(&head, f) else {
                continue;
            };
            let wf = match Workflow::parse(f, &src) {
                Ok(w) => w,
                Err(e) => {
                    checks.push(Check::warn("Workflow", format!("{f}: {}", e.message)));
                    continue;
                }
            };
            if wf.on.push.is_none() && wf.on.pull_request.is_none() {
                continue;
            }
            let ctx = json!({"github": {"event_name": "push", "ref": "refs/heads/main"}, "needs": {}, "vars": {}, "inputs": {}});
            for job in &wf.jobs {
                if depends_on_needs(job) {
                    contexts.push(plan::status_context(
                        &wf.display_name(),
                        job.name.as_deref().unwrap_or(&job.id),
                    ));
                    continue;
                }
                let Ok(instances) = expand_job(&wf, job, ctx.as_object().unwrap()) else {
                    continue;
                };
                for inst in instances {
                    match inst.placement {
                        Placement::Local => {
                            local_count += 1;
                            contexts.push(plan::status_context(&wf.display_name(), &inst.name));
                            for name in crate::bundle::referenced_secrets(&wf.env, job) {
                                if name == "GITHUB_TOKEN" || missing.contains(&name) {
                                    continue;
                                }
                                let set = secrets
                                    .as_ref()
                                    .ok()
                                    .and_then(|s| s.get(&name).ok().flatten())
                                    .is_some();
                                if !set {
                                    missing.push(name);
                                }
                            }
                        }
                        Placement::GitHub(reason) => {
                            handed.push(format!("{} / {}: {reason}", wf.display_name(), inst.name))
                        }
                    }
                }
            }
        }
        checks.push(if local_count > 0 {
            Check::pass(
                "Workflows",
                format!(
                    "{local_count} job(s) run locally, {} handed to GitHub",
                    handed.len()
                ),
            )
        } else {
            Check::warn(
                "Workflows",
                "no push or pull_request jobs run locally at HEAD",
            )
        });
        match &secrets {
            Ok(s) if missing.is_empty() => checks.push(Check::pass("Secrets", format!("stored in {}", s.describe()))),
            Ok(_) => checks.push(Check::warn(
                "Secrets",
                format!(
                    "not set locally: {} (set with `baste secrets set NAME`); jobs using them will fail",
                    missing.join(", ")
                ),
            )),
            Err(e) => checks.push(Check::warn("Secrets", e.to_string())),
        }
    }
    contexts.dedup();
    Report {
        checks,
        contexts,
        handed,
    }
}

/// The hook records this binary's path. When that path is in npx's cache,
/// which npm may clear, say how to install Baste for good. (The hook falls
/// back to `baste` on `PATH`, so a later global install keeps it working.)
fn transient_install_note(exe: &std::path::Path) -> Option<String> {
    let path = exe.to_string_lossy();
    path.contains("/_npx/").then(|| {
        "You ran Baste through npx, so the hook points into npm's cache, which npm may clear. \
         Install it for good with `npm install -g @weftsh/baste`."
            .to_string()
    })
}

/// `baste init`: check everything, change nothing on failure, else install.
pub fn init(git: Option<&Git>, backend_override: Option<&str>) -> Result<bool> {
    let mut config = Config::load()?;
    if let Some(b) = backend_override {
        config.backend = b.to_string();
    }
    let report = run_checks(git, &config, backend_override);
    report.print();
    let blocking = report.blocking();
    if !blocking.is_empty() {
        println!("\n{} Nothing was changed.", red("baste init stopped:"));
        for c in blocking {
            println!("  • {}: {}", c.name, c.detail);
        }
        return Ok(false);
    }
    let git = git.expect("checked above");
    let installed = crate::hook::install(git)?;
    if backend_override.is_some() || !Config::path().exists() {
        config.save()?;
    }
    println!();
    match installed {
        crate::hook::Installed::Chained => println!(
            "{} Installed the pre-push hook (your existing hook still runs first).",
            green("✓")
        ),
        crate::hook::Installed::Updated => println!("{} Updated the pre-push hook.", green("✓")),
        crate::hook::Installed::Fresh => println!("{} Installed the pre-push hook.", green("✓")),
    }
    if let Some(note) = std::env::current_exe()
        .ok()
        .and_then(|p| transient_install_note(&p))
    {
        println!("{} {note}", yellow("!"));
    }
    println!(
        "\nNext: push a commit. The push returns immediately and the run starts in the background."
    );
    println!("      Watch it with `baste status` and `baste logs latest`.");
    if !report.contexts.is_empty() {
        println!(
            "\nTo merge on a local pass, add these as required status checks in branch protection:"
        );
        for c in &report.contexts {
            println!("  {c}");
        }
        println!("{}", dim("and make the GitHub-hosted versions of those jobs non-required (or add the gate action: `baste gate`)."));
    }
    if !report.handed.is_empty() {
        println!("\nThese jobs stay on GitHub:");
        for h in &report.handed {
            println!("  {}", dim(h));
        }
    }
    Ok(true)
}

#[cfg(test)]
mod npx_tests {
    #[test]
    fn npx_cache_install_gets_a_note() {
        use std::path::Path;
        let npx =
            Path::new("/home/u/.npm/_npx/1a2b/node_modules/@weftsh/baste-linux-x64/bin/baste");
        assert!(super::transient_install_note(npx).is_some());
        let global = Path::new("/usr/local/lib/node_modules/@weftsh/baste-linux-x64/bin/baste");
        assert!(super::transient_install_note(global).is_none());
        assert!(super::transient_install_note(Path::new("/home/u/.local/bin/baste")).is_none());
    }
}
