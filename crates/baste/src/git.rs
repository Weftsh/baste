//! Git plumbing: reading workflows at a commit, diffs, test merges, and
//! building the packs the VM checks the pushed commit out from.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A GitHub repository, from a remote URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRef {
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl RepoRef {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    pub fn server_url(&self) -> String {
        format!("https://{}", self.host)
    }

    pub fn api_url(&self) -> String {
        if let Ok(url) = std::env::var("BASTE_GITHUB_API_URL") {
            return url.trim_end_matches('/').to_string();
        }
        if self.host == "github.com" {
            "https://api.github.com".into()
        } else {
            format!("https://{}/api/v3", self.host)
        }
    }

    /// Parse `https://github.com/o/r.git`, `git@github.com:o/r.git`,
    /// `ssh://git@github.com/o/r`, and similar.
    pub fn parse(url: &str) -> Option<RepoRef> {
        let url = url.trim();
        let (host, path) = if let Some(rest) = url.split_once("://").map(|(_, r)| r) {
            let rest = rest.split_once('@').map(|(_, r)| r).unwrap_or(rest);
            let (host, path) = rest.split_once('/')?;
            (host.split(':').next()?.to_string(), path.to_string())
        } else if let Some((left, path)) = url.split_once(':') {
            let host = left.split_once('@').map(|(_, h)| h).unwrap_or(left);
            (host.to_string(), path.to_string())
        } else {
            return None;
        };
        let path = path.trim_end_matches('/').trim_end_matches(".git");
        let mut parts = path.split('/').filter(|p| !p.is_empty());
        let owner = parts.next()?.to_string();
        let name = parts.next()?.to_string();
        if parts.next().is_some() || host.is_empty() {
            return None;
        }
        Some(RepoRef { host, owner, name })
    }
}

#[derive(Debug, Clone)]
pub struct Git {
    pub root: PathBuf,
    pub common_dir: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommitInfo {
    pub sha: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    pub committer_name: String,
    pub committer_email: String,
    pub timestamp: String,
}

fn base_command() -> Command {
    let mut c = Command::new("git");
    // Hooks run with GIT_DIR and friends set for the pushing repository;
    // always address the repository explicitly instead.
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_PREFIX",
        "GIT_QUARANTINE_PATH",
    ] {
        c.env_remove(var);
    }
    c.env("GIT_TERMINAL_PROMPT", "0");
    c
}

impl Git {
    pub fn discover(start: &Path) -> Result<Git> {
        let out = base_command()
            .arg("-C")
            .arg(start)
            .args(["rev-parse", "--show-toplevel", "--git-common-dir"])
            .output()
            .context("running git (is it installed?)")?;
        if !out.status.success() {
            bail!("not inside a git repository ({})", start.display());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines();
        let root = PathBuf::from(lines.next().unwrap_or_default());
        let common = PathBuf::from(lines.next().unwrap_or(".git"));
        let common_dir = if common.is_absolute() {
            common
        } else {
            start.join(common)
        };
        Ok(Git {
            root,
            common_dir: common_dir.canonicalize().unwrap_or(common_dir),
        })
    }

    pub fn command(&self) -> Command {
        let mut c = base_command();
        c.arg("-C").arg(&self.root);
        c
    }

    /// Run git and return trimmed stdout, failing with stderr on error.
    pub fn run(&self, args: &[&str]) -> Result<String> {
        let out = self.command().args(args).output().context("running git")?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    }

    pub fn try_run(&self, args: &[&str]) -> Option<String> {
        self.run(args).ok()
    }

    pub fn rev_parse(&self, rev: &str) -> Result<String> {
        self.run(&["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")])
            .map_err(|_| anyhow!("unknown revision '{rev}'"))
    }

    pub fn object_exists(&self, sha: &str) -> bool {
        self.command()
            .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    pub fn current_branch(&self) -> Option<String> {
        self.try_run(&["symbolic-ref", "--quiet", "--short", "HEAD"])
    }

    /// The remote a branch pushes to (defaults to `origin`).
    pub fn remote_for(&self, branch: Option<&str>) -> String {
        branch
            .and_then(|b| {
                self.try_run(&["config", &format!("branch.{b}.pushRemote")])
                    .or_else(|| self.try_run(&["config", "remote.pushDefault"]))
                    .or_else(|| self.try_run(&["config", &format!("branch.{b}.remote")]))
            })
            .filter(|r| !r.is_empty() && r != ".")
            .unwrap_or_else(|| "origin".into())
    }

    pub fn remote_url(&self, remote: &str) -> Result<String> {
        self.run(&["remote", "get-url", "--push", remote])
            .or_else(|_| self.run(&["remote", "get-url", remote]))
            .with_context(|| format!("remote '{remote}' is not configured"))
    }

    /// The GitHub repository behind a remote name or URL.
    pub fn repo(&self, remote_or_url: &str) -> Result<RepoRef> {
        let url = if remote_or_url.contains("://") || remote_or_url.contains('@') {
            remote_or_url.to_string()
        } else {
            self.remote_url(remote_or_url)?
        };
        RepoRef::parse(&url).ok_or_else(|| anyhow!("'{url}' doesn't look like a GitHub repository URL"))
    }

    /// Contents of `path` at `sha`, or `None` if it doesn't exist there.
    pub fn show(&self, sha: &str, path: &str) -> Result<Option<String>> {
        let out = self
            .command()
            .args(["show", &format!("{sha}:{path}")])
            .output()
            .context("running git show")?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
    }

    /// File names directly under `dir` at `sha`.
    pub fn ls_tree(&self, sha: &str, dir: &str) -> Result<Vec<String>> {
        let out = self.run(&["ls-tree", "--name-only", sha, &format!("{}/", dir.trim_end_matches('/'))])?;
        Ok(out.lines().map(str::to_string).collect())
    }

    pub fn changed_files(&self, from: &str, to: &str) -> Result<Vec<String>> {
        let out = self.run(&["diff", "--name-only", "--no-renames", from, to])?;
        Ok(out.lines().filter(|l| !l.is_empty()).map(str::to_string).collect())
    }

    pub fn merge_base(&self, a: &str, b: &str) -> Option<String> {
        self.try_run(&["merge-base", a, b])
    }

    /// Create GitHub's test merge of `head` into `base`. `None` on conflicts.
    pub fn test_merge(&self, base: &str, head: &str, message: &str) -> Result<Option<String>> {
        let out = self
            .command()
            .args(["merge-tree", "--write-tree", "--no-messages", base, head])
            .output()
            .context("running git merge-tree")?;
        match out.status.code() {
            Some(0) => {}
            Some(1) => return Ok(None),
            _ => bail!(
                "git merge-tree failed (git 2.38 or newer is needed): {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        }
        let tree = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        let merge = self
            .command()
            .args(["commit-tree", &tree, "-p", base, "-p", head, "-m", message])
            .env("GIT_AUTHOR_NAME", "GitHub")
            .env("GIT_AUTHOR_EMAIL", "noreply@github.com")
            .env("GIT_COMMITTER_NAME", "GitHub")
            .env("GIT_COMMITTER_EMAIL", "noreply@github.com")
            .output()
            .context("running git commit-tree")?;
        if !merge.status.success() {
            bail!("git commit-tree failed: {}", String::from_utf8_lossy(&merge.stderr).trim());
        }
        Ok(Some(String::from_utf8_lossy(&merge.stdout).trim().to_string()))
    }

    pub fn commit_info(&self, sha: &str) -> Result<CommitInfo> {
        let out = self.run(&["show", "-s", "--format=%H%x00%an%x00%ae%x00%cn%x00%ce%x00%cI%x00%B", sha])?;
        let mut parts = out.splitn(7, '\0');
        let mut next = || parts.next().unwrap_or_default().to_string();
        Ok(CommitInfo {
            sha: next(),
            author_name: next(),
            author_email: next(),
            committer_name: next(),
            committer_email: next(),
            timestamp: next(),
            message: next().trim_end().to_string(),
        })
    }

    pub fn hooks_dir(&self) -> Result<PathBuf> {
        match self.try_run(&["config", "core.hooksPath"]) {
            Some(p) if !p.is_empty() => {
                let p = PathBuf::from(p);
                Ok(if p.is_absolute() { p } else { self.root.join(p) })
            }
            _ => Ok(self.common_dir.join("hooks")),
        }
    }

    /// Write a pack with what a checkout of `sha` at `depth` needs (0 = full
    /// history). Returns the shallow boundary commits.
    pub fn write_pack(&self, sha: &str, depth: u32, out: &Path) -> Result<Vec<String>> {
        let (commits, shallow): (Vec<String>, Vec<String>) = if depth == 0 {
            (vec![], vec![])
        } else {
            self.shallow_commits(sha, depth)?
        };
        let mut revlist = self.command();
        revlist.arg("rev-list").arg("--objects");
        if depth == 0 {
            revlist.arg(sha);
        } else {
            revlist.arg("--no-walk=unsorted").args(&commits);
        }
        let objects = revlist.output().context("running git rev-list")?;
        if !objects.status.success() {
            bail!("git rev-list failed: {}", String::from_utf8_lossy(&objects.stderr).trim());
        }
        let file = std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
        let mut pack = self
            .command()
            .args(["pack-objects", "--stdout", "-q"])
            .stdin(Stdio::piped())
            .stdout(file)
            .spawn()
            .context("running git pack-objects")?;
        pack.stdin
            .take()
            .expect("piped")
            .write_all(&objects.stdout)?;
        if !pack.wait()?.success() {
            bail!("git pack-objects failed");
        }
        Ok(shallow)
    }

    /// Commits within `depth` of `sha` (breadth-first over all parents) and
    /// the boundary commits whose parents are left out.
    fn shallow_commits(&self, sha: &str, depth: u32) -> Result<(Vec<String>, Vec<String>)> {
        let mut commits = vec![sha.to_string()];
        let mut frontier = vec![sha.to_string()];
        let mut shallow = Vec::new();
        for level in 1..=depth {
            let mut next = Vec::new();
            for c in &frontier {
                let line = self.run(&["rev-list", "--parents", "-n1", c])?;
                let parents: Vec<String> = line.split_whitespace().skip(1).map(str::to_string).collect();
                if level == depth {
                    if !parents.is_empty() {
                        shallow.push(c.clone());
                    }
                    continue;
                }
                for p in parents {
                    if !commits.contains(&p) {
                        commits.push(p.clone());
                        next.push(p);
                    }
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        Ok((commits, shallow))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remote_urls() {
        let r = RepoRef::parse("https://github.com/weftsh/baste.git").unwrap();
        assert_eq!((r.host.as_str(), r.full_name().as_str()), ("github.com", "weftsh/baste"));
        assert_eq!(RepoRef::parse("git@github.com:weftsh/baste.git").unwrap().full_name(), "weftsh/baste");
        assert_eq!(RepoRef::parse("ssh://git@github.com/weftsh/baste").unwrap().name, "baste");
        assert_eq!(RepoRef::parse("https://x-access-token@ghe.corp:8443/a/b/").unwrap().host, "ghe.corp");
        assert!(RepoRef::parse("/local/path").is_none());
        assert!(RepoRef::parse("https://github.com/only-owner").is_none());
    }

    pub(crate) fn init_repo(dir: &Path) -> Git {
        let run = |args: &[&str]| {
            let s = Command::new("git").arg("-C").arg(dir).args(args)
                .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@e")
                .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@e")
                .status().unwrap();
            assert!(s.success(), "git {args:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "1").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "one"]);
        std::fs::write(dir.join("b.txt"), "2").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "two"]);
        Git::discover(dir).unwrap()
    }

    #[test]
    fn packs_are_checkout_able() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let git = init_repo(&src);
        let head = git.rev_parse("HEAD").unwrap();
        let pack = d.path().join("p.pack");
        let shallow = git.write_pack(&head, 1, &pack).unwrap();
        assert_eq!(shallow, vec![head.clone()]);
        let full = d.path().join("f.pack");
        assert!(git.write_pack(&head, 0, &full).unwrap().is_empty());
        assert!(std::fs::metadata(&full).unwrap().len() > std::fs::metadata(&pack).unwrap().len());
        assert_eq!(git.changed_files(&format!("{head}~1"), &head).unwrap(), vec!["b.txt"]);
        let info = git.commit_info(&head).unwrap();
        assert_eq!(info.message, "two");
    }
}
