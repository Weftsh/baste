//! Test harness: a fake GitHub API, a fake `gh`, and an isolated repository.

#![allow(dead_code)]

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Status {
    pub sha: String,
    pub state: String,
    pub context: String,
    pub description: String,
    pub target_url: String,
    pub at: Instant,
}

#[derive(Default)]
pub struct State {
    pub statuses: Vec<Status>,
    /// Reject this many status posts with "No commit found" first.
    pub missing_commit_posts: usize,
    pub push_permission: bool,
    pub open_pr: Option<Value>,
    pub workflow_runs: Value,
    pub requests: Vec<String>,
    /// Action tarballs by `owner/repo`: file name -> content.
    pub actions: HashMap<String, Vec<(String, String)>>,
}

pub struct FakeGitHub {
    pub url: String,
    pub state: Arc<Mutex<State>>,
}

impl FakeGitHub {
    pub fn start() -> FakeGitHub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State {
            push_permission: true,
            workflow_runs: json!({"workflow_runs": []}),
            ..Default::default()
        }));
        let s = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let s = s.clone();
                std::thread::spawn(move || handle(stream, &s));
            }
        });
        FakeGitHub { url, state }
    }

    pub fn statuses(&self) -> Vec<Status> {
        self.state.lock().unwrap().statuses.clone()
    }

    /// The latest status per context for a commit.
    pub fn latest(&self, sha: &str) -> HashMap<String, Status> {
        let mut out = HashMap::new();
        for s in self.statuses().into_iter().filter(|s| s.sha == sha) {
            out.insert(s.context.clone(), s);
        }
        out
    }
}

fn respond(stream: &mut TcpStream, code: u16, body: &[u8], content_type: &str, extra: &str) {
    let reason = match code {
        200 => "OK",
        201 => "Created",
        404 => "Not Found",
        422 => "Unprocessable Entity",
        _ => "Status",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

fn json_resp(stream: &mut TcpStream, code: u16, v: Value) {
    respond(
        stream,
        code,
        v.to_string().as_bytes(),
        "application/json",
        "",
    );
}

fn tarball(prefix: &str, files: &[(String, String)]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut b = tar::Builder::new(gz);
    for (name, content) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(content.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, format!("{prefix}/{name}"), content.as_bytes())
            .unwrap();
    }
    b.into_inner().unwrap().finish().unwrap()
}

fn handle(mut stream: TcpStream, state: &Mutex<State>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    let _ = reader.read_exact(&mut body);
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let (path, _query) = target.split_once('?').unwrap_or((&target, ""));
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    state
        .lock()
        .unwrap()
        .requests
        .push(format!("{method} {target}"));

    match (method.as_str(), segs.as_slice()) {
        ("GET", ["user"]) => json_resp(&mut stream, 200, json!({"login": "octo"})),
        ("GET", ["repos", o, r]) => {
            let push = state.lock().unwrap().push_permission;
            let body = json!({
                "id": 42, "full_name": format!("{o}/{r}"), "name": r, "private": false,
                "default_branch": "main", "owner": {"login": o, "id": 7},
                "permissions": {"admin": false, "maintain": false, "push": push, "pull": true}
            });
            respond(
                &mut stream,
                200,
                body.to_string().as_bytes(),
                "application/json",
                "X-OAuth-Scopes: repo, read:org\r\n",
            );
        }
        ("POST", ["repos", _, _, "statuses", sha]) => {
            let mut st = state.lock().unwrap();
            if st.missing_commit_posts > 0 {
                st.missing_commit_posts -= 1;
                drop(st);
                return json_resp(
                    &mut stream,
                    422,
                    json!({"message": format!("No commit found for SHA: {sha}")}),
                );
            }
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            // GitHub rejects descriptions longer than 140 characters.
            if v["description"].as_str().unwrap_or("").chars().count() > 140 {
                drop(st);
                return json_resp(
                    &mut stream,
                    422,
                    json!({"message": "Validation Failed", "errors": [{
                        "resource": "Status", "code": "custom", "field": "description",
                        "message": "description is too long (maximum is 140 characters)"
                    }]}),
                );
            }
            st.statuses.push(Status {
                sha: sha.to_string(),
                state: v["state"].as_str().unwrap_or("").into(),
                context: v["context"].as_str().unwrap_or("").into(),
                description: v["description"].as_str().unwrap_or("").into(),
                target_url: v["target_url"].as_str().unwrap_or("").into(),
                at: Instant::now(),
            });
            drop(st);
            json_resp(&mut stream, 201, json!({"id": 1}));
        }
        ("GET", ["repos", _, _, "pulls"]) => {
            let pr = state.lock().unwrap().open_pr.clone();
            json_resp(
                &mut stream,
                200,
                json!(pr.map(|p| vec![p]).unwrap_or_default()),
            );
        }
        ("GET", ["repos", _, _, "actions", "variables"]) => json_resp(
            &mut stream,
            200,
            json!({"variables": [{"name": "GREETING", "value": "hello from vars"}]}),
        ),
        ("GET", ["repos", _, _, "actions", "workflows", _, "runs"]) => {
            let runs = state.lock().unwrap().workflow_runs.clone();
            json_resp(&mut stream, 200, runs)
        }
        ("GET", ["repos", _, _, "actions", "runs", _, "jobs"]) => json_resp(
            &mut stream,
            200,
            json!({"jobs": [{"name": "build", "conclusion": "success", "started_at": "2026-09-01T10:01:00Z", "completed_at": "2026-09-01T10:05:30Z"}]}),
        ),
        ("GET", ["repos", o, r, "commits", git_ref]) => {
            let sha = format!(
                "{:0>40}",
                format!("{:x}", git_ref.len() * 7919 + o.len() * 31 + r.len())
            );
            json_resp(&mut stream, 200, json!({"sha": sha}))
        }
        ("GET", ["repos", o, r, "tarball", sha]) => {
            let key = format!("{o}/{r}");
            let files = state
                .lock()
                .unwrap()
                .actions
                .get(&key)
                .cloned()
                .unwrap_or_else(|| {
                    vec![
                        (
                            "action.yml".to_string(),
                            format!("name: {key}\nruns:\n  using: node20\n  main: index.js\n"),
                        ),
                        (
                            "index.js".to_string(),
                            format!("console.log('REAL ACTION {key} RAN'); process.exit(1);\n"),
                        ),
                    ]
                });
            let body = tarball(&format!("{o}-{r}-{}", &sha[..7.min(sha.len())]), &files);
            respond(&mut stream, 200, &body, "application/x-gzip", "");
        }
        _ => json_resp(&mut stream, 404, json!({"message": "Not Found"})),
    }
}

/// An isolated repository whose `origin` is a GitHub URL but pushes go to a
/// local bare repository.
pub struct TestEnv {
    pub dir: tempfile::TempDir,
    pub repo: PathBuf,
    pub github: FakeGitHub,
    pub gh: PathBuf,
}

pub const REPO_URL: &str = "https://github.com/acme/app.git";

impl TestEnv {
    pub fn new() -> TestEnv {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let root = root.as_path();
        for d in ["home", "config", "cache", "state", "repo", "remote.git"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let gh = root.join("gh");
        std::fs::write(
            &gh,
            "#!/bin/sh\nif [ \"$1\" = auth ] && [ \"$2\" = token ]; then echo gho_faketoken_123456; exit 0; fi\nexit 1\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = TestEnv {
            repo: root.join("repo"),
            github: FakeGitHub::start(),
            gh,
            dir,
        };
        env.git_in(
            &root.join("remote.git"),
            &["init", "-q", "--bare", "-b", "main"],
        );
        env.git(&["init", "-q", "-b", "main"]);
        env.git(&["remote", "add", "origin", REPO_URL]);
        let bare = root.join("remote.git").display().to_string();
        env.git(&["config", "remote.origin.pushurl", &bare]);
        env
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    pub fn command(&self, program: &str) -> Command {
        let mut c = Command::new(program);
        for (k, _) in std::env::vars() {
            if k.starts_with("GIT_")
                || k.to_ascii_lowercase().ends_with("_proxy")
                || k.starts_with("BASTE_")
            {
                c.env_remove(&k);
            }
        }
        let root = self.root();
        c.env("HOME", root.join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("BASTE_CONFIG_DIR", root.join("config"))
            .env("BASTE_CACHE_DIR", root.join("cache"))
            .env("BASTE_STATE_DIR", root.join("state"))
            .env("BASTE_GITHUB_API_URL", &self.github.url)
            .env("BASTE_GH", &self.gh)
            .env("BASTE_SECRETS_FILE", root.join("config/secrets.json"))
            .env("BASTE_BACKEND", "host")
            .env("NO_COLOR", "1")
            .env("BASTE_NO_NOTIFY", "1")
            .current_dir(&self.repo);
        c
    }

    pub fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = self
            .command("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    pub fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.repo, args)
    }

    pub fn baste(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_baste"))
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    pub fn baste_stdin(&self, args: &[&str], input: &str) -> Output {
        let mut child = self
            .command(env!("CARGO_BIN_EXE_baste"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    pub fn write(&self, path: &str, content: &str) {
        let p = self.repo.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    pub fn commit(&self, msg: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["-c", "commit.gpgsign=false", "commit", "-q", "-m", msg]);
        self.git(&["rev-parse", "HEAD"])
    }

    pub fn runs(&self) -> Vec<Value> {
        let out = self.baste(&["status", "--json", "-n", "100"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    pub fn run(&self, id: &str) -> Value {
        let out = self.baste(&["status", id, "--json"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
        v[0].clone()
    }

    /// Wait for a run to finish (worker gone) and return it.
    pub fn wait(&self, id: &str, timeout: Duration) -> Value {
        let start = Instant::now();
        loop {
            let r = self.run(id);
            let done = !matches!(r["state"].as_str(), Some("queued" | "running"));
            if done && r["worker_pid"].is_null() {
                return r;
            }
            if start.elapsed() > timeout {
                let log = std::fs::read_to_string(self.run_dir(id).join("worker.log"))
                    .unwrap_or_default();
                panic!("run {id} didn't finish in time: {r:#}\nworker.log:\n{log}");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    pub fn run_dir(&self, id: &str) -> PathBuf {
        self.repo.join(".git/baste/runs").join(id)
    }

    pub fn job<'a>(&self, run: &'a Value, name: &str) -> &'a Value {
        run["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["name"] == name || j["job_id"] == name)
            .unwrap_or_else(|| panic!("no job {name} in {run:#}"))
    }
}

pub fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

pub fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}
