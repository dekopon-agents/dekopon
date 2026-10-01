use std::{fmt, sync::Arc, time::Duration};

use thiserror::Error;

use crate::{ExitCode, TreeContext, ast::Statement, interp::JobScope};

pub const JOBS_OFF: &str = "jobs are off in this chat: its route sets no limits.jobTimeoutMs";

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct JobId(u64);

impl JobId {
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobOutcome {
    Succeeded,
    Failed,
    Cancelled,
    Deadline,
    Killed,
}

impl JobOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Deadline => "deadline",
            Self::Killed => "killed",
        }
    }
}

impl fmt::Display for JobOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobState {
    Running {
        elapsed: Duration,
    },
    Finished {
        outcome: JobOutcome,
        exit: ExitCode,
        after: Duration,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobSummary {
    pub id: JobId,
    pub state: JobState,
    pub text: Arc<str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobWait {
    Exited(ExitCode),
    Interrupted,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum JobRefusal {
    #[error("{maximum} jobs are already running, the most this gateway allows (sessions.maxJobs)")]
    Full { maximum: usize },
    #[error("no job of this person has that id")]
    NotYours,
    #[error("no such job")]
    Finished,
}

/// Every call answers for the person and conversation the control was built for; an id another
/// person started is `NotYours`, never a different answer that would confirm it exists.
pub trait JobControl: Send + Sync {
    fn start(&self, seed: JobSeed) -> Result<JobId, JobRefusal>;

    fn list(&self) -> Vec<JobSummary>;

    /// `keep_waiting` is asked at least once a second; the call returns once it answers false.
    fn wait(&self, id: JobId, keep_waiting: &dyn Fn() -> bool) -> Result<JobWait, JobRefusal>;

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

impl fmt::Debug for JobSeed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JobSeed")
            .field("text", &self.text)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ScriptJobs {
    pub(crate) started: Vec<JobId>,
    pub(crate) last: Option<JobId>,
}
