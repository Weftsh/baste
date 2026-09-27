//! Finding a Linux build of `baste` to run as the agent inside a VM.
//!
//! The guest is Linux even when the host is macOS, so the agent is a separate
//! (statically linked) build of the same binary. It is looked up, in order:
//! `BASTE_AGENT_BIN`, a `baste-linux-<arch>` file next to this executable, the
//! running executable itself (Linux hosts of the same architecture), the
//! cache, and finally the matching GitHub release.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::PathBuf;

pub const RELEASES: &str = "https://github.com/weftsh/baste/releases/download";

pub fn agent_binary(arch: &str) -> Result<PathBuf> {
    if let Some(p) = std::env::var_os(format!("BASTE_AGENT_BIN_{}", arch.to_ascii_uppercase()))
        .or_else(|| std::env::var_os("BASTE_AGENT_BIN"))
        .filter(|p| !p.is_empty())
    {
        let p = PathBuf::from(p);
        if !p.is_file() {
            bail!("BASTE_AGENT_BIN points to {}, which doesn't exist", p.display());
        }
        return Ok(p);
    }
    let exe = std::env::current_exe().context("locating the baste binary")?;
    if let Some(dir) = exe.parent() {
        let sibling = dir.join(format!("baste-linux-{arch}"));
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    if cfg!(target_os = "linux") && std::env::consts::ARCH == arch {
        return Ok(exe);
    }
    let version = env!("CARGO_PKG_VERSION");
    let cached = crate::config::cache_dir()
        .join("agent")
        .join(version)
        .join(format!("baste-linux-{arch}"));
    if cached.is_file() {
        return Ok(cached);
    }
    download(version, arch, &cached)?;
    Ok(cached)
}

fn download(version: &str, arch: &str, to: &std::path::Path) -> Result<()> {
    let target = format!("{arch}-unknown-linux-musl");
    let name = format!("baste-v{version}-{target}.tar.gz");
    let url = format!("{RELEASES}/v{version}/{name}");
    let sums_url = format!("{RELEASES}/v{version}/SHA256SUMS");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(300)))
        .build()
        .into();
    let fetch = |u: &str| -> Result<Vec<u8>> {
        let mut resp = agent
            .get(u)
            .call()
            .with_context(|| format!("downloading {u}"))?;
        let mut buf = Vec::new();
        resp.body_mut()
            .with_config()
            .limit(200 << 20)
            .reader()
            .read_to_end(&mut buf)?;
        Ok(buf)
    };
    let archive = fetch(&url).with_context(|| {
        format!("no Linux agent for {arch} found locally; set BASTE_AGENT_BIN to a Linux build of baste")
    })?;
    let sums = String::from_utf8(fetch(&sums_url)?).unwrap_or_default();
    let digest = hex::encode(Sha256::digest(&archive));
    let expected = sums
        .lines()
        .find(|l| l.ends_with(&name))
        .and_then(|l| l.split_whitespace().next())
        .unwrap_or_default();
    if expected != digest {
        bail!("checksum mismatch for {name}");
    }
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(&archive[..]));
    std::fs::create_dir_all(to.parent().unwrap())?;
    for entry in tar.entries()? {
        let mut entry = entry?;
        if entry.path()?.file_name().is_some_and(|n| n == "baste") {
            let tmp = to.with_extension("tmp");
            entry.unpack(&tmp)?;
            std::fs::rename(&tmp, to)?;
            return Ok(());
        }
    }
    bail!("{name} has no baste binary")
}
