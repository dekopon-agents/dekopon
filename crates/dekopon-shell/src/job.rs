use std::sync::Arc;

use thiserror::Error;

use crate::{ast::Statement, interp::JobScope};
use dekopon_process::{JOBS_OFF, JobId, JobSummary, JobWait, TreeContext};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum JobRefusal {
    #[error("{maximum} jobs are already running, the most this gateway allows (sessions.maxJobs)")]
    Full { maximum: usize },
    #[error("no job of this person has that id")]
    NotYours,
    #[error("no such job")]
    Finished,
    #[error("{}", JOBS_OFF)]
    Off,
}

/// Every call answers for the person and conversation the control was built for; an id another
/// person started is `NotYours`, never a different answer that would confirm it exists.
pub trait JobControl: Send + Sync {
    fn start(&self, seed: JobSeed) -> Result<JobId, JobRefusal>;

    fn list(&self) -> Vec<JobSummary>;

    /// Every owned id is held before any is awaited, so none sends a notice or loses its row while
    /// another is still awaited. `keep_waiting` is asked at least once a second; the call returns
    /// once it answers false. One answer per id, in order.
    fn wait(
        &self,
        ids: &[JobId],
        keep_waiting: &dyn Fn() -> bool,
    ) -> Vec<Result<JobWait, JobRefusal>>;

    fn kill(&self, id: JobId) -> Result<(), JobRefusal>;
}

pub struct JobSeed {
    pub(crate) statement: Arc<Statement>,
    pub(crate) text: Arc<str>,
    pub(crate) scope: JobScope,
    pub(crate) tree: TreeContext,
}

impl JobSeed {
    #[must_use]
    pub fn text(&self) -> &Arc<str> {
        &self.text
    }

    #[must_use]
    pub const fn tree(&self) -> &TreeContext {
        &self.tree
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ScriptJobs {
    pub(crate) started: Vec<JobId>,
    pub(crate) last: Option<JobId>,
}
