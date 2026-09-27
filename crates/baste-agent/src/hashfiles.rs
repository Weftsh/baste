//! `hashFiles()`: a SHA-256 over the SHA-256 of every matching file.

use globset::{GlobBuilder, GlobSetBuilder};
use sha2::{Digest, Sha256};
use std::path::Path;
use walkdir::WalkDir;

/// Hash files under `root` matching `patterns` (relative globs; a leading `!`
/// excludes). Returns an empty string when nothing matches, like GitHub.
pub fn hash_files(root: &Path, patterns: &[String]) -> Result<String, String> {
    let mut include = GlobSetBuilder::new();
    let mut exclude = GlobSetBuilder::new();
    for p in patterns {
        for line in p.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let (neg, pat) = match line.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, line),
            };
            let pat = pat.trim_start_matches("./");
            let rel = match Path::new(pat).strip_prefix(root) {
                Ok(r) => r.to_string_lossy().into_owned(),
                Err(_) => pat.to_string(),
            };
            let glob = GlobBuilder::new(&rel)
                .literal_separator(true)
                .build()
                .map_err(|e| format!("hashFiles: invalid pattern '{line}': {e}"))?;
            if neg {
                exclude.add(glob);
            } else {
                include.add(glob);
            }
        }
    }
    let include = include.build().map_err(|e| e.to_string())?;
    let exclude = exclude.build().map_err(|e| e.to_string())?;

    let mut files: Vec<_> = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let rel = e.path().strip_prefix(root).ok()?.to_path_buf();
            (include.is_match(&rel) && !exclude.is_match(&rel)).then(|| e.into_path())
        })
        .collect();
    files.sort();
    if files.is_empty() {
        return Ok(String::new());
    }
    let mut outer = Sha256::new();
    for f in files {
        let bytes = std::fs::read(&f).map_err(|e| format!("hashFiles: {}: {e}", f.display()))?;
        outer.update(Sha256::digest(&bytes));
    }
    Ok(hex::encode(outer.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_matching_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("a/b")).unwrap();
        std::fs::write(d.path().join("package-lock.json"), "1").unwrap();
        std::fs::write(d.path().join("a/b/package-lock.json"), "2").unwrap();
        std::fs::write(d.path().join("a/other.txt"), "3").unwrap();
        let all = hash_files(d.path(), &["**/package-lock.json".into()]).unwrap();
        assert_eq!(all.len(), 64);
        let root_only = hash_files(d.path(), &["package-lock.json".into()]).unwrap();
        assert_ne!(all, root_only);
        let excluded =
            hash_files(d.path(), &["**/package-lock.json".into(), "!a/**".into()]).unwrap();
        assert_eq!(excluded, root_only);
        assert_eq!(hash_files(d.path(), &["*.nope".into()]).unwrap(), "");
    }
}
