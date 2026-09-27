//! The git pre-push hook: installing it, and turning a push into background runs.

use crate::git::{Git, RepoRef};
use crate::plan::ZERO_SHA;
use crate::store::{Run, Store, Trigger};
use anyhow::{Context, Result};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MARKER: &str = "# baste pre-push hook";
const CHAINED: &str = "pre-push.baste-chained";

fn script(baste: &Path) -> String {
    format!(
        r#"#!/bin/sh
{MARKER} (installed by `baste init`; remove with `baste uninstall`).
# Runs this push's GitHub Actions workflows locally in the background and
# reports the results to GitHub as commit statuses. Never blocks the push.
input=$(cat)
hooks_dir=$(dirname "$0")
if [ -x "$hooks_dir/{CHAINED}" ]; then
  printf '%s\n' "$input" | "$hooks_dir/{CHAINED}" "$@" || exit $?
fi
if [ -z "$BASTE_DISABLE" ]; then
  BASTE='{}'
  [ -x "$BASTE" ] || BASTE=$(command -v baste 2>/dev/null)
  if [ -n "$BASTE" ]; then
    printf '%s\n' "$input" | "$BASTE" hook pre-push "$@" || true
  fi
fi
exit 0
"#,
        baste.display().to_string().replace('\'', "'\\''")
    )
}

fn hook_path(git: &Git) -> Result<PathBuf> {
    Ok(git.hooks_dir()?.join("pre-push"))
}

pub fn is_installed(git: &Git) -> bool {
    hook_path(git)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .is_some_and(|s| s.contains(MARKER))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Installed {
    Fresh,
    Updated,
    /// An existing hook was kept and now runs first.
    Chained,
}

pub fn install(git: &Git) -> Result<Installed> {
    let path = hook_path(git)?;
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let exe = std::env::current_exe().context("locating the baste binary")?;
    let outcome = match std::fs::read_to_string(&path) {
        Ok(existing) if existing.contains(MARKER) => Installed::Updated,
        Ok(_) => {
            let chained = dir.join(CHAINED);
            if chained.exists() {
                anyhow::bail!(
                    "{} exists and isn't Baste's, and {} is also taken; merge them by hand",
                    path.display(),
                    chained.display()
                );
            }
            std::fs::rename(&path, &chained)?;
            Installed::Chained
        }
        Err(_) => Installed::Fresh,
    };
    std::fs::write(&path, script(&exe))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(outcome)
}

/// Remove the hook, restoring a chained one. Returns whether anything changed.
pub fn uninstall(git: &Git) -> Result<bool> {
    let path = hook_path(git)?;
    if !is_installed(git) {
        return Ok(false);
    }
    std::fs::remove_file(&path)?;
    let chained = path.parent().unwrap().join(CHAINED);
    if chained.exists() {
        std::fs::rename(chained, &path)?;
    }
    Ok(true)
}

/// One line of pre-push input: `<local ref> <local sha> <remote ref> <remote sha>`.
#[derive(Debug, PartialEq, Eq)]
pub struct PushedRef {
    pub local_sha: String,
    pub remote_ref: String,
    pub remote_sha: String,
}

pub fn parse_input(input: &str) -> Vec<PushedRef> {
    input
        .lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.split_whitespace().collect();
            if p.len() != 4 || p[1] == ZERO_SHA {
                return None; // deletions and noise
            }
            if !(p[2].starts_with("refs/heads/") || p[2].starts_with("refs/tags/")) {
                return None;
            }
            Some(PushedRef {
                local_sha: p[1].to_string(),
                remote_ref: p[2].to_string(),
                remote_sha: p[3].to_string(),
            })
        })
        .collect()
}

/// `baste hook pre-push <remote> <url>`: queue a run per pushed ref and start
/// its worker in the background. Always returns quickly.
pub fn pre_push(remote: &str, url: &str) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let refs = parse_input(&input);
    if refs.is_empty() {
        return Ok(());
    }
    let git = Git::discover(&std::env::current_dir()?)?;
    let Some(repo) = RepoRef::parse(url).or_else(|| git.repo(remote).ok()) else {
        return Ok(()); // not a GitHub remote
    };
    let store = Store::for_repo(&git);
    for r in refs {
        let mut run = Run::new(
            store.new_id(),
            repo.clone(),
            r.local_sha.clone(),
            r.remote_ref.clone(),
            Trigger::Push,
        );
        run.before = (r.remote_sha != ZERO_SHA).then(|| r.remote_sha.clone());
        run.remote = Some(remote.to_string());
        store.save(&run)?;
        spawn_worker(&git, &store, &run.id)?;
        eprintln!(
            "baste: running CI for {} ({}) locally in run {}; see `baste status`",
            run.short_sha(),
            run.branch(),
            run.id
        );
    }
    Ok(())
}

/// Start `baste worker <id>` detached from the terminal and the push.
pub fn spawn_worker(git: &Git, store: &Store, run_id: &str) -> Result<()> {
    let exe = std::env::current_exe()?;
    let log = std::fs::File::create(store.run_dir(run_id).join("worker.log"))?;
    let mut cmd = Command::new(exe);
    cmd.args(["worker", run_id])
        .current_dir(&git.root)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_PREFIX",
        "GIT_QUARANTINE_PATH",
    ] {
        cmd.env_remove(var);
    }
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("starting the baste worker")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_push_input() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let input = format!(
            "refs/heads/main {a} refs/heads/main {b}\nrefs/heads/gone {ZERO_SHA} refs/heads/gone {b}\nrefs/tags/v1 {a} refs/tags/v1 {ZERO_SHA}\nrefs/notes/x {a} refs/notes/x {b}\n\n"
        );
        let refs = parse_input(&input);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].remote_ref, "refs/heads/main");
        assert_eq!(refs[1].remote_sha, ZERO_SHA);
    }

    #[test]
    fn install_chains_existing_hook() {
        let d = tempfile::tempdir().unwrap();
        let s = Command::new("git")
            .args(["init", "-q"])
            .arg(d.path())
            .status()
            .unwrap();
        assert!(s.success());
        let git = Git::discover(d.path()).unwrap();
        let hook = git.hooks_dir().unwrap().join("pre-push");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(install(&git).unwrap(), Installed::Chained);
        assert!(is_installed(&git));
        assert_eq!(install(&git).unwrap(), Installed::Updated);
        assert!(uninstall(&git).unwrap());
        assert_eq!(
            std::fs::read_to_string(&hook).unwrap(),
            "#!/bin/sh\nexit 0\n"
        );
        assert!(!uninstall(&git).unwrap());
    }
}
