use crate::ExitCode;
use std::{fmt, sync::Arc, time::Duration};

pub const JOBS_OFF: &str = "jobs are off in this chat";

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
