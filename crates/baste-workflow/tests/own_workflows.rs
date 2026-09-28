//! Baste's own workflows must parse and classify as expected.

use baste_workflow::{expand_job, Placement, Workflow};
use serde_json::json;
use std::path::Path;

fn load(rel: &str) -> Workflow {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    let src = std::fs::read_to_string(&path).unwrap();
    Workflow::parse(rel, &src).unwrap_or_else(|e| panic!("{e}"))
}

fn placements(wf: &Workflow) -> Vec<(String, bool)> {
    let ctx = json!({"github": {"event_name": "push"}, "needs": {}, "vars": {}, "inputs": {}});
    wf.jobs
        .iter()
        .flat_map(|j| expand_job(wf, j, ctx.as_object().unwrap()).unwrap())
        .map(|i| (i.name, i.placement == Placement::Local))
        .collect()
}

#[test]
fn ci_workflow() {
    let wf = load(".github/workflows/ci.yml");
    assert!(wf.on.matches_push("refs/heads/any", None).unwrap());
    let p = placements(&wf);
    assert!(p.contains(&("Test (Linux)".into(), true)), "{p:?}");
    assert!(p.contains(&("Test (macOS)".into(), false)), "{p:?}");
    assert!(p.contains(&("Website".into(), true)), "{p:?}");
}

#[test]
fn release_and_pages_workflows() {
    let wf = load(".github/workflows/release.yml");
    assert!(wf.on.matches_push("refs/tags/v0.1.0", None).unwrap());
    assert!(wf.on.matches_push("refs/tags/v0.2.0-rc.1", None).unwrap());
    // The gate action's major tag isn't a release.
    assert!(!wf.on.matches_push("refs/tags/v1", None).unwrap());
    assert!(!wf.on.matches_push("refs/heads/main", None).unwrap());
    let p = placements(&wf);
    assert_eq!(p.iter().filter(|(_, local)| *local).count(), 3, "{p:?}");
    // Publishing to npm is only called or dispatched, never started by a push.
    let npm = load(".github/workflows/npm.yml");
    assert!(!npm.on.matches_push("refs/heads/main", None).unwrap());
    assert!(!npm.on.matches_push("refs/tags/v0.1.0", None).unwrap());
    let pages = placements(&load(".github/workflows/pages.yml"));
    // The site builds locally; the deployment uses an environment, so it
    // stays on GitHub.
    assert_eq!(
        pages,
        vec![("build".to_string(), true), ("deploy".to_string(), false)]
    );
}

#[test]
fn vm_e2e_workflow() {
    let wf = load("tests/vm/workflow.yml");
    let p = placements(&wf);
    assert_eq!(
        p,
        vec![
            ("build".to_string(), true),
            ("test".to_string(), true),
            ("windows".to_string(), false)
        ]
    );
}

#[test]
fn sandbox_workflows() {
    let ci = load("tests/sandbox/ci.yml");
    assert_eq!(
        placements(&ci),
        vec![
            ("build".to_string(), true),
            ("test (1)".to_string(), true),
            ("test (2)".to_string(), true),
            ("maybe-fail".to_string(), true),
            ("uses-a-secret".to_string(), true),
            ("windows".to_string(), false)
        ]
    );
    let pr = load("tests/sandbox/pr.yml");
    assert!(!pr.on.matches_push("refs/heads/main", None).unwrap());
}

#[test]
fn gate_action_metadata_parses() {
    // GitHub refuses to load an action whose action.yml isn't valid YAML.
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../gate/action.yml");
    let meta = baste_workflow::ActionMeta::parse(&std::fs::read_to_string(path).unwrap())
        .unwrap_or_else(|e| panic!("gate/action.yml: {e}"));
    assert!(matches!(meta.runs, baste_workflow::Runs::Composite { .. }));
}
