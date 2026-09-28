//! GitHub access: the token from the gh CLI and the few REST endpoints Baste uses.
//!
//! Baste never stores a token. It asks `gh auth token` each time it needs one.

use crate::git::RepoRef;
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// The gh CLI binary (overridable for tests).
fn gh_binary() -> String {
    std::env::var("BASTE_GH").unwrap_or_else(|_| "gh".into())
}

#[derive(Debug)]
pub enum TokenError {
    GhMissing,
    LoggedOut(String),
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::GhMissing => write!(
                f,
                "the GitHub CLI (gh) is not installed. Install it from https://cli.github.com and run `gh auth login`."
            ),
            TokenError::LoggedOut(host) => write!(
                f,
                "gh is not logged in to {host}. Run `gh auth login --hostname {host}`."
            ),
        }
    }
}

impl std::error::Error for TokenError {}

/// Get a token for `host` from `gh auth token`.
pub fn gh_token(host: &str) -> std::result::Result<String, TokenError> {
    let out = Command::new(gh_binary())
        .args(["auth", "token", "--hostname", host])
        .output()
        .map_err(|_| TokenError::GhMissing)?;
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || token.is_empty() {
        return Err(TokenError::LoggedOut(host.to_string()));
    }
    Ok(token)
}

#[derive(Clone)]
pub struct GitHub {
    agent: ureq::Agent,
    api: String,
    token: String,
    pub repo: RepoRef,
}

/// An HTTP failure with its status code.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GitHub API returned {}: {}", self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug, Clone, Deserialize)]
pub struct Permissions {
    #[serde(default)]
    pub admin: bool,
    #[serde(default)]
    pub maintain: bool,
    #[serde(default)]
    pub push: bool,
}

#[derive(Debug, Clone)]
pub struct RepoInfo {
    pub private: bool,
    pub default_branch: String,
    pub permissions: Option<Permissions>,
    /// `X-OAuth-Scopes` for classic tokens; `None` for fine-grained tokens.
    pub scopes: Option<Vec<String>>,
    pub raw: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
    pub head_branch: Option<String>,
    pub head_sha: String,
    pub conclusion: Option<String>,
    pub created_at: String,
    pub run_started_at: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActionsJob {
    pub name: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

impl GitHub {
    pub fn new(repo: RepoRef, token: String) -> GitHub {
        let mut tls = ureq::tls::TlsConfig::builder();
        if std::env::var_os("BASTE_WEBPKI_ROOTS").is_none() {
            tls = tls.root_certs(ureq::tls::RootCerts::PlatformVerifier);
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .user_agent(format!("baste/{}", env!("CARGO_PKG_VERSION")))
            .tls_config(tls.build())
            .build()
            .into();
        GitHub {
            agent,
            api: repo.api_url(),
            token,
            repo,
        }
    }

    /// Connect using the gh CLI token for the repo's host.
    pub fn from_gh(repo: RepoRef) -> Result<GitHub> {
        let token = gh_token(&repo.host)?;
        Ok(GitHub::new(repo, token))
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}{path}", self.api)
        }
    }

    fn repo_path(&self) -> String {
        format!("/repos/{}/{}", self.repo.owner, self.repo.name)
    }

    fn check(status: u16, body: &str) -> Result<()> {
        if (200..300).contains(&status) {
            return Ok(());
        }
        let message = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| body.chars().take(200).collect());
        Err(ApiError { status, message }.into())
    }

    fn get_json(&self, path: &str) -> Result<(Value, ureq::http::HeaderMap)> {
        let mut resp = self
            .agent
            .get(&self.url(path))
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .call()
            .with_context(|| format!("GET {path}"))?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        Self::check(status, &body)?;
        Ok((serde_json::from_str(&body).unwrap_or(Value::Null), headers))
    }

    fn post_json(&self, path: &str, body: &Value) -> Result<Value> {
        let mut resp = self
            .agent
            .post(&self.url(path))
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send_json(body)
            .with_context(|| format!("POST {path}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        Self::check(status, &text)?;
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    pub fn repo_info(&self) -> Result<RepoInfo> {
        let (v, headers) = self.get_json(&self.repo_path())?;
        let scopes = headers
            .get("x-oauth-scopes")
            .and_then(|h| h.to_str().ok())
            .map(|s| {
                s.split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect()
            });
        Ok(RepoInfo {
            private: v.get("private").and_then(Value::as_bool).unwrap_or(true),
            default_branch: v
                .get("default_branch")
                .and_then(Value::as_str)
                .unwrap_or("main")
                .to_string(),
            permissions: v
                .get("permissions")
                .and_then(|p| serde_json::from_value(p.clone()).ok()),
            scopes,
            raw: v,
        })
    }

    pub fn current_user(&self) -> Result<Value> {
        Ok(self.get_json("/user")?.0)
    }

    /// Create a commit status. `state` is pending, success, failure or error.
    pub fn create_status(
        &self,
        sha: &str,
        state: &str,
        context: &str,
        description: &str,
        target_url: Option<&str>,
    ) -> Result<()> {
        let mut body = json!({
            "state": state,
            "context": context,
            "description": truncate(description, 140),
        });
        if let Some(url) = target_url {
            body["target_url"] = json!(url);
        }
        self.post_json(&format!("{}/statuses/{sha}", self.repo_path()), &body)?;
        Ok(())
    }

    /// The open pull request whose head is `branch` in this repository.
    pub fn open_pull_request(&self, branch: &str) -> Result<Option<Value>> {
        let (v, _) = self.get_json(&format!(
            "{}/pulls?state=open&head={}:{}&per_page=5",
            self.repo_path(),
            self.repo.owner,
            urlencode(branch)
        ))?;
        Ok(v.as_array().and_then(|a| a.first()).cloned())
    }

    /// Repository variables for the `vars` context (empty without access).
    pub fn variables(&self) -> Result<serde_json::Map<String, Value>> {
        let (v, _) = self.get_json(&format!(
            "{}/actions/variables?per_page=30",
            self.repo_path()
        ))?;
        let mut out = serde_json::Map::new();
        for var in v
            .get("variables")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let (Some(n), Some(val)) = (
                var.get("name").and_then(Value::as_str),
                var.get("value").and_then(Value::as_str),
            ) {
                out.insert(n.to_string(), Value::String(val.to_string()));
            }
        }
        Ok(out)
    }

    /// Recent completed runs of a workflow file on GitHub.
    pub fn workflow_runs(&self, workflow_file: &str) -> Result<Vec<WorkflowRun>> {
        let name = Path::new(workflow_file)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (v, _) = self.get_json(&format!(
            "{}/actions/workflows/{}/runs?status=completed&per_page=20",
            self.repo_path(),
            urlencode(&name)
        ))?;
        Ok(v.get("workflow_runs")
            .and_then(|r| serde_json::from_value(r.clone()).ok())
            .unwrap_or_default())
    }

    pub fn run_jobs(&self, run_id: u64) -> Result<Vec<ActionsJob>> {
        let (v, _) = self.get_json(&format!(
            "{}/actions/runs/{run_id}/jobs?per_page=100",
            self.repo_path()
        ))?;
        Ok(v.get("jobs")
            .and_then(|r| serde_json::from_value(r.clone()).ok())
            .unwrap_or_default())
    }

    /// Resolve an action ref (tag, branch, sha) to a commit sha.
    pub fn resolve_ref(&self, owner: &str, repo: &str, git_ref: &str) -> Result<String> {
        let (v, _) = self.get_json(&format!(
            "/repos/{owner}/{repo}/commits/{}",
            urlencode(git_ref)
        ))?;
        v.get("sha")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("no commit for {owner}/{repo}@{git_ref}"))
    }

    /// Download a repository at `sha` as a tar.gz and unpack it into `dest`
    /// (without GitHub's top-level directory).
    pub fn download_repo(&self, owner: &str, repo: &str, sha: &str, dest: &Path) -> Result<()> {
        let mut resp = self
            .agent
            .get(&self.url(&format!("/repos/{owner}/{repo}/tarball/{sha}")))
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .call()
            .with_context(|| format!("downloading {owner}/{repo}@{sha}"))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let body = resp.body_mut().read_to_string().unwrap_or_default();
            Self::check(status, &body)?;
        }
        let reader = resp.body_mut().with_config().limit(1 << 30).reader();
        unpack_tarball(reader, dest)
    }
}

/// Marks a fully unpacked directory: one without it is incomplete.
pub const COMPLETE_MARKER: &str = ".baste-complete";

/// Unpack a GitHub tarball into `dest`, dropping the first path component.
///
/// It unpacks into a directory of its own and moves that into place, so
/// concurrent calls (jobs starting together need the same action) never see
/// or disturb each other's partial work. If `dest` is complete by then, the
/// copy already there wins.
pub fn unpack_tarball(reader: impl Read, dest: &Path) -> Result<()> {
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dest.with_file_name(format!(
        "{name}.partial-{}-{:x}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&tmp)?;
    let result = unpack_into(reader, &tmp).and_then(|()| {
        std::fs::write(tmp.join(COMPLETE_MARKER), "")?;
        if dest.join(COMPLETE_MARKER).exists() {
            return Ok(());
        }
        // An incomplete copy from an older Baste: replace it.
        let _ = std::fs::remove_dir_all(dest);
        match std::fs::rename(&tmp, dest) {
            Err(_) if dest.join(COMPLETE_MARKER).exists() => Ok(()),
            other => other.map_err(Into::into),
        }
    });
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

fn unpack_into(reader: impl Read, tmp: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(reader));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let rel: std::path::PathBuf = path.components().skip(1).collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        if rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            bail!("unsafe path in tarball: {}", path.display());
        }
        let out = tmp.join(&rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        entry.unpack(&out)?;
    }
    Ok(())
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Whether an error is GitHub saying the commit doesn't exist (yet).
pub fn is_missing_commit(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ApiError>().is_some_and(|a| {
        a.status == 422 || (a.status == 404 && a.message.to_ascii_lowercase().contains("commit"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tarball shaped like GitHub's: everything under one top-level folder.
    fn tarball(files: &[(&str, &str)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (path, body) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(
                    &mut header,
                    format!("owner-repo-abc123/{path}"),
                    body.as_bytes(),
                )
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn concurrent_unpacks_leave_one_complete_copy() {
        // Jobs that start together download the same action at once.
        let names: Vec<String> = (0..200).map(|i| format!("src/file{i}.txt")).collect();
        let mut files: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "x")).collect();
        files.push(("action.yml", "runs: {using: node20, main: dist/index.js}"));
        let data = tarball(&files);
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("abc123");
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| unpack_tarball(data.as_slice(), &dest).unwrap());
            }
        });
        assert!(dest.join(COMPLETE_MARKER).exists());
        assert!(dest.join("action.yml").exists());
        for n in &names {
            assert!(dest.join(n).exists(), "{n} is missing");
        }
        // No temporary directories are left behind.
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1);
    }

    #[test]
    fn an_incomplete_copy_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("abc123");
        std::fs::create_dir_all(dest.join("src")).unwrap();
        unpack_tarball(tarball(&[("action.yml", "x")]).as_slice(), &dest).unwrap();
        assert!(dest.join("action.yml").exists());
        assert!(dest.join(COMPLETE_MARKER).exists());
    }

    #[test]
    fn truncates_descriptions() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate(&"x".repeat(200), 140).chars().count(), 140);
    }

    #[test]
    fn encodes_urls() {
        assert_eq!(urlencode("feature/a b"), "feature/a%20b");
    }
}
