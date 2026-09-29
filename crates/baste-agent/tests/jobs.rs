//! Runs real jobs through the agent (steps execute as local processes).

use baste_agent::{run_job, AgentOptions, VecSink};
use baste_protocol::*;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

struct Fixture {
    _dir: tempfile::TempDir,
    bundle: PathBuf,
    work: PathBuf,
    spec: JobSpec,
}

fn yaml(src: &str) -> Value {
    baste_workflow::yaml::parse(src).unwrap()
}

fn fixture(steps: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("bundle");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&bundle).unwrap();
    std::fs::write(bundle.join("event.json"), r#"{"ref":"refs/heads/main"}"#).unwrap();
    let steps = yaml(steps).as_array().cloned().unwrap();
    let spec = JobSpec {
        protocol: PROTOCOL_VERSION,
        run_id: "r1".into(),
        job_key: "ci-build".into(),
        job_id: "build".into(),
        job_name: "build".into(),
        workflow: WorkflowInfo {
            name: "CI".into(),
            file: ".github/workflows/ci.yml".into(),
        },
        workflow_env: Map::new(),
        job_env: Map::new(),
        defaults: RunDefaults::default(),
        steps,
        outputs: Map::new(),
        timeout_minutes: None,
        contexts: json!({
            "github": {"repository": "acme/app", "sha": "abc", "ref": "refs/heads/main", "event_name": "push", "workflow": "CI"},
            "matrix": {"node": 20},
            "strategy": {"fail-fast": true, "job-index": 0, "job-total": 1, "max-parallel": 1},
            "needs": {},
            "vars": {"GREETING": "hello"},
            "inputs": {}
        })
        .as_object()
        .unwrap()
        .clone(),
        secrets: json!({"GITHUB_TOKEN": "ghs_faketoken123", "API_KEY": "sup3r-s3cret"})
            .as_object()
            .unwrap()
            .clone(),
        missing_secrets: vec![],
        masks: vec![],
        actions: vec![],
        checkout: CheckoutSource {
            repository: "acme/app".into(),
            sha: "abc".into(),
            git_ref: "refs/heads/main".into(),
            packs: vec![],
            server_url: "https://github.com".into(),
        },
        artifacts: vec![],
        caches: vec![],
        event_file: "event.json".into(),
        runner: RunnerInfo {
            os: "Linux".into(),
            arch: "X64".into(),
            name: "test".into(),
            work_root: work.display().to_string(),
            tool_cache: dir.path().join("toolcache").display().to_string(),
            user: None,
            node: Map::new(),
            docker_platform: None,
            env: Map::new(),
        },
    };
    Fixture {
        _dir: dir,
        bundle,
        work,
        spec,
    }
}

struct Result {
    outcome: Outcome,
    events: Vec<Event>,
}

impl Result {
    fn log(&self) -> String {
        self.events
            .iter()
            .filter_map(|e| match e {
                Event::Log { line, .. } => Some(line.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn steps(&self) -> Vec<(String, Outcome, Outcome)> {
        let mut names = std::collections::HashMap::new();
        let mut out = Vec::new();
        for e in &self.events {
            match e {
                Event::StepStarted { index, name, .. } => {
                    names.insert(*index, name.clone());
                }
                Event::StepFinished {
                    index,
                    outcome,
                    conclusion,
                    ..
                } => out.push((names[index].clone(), *outcome, *conclusion)),
                _ => {}
            }
        }
        out
    }

    fn outputs(&self) -> Map<String, Value> {
        self.events
            .iter()
            .find_map(|e| match e {
                Event::JobFinished { outputs, .. } => Some(outputs.clone()),
                _ => None,
            })
            .unwrap()
    }
}

fn run(f: &Fixture) -> Result {
    let sink = VecSink::default();
    let opts = AgentOptions {
        bundle: f.bundle.clone(),
        cancel: Arc::new(AtomicBool::new(false)),
    };
    let outcome = run_job(&f.spec, &opts, &sink);
    let events = sink.events.lock().unwrap().clone();
    Result { outcome, events }
}

#[test]
fn runs_steps_with_env_files_and_outputs() {
    let mut f = fixture(
        r#"
- id: first
  run: |
    echo "FROM_ENV=hello world" >> "$GITHUB_ENV"
    echo "value=42" >> "$GITHUB_OUTPUT"
    {
      echo 'multi<<EOF'
      echo 'line one'
      echo 'line two'
      echo EOF
    } >> "$GITHUB_OUTPUT"
    echo "$PWD" > where.txt
- name: Use outputs for ${{ matrix.node }}
  run: |
    echo "env=$FROM_ENV"
    echo "out=${{ steps.first.outputs.value }}"
    echo "multi=${{ steps.first.outputs.multi }}"
    echo "vars=${{ vars.GREETING }} job=$GITHUB_JOB ws=$GITHUB_WORKSPACE"
    echo "${{ env.FROM_ENV }}" | tr a-z A-Z
- run: echo "$STEP_VAR and $JOB_VAR"
  env:
    STEP_VAR: step-${{ matrix.node }}
"#,
    );
    f.spec.job_env.insert("JOB_VAR".into(), json!("job-level"));
    f.spec
        .outputs
        .insert("answer".into(), json!("${{ steps.first.outputs.value }}"));
    let r = run(&f);
    let log = r.log();
    assert_eq!(r.outcome, Outcome::Success, "{log}");
    assert!(log.contains("env=hello world"), "{log}");
    assert!(log.contains("out=42"));
    assert!(log.contains("multi=line one"));
    assert!(log.contains("line two"));
    assert!(log.contains("vars=hello job=build ws="));
    assert!(log.contains("HELLO WORLD"));
    assert!(log.contains("step-20 and job-level"));
    let steps = r.steps();
    assert_eq!(steps[0].0, "Set up job");
    assert_eq!(steps[2].0, "Use outputs for 20");
    assert_eq!(r.outputs()["answer"], json!("42"));
    let ws = f.work.join("app/app");
    // Compare physical paths: on macOS the temp dir lives behind /var -> /private/var.
    let pwd = std::fs::read_to_string(ws.join("where.txt")).unwrap();
    assert_eq!(
        std::path::Path::new(pwd.trim()).canonicalize().unwrap(),
        ws.canonicalize().unwrap()
    );
}

#[test]
fn conditions_and_continue_on_error() {
    let f = fixture(
        r#"
- id: flaky
  run: exit 3
  continue-on-error: true
- run: echo "after flaky ${{ steps.flaky.outcome }}/${{ steps.flaky.conclusion }}"
- name: Break
  run: |
    echo about to fail
    exit 1
- name: Skipped by default
  run: echo SHOULD-NOT-RUN
- name: On failure
  if: failure()
  run: echo "ran on failure, job ${{ job.status }}"
- name: Always
  if: always()
  run: echo always-ran
- name: Condition false
  if: ${{ github.event_name == 'pull_request' }}
  run: echo NOPE
"#,
    );
    let r = run(&f);
    let log = r.log();
    assert_eq!(r.outcome, Outcome::Failure);
    assert!(log.contains("after flaky failure/success"), "{log}");
    assert!(log.contains("Process completed with exit code 1."));
    assert!(!log.contains("SHOULD-NOT-RUN"));
    assert!(log.contains("ran on failure, job failure"));
    assert!(log.contains("always-ran"));
    assert!(!log.contains("NOPE"));
    let steps = r.steps();
    let find = |n: &str| steps.iter().find(|s| s.0 == n).unwrap().clone();
    assert_eq!(find("Run exit 3").1, Outcome::Failure);
    assert_eq!(find("Run exit 3").2, Outcome::Success);
    assert_eq!(find("Skipped by default").1, Outcome::Skipped);
    assert_eq!(find("Condition false").1, Outcome::Skipped);
    let exit = r.events.iter().find_map(|e| match e {
        Event::StepFinished {
            exit_code: Some(1), ..
        } => Some(1),
        _ => None,
    });
    assert_eq!(exit, Some(1));
}

#[test]
fn masks_secrets_and_add_mask() {
    let f = fixture(
        r#"
- run: |
    echo "key is ${{ secrets.API_KEY }}"
    echo "token is $GITHUB_TOKEN"
    echo "::add-mask::runtime-value"
    echo "later runtime-value appears"
  env:
    GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
"#,
    );
    let r = run(&f);
    let log = r.log();
    assert_eq!(r.outcome, Outcome::Success, "{log}");
    assert!(!log.contains("sup3r-s3cret"), "{log}");
    assert!(!log.contains("ghs_faketoken123"), "{log}");
    // The script is echoed before it runs (and registers the mask), as on GitHub.
    let output: Vec<&str> = log
        .lines()
        .filter(|l| !l.starts_with("##[command]"))
        .collect();
    assert!(!output.iter().any(|l| l.contains("runtime-value")), "{log}");
    assert!(log.contains("key is ***"));
    assert!(log.contains("token is ***"));
    assert!(log.contains("later *** appears"));
}

#[test]
fn missing_secret_fails_before_any_step() {
    let mut f = fixture("- run: echo SHOULD-NOT-RUN\n");
    f.spec.missing_secrets = vec!["NPM_TOKEN".into()];
    let r = run(&f);
    let log = r.log();
    assert_eq!(r.outcome, Outcome::Failure);
    assert!(log.contains("Secret NPM_TOKEN is not set locally"), "{log}");
    assert!(log.contains("baste secrets set NPM_TOKEN"));
    assert!(!log.contains("SHOULD-NOT-RUN"));
    assert_eq!(r.steps().len(), 1);
}

#[test]
fn workflow_commands_and_annotations() {
    let f = fixture(
        r#"
- id: legacy
  run: |
    echo "::group::Setup things"
    echo inside
    echo "::endgroup::"
    echo "::warning file=app.js,line=3::careful"
    echo "::set-output name=old::legacy-value"
    echo "::stop-commands::tok123"
    echo "::error::not a command"
    echo "::tok123::"
- run: echo "legacy=${{ steps.legacy.outputs.old }}"
- run: echo "::set-env name=X::y"
"#,
    );
    let r = run(&f);
    let log = r.log();
    assert!(log.contains("##[group]Setup things"), "{log}");
    assert!(log.contains("##[warning]careful (app.js:3)"));
    assert!(
        log.contains("::error::not a command"),
        "stopped commands are printed raw"
    );
    assert!(log.contains("legacy=legacy-value"));
    assert!(log.contains("The `set-env` command is disabled"));
    assert_eq!(r.outcome, Outcome::Failure);
    assert!(r.events.iter().any(|e| matches!(e, Event::Annotation { level: AnnotationLevel::Warning, file: Some(f), line: Some(3), .. } if f == "app.js")));
}

#[test]
fn shells_workdir_and_path() {
    let f = fixture(
        r#"
- run: |
    mkdir -p sub/bin
    printf '#!/bin/sh\necho custom-tool-ran\n' > sub/bin/mytool
    chmod +x sub/bin/mytool
    echo "$PWD/sub/bin" >> "$GITHUB_PATH"
- run: mytool
- run: pwd
  working-directory: sub
- shell: sh
  run: echo "sh says hi"
- shell: python3 {0}
  run: |
    import os
    print("python sees", os.environ["GITHUB_REPOSITORY"])
- run: echo nope
  working-directory: does-not-exist
"#,
    );
    let r = run(&f);
    let log = r.log();
    assert!(log.contains("custom-tool-ran"), "{log}");
    assert!(log.contains("/app/app/sub"));
    assert!(log.contains("sh says hi"));
    assert!(log.contains("python sees acme/app"));
    assert!(log.contains("No such file or directory"));
    assert_eq!(r.outcome, Outcome::Failure);
}

#[test]
fn step_timeout() {
    let f = fixture(
        r#"
- run: sleep 20
  timeout-minutes: 0.01
"#,
    );
    let start = std::time::Instant::now();
    let r = run(&f);
    assert!(start.elapsed().as_secs() < 15);
    assert_eq!(r.outcome, Outcome::Failure);
    assert!(r.log().contains("timed out"), "{}", r.log());
}

#[test]
fn composite_local_action_with_inputs_and_outputs() {
    let f = fixture(
        r#"
- id: greet
  uses: ./.github/actions/greet
  with:
    who: Baste
- run: echo "got ${{ steps.greet.outputs.greeting }} and $COMPOSITE_SET"
"#,
    );
    // The workspace already holds the checked-out action.
    let action = f.work.join("app/app/.github/actions/greet");
    std::fs::create_dir_all(&action).unwrap();
    std::fs::write(
        action.join("action.yml"),
        r#"name: Greet
inputs:
  who:
    description: who
    required: true
  punctuation:
    default: '!'
outputs:
  greeting:
    value: ${{ steps.make.outputs.text }}
runs:
  using: composite
  steps:
    - id: make
      shell: bash
      run: echo "text=Hello, ${{ inputs.who }}${{ inputs.punctuation }}" >> "$GITHUB_OUTPUT"
    - shell: bash
      run: echo "action path is $GITHUB_ACTION_PATH"
    - shell: bash
      run: echo "COMPOSITE_SET=yes" >> "$GITHUB_ENV"
"#,
    )
    .unwrap();
    let r = run(&f);
    let log = r.log();
    assert_eq!(r.outcome, Outcome::Success, "{log}");
    assert!(log.contains("got Hello, Baste! and yes"), "{log}");
    assert!(log.contains("action path is /"), "{log}");
    assert!(log.contains("app/app/.github/actions/greet"), "{log}");
}

fn write_action(bundle: &Path, rel: &str, files: &[(&str, &str)]) {
    let dir = bundle.join(rel);
    std::fs::create_dir_all(&dir).unwrap();
    for (name, content) in files {
        std::fs::write(dir.join(name), content).unwrap();
    }
}

#[test]
fn node_action_with_state_and_post() {
    if baste_agent::process::which("node", &std::env::var("PATH").unwrap_or_default()).is_none() {
        eprintln!("node not installed; skipping");
        return;
    }
    let mut f = fixture(
        r#"
- id: hi
  uses: acme/hello@v1
  with:
    who-to-greet: World
- run: echo "time=${{ steps.hi.outputs.time }}"
"#,
    );
    write_action(
        &f.bundle,
        "actions/acme/hello/v1",
        &[
            (
                "action.yml",
                "name: hello\ninputs:\n  who-to-greet:\n    required: true\n  unused:\n    default: dflt-${{ github.repository }}\nruns:\n  using: node20\n  main: main.js\n  post: post.js\n",
            ),
            (
                "main.js",
                r#"const fs = require('fs');
console.log(`Hello ${process.env['INPUT_WHO-TO-GREET']} (${process.env.INPUT_UNUSED})`);
fs.appendFileSync(process.env.GITHUB_OUTPUT, 'time=noon\n');
fs.appendFileSync(process.env.GITHUB_STATE, 'saved=from-main\n');
"#,
            ),
            ("post.js", "console.log(`post sees ${process.env.STATE_saved}`);\n"),
        ],
    );
    f.spec.actions.push(ActionSource {
        repo_ref: "acme/hello@v1".into(),
        path: "actions/acme/hello/v1".into(),
    });
    let r = run(&f);
    let log = r.log();
    assert_eq!(r.outcome, Outcome::Success, "{log}");
    assert!(log.contains("Hello World (dflt-acme/app)"), "{log}");
    assert!(log.contains("time=noon"));
    assert!(log.contains("post sees from-main"));
    assert_eq!(r.steps().last().unwrap().0, "Post Run acme/hello@v1");
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn checkout_shim_uses_local_pack() {
    let mut f = fixture(
        r#"
- uses: actions/checkout@v4
- run: |
    cat hello.txt
    git log --oneline | wc -l | tr -d ' '
    git rev-parse HEAD
    git rev-parse --abbrev-ref HEAD
    git config --get remote.origin.url
- uses: actions/checkout@v4
  with:
    path: sub
"#,
    );
    let src = f.bundle.parent().unwrap().join("src");
    std::fs::create_dir_all(&src).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    std::fs::write(src.join("hello.txt"), "one\n").unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "-qm", "first"]);
    std::fs::write(src.join("hello.txt"), "from the pushed commit\n").unwrap();
    git(&src, &["commit", "-qam", "second"]);
    let sha = git(&src, &["rev-parse", "HEAD"]);
    let objects = Command::new("git")
        .arg("-C")
        .arg(&src)
        .args(["rev-list", "--objects", "--no-walk", &sha])
        .output()
        .unwrap()
        .stdout;
    std::fs::create_dir_all(f.bundle.join("checkout")).unwrap();
    let pack = std::fs::File::create(f.bundle.join("checkout/depth-1.pack")).unwrap();
    let mut child = Command::new("git")
        .arg("-C")
        .arg(&src)
        .args(["pack-objects", "--stdout", "-q"])
        .stdin(std::process::Stdio::piped())
        .stdout(pack)
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(&objects).unwrap();
    assert!(child.wait().unwrap().success());
    f.spec.checkout.sha = sha.clone();
    f.spec.checkout.packs = vec![CheckoutPack {
        depth: 1,
        file: "checkout/depth-1.pack".into(),
        shallow: vec![sha.clone()],
    }];
    // An uncommitted edit in the source must not leak into the job.
    std::fs::write(src.join("hello.txt"), "UNCOMMITTED\n").unwrap();
    let r = run(&f);
    let log = r.log();
    assert_eq!(r.outcome, Outcome::Success, "{log}");
    assert!(log.contains("from the pushed commit"), "{log}");
    assert!(!log.contains("UNCOMMITTED"));
    assert!(
        log.lines().any(|l| l.trim() == "1"),
        "shallow history of depth 1: {log}"
    );
    assert!(log.contains(&sha));
    assert!(log.contains("https://github.com/acme/app"));
    assert!(log.lines().any(|l| l.trim() == "main"));
    assert!(f.work.join("app/app/sub/hello.txt").exists());
}

#[test]
fn artifacts_round_trip() {
    let f = fixture(
        r#"
- run: |
    mkdir -p dist/js
    echo a > dist/a.txt
    echo b > dist/js/b.js
- uses: actions/upload-artifact@v4
  with:
    name: build-output
    path: dist
- uses: actions/upload-artifact@v4
  with:
    name: nothing
    path: nope/**
    if-no-files-found: error
"#,
    );
    let r = run(&f);
    assert_eq!(r.outcome, Outcome::Failure);
    let mut tar = Vec::new();
    for e in &r.events {
        if let Event::ArtifactChunk { name, data } = e {
            assert_eq!(name, "build-output");
            use base64::Engine;
            tar.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap(),
            );
        }
    }
    assert!(r
        .events
        .iter()
        .any(|e| matches!(e, Event::ArtifactEnd { files: 2, .. })));
    assert!(r
        .log()
        .contains("No files were found with the provided path: nope/**"));

    // Feed the artifact to a second job.
    let mut g = fixture(
        r#"
- uses: actions/download-artifact@v4
  with:
    name: build-output
    path: out
- run: cat out/a.txt out/js/b.js
- uses: actions/download-artifact@v4
- run: ls build-output
"#,
    );
    std::fs::create_dir_all(g.bundle.join("artifacts")).unwrap();
    std::fs::write(g.bundle.join("artifacts/build-output.tar"), &tar).unwrap();
    g.spec.artifacts.push(ArtifactSource {
        name: "build-output".into(),
        file: "artifacts/build-output.tar".into(),
    });
    let r = run(&g);
    let log = r.log();
    assert_eq!(r.outcome, Outcome::Success, "{log}");
    assert!(log.contains("a\nb"), "{log}");
    assert!(log.contains("a.txt"));
}

#[test]
fn hash_files_and_expressions_in_names() {
    let f = fixture(
        r#"
- run: echo '{}' > package-lock.json
- name: key ${{ hashFiles('**/package-lock.json') != '' }}
  run: echo "key=${{ runner.os }}-${{ hashFiles('**/package-lock.json') }}"
"#,
    );
    let r = run(&f);
    assert_eq!(r.outcome, Outcome::Success, "{}", r.log());
    assert!(r.steps().iter().any(|s| s.0 == "key true"));
    let log = r.log();
    let key = log.lines().find(|l| l.starts_with("key=Linux-")).unwrap();
    assert_eq!(key.len(), "key=Linux-".len() + 64);
}

#[test]
fn job_outputs_skip_secrets() {
    let mut f = fixture(
        r#"
- id: s
  run: |
    echo "safe=ok" >> "$GITHUB_OUTPUT"
    echo "leak=${{ secrets.API_KEY }}" >> "$GITHUB_OUTPUT"
"#,
    );
    f.spec
        .outputs
        .insert("safe".into(), json!("${{ steps.s.outputs.safe }}"));
    f.spec
        .outputs
        .insert("leak".into(), json!("${{ steps.s.outputs.leak }}"));
    let r = run(&f);
    let out = r.outputs();
    assert_eq!(out.get("safe"), Some(&json!("ok")));
    assert!(!out.contains_key("leak"));
}

#[test]
fn cancellation_stops_the_job() {
    let f = fixture(
        r#"
- run: sleep 30
- run: echo SHOULD-NOT-RUN
- if: always()
  run: echo cleanup-ran
"#,
    );
    let sink = VecSink::default();
    let cancel = Arc::new(AtomicBool::new(false));
    let opts = AgentOptions {
        bundle: f.bundle.clone(),
        cancel: cancel.clone(),
    };
    let c = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(800));
        c.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let start = std::time::Instant::now();
    let outcome = run_job(&f.spec, &opts, &sink);
    assert!(start.elapsed().as_secs() < 15);
    assert_eq!(outcome, Outcome::Cancelled);
    let r = Result {
        outcome,
        events: sink.events.lock().unwrap().clone(),
    };
    let log = r.log();
    assert!(!log.contains("SHOULD-NOT-RUN"), "{log}");
    assert!(log.contains("cleanup-ran"), "{log}");
}

#[test]
fn steps_know_they_run_locally() {
    let f = fixture(
        r#"
- run: echo "process=$BASTE"
- if: env.BASTE == 'true'
  run: echo "local-only step ran"
- if: env.BASTE != 'true'
  run: echo "github-only step ran"
"#,
    );
    let r = run(&f);
    let log = r.log();
    assert_eq!(r.outcome, Outcome::Success, "{log}");
    assert!(log.contains("process=true"), "{log}");
    assert!(log.contains("local-only step ran"), "{log}");
    assert!(!log.contains("github-only step ran"), "{log}");
}
