//! `actions/cache`, `actions/cache/restore` and `actions/cache/save` against
//! Baste's local cache store instead of GitHub's cache service.
//!
//! The host puts the saved entries this job may restore into the bundle
//! (`JobSpec::caches`, newest first). Matching follows GitHub: the primary key
//! and then each restore key, each tried as an exact key and then as a
//! prefix, newest first, and only entries saved with the same paths. A save
//! streams a gzipped tar back to the host, which keeps it. Paths under the
//! workspace or home directory are stored relative to them, so a cache
//! restores even when those directories differ between runs.

use crate::job::{Job, PostKind, PostStep, Scope, StepResult};
use base64::Engine;
use baste_expr::{Map, Value};
use baste_protocol::{CacheSource, Event};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

const CHUNK: usize = 512 * 1024;

fn input<'m>(inputs: &'m Map<String, Value>, key: &str) -> &'m str {
    inputs.get(key).and_then(Value::as_str).unwrap_or("").trim()
}

fn flag(inputs: &Map<String, Value>, key: &str) -> bool {
    input(inputs, key).eq_ignore_ascii_case("true")
}

fn lines(s: &str) -> Vec<String> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

fn output(outputs: &mut Map<String, Value>, k: &str, v: &str) {
    outputs.insert(k.into(), Value::String(v.into()));
}

/// Entries only restore for the same paths (and archive options), as on
/// GitHub, where the version is a digest of the same things.
pub(crate) fn version(paths: &[String], cross_os: bool) -> String {
    let mut h = Sha256::new();
    h.update(paths.join("\n"));
    h.update("|gzip|baste-cache-1");
    if cross_os {
        h.update("|cross-os");
    }
    hex::encode(h.finalize())
}

/// The entry to restore: each key as an exact key, then as a prefix, in
/// order; newest first within each (`caches` comes newest first). Keys match
/// ignoring ASCII case, as GitHub's cache service does.
pub(crate) fn find<'a>(
    caches: &'a [CacheSource],
    version: &str,
    keys: &[String],
) -> Option<&'a CacheSource> {
    let lower = |s: &str| s.to_ascii_lowercase();
    for k in keys {
        let k = lower(k);
        let same = |c: &&CacheSource| c.version == version;
        if let Some(c) = caches.iter().filter(same).find(|c| lower(&c.key) == k) {
            return Some(c);
        }
        if let Some(c) = caches
            .iter()
            .filter(same)
            .find(|c| lower(&c.key).starts_with(&k))
        {
            return Some(c);
        }
    }
    None
}

fn is_glob(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// Where a cache path points: `~` is the home directory steps run with,
/// relative paths are in the workspace.
fn resolve(pattern: &str, workspace: &Path, home: &Path) -> PathBuf {
    if pattern == "~" {
        home.to_path_buf()
    } else if let Some(rest) = pattern.strip_prefix("~/") {
        home.join(rest)
    } else if Path::new(pattern).is_absolute() {
        PathBuf::from(pattern)
    } else {
        workspace.join(pattern.trim_start_matches("./"))
    }
}

/// Every path to save, parents before children: whole directories for plain
/// paths, matching files for globs, minus `!` exclusions.
fn expand(paths: &[String], workspace: &Path, home: &Path) -> Result<Vec<PathBuf>, String> {
    let mut excludes = globset::GlobSetBuilder::new();
    let mut includes: Vec<(PathBuf, Option<globset::GlobMatcher>)> = Vec::new();
    for line in paths {
        let (neg, pat) = match line.strip_prefix('!') {
            Some(p) => (true, p.trim()),
            None => (false, line.as_str()),
        };
        let abs = resolve(pat, workspace, home);
        let abs_str = abs.display().to_string();
        let glob = || {
            globset::GlobBuilder::new(&abs_str)
                .literal_separator(true)
                .build()
                .map_err(|e| format!("{pat}: {e}"))
        };
        if neg {
            excludes.add(glob()?);
        } else if is_glob(&abs_str) {
            let base: PathBuf = abs
                .components()
                .take_while(|c| !is_glob(&c.as_os_str().to_string_lossy()))
                .collect();
            includes.push((base, Some(glob()?.compile_matcher())));
        } else {
            includes.push((abs, None));
        }
    }
    let excludes = excludes.build().map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (base, matcher) in includes {
        if std::fs::symlink_metadata(&base).is_err() {
            continue;
        }
        for e in walkdir::WalkDir::new(&base).follow_links(false) {
            let Ok(e) = e else { continue };
            let p = e.path();
            if excludes.is_match(p) {
                continue;
            }
            if let Some(m) = &matcher {
                if e.file_type().is_dir() || !m.is_match(p) {
                    continue;
                }
            }
            if seen.insert(p.to_path_buf()) {
                out.push(p.to_path_buf());
            }
        }
    }
    Ok(out)
}

/// A path's name in the archive: `W/…` under the workspace, `H/…` under the
/// home directory, `A/…` for anything else (absolute).
fn archive_name(path: &Path, workspace: &Path, home: &Path) -> PathBuf {
    if let Ok(rel) = path.strip_prefix(workspace) {
        Path::new("W").join(rel)
    } else if let Ok(rel) = path.strip_prefix(home) {
        Path::new("H").join(rel)
    } else {
        Path::new("A").join(path.strip_prefix("/").unwrap_or(path))
    }
}

/// Where an archived path goes on restore, or None if it isn't one of ours.
fn restore_path(name: &Path, workspace: &Path, home: &Path) -> Option<PathBuf> {
    let mut parts = name.components();
    let base = match parts.next()?.as_os_str().to_str()? {
        "W" => workspace.to_path_buf(),
        "H" => home.to_path_buf(),
        "A" => PathBuf::from("/"),
        _ => return None,
    };
    let rel: PathBuf = parts.collect();
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(base.join(rel))
}

/// `actions/cache` (restore now, save in a post step), `actions/cache/restore`
/// or `actions/cache/save`, by the action's path.
pub(crate) fn run(
    job: &mut Job,
    action_path: Option<&str>,
    inputs: &Map<String, Value>,
    scope: &Scope,
    step_name: &str,
) -> StepResult {
    let index = scope.step_index;
    let paths = lines(input(inputs, "path"));
    let key = input(inputs, "key").to_string();
    if paths.is_empty() {
        job.error(index, "Input required and not supplied: path");
        return StepResult::failed(None);
    }
    if key.is_empty() {
        job.error(index, "Input required and not supplied: key");
        return StepResult::failed(None);
    }
    let version = version(&paths, flag(inputs, "enableCrossOsArchive"));
    if action_path == Some("save") {
        save(job, index, &key, &version, &paths);
        return StepResult::succeeded(Map::new());
    }
    let restore_only = action_path == Some("restore");
    let mut outputs = Map::new();
    if restore_only {
        output(&mut outputs, "cache-primary-key", &key);
    }
    let mut keys = vec![key.clone()];
    keys.extend(lines(input(inputs, "restore-keys")));
    let found = find(&job.spec.caches, &version, &keys).cloned();
    let exact = found
        .as_ref()
        .is_some_and(|c| c.key.eq_ignore_ascii_case(&key));
    match &found {
        None => {
            if flag(inputs, "fail-on-cache-miss") {
                job.error(
                    index,
                    &format!(
                        "Failed to restore cache entry. Exiting as fail-on-cache-miss is set. Input key: {key}"
                    ),
                );
                return StepResult::failed(None);
            }
            job.log(
                index,
                &format!("Cache not found for input keys: {}", keys.join(", ")),
            );
        }
        Some(c) => {
            if flag(inputs, "lookup-only") {
                job.log(
                    index,
                    &format!("Cache found and can be restored from key: {}", c.key),
                );
            } else {
                match restore(job, c) {
                    Ok(bytes) => {
                        job.log(
                            index,
                            &format!("Cache Size: ~{} MB ({bytes} B)", megabytes(bytes)),
                        );
                        job.log(index, &format!("Cache restored from key: {}", c.key));
                    }
                    Err(e) => {
                        // As on GitHub, a failed restore is a warning, not a failure.
                        job.warning(index, &format!("Failed to restore: {e}"));
                    }
                }
            }
            output(
                &mut outputs,
                "cache-hit",
                if exact { "true" } else { "false" },
            );
            if restore_only {
                output(&mut outputs, "cache-matched-key", &c.key);
            }
        }
    }
    if !restore_only {
        job.posts.push(PostStep {
            name: format!("Post {step_name}"),
            condition: "success()".into(),
            scope: scope.clone(),
            kind: PostKind::CacheSave {
                key,
                version,
                paths,
                exact_hit: exact,
            },
        });
    }
    StepResult::succeeded(outputs)
}

/// The post step of `actions/cache`.
pub(crate) fn post_save(
    job: &mut Job,
    index: usize,
    key: &str,
    version: &str,
    paths: &[String],
    exact_hit: bool,
) -> StepResult {
    if exact_hit {
        job.log(
            index,
            &format!("Cache hit occurred on the primary key {key}, not saving cache."),
        );
    } else {
        save(job, index, key, version, paths);
    }
    StepResult::succeeded(Map::new())
}

fn restore(job: &mut Job, cache: &CacheSource) -> Result<u64, String> {
    let file = job.opts.bundle.join(&cache.file);
    let bytes = std::fs::metadata(&file).map_err(|e| e.to_string())?.len();
    let f = std::fs::File::open(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(f));
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);
    archive.set_overwrite(true);
    let workspace = job.dirs.workspace.clone();
    let home = job.step_home();
    let owner = job.user.as_ref().map(|u| (u.uid, u.gid));
    let mut owned: HashSet<PathBuf> = HashSet::new();
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let name = entry.path().map_err(|e| e.to_string())?.into_owned();
        let Some(dest) = restore_path(&name, &workspace, &home) else {
            continue;
        };
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        if entry.header().entry_type().is_file() {
            unpack_sparse(&mut entry, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
        } else {
            entry
                .unpack(&dest)
                .map_err(|e| format!("{}: {e}", dest.display()))?;
        }
        // Restored files (and directories created for them) belong to the
        // user the steps run as.
        if let Some((uid, gid)) = owner {
            let mut p = Some(dest.as_path());
            while let Some(path) = p {
                if path == workspace || path == home || path == Path::new("/") {
                    break;
                }
                if !owned.insert(path.to_path_buf()) {
                    break;
                }
                let _ = std::os::unix::fs::lchown(path, Some(uid), Some(gid));
                p = path.parent();
            }
        }
    }
    Ok(bytes)
}

/// Write a regular file, leaving holes where the data is all zeros, so a
/// cached sparse file (a VM disk image, say) stays sparse instead of turning
/// into gigabytes of written zeros.
fn unpack_sparse(entry: &mut tar::Entry<impl Read>, dest: &Path) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::PermissionsExt;
    let mode = entry.header().mode().unwrap_or(0o644);
    let mtime = entry.header().mtime().ok();
    let size = entry.header().size()?;
    let _ = std::fs::remove_file(dest);
    let mut f = std::fs::File::create(dest)?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = entry.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if buf[..n].iter().all(|&b| b == 0) {
            f.seek(SeekFrom::Current(n as i64))?;
        } else {
            f.write_all(&buf[..n])?;
        }
    }
    f.set_len(size)?;
    f.set_permissions(std::fs::Permissions::from_mode(mode & 0o7777))?;
    if let Some(t) = mtime {
        let _ = f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(t));
    }
    Ok(())
}

/// GitHub's "Cache Size: ~N MB" rounds to the nearest megabyte.
fn megabytes(bytes: u64) -> u64 {
    (bytes + 512 * 1024) / (1024 * 1024)
}

fn save(job: &mut Job, index: usize, key: &str, version: &str, paths: &[String]) {
    let workspace = job.dirs.workspace.clone();
    let home = job.step_home();
    let files = match expand(paths, &workspace, &home) {
        Ok(f) => f,
        Err(e) => {
            job.warning(index, &format!("Failed to save: {e}"));
            return;
        }
    };
    if files.is_empty() {
        job.warning(
            index,
            "Path Validation Error: Path(s) specified in the action for caching do(es) not exist, hence no cache is being saved.",
        );
        return;
    }
    let tmp = job.dirs.temp.join(format!("cache-{}.tgz", job.unique()));
    let built = (|| -> Result<u64, String> {
        let f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        let gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
        let mut b = tar::Builder::new(gz);
        b.follow_symlinks(false);
        for p in &files {
            let name = archive_name(p, &workspace, &home);
            let meta = std::fs::symlink_metadata(p).map_err(|e| e.to_string())?;
            if meta.is_dir() {
                b.append_dir(&name, p)
            } else {
                b.append_path_with_name(p, &name)
            }
            .map_err(|e| format!("adding {}: {e}", p.display()))?;
        }
        b.into_inner()
            .and_then(|gz| gz.finish())
            .map_err(|e| e.to_string())?;
        Ok(std::fs::metadata(&tmp).map_err(|e| e.to_string())?.len())
    })();
    let bytes = match built {
        Ok(b) => b,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            job.warning(index, &format!("Failed to save: {e}"));
            return;
        }
    };
    if let Ok(mut f) = std::fs::File::open(&tmp) {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match f.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => job.sink.send(Event::CacheChunk {
                    key: key.to_string(),
                    version: version.to_string(),
                    data: base64::engine::general_purpose::STANDARD.encode(&buf[..n]),
                }),
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    job.sink.send(Event::CacheEnd {
        key: key.to_string(),
        version: version.to_string(),
        bytes,
    });
    job.log(
        index,
        &format!("Cache Size: ~{} MB ({bytes} B)", megabytes(bytes)),
    );
    job.log(index, &format!("Cache saved with key: {key}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, version: &str) -> CacheSource {
        CacheSource {
            key: key.into(),
            version: version.into(),
            file: format!("{key}.tgz"),
        }
    }

    #[test]
    fn matches_like_github() {
        // Newest first, as the host lists them.
        let caches = vec![
            entry("linux-deps-bbb", "v1"),
            entry("linux-deps-aaa", "v1"),
            entry("linux-deps-ccc", "v2"),
        ];
        let keys = |k: &[&str]| k.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Exact key.
        assert_eq!(
            find(&caches, "v1", &keys(&["linux-deps-aaa"])).unwrap().key,
            "linux-deps-aaa"
        );
        // Case doesn't matter.
        assert_eq!(
            find(&caches, "v1", &keys(&["LINUX-DEPS-AAA"])).unwrap().key,
            "linux-deps-aaa"
        );
        // A restore key matches the newest entry it prefixes.
        assert_eq!(
            find(&caches, "v1", &keys(&["linux-deps-zzz", "linux-deps-"]))
                .unwrap()
                .key,
            "linux-deps-bbb"
        );
        // The primary key is a prefix too.
        assert_eq!(
            find(&caches, "v1", &keys(&["linux-deps"])).unwrap().key,
            "linux-deps-bbb"
        );
        // Only entries saved with the same paths.
        assert_eq!(
            find(&caches, "v2", &keys(&["linux-deps-"])).unwrap().key,
            "linux-deps-ccc"
        );
        assert!(find(&caches, "v3", &keys(&["linux-deps-"])).is_none());
    }

    #[test]
    fn versions_depend_on_paths_and_options() {
        let a = version(&["node_modules".into()], false);
        assert_eq!(a, version(&["node_modules".into()], false));
        assert_ne!(a, version(&["node_modules".into(), "~/.npm".into()], false));
        assert_ne!(a, version(&["node_modules".into()], true));
    }

    #[test]
    fn archive_names_round_trip() {
        let ws = Path::new("/home/runner/work/app/app");
        let home = Path::new("/home/runner");
        for p in [
            "/home/runner/work/app/app/node_modules/x",
            "/home/runner/.cargo/registry",
            "/opt/tool/bin",
        ] {
            let name = archive_name(Path::new(p), ws, home);
            assert_eq!(
                restore_path(&name, ws, home).unwrap(),
                Path::new(p),
                "{}",
                name.display()
            );
        }
        // Restores into the current workspace when it moved.
        let name = archive_name(Path::new("/old/ws/target"), Path::new("/old/ws"), home);
        assert_eq!(
            restore_path(&name, Path::new("/new/ws"), home).unwrap(),
            Path::new("/new/ws/target")
        );
        // Nothing escapes its base.
        assert!(restore_path(Path::new("W/../etc/passwd"), ws, home).is_none());
        assert!(restore_path(Path::new("X/file"), ws, home).is_none());
    }

    #[test]
    fn restores_zero_blocks_as_holes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut data = vec![0u8; 8 << 20];
        data[3 << 20] = 7;
        let mut archive = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o640);
        header.set_cksum();
        archive
            .append_data(&mut header, "disk.img", data.as_slice())
            .unwrap();
        let bytes = archive.into_inner().unwrap();
        let mut archive = tar::Archive::new(bytes.as_slice());
        let dest = tmp.path().join("disk.img");
        for entry in archive.entries().unwrap() {
            unpack_sparse(&mut entry.unwrap(), &dest).unwrap();
        }
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&dest).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o640);
        // 8 MiB long, but only the block with data is allocated. (Linux, as in
        // the VMs: APFS allocates small files like this in full regardless.)
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            assert!(meta.blocks() * 512 < 4 << 20, "{} blocks", meta.blocks());
        }
    }

    #[test]
    fn sizes_round_like_github() {
        assert_eq!(megabytes(211), 0);
        assert_eq!(megabytes(3 * 1024 * 1024 + 600 * 1024), 4);
    }

    #[test]
    fn saves_directories_globs_and_exclusions() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(ws.join("deps/sub")).unwrap();
        std::fs::create_dir_all(ws.join("deps/empty")).unwrap();
        std::fs::write(ws.join("deps/a.txt"), "a").unwrap();
        std::fs::write(ws.join("deps/sub/b.log"), "b").unwrap();
        std::fs::create_dir_all(home.join(".tool")).unwrap();
        std::fs::write(home.join(".tool/c"), "c").unwrap();
        let got = expand(
            &[
                "deps".into(),
                "!**/*.log".into(),
                "~/.tool/*".into(),
                "missing".into(),
            ],
            &ws,
            &home,
        )
        .unwrap();
        let rel: Vec<String> = got
            .iter()
            .map(|p| p.strip_prefix(tmp.path()).unwrap().display().to_string())
            .collect();
        assert!(rel.contains(&"ws/deps".to_string()));
        assert!(rel.contains(&"ws/deps/empty".to_string()));
        assert!(rel.contains(&"ws/deps/a.txt".to_string()));
        assert!(!rel.contains(&"ws/deps/sub/b.log".to_string()), "{rel:?}");
        assert!(rel.contains(&"home/.tool/c".to_string()));
    }
}
