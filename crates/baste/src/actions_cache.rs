//! Baste's own store for `actions/cache`, standing in for GitHub's cache
//! service, which a local run can't reach.
//!
//! Entries are gzipped tars the agent streams back when a job saves a cache,
//! kept per repository under the cache directory with an index of key,
//! version (a digest of the cached paths), branch and time. As on GitHub, a
//! job can restore entries saved on its own branch or the default branch (and
//! a pull request's base branch), entries are never overwritten, and the
//! oldest go first once the repository's caches pass 10 GB.

use crate::git::RepoRef;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

/// Total size kept per repository, as on GitHub.
const MAX_TOTAL: u64 = 10 * 1024 * 1024 * 1024;
/// At most this many entries (and bytes) go into a job's bundle.
const MAX_CANDIDATES: usize = 20;
const MAX_CANDIDATE_BYTES: u64 = 5 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub key: String,
    pub version: String,
    pub git_ref: String,
    /// File name in the store directory.
    pub file: String,
    pub size: u64,
    pub created_at: DateTime<Utc>,
}

pub struct CacheStore {
    dir: PathBuf,
}

impl CacheStore {
    pub fn for_repo(repo: &RepoRef) -> CacheStore {
        CacheStore::at(
            crate::config::cache_dir()
                .join("actions-cache")
                .join(crate::store::slug(&format!(
                    "{}/{}",
                    repo.host,
                    repo.full_name()
                ))),
        )
    }

    pub fn at(dir: PathBuf) -> CacheStore {
        CacheStore { dir }
    }

    /// Held while reading and writing the index, across Baste processes.
    fn lock(&self) -> Result<std::fs::File> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let f = std::fs::File::create(self.dir.join("index.lock"))?;
        // SAFETY: flock on an fd we own; blocks until the lock is free.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            anyhow::bail!("locking {}", self.dir.display());
        }
        Ok(f)
    }

    fn read(&self) -> Vec<Entry> {
        std::fs::read(self.dir.join("index.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn write(&self, entries: &[Entry]) -> Result<()> {
        let tmp = self
            .dir
            .join(format!("index.json.{:x}", rand::random::<u64>()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(entries)?)?;
        std::fs::rename(&tmp, self.dir.join("index.json"))?;
        Ok(())
    }

    /// Entries a job on one of `refs` may restore, newest first, up to the
    /// bundle limits.
    pub fn candidates(&self, refs: &[String]) -> Vec<(Entry, PathBuf)> {
        let Ok(_lock) = self.lock() else {
            return vec![];
        };
        let mut entries: Vec<Entry> = self
            .read()
            .into_iter()
            .filter(|e| refs.contains(&e.git_ref))
            .filter(|e| self.dir.join(&e.file).is_file())
            .collect();
        entries.sort_by_key(|e| std::cmp::Reverse(e.created_at));
        let mut total = 0;
        let mut out = Vec::new();
        for e in entries {
            if out.len() >= MAX_CANDIDATES || total + e.size > MAX_CANDIDATE_BYTES {
                continue;
            }
            total += e.size;
            let path = self.dir.join(&e.file);
            out.push((e, path));
        }
        out
    }

    /// A new file in the store directory for an entry being received.
    pub fn partial_path(&self) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)?;
        Ok(self
            .dir
            .join(format!("{:016x}.tgz.partial", rand::random::<u64>())))
    }

    /// Keep `partial` as the entry for `key` and `version` on `git_ref`.
    /// Returns false (and drops it) when that entry already exists: entries
    /// are immutable, as on GitHub.
    pub fn commit(&self, key: &str, version: &str, git_ref: &str, partial: &Path) -> Result<bool> {
        let _lock = self.lock()?;
        let mut entries = self.read();
        if entries
            .iter()
            .any(|e| e.key == key && e.version == version && e.git_ref == git_ref)
        {
            let _ = std::fs::remove_file(partial);
            return Ok(false);
        }
        let file = format!("{:016x}.tgz", rand::random::<u64>());
        std::fs::rename(partial, self.dir.join(&file))?;
        let size = std::fs::metadata(self.dir.join(&file))?.len();
        entries.push(Entry {
            key: key.to_string(),
            version: version.to_string(),
            git_ref: git_ref.to_string(),
            file,
            size,
            created_at: Utc::now(),
        });
        // The oldest go first once the total passes the limit.
        entries.sort_by_key(|e| std::cmp::Reverse(e.created_at));
        let mut total = 0;
        entries.retain(|e| {
            total += e.size;
            let keep = total <= MAX_TOTAL;
            if !keep {
                let _ = std::fs::remove_file(self.dir.join(&e.file));
            }
            keep
        });
        self.write(&entries)?;
        Ok(true)
    }
}

/// Whether a job uses `actions/cache` (or its `restore`/`save` actions).
pub fn job_uses_cache(steps: &[serde_json::Value]) -> bool {
    steps.iter().any(|s| {
        s.get("uses")
            .and_then(|u| u.as_str())
            .is_some_and(|u| u.to_ascii_lowercase().starts_with("actions/cache"))
    })
}

/// The refs whose caches a job may restore: its own, the pull request's base
/// branch, and the default branch.
pub fn scope_refs(git_ref: &str, github: &serde_json::Value) -> Vec<String> {
    let mut refs = vec![git_ref.to_string()];
    if let Some(base) = github["base_ref"].as_str().filter(|b| !b.is_empty()) {
        refs.push(format!("refs/heads/{base}"));
    }
    if let Some(default) = github["event"]["repository"]["default_branch"].as_str() {
        refs.push(format!("refs/heads/{default}"));
    }
    refs.dedup();
    refs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, CacheStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = CacheStore::at(tmp.path().join("cache"));
        (tmp, store)
    }

    fn add(store: &CacheStore, key: &str, git_ref: &str, body: &str) -> bool {
        let p = store.partial_path().unwrap();
        std::fs::write(&p, body).unwrap();
        store.commit(key, "v1", git_ref, &p).unwrap()
    }

    #[test]
    fn entries_are_scoped_to_refs_and_newest_first() {
        let (_tmp, store) = store();
        assert!(add(&store, "deps-a", "refs/heads/main", "a"));
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(add(&store, "deps-b", "refs/heads/feature", "b"));
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(add(&store, "deps-c", "refs/heads/other", "c"));
        let keys: Vec<String> = store
            .candidates(&["refs/heads/feature".into(), "refs/heads/main".into()])
            .into_iter()
            .map(|(e, _)| e.key)
            .collect();
        assert_eq!(keys, ["deps-b", "deps-a"]);
    }

    #[test]
    fn entries_are_immutable() {
        let (_tmp, store) = store();
        assert!(add(&store, "deps", "refs/heads/main", "first"));
        assert!(!add(&store, "deps", "refs/heads/main", "second"));
        let (entry, path) = store.candidates(&["refs/heads/main".into()]).remove(0);
        assert_eq!(entry.key, "deps");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "first");
        // No partial files are left behind.
        let partials = std::fs::read_dir(&store.dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".partial")
            })
            .count();
        assert_eq!(partials, 0);
    }

    #[test]
    fn scope_includes_base_and_default_branches() {
        let github = serde_json::json!({
            "base_ref": "develop",
            "event": {"repository": {"default_branch": "main"}}
        });
        assert_eq!(
            scope_refs("refs/heads/feature", &github),
            [
                "refs/heads/feature",
                "refs/heads/develop",
                "refs/heads/main"
            ]
        );
        assert_eq!(
            scope_refs(
                "refs/heads/main",
                &serde_json::json!({"event": {"repository": {"default_branch": "main"}}})
            ),
            ["refs/heads/main"]
        );
    }
}
