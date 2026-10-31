use std::sync::Arc;

use thiserror::Error;

use crate::{ast::Statement, interp::JobScope};
use dekopon_process::{JOBS_OFF, JobId, JobSummary, JobWait, LimitExceeded, TreeContext};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum JobRefusal {
    #[error("shell thread admission refused: {0:?}")]
    Limit(LimitExceeded),
    #[error("{maximum} jobs are already running, the most this gateway allows (sessions.maxJobs)")]
    Full { maximum: usize },
    #[error("no job of this person has that id")]
    NotYours,
    #[error("no such job")]
    Finished,
    #[error("{}", JOBS_OFF)]
    Off,
}

pub trait JobControl: Send + Sync {
    fn start(&self, seed: JobSeed) -> Result<JobId, JobRefusal>;

    fn list(&self) -> Vec<JobSummary>;

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
