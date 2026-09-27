//! Where a job runs, as a replaceable policy.
//!
//! Phase 1 has one rule: the pusher's machine runs the push. Later phases add
//! a router that can send jobs to cloud runners (e.g. "main requires cloud").
//! Keeping the decision here, and recording it in each run's provenance,
//! means that is a policy change rather than a rewrite.

use crate::store::JobRecord;

/// Name of the active policy, recorded in run provenance.
pub const POLICY: &str = "pusher-runs-push";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// This machine, in a local VM.
    Local,
}

/// Decide where a job that can run outside GitHub goes.
pub fn route(_job: &JobRecord) -> Route {
    Route::Local
}
