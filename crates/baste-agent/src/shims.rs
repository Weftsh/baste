//! Built-in replacements for actions that would otherwise talk to GitHub's
//! services: `actions/checkout` (checks out the pushed commit from packs the
//! host prepared, so it works before the push lands) and
//! `actions/upload-artifact` / `actions/download-artifact` (artifacts travel
//! through the runner protocol).
//!
//! A shim declines (returns `None`) when inputs ask for something it doesn't
//! handle, and the real action runs instead.

use crate::job::{Job, Scope, StepResult};
use base64::Engine;
use baste_expr::{Env, Map, Value};
use baste_protocol::{Event, Outcome};
use baste_workflow::{Step, Uses};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

const ARTIFACT_CHUNK: usize = 512 * 1024;

pub(crate) fn is_shimmed(job: &Job, uses: &Uses) -> bool {
    matches!(
        uses.repository().as_deref(),
        Some("actions/upload-artifact" | "actions/download-artifact")
    ) || (uses.repository().as_deref() == Some("actions/checkout")
        && job.spec.checkout.packs.iter().any(|_| true))
}

pub(crate) fn try_shim(
    job: &mut Job,
    step: &Step,
    uses: &Uses,
    scope: &mut Scope,
    step_env: &Map<String, Value>,
) -> Option<StepResult> {
    let repo = uses.repository()?;
    let inputs = match eval_with(job, step, scope, step_env) {
        Ok(i) => i,
        Err(e) => {
            job.error(scope.step_index, &e);
            return Some(StepResult::failed(None));
        }
    };
    match repo.as_str() {
        "actions/checkout" => checkout(job, &inputs, scope),
        "actions/upload-artifact" => Some(upload_artifact(job, &inputs, scope)),
        "actions/download-artifact" => Some(download_artifact(job, &inputs, scope)),
        _ => None,
    }
}

/// `with:` values evaluated to strings, keys lowercased.
fn eval_with(
    job: &Job,
    step: &Step,
    scope: &Scope,
    step_env: &Map<String, Value>,
) -> Result<Map<String, Value>, String> {
    let ctx = job.contexts(scope, Some(step_env));
    let f = job.functions(scope);
    let env = Env {
        contexts: &ctx,
        functions: &f,
    };
    let mut out = Map::new();
    for (k, v) in &step.with {
        let s = crate::job::eval_to_string(v, &env)
            .map_err(|e| format!("evaluating input '{k}': {e}"))?;
        out.insert(k.to_ascii_lowercase(), Value::String(s));
    }
    Ok(out)
}

fn input<'m>(inputs: &'m Map<String, Value>, key: &str) -> &'m str {
    inputs.get(key).and_then(Value::as_str).unwrap_or("").trim()
}

fn flag(inputs: &Map<String, Value>, key: &str, default: bool) -> bool {
    match input(inputs, key).to_ascii_lowercase().as_str() {
        "" => default,
        "false" => false,
        _ => true,
    }
}

fn success(outputs: Map<String, Value>) -> StepResult {
    StepResult {
        outcome: Outcome::Success,
        conclusion: Outcome::Success,
        exit_code: Some(0),
        outputs,
    }
}

/// Resolve `rel` inside `root`, refusing paths that escape it.
fn within(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let p = Path::new(rel);
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    let mut out = root.to_path_buf();
    for c in p.components() {
        match c {
            Component::Normal(x) => out.push(x),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() || !out.starts_with(root.parent().unwrap_or(root)) {
                    return Err(format!("path '{rel}' escapes the workspace"));
                }
            }
            _ => return Err(format!("invalid path '{rel}'")),
        }
    }
    Ok(out)
}

// ----- checkout -------------------------------------------------------------

fn checkout(job: &mut Job, inputs: &Map<String, Value>, scope: &Scope) -> Option<StepResult> {
    let spec = job.spec;
    let co = &spec.checkout;
    let index = scope.step_index;
    let repository = input(inputs, "repository");
    if !repository.is_empty() && !repository.eq_ignore_ascii_case(&co.repository) {
        return None;
    }
    let git_ref = input(inputs, "ref");
    let branch = co.git_ref.strip_prefix("refs/heads/").unwrap_or("");
    let tag = co.git_ref.strip_prefix("refs/tags/").unwrap_or("");
    let ours = git_ref.is_empty()
        || git_ref == co.sha
        || git_ref == co.git_ref
        || (!branch.is_empty() && git_ref == branch)
        || (!tag.is_empty() && git_ref == tag);
    if !ours
        || !input(inputs, "ssh-key").is_empty()
        || !input(inputs, "sparse-checkout").is_empty()
        || !input(inputs, "filter").is_empty()
        || co.packs.is_empty()
    {
        return None;
    }
    let depth: u32 = match input(inputs, "fetch-depth") {
        "" => 1,
        d => match d.parse() {
            Ok(n) => n,
            Err(_) => {
                job.error(index, &format!("fetch-depth must be a number, got '{d}'"));
                return Some(StepResult::failed(None));
            }
        },
    };
    let pack = co
        .packs
        .iter()
        .filter(|p| p.depth == 0 || (depth != 0 && p.depth >= depth))
        .min_by_key(|p| if p.depth == 0 { u32::MAX } else { p.depth })
        .or_else(|| co.packs.iter().find(|p| p.depth == 0))?;

    let dir = match within(&job.dirs.workspace, input(inputs, "path")) {
        Ok(d) => d,
        Err(e) => {
            job.error(index, &e);
            return Some(StepResult::failed(None));
        }
    };
    job.log(
        index,
        &format!(
            "Checking out {} at {} from the pushed commit (local, fetch-depth {})",
            co.repository,
            &co.sha[..co.sha.len().min(12)],
            if pack.depth == 0 {
                "0".to_string()
            } else {
                pack.depth.to_string()
            }
        ),
    );
    let result = (|| -> Result<(), String> {
        if flag(inputs, "clean", true) && dir.exists() {
            for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
                let p = entry.map_err(|e| e.to_string())?.path();
                if p.is_dir() && !p.is_symlink() {
                    std::fs::remove_dir_all(&p)
                } else {
                    std::fs::remove_file(&p)
                }
                .map_err(|e| format!("cleaning {}: {e}", p.display()))?;
            }
        }
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        if let Some(u) = &job.user {
            crate::job::chown_path(&dir, u.uid, u.gid);
        }
        let remote = format!("{}/{}", co.server_url.trim_end_matches('/'), co.repository);
        let pack_file = job.opts.bundle.join(&pack.file);
        let user = job.user.clone();
        run_git(job, index, &dir, &["init", "-q", "."])?;
        let config = std::fs::read_to_string(dir.join(".git/config")).unwrap_or_default();
        if config.contains("[remote \"origin\"]") {
            run_git(job, index, &dir, &["remote", "remove", "origin"])?;
        }
        run_git(job, index, &dir, &["remote", "add", "origin", &remote])?;
        run_git(job, index, &dir, &["config", "--local", "gc.auto", "0"])?;
        let pack_dir = dir.join(".git/objects/pack");
        std::fs::create_dir_all(&pack_dir).map_err(|e| e.to_string())?;
        let tmp = pack_dir.join("pack-baste-incoming.pack");
        std::fs::copy(&pack_file, &tmp)
            .map_err(|e| format!("copying {}: {e}", pack_file.display()))?;
        if let Some(u) = &user {
            crate::job::chown_path(&tmp, u.uid, u.gid);
        }
        let out = run_git(
            job,
            index,
            &dir,
            &["index-pack", &tmp.display().to_string()],
        )?;
        let hash = out.lines().last().unwrap_or("").trim().to_string();
        if !hash.is_empty() && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            for ext in ["pack", "idx", "rev"] {
                let from = pack_dir.join(format!("pack-baste-incoming.{ext}"));
                if from.exists() {
                    std::fs::rename(&from, pack_dir.join(format!("pack-{hash}.{ext}")))
                        .map_err(|e| e.to_string())?;
                }
            }
        }
        let shallow = dir.join(".git/shallow");
        if pack.shallow.is_empty() {
            let _ = std::fs::remove_file(&shallow);
        } else {
            std::fs::write(&shallow, pack.shallow.join("\n") + "\n").map_err(|e| e.to_string())?;
        }
        let sha = co.sha.as_str();
        if let Some(b) = co.git_ref.strip_prefix("refs/heads/") {
            let remote_ref = format!("refs/remotes/origin/{b}");
            run_git(job, index, &dir, &["update-ref", &remote_ref, sha])?;
            run_git(
                job,
                index,
                &dir,
                &["checkout", "--progress", "--force", "-B", b, &remote_ref],
            )?;
        } else if let Some(t) = co.git_ref.strip_prefix("refs/tags/") {
            let tag_ref = format!("refs/tags/{t}");
            run_git(job, index, &dir, &["update-ref", &tag_ref, sha])?;
            run_git(
                job,
                index,
                &dir,
                &["checkout", "--progress", "--force", &tag_ref],
            )?;
        } else if let Some(rest) = co.git_ref.strip_prefix("refs/pull/") {
            let remote_ref = format!("refs/remotes/pull/{rest}");
            run_git(job, index, &dir, &["update-ref", &remote_ref, sha])?;
            run_git(
                job,
                index,
                &dir,
                &["checkout", "--progress", "--force", &remote_ref],
            )?;
        } else {
            run_git(
                job,
                index,
                &dir,
                &["checkout", "--progress", "--force", sha],
            )?;
        }
        if flag(inputs, "persist-credentials", true) {
            let token = match input(inputs, "token") {
                "" => job
                    .spec
                    .secrets
                    .get("GITHUB_TOKEN")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                t => t.to_string(),
            };
            if !token.is_empty() {
                let basic = base64::engine::general_purpose::STANDARD
                    .encode(format!("x-access-token:{token}"));
                job.masker.add(&basic);
                let key = format!("http.{}/.extraheader", co.server_url.trim_end_matches('/'));
                run_git(
                    job,
                    index,
                    &dir,
                    &[
                        "config",
                        "--local",
                        &key,
                        &format!("AUTHORIZATION: basic {basic}"),
                    ],
                )?;
            }
        }
        if flag(inputs, "set-safe-directory", true) {
            let d = dir.display().to_string();
            let _ = run_git(
                job,
                index,
                &dir,
                &["config", "--global", "--add", "safe.directory", &d],
            );
        }
        match input(inputs, "submodules").to_ascii_lowercase().as_str() {
            "" | "false" => {}
            mode => {
                let recursive = mode == "recursive";
                let mut sync = vec!["submodule", "sync"];
                let mut update = vec![
                    "-c",
                    "protocol.version=2",
                    "submodule",
                    "update",
                    "--init",
                    "--force",
                    "--depth=1",
                ];
                if recursive {
                    sync.push("--recursive");
                    update.push("--recursive");
                }
                run_git(job, index, &dir, &sync)?;
                run_git(job, index, &dir, &update)?;
            }
        }
        if flag(inputs, "lfs", false) {
            run_git(job, index, &dir, &["lfs", "install", "--local"])?;
            run_git(job, index, &dir, &["lfs", "pull"])?;
        }
        Ok(())
    })();
    Some(match result {
        Ok(()) => {
            let mut outputs = Map::new();
            outputs.insert("ref".into(), Value::String(co.git_ref.clone()));
            outputs.insert("commit".into(), Value::String(co.sha.clone()));
            success(outputs)
        }
        Err(e) => {
            job.error(index, &e);
            StepResult::failed(None)
        }
    })
}

/// Run git in `dir` as the step user, logging output. Returns stdout lines joined.
fn run_git(job: &mut Job, index: usize, dir: &Path, args: &[&str]) -> Result<String, String> {
    let scope = Scope {
        step_index: index,
        steps: Map::new(),
        inputs: None,
        action_path: None,
        action_repository: String::new(),
        action_ref: String::new(),
        env: Map::new(),
        status: crate::job::Status::Success,
        depth: 0,
        defaults: Default::default(),
    };
    let mut env = job.process_env(&scope, &Map::new());
    env.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
    let shown: Vec<&str> = args.to_vec();
    job.log(index, &format!("##[command]git {}", shown.join(" ")));
    let spawn = crate::process::Spawn {
        program: "git".into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        env,
        cwd: dir.to_path_buf(),
        user: job.user.clone(),
        timeout: None,
    };
    let mut out = Vec::new();
    let (exit, _) = crate::process::run(&spawn, &job.opts.cancel, &mut |line| {
        job.log(index, &line);
        out.push(line);
    })
    .map_err(|e| format!("git: {e}"))?;
    if exit.success() {
        Ok(out.join("\n"))
    } else {
        Err(format!(
            "git {} failed ({exit:?})",
            args.first().unwrap_or(&"")
        ))
    }
}

// ----- artifacts ------------------------------------------------------------

fn is_glob(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

fn hidden(rel: &Path) -> bool {
    rel.components()
        .any(|c| matches!(c, Component::Normal(n) if n.to_string_lossy().starts_with('.')))
}

/// Files matched by upload-artifact's `path` input, and the root they are
/// stored relative to (the least common ancestor of the search paths).
fn collect_artifact_files(
    workspace: &Path,
    spec: &str,
    include_hidden: bool,
) -> Result<(PathBuf, Vec<PathBuf>), String> {
    let mut includes: Vec<(PathBuf, Option<globset::GlobMatcher>)> = Vec::new();
    let mut excludes = globset::GlobSetBuilder::new();
    for line in spec.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (neg, pat) = match line.strip_prefix('!') {
            Some(p) => (true, p.trim()),
            None => (false, line),
        };
        let pat = if let Some(rest) = pat.strip_prefix("~/") {
            format!("{}/{rest}", std::env::var("HOME").unwrap_or_default())
        } else {
            pat.to_string()
        };
        let abs = if Path::new(&pat).is_absolute() {
            PathBuf::from(&pat)
        } else {
            workspace.join(pat.trim_start_matches("./"))
        };
        let abs_str = abs.display().to_string();
        if neg {
            excludes.add(
                globset::GlobBuilder::new(&abs_str)
                    .literal_separator(true)
                    .build()
                    .map_err(|e| e.to_string())?,
            );
            continue;
        }
        if is_glob(&abs_str) {
            let base: PathBuf = abs
                .components()
                .take_while(|c| !is_glob(&c.as_os_str().to_string_lossy()))
                .collect();
            let m = globset::GlobBuilder::new(&abs_str)
                .literal_separator(true)
                .build()
                .map_err(|e| e.to_string())?
                .compile_matcher();
            includes.push((base, Some(m)));
        } else {
            includes.push((abs, None));
        }
    }
    let excludes = excludes.build().map_err(|e| e.to_string())?;
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    for (base, matcher) in includes {
        if base.is_file() && matcher.is_none() {
            roots.push(base.parent().unwrap_or(&base).to_path_buf());
            files.push(base);
            continue;
        }
        roots.push(base.clone());
        for e in walkdir::WalkDir::new(&base)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !e.file_type().is_file() {
                continue;
            }
            if matcher.as_ref().is_some_and(|m| !m.is_match(e.path())) {
                continue;
            }
            files.push(e.into_path());
        }
    }
    let root = roots.iter().skip(1).fold(
        roots
            .first()
            .cloned()
            .unwrap_or_else(|| workspace.to_path_buf()),
        |acc, r| {
            let mut common = PathBuf::new();
            for (a, b) in acc.components().zip(r.components()) {
                if a != b {
                    break;
                }
                common.push(a);
            }
            common
        },
    );
    files.retain(|f| !excludes.is_match(f));
    files.retain(|f| include_hidden || !hidden(f.strip_prefix(&root).unwrap_or(f)));
    files.sort();
    files.dedup();
    Ok((root, files))
}

fn upload_artifact(job: &mut Job, inputs: &Map<String, Value>, scope: &Scope) -> StepResult {
    let index = scope.step_index;
    let name = match input(inputs, "name") {
        "" => "artifact".to_string(),
        n => n.to_string(),
    };
    let path = input(inputs, "path").to_string();
    if path.is_empty() {
        job.error(index, "Input required and not supplied: path");
        return StepResult::failed(None);
    }
    let (root, files) = match collect_artifact_files(
        &job.dirs.workspace,
        &path,
        flag(inputs, "include-hidden-files", false),
    ) {
        Ok(r) => r,
        Err(e) => {
            job.error(index, &e);
            return StepResult::failed(None);
        }
    };
    if files.is_empty() {
        let msg = format!(
            "No files were found with the provided path: {path}. No artifacts will be uploaded."
        );
        match input(inputs, "if-no-files-found") {
            "error" => {
                job.error(index, &msg);
                return StepResult::failed(None);
            }
            "ignore" => job.log(index, &msg),
            _ => job.warning(index, &msg),
        }
        return success(Map::new());
    }
    let tmp = job.dirs.temp.join(format!("artifact-{}.tar", job.unique()));
    let built = (|| -> Result<u64, String> {
        let f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        let mut b = tar::Builder::new(f);
        b.follow_symlinks(true);
        for file in &files {
            let rel = file.strip_prefix(&root).unwrap_or(file);
            b.append_path_with_name(file, rel)
                .map_err(|e| format!("adding {}: {e}", file.display()))?;
        }
        b.into_inner().map_err(|e| e.to_string())?;
        Ok(std::fs::metadata(&tmp).map_err(|e| e.to_string())?.len())
    })();
    let bytes = match built {
        Ok(b) => b,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            job.error(index, &e);
            return StepResult::failed(None);
        }
    };
    job.log(
        index,
        &format!(
            "Uploading artifact '{name}': {} file(s), {bytes} bytes (kept locally)",
            files.len()
        ),
    );
    let mut digest = Sha256::new();
    if let Ok(mut f) = std::fs::File::open(&tmp) {
        let mut buf = vec![0u8; ARTIFACT_CHUNK];
        loop {
            match f.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    digest.update(&buf[..n]);
                    job.sink.send(Event::ArtifactChunk {
                        name: name.clone(),
                        data: base64::engine::general_purpose::STANDARD.encode(&buf[..n]),
                    });
                }
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    job.sink.send(Event::ArtifactEnd {
        name: name.clone(),
        files: files.len() as u64,
        bytes,
    });
    let mut outputs = Map::new();
    outputs.insert(
        "artifact-id".into(),
        Value::String(format!("{}", bytes % 1_000_000_007)),
    );
    outputs.insert("artifact-url".into(), Value::String(String::new()));
    outputs.insert(
        "artifact-digest".into(),
        Value::String(hex::encode(digest.finalize())),
    );
    success(outputs)
}

fn download_artifact(job: &mut Job, inputs: &Map<String, Value>, scope: &Scope) -> StepResult {
    let index = scope.step_index;
    let name = input(inputs, "name").to_string();
    let pattern = input(inputs, "pattern").to_string();
    let merge = flag(inputs, "merge-multiple", false);
    let dest = match input(inputs, "path") {
        "" => job.dirs.workspace.clone(),
        p => match within(&job.dirs.workspace, p) {
            Ok(d) => d,
            Err(e) => {
                job.error(index, &e);
                return StepResult::failed(None);
            }
        },
    };
    let available = job.spec.artifacts.clone();
    let selected: Vec<_> = if !name.is_empty() {
        match available.iter().find(|a| a.name == name) {
            Some(a) => vec![(a.clone(), dest.clone())],
            None => {
                job.error(
                    index,
                    &format!("Unable to download artifact(s): Artifact not found for name: {name}"),
                );
                return StepResult::failed(None);
            }
        }
    } else {
        let matcher = if pattern.is_empty() {
            None
        } else {
            match globset::Glob::new(&pattern) {
                Ok(g) => Some(g.compile_matcher()),
                Err(e) => {
                    job.error(index, &format!("invalid pattern: {e}"));
                    return StepResult::failed(None);
                }
            }
        };
        available
            .iter()
            .filter(|a| matcher.as_ref().is_none_or(|m| m.is_match(&a.name)))
            .map(|a| {
                let d = if merge {
                    dest.clone()
                } else {
                    dest.join(&a.name)
                };
                (a.clone(), d)
            })
            .collect()
    };
    for (artifact, to) in &selected {
        let src = job.opts.bundle.join(&artifact.file);
        let r = std::fs::create_dir_all(to)
            .map_err(|e| e.to_string())
            .and_then(|_| std::fs::File::open(&src).map_err(|e| e.to_string()))
            .and_then(|f| tar::Archive::new(f).unpack(to).map_err(|e| e.to_string()));
        if let Err(e) = r {
            job.error(
                index,
                &format!("extracting artifact '{}': {e}", artifact.name),
            );
            return StepResult::failed(None);
        }
        if let Some(u) = &job.user {
            crate::job::chown_tree(to, u.uid, u.gid);
        }
        job.log(
            index,
            &format!(
                "Downloaded artifact '{}' to {}",
                artifact.name,
                to.display()
            ),
        );
    }
    if selected.is_empty() {
        job.log(index, "No artifacts to download");
    }
    let mut outputs = Map::new();
    outputs.insert(
        "download-path".into(),
        Value::String(dest.display().to_string()),
    );
    success(outputs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_paths_use_common_root() {
        let d = tempfile::tempdir().unwrap();
        let w = d.path();
        std::fs::create_dir_all(w.join("dist/js")).unwrap();
        std::fs::create_dir_all(w.join("dist/.cache")).unwrap();
        std::fs::write(w.join("dist/a.txt"), "a").unwrap();
        std::fs::write(w.join("dist/js/b.js"), "b").unwrap();
        std::fs::write(w.join("dist/js/b.map"), "m").unwrap();
        std::fs::write(w.join("dist/.cache/x"), "x").unwrap();
        std::fs::write(w.join("report.xml"), "r").unwrap();

        let (root, files) = collect_artifact_files(w, "dist", false).unwrap();
        assert_eq!(root, w.join("dist"));
        assert_eq!(files.len(), 3);

        let (_, files) = collect_artifact_files(w, "dist\n!dist/**/*.map", false).unwrap();
        assert_eq!(files.len(), 2);

        let (root, files) = collect_artifact_files(w, "dist/**/*.js\nreport.xml", false).unwrap();
        assert_eq!(root, w.to_path_buf());
        assert_eq!(files.len(), 2);

        let (_, files) = collect_artifact_files(w, "dist", true).unwrap();
        assert_eq!(files.len(), 4);
    }

    #[test]
    fn paths_stay_inside_workspace() {
        let root = Path::new("/w/repo/repo");
        assert_eq!(within(root, "sub/dir").unwrap(), root.join("sub/dir"));
        assert_eq!(within(root, "").unwrap(), root.to_path_buf());
        assert!(within(root, "../../../etc").is_err());
    }
}
