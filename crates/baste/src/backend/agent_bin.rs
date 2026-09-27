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
            bail!(
                "BASTE_AGENT_BIN points to {}, which doesn't exist",
                p.display()
            );
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
    // The agent runs as the VM's first process, before any libraries exist,
    // so only a statically linked build of ourselves will do.
    if cfg!(target_os = "linux") && std::env::consts::ARCH == arch && is_static_elf(&exe) {
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

/// Whether an ELF executable has no program interpreter (is statically linked).
pub fn is_static_elf(path: &std::path::Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    if bytes.len() < 64 || &bytes[..4] != b"\x7fELF" || bytes[4] != 2 {
        return false; // not a 64-bit ELF
    }
    let u16_at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]) as usize;
    let phoff = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
    let (phentsize, phnum) = (u16_at(54), u16_at(56));
    const PT_INTERP: u32 = 3;
    !(0..phnum).any(|i| {
        let o = phoff + i * phentsize;
        o + 4 <= bytes.len() && u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap()) == PT_INTERP
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_dynamic_executables() {
        // /bin/sh is dynamically linked on every distribution we build on.
        assert!(!is_static_elf(std::path::Path::new("/bin/sh")));
        assert!(!is_static_elf(std::path::Path::new("/etc/hostname")));
        let musl = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/x86_64-unknown-linux-musl/release/baste");
        if musl.is_file() {
            assert!(is_static_elf(&musl));
        }
    }
}
