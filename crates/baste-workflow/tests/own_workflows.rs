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
}

#[test]
fn release_and_pages_workflows() {
    let wf = load(".github/workflows/release.yml");
    assert!(wf.on.matches_push("refs/tags/v0.1.0", None).unwrap());
    assert!(!wf.on.matches_push("refs/heads/main", None).unwrap());
    let p = placements(&wf);
    assert_eq!(p.iter().filter(|(_, local)| *local).count(), 3, "{p:?}");
    let pages = load(".github/workflows/pages.yml");
    // Deploys to an environment, so it stays on GitHub.
    assert!(placements(&pages).iter().all(|(_, local)| !local));
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
