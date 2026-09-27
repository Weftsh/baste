//! End-to-end tests of the P0 flows: real git pushes, the real binary, the
//! host backend, and a fake GitHub API.

mod common;

use common::{stderr, stdout, TestEnv};
use serde_json::json;
use std::time::{Duration, Instant};

const CI: &str = r#"
name: CI
on:
  push:
    branches: [main]
env:
  GLOBAL: from-workflow
jobs:
  build:
    runs-on: ubuntu-latest
    outputs:
      version: ${{ steps.v.outputs.version }}
    steps:
      - uses: actions/checkout@v4
      - id: v
        name: Check the pushed commit
        run: |
          test "$(cat app.txt)" = "committed"
          echo "vars: ${{ vars.GREETING }}, env: $GLOBAL"
          echo "version=1.2.3" >> "$GITHUB_OUTPUT"
          sleep 3
      - uses: actions/upload-artifact@v4
        with:
          name: dist
          path: app.txt
  test:
    needs: build
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: [ubuntu-latest, windows-latest]
    steps:
      - uses: actions/checkout@v4
      - uses: actions/download-artifact@v4
        with:
          name: dist
          path: out
      - run: |
          echo "testing ${{ needs.build.outputs.version }} on ${{ matrix.os }}"
          test "$(cat out/app.txt)" = committed
  lint:
    runs-on: macos-latest
    steps:
      - run: echo lint
"#;

#[test]
fn push_runs_ci_locally_and_reports_statuses() {
    let env = TestEnv::new();
    env.github.state.lock().unwrap().workflow_runs = json!({"workflow_runs": [{
        "id": 99, "head_branch": "main", "head_sha": "0".repeat(40), "conclusion": "success",
        "created_at": "2026-09-01T10:00:00Z", "run_started_at": "2026-09-01T10:03:00Z",
        "updated_at": "2026-09-01T10:10:00Z", "event": "push"
    }]});
    env.write("app.txt", "committed\n");
    env.write(".github/workflows/ci.yml", CI);
    let sha = env.commit("first");

    // init checks everything and installs the hook.
    let out = env.baste(&["init", "--backend", "host"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("Installed the pre-push hook"), "{text}");
    assert!(text.contains("baste/CI/build"), "{text}");
    assert!(text.contains("baste/CI/test (ubuntu-latest)"), "{text}");
    assert!(text.contains("windows-latest jobs run on GitHub"), "{text}");

    // An uncommitted edit must not leak into the run.
    env.write("app.txt", "uncommitted\n");

    let started = Instant::now();
    let push = env
        .command("git")
        .args(["push", "-q", "origin", "main"])
        .output()
        .unwrap();
    let push_time = started.elapsed();
    assert!(push.status.success(), "{}", stderr(&push));
    assert!(
        push_time < Duration::from_secs(5),
        "push took {push_time:?}"
    );
    assert!(
        stderr(&push).contains("running CI for"),
        "{}",
        stderr(&push)
    );

    // A pending status appears within seconds.
    loop {
        if env.github.latest(&sha).contains_key("baste/CI/build") {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "no pending status yet"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let first = env
        .github
        .statuses()
        .into_iter()
        .find(|s| s.context == "baste/CI/build")
        .unwrap();
    assert_eq!(first.state, "pending");

    let runs = env.runs();
    let id = runs[0]["id"].as_str().unwrap().to_string();
    let run = env.wait(&id, Duration::from_secs(120));
    assert_eq!(run["state"], "passed", "{run:#}");
    assert_eq!(run["sha"], sha.as_str());
    assert_eq!(env.job(&run, "build")["state"], "passed");
    assert_eq!(env.job(&run, "test (ubuntu-latest)")["state"], "passed");
    assert_eq!(
        env.job(&run, "test (windows-latest)")["state"],
        "handed_to_github"
    );
    assert_eq!(env.job(&run, "lint")["state"], "handed_to_github");
    assert_eq!(run["provenance"]["executor"], "local");
    assert_eq!(run["provenance"]["policy"], "pusher-runs-push");

    // Final statuses: one per local job, pass/fail, duration and run id.
    let latest = env.github.latest(&sha);
    let build = &latest["baste/CI/build"];
    assert_eq!(build.state, "success");
    assert!(
        build.description.starts_with("Passed in "),
        "{}",
        build.description
    );
    assert!(build.description.contains(&id), "{}", build.description);
    assert!(build.target_url.contains(&id));
    assert_eq!(latest["baste/CI/test (ubuntu-latest)"].state, "success");
    assert!(
        !latest
            .keys()
            .any(|k| k.contains("windows") || k.contains("lint")),
        "{:?}",
        latest.keys()
    );

    // Logs per job and step.
    let logs = env.baste(&["logs", &id, "build"]);
    let text = stdout(&logs);
    assert!(
        text.contains("vars: hello from vars, env: from-workflow"),
        "{text}"
    );
    assert!(text.contains("Check the pushed commit"));
    let logs = stdout(&env.baste(&["logs", &id]));
    assert!(logs.contains("testing 1.2.3 on ubuntu-latest"), "{logs}");
    assert!(
        !logs.contains("REAL ACTION"),
        "shimmed actions must not run: {logs}"
    );
    assert!(logs.contains("handed to GitHub"));

    // status --commit finds the run; insights compare with GitHub.
    let st = stdout(&env.baste(&["status", "--commit", &sha[..8]]));
    assert!(st.contains(&id), "{st}");
    let ins = &run["insights"];
    assert!(ins["time_saved_ms"].as_i64().unwrap() > 0, "{ins:#}");
    let text = stdout(&env.baste(&["insights"]));
    assert!(text.contains("Time saved"), "{text}");
    assert!(text.contains("Slowest steps"), "{text}");
}

#[test]
fn init_fails_without_gh_and_changes_nothing() {
    let env = TestEnv::new();
    env.write(".github/workflows/ci.yml", CI);
    env.commit("first");
    let out = env
        .command(env!("CARGO_BIN_EXE_baste"))
        .args(["init", "--backend", "host"])
        .env("BASTE_GH", "/nonexistent/gh")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let text = stdout(&out);
    assert!(text.contains("GitHub CLI (gh) is not installed"), "{text}");
    assert!(text.contains("Nothing was changed"), "{text}");
    assert!(!env.repo.join(".git/hooks/pre-push").exists());
}

#[test]
fn init_names_missing_status_permission() {
    let env = TestEnv::new();
    env.github.state.lock().unwrap().push_permission = false;
    env.write(".github/workflows/ci.yml", CI);
    env.commit("first");
    let out = env.baste(&["init", "--backend", "host"]);
    assert!(!out.status.success());
    let text = stdout(&out);
    assert!(text.contains("missing permission"), "{text}");
    assert!(!env.repo.join(".git/hooks/pre-push").exists());
}

const SECRET_WF: &str = r#"
name: Deploy check
on: push
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - name: Use the token
        run: |
          test -n "$TOKEN"
          echo "token is $TOKEN"
        env:
          TOKEN: ${{ secrets.NPM_TOKEN }}
"#;

#[test]
fn missing_secret_fails_clearly_then_rerun_passes() {
    let env = TestEnv::new();
    env.write(".github/workflows/check.yml", SECRET_WF);
    let sha = env.commit("first");

    let out = env.baste(&["run"]);
    assert!(!out.status.success(), "{}", stdout(&out));
    let text = stdout(&out);
    assert!(
        text.contains("Secret NPM_TOKEN is not set locally"),
        "{text}"
    );
    let id = env.runs()[0]["id"].as_str().unwrap().to_string();
    let run = env.wait(&id, Duration::from_secs(60));
    assert_eq!(run["state"], "failed");
    let st = &env.github.latest(&sha)["baste/Deploy check/check"];
    assert_eq!(st.state, "failure");

    let out = env.baste_stdin(&["secrets", "set", "NPM_TOKEN"], "s3cr3t-token-value\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let list = stdout(&env.baste(&["secrets", "list"]));
    assert!(list.contains("NPM_TOKEN"));
    assert!(!list.contains("s3cr3t"));

    let out = env.baste(&["rerun", &id]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}{}", stderr(&out));
    assert!(text.contains("token is ***"), "{text}");
    assert!(!text.contains("s3cr3t-token-value"));
    let runs = env.runs();
    let rerun = &runs[0];
    assert_eq!(rerun["trigger"]["kind"], "rerun");
    assert_eq!(rerun["trigger"]["of"], id.as_str());
    let st = &env.github.latest(&sha)["baste/Deploy check/check"];
    assert_eq!(st.state, "success");
    assert!(st.description.contains(rerun["id"].as_str().unwrap()));
}

#[test]
fn statuses_wait_for_the_push_to_land() {
    let env = TestEnv::new();
    env.github.state.lock().unwrap().missing_commit_posts = 4;
    env.write(
        ".github/workflows/ci.yml",
        "on: push\njobs:\n  quick:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo quick\n",
    );
    let sha = env.commit("first");
    assert!(env.baste(&["init", "--backend", "host"]).status.success());
    let push = env
        .command("git")
        .args(["push", "-q", "origin", "main"])
        .output()
        .unwrap();
    assert!(push.status.success());
    let id = env.runs()[0]["id"].as_str().unwrap().to_string();
    env.wait(&id, Duration::from_secs(90));
    let latest = env.github.latest(&sha);
    assert_eq!(
        latest["baste/.github/workflows/ci.yml/quick"].state, "success",
        "{latest:?}"
    );
}

#[test]
fn failing_step_is_reported_with_exit_code() {
    let env = TestEnv::new();
    env.write(
        ".github/workflows/ci.yml",
        r#"
name: CI
on: push
jobs:
  unit:
    runs-on: ubuntu-latest
    steps:
      - run: echo setup
      - name: Run tests
        run: |
          echo "1 test failed: expected 2, got 3"
          exit 3
      - name: Never
        run: echo never
"#,
    );
    let sha = env.commit("first");
    let out = env.baste(&["run", "--detach"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let id = env.runs()[0]["id"].as_str().unwrap().to_string();
    let run = env.wait(&id, Duration::from_secs(60));
    assert_eq!(run["state"], "failed");
    let st = &env.github.latest(&sha)["baste/CI/unit"];
    assert_eq!(st.state, "failure");
    assert!(
        st.description.contains("Failed at 'Run tests'"),
        "{}",
        st.description
    );
    assert!(st.description.contains(&id));

    // The run id from the status description gets you the failing output.
    let out = env.baste(&["logs", &id, "--failed"]);
    assert!(!out.status.success());
    let text = stdout(&out);
    assert!(text.contains("1 test failed: expected 2, got 3"), "{text}");
    assert!(text.contains("exit code 3"), "{text}");
    assert!(!text.contains("never"));
}

#[test]
fn javascript_action_is_downloaded_and_cached() {
    let env = TestEnv::new();
    env.github.state.lock().unwrap().actions.insert(
        "acme/hello".into(),
        vec![
            (
                "action.yml".into(),
                "name: hello\ninputs:\n  who:\n    required: true\noutputs:\n  greeting: {}\nruns:\n  using: node20\n  main: dist/index.js\n".into(),
            ),
            (
                "dist/index.js".into(),
                "const fs=require('fs');console.log('Hello '+process.env.INPUT_WHO);fs.appendFileSync(process.env.GITHUB_OUTPUT,'greeting=hi\\n');\n".into(),
            ),
        ],
    );
    env.write(
        ".github/workflows/ci.yml",
        r#"
on: push
jobs:
  greet:
    runs-on: ubuntu-latest
    steps:
      - id: hello
        uses: acme/hello@v1
        with:
          who: world
      - run: echo "output=${{ steps.hello.outputs.greeting }}"
"#,
    );
    env.commit("first");
    if baste_agent_node_missing() {
        return;
    }
    let out = env.baste(&["run", "--no-status"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}{}", stderr(&out));
    assert!(text.contains("Hello world"), "{text}");
    assert!(text.contains("output=hi"), "{text}");
    assert!(env.root().join("cache/actions/acme/hello").is_dir());
    assert!(
        env.github.statuses().is_empty(),
        "--no-status posts nothing"
    );
}

fn baste_agent_node_missing() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
}

#[test]
fn pull_request_runs_on_the_test_merge() {
    let env = TestEnv::new();
    env.write("base.txt", "base\n");
    env.write(
        ".github/workflows/pr.yml",
        r#"
name: PR
on:
  pull_request:
    branches: [main]
jobs:
  merge:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: |
          echo "event=$GITHUB_EVENT_NAME ref=$GITHUB_REF base=$GITHUB_BASE_REF head=$GITHUB_HEAD_REF"
          echo "subject=$(git log -1 --format=%s)"
          cat base.txt feature.txt main-later.txt
"#,
    );
    let base = env.commit("base");
    assert!(env.baste(&["init", "--backend", "host"]).status.success());
    env.git(&["checkout", "-q", "-b", "feature"]);
    env.write("feature.txt", "feature\n");
    let head = env.commit("feature work");
    env.git(&["checkout", "-q", "main"]);
    env.write("main-later.txt", "main moved on\n");
    env.commit("main moves");
    let push = env
        .command("git")
        .args(["push", "-q", "origin", "main"])
        .output()
        .unwrap();
    assert!(push.status.success());
    // The push of main triggers nothing (no push workflows); wait for it.
    let first = env.runs()[0]["id"].as_str().unwrap().to_string();
    env.wait(&first, Duration::from_secs(60));

    env.github.state.lock().unwrap().open_pr = Some(json!({
        "number": 7,
        "head": {"ref": "feature", "sha": head},
        "base": {"ref": "main", "sha": base},
        "title": "Feature"
    }));
    env.git(&["checkout", "-q", "feature"]);
    let push = env
        .command("git")
        .args(["push", "-q", "origin", "feature"])
        .output()
        .unwrap();
    assert!(push.status.success(), "{}", stderr(&push));
    let id = env
        .runs()
        .iter()
        .find(|r| r["git_ref"] == "refs/heads/feature")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let run = env.wait(&id, Duration::from_secs(90));
    assert_eq!(run["state"], "passed", "{run:#}");
    assert_eq!(run["pull_request"]["number"], 7);
    let text = stdout(&env.baste(&["logs", &id]));
    assert!(
        text.contains("event=pull_request ref=refs/pull/7/merge base=main head=feature"),
        "{text}"
    );
    assert!(
        text.contains(&format!("subject=Merge {head} into")),
        "{text}"
    );
    assert!(
        text.contains("main moved on"),
        "the merge includes the latest base: {text}"
    );
    // The status goes on the pushed head commit.
    assert_eq!(env.github.latest(&head)["baste/PR/merge"].state, "success");
}

#[test]
fn needs_if_and_fail_fast() {
    let env = TestEnv::new();
    env.write(
        ".github/workflows/ci.yml",
        r#"
name: CI
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    steps:
      - run: exit 1
  after-a:
    needs: a
    runs-on: ubuntu-latest
    steps:
      - run: echo SHOULD-NOT-RUN
  cleanup:
    needs: a
    if: always()
    runs-on: ubuntu-latest
    steps:
      - run: echo "cleanup sees ${{ needs.a.result }}"
  skipped:
    if: github.event_name == 'pull_request'
    runs-on: ubuntu-latest
    steps:
      - run: echo nope
  matrix:
    runs-on: ubuntu-latest
    strategy:
      max-parallel: 1
      matrix:
        n: [1, 2, 3]
    steps:
      - run: |
          if [ ${{ matrix.n }} = 1 ]; then exit 1; fi
          echo "n=${{ matrix.n }}"
"#,
    );
    let sha = env.commit("first");
    let out = env.baste(&["run"]);
    let text = stdout(&out);
    assert!(!out.status.success());
    assert!(!text.contains("SHOULD-NOT-RUN"), "{text}");
    assert!(text.contains("cleanup sees failure"), "{text}");
    let id = env.runs()[0]["id"].as_str().unwrap().to_string();
    let run = env.wait(&id, Duration::from_secs(60));
    assert_eq!(env.job(&run, "after-a")["state"], "not_run");
    assert_eq!(env.job(&run, "cleanup")["state"], "passed");
    assert_eq!(env.job(&run, "skipped")["state"], "skipped");
    assert_eq!(env.job(&run, "matrix (1)")["state"], "failed");
    // fail-fast cancels the rest of the matrix.
    assert_eq!(env.job(&run, "matrix (2)")["state"], "cancelled");
    let latest = env.github.latest(&sha);
    assert_eq!(latest["baste/CI/after-a"].state, "failure");
    assert!(latest["baste/CI/after-a"]
        .description
        .contains("needs 'a' failed"));
    assert_eq!(latest["baste/CI/skipped"].state, "success");
    assert_eq!(latest["baste/CI/matrix (2)"].state, "error");
}

#[test]
fn uninstall_restores_hook() {
    let env = TestEnv::new();
    env.write(".github/workflows/ci.yml", CI);
    env.commit("first");
    let hook = env.repo.join(".git/hooks/pre-push");
    std::fs::write(&hook, "#!/bin/sh\necho mine\n").unwrap();
    assert!(env.baste(&["init", "--backend", "host"]).status.success());
    assert!(std::fs::read_to_string(&hook)
        .unwrap()
        .contains("baste pre-push hook"));
    assert!(env.baste(&["uninstall"]).status.success());
    assert_eq!(
        std::fs::read_to_string(&hook).unwrap(),
        "#!/bin/sh\necho mine\n"
    );
}
