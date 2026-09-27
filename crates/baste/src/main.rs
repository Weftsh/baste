//! Baste: run your GitHub Actions workflows locally in a pinned VM on push,
//! and report the result to GitHub as commit statuses.

mod backend;
mod bundle;
mod commands;
mod config;
mod doctor;
mod git;
mod github;
mod guest;
mod hook;
mod insights;
mod plan;
mod poster;
mod routing;
mod secrets;
mod store;
mod sys;
mod ui;
mod worker;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "baste",
    version,
    about = "Local CI for GitHub Actions: run workflows in a pinned VM on push and report to GitHub",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check this machine and repository, then install the pre-push hook.
    Init {
        /// VM backend to use: auto, firecracker, tart, or host (no VM, development only).
        #[arg(long)]
        backend: Option<String>,
    },
    /// Check everything `init` checks, without changing anything.
    Doctor {
        #[arg(long)]
        backend: Option<String>,
    },
    /// Remove the pre-push hook (restoring any hook it chained).
    Uninstall,
    /// List recent runs, or show one run in detail.
    Status {
        /// Run id (or prefix, or `latest`).
        run: Option<String>,
        /// Show the runs for a commit.
        #[arg(long, value_name = "SHA")]
        commit: Option<String>,
        /// How many runs to list.
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Show a run's output per job and step. Streams while the run is in progress.
    Logs {
        /// Run id (or prefix, or `latest`), as shown in the commit status.
        run: String,
        /// Only this job (name, id, or status context).
        job: Option<String>,
        /// Keep streaming until the run finishes (default while it runs).
        #[arg(short, long, overrides_with = "no_follow")]
        follow: bool,
        #[arg(long)]
        no_follow: bool,
        /// Only show failing steps.
        #[arg(long)]
        failed: bool,
    },
    /// Run the workflows for a commit now, without pushing.
    Run {
        /// Commit, branch or tag (default HEAD).
        #[arg(long = "ref")]
        git_ref: Option<String>,
        /// Only this workflow (file name or path). Repeatable.
        #[arg(short, long = "workflow")]
        workflows: Vec<String>,
        /// Only this job id (plus the jobs it needs). Repeatable.
        #[arg(short, long = "job")]
        jobs: Vec<String>,
        /// Force the event instead of detecting it: push or pull_request.
        #[arg(long)]
        event: Option<String>,
        /// Don't post commit statuses.
        #[arg(long)]
        no_status: bool,
        #[arg(long)]
        backend: Option<String>,
        /// Start in the background and return immediately.
        #[arg(short, long)]
        detach: bool,
    },
    /// Rerun a run on the same commit and update its GitHub statuses.
    Rerun {
        run: String,
        #[arg(short, long)]
        detach: bool,
    },
    /// Cancel a run in progress.
    Cancel { run: String },
    /// Time per step, local vs GitHub durations, time saved, flaky jobs.
    Insights {
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
    },
    /// Manage local secrets (kept in the OS keychain, never fetched from GitHub).
    Secrets {
        #[command(subcommand)]
        action: SecretsCmd,
    },
    /// Show or change settings (~/.config/baste/config.toml).
    Config {
        #[command(subcommand)]
        action: Option<ConfigCmd>,
    },
    /// Manage the pinned VM images.
    Image {
        #[command(subcommand)]
        action: ImageCmd,
    },
    /// Linux: create the tap devices and NAT Firecracker VMs use (run with sudo).
    SetupNetwork {
        /// The user who will run baste (default: $SUDO_USER).
        #[arg(long)]
        user: Option<String>,
        /// Number of VM slots to create devices for.
        #[arg(long, default_value_t = 4)]
        slots: usize,
    },
    /// Print the opt-in gate action snippet for a workflow.
    Gate,
    #[command(hide = true)]
    Hook {
        #[command(subcommand)]
        which: HookCmd,
    },
    #[command(hide = true)]
    Worker { run: String },
    #[command(hide = true)]
    Agent {
        #[command(subcommand)]
        action: guest::AgentCmd,
    },
}

#[derive(Subcommand)]
enum SecretsCmd {
    /// Store a secret (reads the value from stdin or prompts).
    Set {
        name: String,
        /// Available to every repository, not just this one.
        #[arg(long)]
        global: bool,
    },
    /// List secret names.
    List,
    /// Delete a secret.
    Rm {
        name: String,
        #[arg(long)]
        global: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    Get { key: String },
    Set { key: String, value: String },
    Path,
}

#[derive(Subcommand)]
enum ImageCmd {
    /// Download and provision the pinned image now.
    Prepare {
        #[arg(long)]
        backend: Option<String>,
    },
    /// Write the Firecracker initramfs (for debugging VM boots).
    #[command(hide = true)]
    Initramfs {
        #[arg(long)]
        out: std::path::PathBuf,
    },
}

#[derive(Subcommand)]
enum HookCmd {
    PrePush { remote: String, url: String },
}

fn repo() -> Result<(git::Git, store::Store)> {
    let git = git::Git::discover(&std::env::current_dir()?)?;
    let store = store::Store::for_repo(&git);
    Ok((git, store))
}

fn main() -> ExitCode {
    // Inside the Firecracker initramfs the binary is PID 1.
    if std::process::id() == 1 {
        return match guest::pid1() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("baste init: {e:#}");
                ExitCode::FAILURE
            }
        };
    }
    let cli = Cli::parse();
    match dispatch(cli.command) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("{} {e:#}", ui::red("error:"));
            ExitCode::FAILURE
        }
    }
}

fn dispatch(command: Command) -> Result<bool> {
    match command {
        Command::Init { backend } => {
            let git = git::Git::discover(&std::env::current_dir()?).ok();
            doctor::init(git.as_ref(), backend.as_deref())
        }
        Command::Doctor { backend } => {
            let git = git::Git::discover(&std::env::current_dir()?).ok();
            let config = config::Config::load()?;
            let report = doctor::run_checks(git.as_ref(), &config, backend.as_deref());
            report.print();
            if let Some(g) = &git {
                let installed = hook::is_installed(g);
                println!(
                    "{} {} {}",
                    if installed {
                        ui::green("✓")
                    } else {
                        ui::yellow("!")
                    },
                    ui::bold("Hook"),
                    if installed {
                        "pre-push hook installed"
                    } else {
                        "not installed (run `baste init`)"
                    }
                );
            }
            Ok(report.blocking().is_empty())
        }
        Command::Uninstall => {
            let (git, _) = repo()?;
            if hook::uninstall(&git)? {
                println!("Removed the pre-push hook.");
            } else {
                println!("No Baste hook installed.");
            }
            Ok(true)
        }
        Command::Status {
            run,
            commit,
            limit,
            json,
        } => {
            let (_, store) = repo()?;
            commands::status(&store, run.as_deref(), commit.as_deref(), limit, json)?;
            Ok(true)
        }
        Command::Logs {
            run,
            job,
            follow,
            no_follow,
            failed,
        } => {
            let (_, store) = repo()?;
            let follow = if follow {
                Some(true)
            } else if no_follow {
                Some(false)
            } else {
                None
            };
            commands::logs(&store, &run, job.as_deref(), follow, failed)
        }
        Command::Run {
            git_ref,
            workflows,
            jobs,
            event,
            no_status,
            backend,
            detach,
        } => {
            let (git, store) = repo()?;
            commands::run_now(
                &git,
                &store,
                commands::RunArgs {
                    git_ref,
                    workflows,
                    jobs,
                    event,
                    no_status,
                    backend,
                    detach,
                },
            )
        }
        Command::Rerun { run, detach } => {
            let (git, store) = repo()?;
            commands::rerun(&git, &store, &run, detach)
        }
        Command::Cancel { run } => {
            let (_, store) = repo()?;
            commands::cancel(&store, &run)?;
            Ok(true)
        }
        Command::Insights { limit } => {
            let (_, store) = repo()?;
            let runs: Vec<_> = store.list()?.into_iter().take(limit).collect();
            insights::print_insights(&runs);
            Ok(true)
        }
        Command::Secrets { action } => {
            let (git, _) = repo()?;
            let remote = git.remote_for(git.current_branch().as_deref());
            let r = git.repo(&remote)?;
            let s = secrets::Secrets::open(&format!("{}/{}", r.host, r.full_name()))?;
            match action {
                SecretsCmd::Set { name, global } => commands::secrets_set(&s, &name, global)?,
                SecretsCmd::List => {
                    let (repo_names, global) = s.list()?;
                    println!("{} ({})", ui::bold(&r.full_name()), s.describe());
                    for n in &repo_names {
                        println!("  {n}");
                    }
                    if !global.is_empty() {
                        println!("{}", ui::bold("All repositories"));
                        for n in &global {
                            println!("  {n}");
                        }
                    }
                    if repo_names.is_empty() && global.is_empty() {
                        println!("  (none)");
                    }
                }
                SecretsCmd::Rm { name, global } => {
                    if s.delete(&name, global)? {
                        println!("Deleted {name}.");
                    } else {
                        println!("{name} was not set.");
                    }
                }
            }
            Ok(true)
        }
        Command::Config { action } => {
            let mut c = config::Config::load()?;
            match action {
                None => print!("{}", toml::to_string_pretty(&c)?),
                Some(ConfigCmd::Path) => println!("{}", config::Config::path().display()),
                Some(ConfigCmd::Get { key }) => {
                    let table: toml::Table = toml::from_str(&toml::to_string(&c)?)?;
                    let v = table
                        .get(&key)
                        .with_context(|| format!("unknown setting '{key}'"))?;
                    println!("{}", v.to_string().trim_matches('"'));
                }
                Some(ConfigCmd::Set { key, value }) => {
                    c.set(&key, &value)?;
                    c.save()?;
                    println!("{key} = {value}");
                }
            }
            Ok(true)
        }
        Command::Image { action } => match action {
            ImageCmd::Prepare { backend } => {
                let c = config::Config::load()?;
                let b = backend::select(&c, backend.as_deref())?;
                b.prepare(&mut |l| println!("{l}"))?;
                println!(
                    "{} image ready: {}",
                    b.name(),
                    b.image().unwrap_or_else(|| "none".into())
                );
                Ok(true)
            }
            ImageCmd::Initramfs { out } => {
                let c = config::Config::load()?;
                let path = backend::firecracker::Firecracker::new(&c).initramfs()?;
                std::fs::copy(&path, &out)?;
                println!("{}", out.display());
                Ok(true)
            }
        },
        Command::SetupNetwork { user, slots } => {
            backend::firecracker::setup_network(user.as_deref(), slots)?;
            Ok(true)
        }
        Command::Gate => {
            commands::gate_snippet();
            Ok(true)
        }
        Command::Hook {
            which: HookCmd::PrePush { remote, url },
        } => {
            // Never block or fail the push.
            if let Err(e) = hook::pre_push(&remote, &url) {
                eprintln!("baste: couldn't start local CI: {e:#}");
            }
            Ok(true)
        }
        Command::Worker { run } => {
            let (git, store) = repo()?;
            worker::work(store, git, &run)?;
            Ok(true)
        }
        Command::Agent { action } => guest::agent(action),
    }
}
