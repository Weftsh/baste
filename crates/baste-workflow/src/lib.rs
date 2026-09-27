//! GitHub Actions workflow model and job planning for Baste.

pub mod action;
pub mod filter;
pub mod matrix;
pub mod model;
pub mod plan;
pub mod trigger;
pub mod yaml;

pub use action::{ActionInput, ActionMeta, Runs};
pub use model::{scalar_string, Job, RunDefaults, Step, Uses, Workflow, WorkflowError};
pub use plan::{
    classify_labels, default_name, depends_on_needs, evaluate_tree, expand_job, runs_on_labels,
    unsupported_reason, JobInstance, Placement,
};
pub use trigger::{RefFilter, Triggers};
