//! Placeholder, replaced below.
use super::{Backend, Check, JobLaunch};
use crate::config::Config;
use anyhow::{bail, Result};
use baste_protocol::{Event, RunnerInfo};
use std::path::Path;

pub struct Firecracker;

impl Firecracker {
    pub fn new(_config: &Config) -> Self {
        Firecracker
    }
}

impl Backend for Firecracker {
    fn name(&self) -> &'static str {
        "firecracker"
    }
    fn doctor(&self) -> Vec<Check> {
        vec![]
    }
    fn runner(&self, _job_dir: &Path) -> RunnerInfo {
        unimplemented!()
    }
    fn image(&self) -> Option<String> {
        None
    }
    fn prepare(&self, _log: &mut dyn FnMut(&str)) -> Result<()> {
        bail!("not implemented")
    }
    fn run(&self, _l: &JobLaunch, _e: &mut dyn FnMut(Event), _log: &mut dyn FnMut(&str)) -> Result<()> {
        bail!("not implemented")
    }
}
