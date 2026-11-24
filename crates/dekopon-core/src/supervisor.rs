use std::{error::Error, future::Future, time::Duration};

use thiserror::Error;
use tokio::{sync::watch, task::JoinSet};

use crate::{Backoff, TaskOutcome};

const MAX_CHILDREN: usize = 16;

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("the supervisor already holds its maximum number of children")]
    TooManyChildren,
}

impl SupervisorError {
    #[must_use]
    pub const fn category(&self) -> &'static str {
        match self {
            Self::TooManyChildren => "too-many-children",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ChildName(&'static str);

impl ChildName {
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

#[derive(Clone, Debug)]
pub enum ChildPolicy {
    Fatal,
    Restart {
        backoff: Backoff,
        healthy_after: Duration,
    },
}

#[derive(Debug)]
pub struct Shutdown(watch::Receiver<bool>);

impl Shutdown {
    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    pub async fn requested(&mut self) {
        let _receiver = &mut self.0;
        todo!("skeleton: daemon/supervisor")
    }
}

#[derive(Debug)]
#[must_use]
pub struct ChildExit {
    pub child: ChildName,
    pub outcome: TaskOutcome,
}

// Owner: the daemon's run loop; bound: MAX_CHILDREN registrations, one task each in `children`;
// no queue: a Restart child waits out its backoff before it runs again, and a Fatal child's exit
// ends `run`; shutdown: `shutdown` signals every child, joins for the grace and aborts the rest.
#[derive(Debug)]
pub struct Supervisor {
    children: JoinSet<ChildExit>,
    shutdown: watch::Sender<bool>,
}

impl Default for Supervisor {
    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    fn default() -> Self {
        todo!("skeleton: daemon/supervisor")
    }
}

impl Supervisor {
    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    pub fn add<F, Fut, E>(
        &mut self,
        name: ChildName,
        policy: ChildPolicy,
        factory: F,
    ) -> Result<(), SupervisorError>
    where
        F: FnMut(Shutdown) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
        E: Error + Send + 'static,
    {
        let _registration = (&mut self.children, MAX_CHILDREN, name, policy, factory);
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    pub async fn run(&mut self) -> ChildExit {
        let _children = &mut self.children;
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    pub async fn shutdown(self, grace: Duration) -> Vec<ChildExit> {
        let _shutdown = (self.shutdown, self.children, grace);
        todo!("skeleton: daemon/supervisor")
    }
}

#[cfg(test)]
mod tests {
    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    #[test]
    #[ignore = "skeleton: daemon/supervisor"]
    fn shutdown_aborts_a_child_that_outlives_the_grace() {
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    #[test]
    #[ignore = "skeleton: daemon/supervisor"]
    fn a_restarted_child_that_stays_healthy_resets_its_backoff() {
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    #[test]
    #[ignore = "skeleton: daemon/supervisor"]
    fn a_child_past_the_maximum_is_refused() {
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    #[test]
    #[ignore = "skeleton: daemon/supervisor"]
    fn a_panicked_fatal_child_ends_run_with_its_cause() {
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    #[test]
    #[ignore = "skeleton: daemon/supervisor"]
    fn a_failed_restart_child_runs_again_after_its_backoff() {
        todo!("skeleton: daemon/supervisor")
    }
}
