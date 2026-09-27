//! Placeholder, replaced below.
use super::{Backend, Check, JobLaunch};
use crate::config::Config;
use anyhow::{bail, Result};
use baste_protocol::{Event, RunnerInfo};
use std::path::Path;

pub struct Tart;

impl Tart {
    pub fn new(_config: &Config) -> Self {
        Tart
    }
}

impl Backend for Tart {
    fn name(&self) -> &'static str {
        "tart"
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
