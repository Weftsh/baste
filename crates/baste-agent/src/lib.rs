//! The Baste agent: executes one job from a [`baste_protocol::JobSpec`] and
//! reports [`baste_protocol::Event`]s. It runs inside the job's VM (or, for
//! development, directly on the host).

mod actions;
mod cache;
pub mod commands;
mod hashfiles;
mod job;
pub mod mask;
pub mod process;
mod shims;
pub mod sink;

pub use hashfiles::hash_files;
pub use job::{run_job, AgentOptions};
pub use sink::{ChannelSink, EventSink, JsonLinesSink, VecSink};

/// Read a job spec from a bundle directory and run it.
pub fn run_bundle(
    bundle: &std::path::Path,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    sink: &dyn EventSink,
) -> anyhow::Result<baste_protocol::Outcome> {
    let path = bundle.join(baste_protocol::JOB_SPEC_FILE);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    let spec: baste_protocol::JobSpec = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
    let opts = AgentOptions {
        bundle: bundle.to_path_buf(),
        cancel,
    };
    Ok(run_job(&spec, &opts, sink))
}
