//! User configuration (`~/.config/baste/config.toml`) and well-known paths.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Default status link: the static page that shows the `baste logs` command.
pub const DEFAULT_DETAILS_URL: &str =
    "https://weftsh.github.io/baste/run/?id={run}&repo={repo}&sha={sha}";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// `auto`, `firecracker`, `tart`, or `host` (no VM; development only).
    pub backend: String,
    /// How many jobs (VMs) may run at once across all runs.
    pub max_parallel_jobs: usize,
    /// vCPUs per VM. 0 picks half the host's cores (at most 4).
    pub cpus: u32,
    /// Memory per VM in MiB. 0 picks a quarter of host memory (2–8 GiB).
    pub memory_mb: u32,
    /// Writable disk per VM in GiB (sparse).
    pub disk_gb: u32,
    /// Wait for AC power before starting runs.
    pub pause_on_battery: bool,
    /// Link on each commit status. `{run}`, `{repo}`, `{sha}` are substituted.
    pub details_url: String,
    /// How long to keep retrying statuses while waiting for the pushed commit
    /// to appear on GitHub.
    pub status_wait_minutes: u64,
    /// Cancel a still-running run when the same branch is pushed again.
    pub cancel_superseded: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            backend: "auto".into(),
            max_parallel_jobs: 2,
            cpus: 0,
            memory_mb: 0,
            disk_gb: 40,
            pause_on_battery: false,
            details_url: DEFAULT_DETAILS_URL.into(),
            status_wait_minutes: 15,
            cancel_superseded: true,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        config_dir().join("config.toml")
    }

    pub fn load() -> Result<Config> {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, toml::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Set one key from a string (for `baste config set`).
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let mut table: toml::Table = toml::from_str(&toml::to_string(self)?)?;
        let current = table
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("unknown setting '{key}'"))?;
        let parsed = match current {
            toml::Value::Integer(_) => toml::Value::Integer(
                value
                    .parse()
                    .with_context(|| format!("'{key}' must be a number"))?,
            ),
            toml::Value::Boolean(_) => toml::Value::Boolean(
                value
                    .parse()
                    .with_context(|| format!("'{key}' must be true or false"))?,
            ),
            _ => toml::Value::String(value.to_string()),
        };
        table.insert(key.to_string(), parsed);
        *self = toml::Value::Table(table).try_into()?;
        Ok(())
    }

    pub fn vm_cpus(&self) -> u32 {
        if self.cpus > 0 {
            return self.cpus;
        }
        let n = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(2);
        (n / 2).clamp(1, 4)
    }

    pub fn vm_memory_mb(&self) -> u32 {
        if self.memory_mb > 0 {
            return self.memory_mb;
        }
        let total = crate::sys::total_memory_mb().unwrap_or(8192);
        (total / 4).clamp(2048, 8192)
    }
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn home() -> PathBuf {
    env_dir("HOME").unwrap_or_else(|| PathBuf::from("/"))
}

pub fn config_dir() -> PathBuf {
    env_dir("BASTE_CONFIG_DIR")
        .or_else(|| env_dir("XDG_CONFIG_HOME").map(|d| d.join("baste")))
        .unwrap_or_else(|| home().join(".config/baste"))
}

/// Downloaded images, actions, firecracker binaries.
pub fn cache_dir() -> PathBuf {
    env_dir("BASTE_CACHE_DIR").unwrap_or_else(|| {
        if cfg!(target_os = "macos") {
            home().join("Library/Caches/baste")
        } else {
            env_dir("XDG_CACHE_HOME")
                .map(|d| d.join("baste"))
                .unwrap_or_else(|| home().join(".cache/baste"))
        }
    })
}

/// Machine-wide runtime state (VM slot locks).
pub fn state_dir() -> PathBuf {
    env_dir("BASTE_STATE_DIR").unwrap_or_else(|| {
        env_dir("XDG_STATE_HOME")
            .map(|d| d.join("baste"))
            .unwrap_or_else(|| home().join(".local/state/baste"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_parses_types() {
        let mut c = Config::default();
        c.set("max_parallel_jobs", "4").unwrap();
        c.set("pause_on_battery", "true").unwrap();
        c.set("backend", "host").unwrap();
        assert_eq!(c.max_parallel_jobs, 4);
        assert!(c.pause_on_battery);
        assert_eq!(c.backend, "host");
        assert!(c.set("nope", "1").is_err());
        assert!(c.set("cpus", "many").is_err());
    }

    #[test]
    fn defaults_round_trip() {
        let c = Config::default();
        let back: Config = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(back.details_url, DEFAULT_DETAILS_URL);
        let partial: Config = toml::from_str("backend = \"tart\"\n").unwrap();
        assert_eq!(partial.max_parallel_jobs, 2);
    }
}
