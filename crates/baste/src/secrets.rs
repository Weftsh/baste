//! Local secrets, kept in the OS keychain and never fetched from GitHub.
//!
//! macOS uses the login Keychain. Linux (and WSL2) uses the Secret Service via
//! `secret-tool`. Machines without either can opt in to a `0600` JSON file by
//! setting `BASTE_SECRETS_FILE`.

#[cfg(target_os = "macos")]
use anyhow::anyhow;
use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const SERVICE: &str = "baste";

#[derive(Debug, Clone)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
enum Backend {
    #[cfg(target_os = "macos")]
    Keychain,
    SecretTool,
    File(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Secrets {
    backend: Backend,
    /// `host/owner/repo`; secrets set for a repo win over global ones.
    scope: String,
}

impl Secrets {
    pub fn open(scope: &str) -> Result<Secrets> {
        Ok(Secrets {
            backend: detect()?,
            scope: scope.to_string(),
        })
    }

    pub fn describe(&self) -> String {
        match &self.backend {
            #[cfg(target_os = "macos")]
            Backend::Keychain => "macOS Keychain".into(),
            Backend::SecretTool => "Secret Service (secret-tool)".into(),
            Backend::File(p) => format!("file {} (BASTE_SECRETS_FILE)", p.display()),
        }
    }

    fn account(&self, name: &str, global: bool) -> String {
        if global {
            format!("*:{name}")
        } else {
            format!("{}:{name}", self.scope)
        }
    }

    /// The value for `name`: the repository's own, else the global one.
    pub fn get(&self, name: &str) -> Result<Option<String>> {
        if let Some(v) = self.get_raw(&self.account(name, false))? {
            return Ok(Some(v));
        }
        self.get_raw(&self.account(name, true))
    }

    pub fn set(&self, name: &str, value: &str, global: bool) -> Result<()> {
        validate_name(name)?;
        self.set_raw(&self.account(name, global), value)?;
        let mut idx = index()?;
        idx.insert(self.account(name, global));
        save_index(&idx)
    }

    pub fn delete(&self, name: &str, global: bool) -> Result<bool> {
        let account = self.account(name, global);
        let existed = self.get_raw(&account)?.is_some();
        self.delete_raw(&account)?;
        let mut idx = index()?;
        idx.remove(&account);
        save_index(&idx)?;
        Ok(existed)
    }

    /// Names set for this repository and globally.
    pub fn list(&self) -> Result<(Vec<String>, Vec<String>)> {
        let idx = index()?;
        let prefix = format!("{}:", self.scope);
        let repo = idx
            .iter()
            .filter_map(|a| a.strip_prefix(&prefix))
            .map(str::to_string)
            .collect();
        let global = idx
            .iter()
            .filter_map(|a| a.strip_prefix("*:"))
            .map(str::to_string)
            .collect();
        Ok((repo, global))
    }

    fn get_raw(&self, account: &str) -> Result<Option<String>> {
        match &self.backend {
            #[cfg(target_os = "macos")]
            Backend::Keychain => {
                match security_framework::passwords::get_generic_password(SERVICE, account) {
                    Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
                    Err(e) if e.code() == -25300 => Ok(None), // errSecItemNotFound
                    // errSecUserCanceled, errSecInteractionNotAllowed, errSecAuthFailed:
                    // macOS asks before a different build of Baste reads an item.
                    Err(e) if matches!(e.code(), -128 | -25308 | -25293) => Err(anyhow!(
                        "macOS asked whether this version of Baste may read a secret from your \
                         Keychain, and it wasn't allowed ({e}). Each new Baste version asks once: \
                         choose Always Allow when the prompt appears, then `baste rerun`"
                    )),
                    Err(e) => Err(anyhow!("reading from the Keychain: {e}")),
                }
            }
            Backend::SecretTool => {
                let out = Command::new("secret-tool")
                    .args(["lookup", "service", SERVICE, "account", account])
                    .stdin(Stdio::null())
                    .output()
                    .context("running secret-tool")?;
                if out.status.success() {
                    Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
                } else if out.stderr.is_empty() {
                    Ok(None)
                } else {
                    bail!(
                        "secret-tool: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    )
                }
            }
            Backend::File(p) => Ok(read_file(p)?
                .get(account)
                .and_then(Value::as_str)
                .map(str::to_string)),
        }
    }

    fn set_raw(&self, account: &str, value: &str) -> Result<()> {
        match &self.backend {
            #[cfg(target_os = "macos")]
            Backend::Keychain => security_framework::passwords::set_generic_password(
                SERVICE,
                account,
                value.as_bytes(),
            )
            .map_err(|e| anyhow!("writing to the Keychain: {e}")),
            Backend::SecretTool => {
                let mut child = Command::new("secret-tool")
                    .args([
                        "store",
                        &format!("--label=baste {account}"),
                        "service",
                        SERVICE,
                        "account",
                        account,
                    ])
                    .stdin(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .context("running secret-tool")?;
                child.stdin.take().unwrap().write_all(value.as_bytes())?;
                let out = child.wait_with_output()?;
                if !out.status.success() {
                    bail!(
                        "secret-tool: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
                Ok(())
            }
            Backend::File(p) => {
                let mut map = read_file(p)?;
                map.insert(account.into(), Value::String(value.into()));
                write_file(p, &map)
            }
        }
    }

    fn delete_raw(&self, account: &str) -> Result<()> {
        match &self.backend {
            #[cfg(target_os = "macos")]
            Backend::Keychain => {
                let _ = security_framework::passwords::delete_generic_password(SERVICE, account);
                Ok(())
            }
            Backend::SecretTool => {
                let _ = Command::new("secret-tool")
                    .args(["clear", "service", SERVICE, "account", account])
                    .status();
                Ok(())
            }
            Backend::File(p) => {
                let mut map = read_file(p)?;
                map.remove(account);
                write_file(p, &map)
            }
        }
    }
}

fn detect() -> Result<Backend> {
    if let Some(p) = std::env::var_os("BASTE_SECRETS_FILE").filter(|p| !p.is_empty()) {
        return Ok(Backend::File(PathBuf::from(p)));
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Backend::Keychain)
    }
    #[cfg(not(target_os = "macos"))]
    {
        if crate::sys::which("secret-tool").is_none() {
            bail!(no_keychain(
                "secret-tool is not installed (package libsecret-tools)"
            ));
        }
        let probe = Command::new("secret-tool")
            .args(["lookup", "service", SERVICE, "account", "__baste_probe__"])
            .stdin(Stdio::null())
            .output()
            .context("running secret-tool")?;
        if !probe.status.success() && !probe.stderr.is_empty() {
            bail!(no_keychain(String::from_utf8_lossy(&probe.stderr).trim()));
        }
        Ok(Backend::SecretTool)
    }
}

#[cfg(not(target_os = "macos"))]
fn no_keychain(why: &str) -> String {
    format!(
        "no OS keychain is available: {why}.\nRun a Secret Service provider (gnome-keyring or KeePassXC), or opt in to a private file with\n  export BASTE_SECRETS_FILE=~/.config/baste/secrets.json"
    )
}

fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.to_ascii_uppercase().starts_with("GITHUB_");
    if !ok {
        bail!("invalid secret name '{name}': use letters, digits and underscores, don't start with a digit or GITHUB_");
    }
    Ok(())
}

fn index_path() -> PathBuf {
    crate::config::config_dir().join("secret-names.json")
}

fn index() -> Result<BTreeSet<String>> {
    match std::fs::read_to_string(index_path()) {
        Ok(t) => Ok(serde_json::from_str(&t).unwrap_or_default()),
        Err(_) => Ok(BTreeSet::new()),
    }
}

fn save_index(idx: &BTreeSet<String>) -> Result<()> {
    let p = index_path();
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(&p, serde_json::to_vec_pretty(idx)?)?;
    Ok(())
}

fn read_file(p: &PathBuf) -> Result<Map<String, Value>> {
    match std::fs::read_to_string(p) {
        Ok(t) => Ok(serde_json::from_str(&t).with_context(|| format!("parsing {}", p.display()))?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

fn write_file(p: &PathBuf, map: &Map<String, Value>) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(&serde_json::to_vec_pretty(map)?)?;
    std::fs::rename(&tmp, p)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_validated() {
        assert!(validate_name("NPM_TOKEN").is_ok());
        assert!(validate_name("1X").is_err());
        assert!(validate_name("A-B").is_err());
        assert!(validate_name("GITHUB_X").is_err());
    }
}
