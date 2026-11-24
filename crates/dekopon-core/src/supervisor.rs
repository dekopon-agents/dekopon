use std::{error::Error, future::Future, time::Duration};

use tokio::{sync::watch, task::JoinSet};

use crate::{Backoff, DaemonError, TaskOutcome};

const MAX_CHILDREN: usize = 16;

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
pub struct RestartPolicy {
    pub backoff: Backoff,
    pub healthy_after: Duration,
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

// Owner: the daemon's boot path; bound: MAX_CHILDREN registrations, one task each; no queue, a
// failed child waits out its backoff before it restarts; shutdown: `shutdown` signals every child,
// joins them for the grace and aborts the rest.
#[derive(Debug)]
pub struct Supervisor {
    children: JoinSet<ChildExit>,
    shutdown: watch::Sender<bool>,
    policy: RestartPolicy,
}

impl Supervisor {
    #[must_use]
    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    pub fn new(policy: RestartPolicy) -> Self {
        let _policy = policy;
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    pub fn add<F, Fut, E>(&mut self, name: ChildName, factory: F) -> Result<(), DaemonError>
    where
        F: FnMut(Shutdown) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
        E: Error + Send + 'static,
    {
        let _registration = (
            &mut self.children,
            &self.policy,
            MAX_CHILDREN,
            name,
            factory,
        );
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
    #[test]
    #[ignore = "skeleton: daemon/supervisor"]
    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    fn shutdown_aborts_a_child_that_outlives_the_grace() {
        todo!("skeleton: daemon/supervisor")
    }

    #[test]
    #[ignore = "skeleton: daemon/supervisor"]
    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    fn a_healthy_run_resets_the_restart_backoff() {
        todo!("skeleton: daemon/supervisor")
    }

    #[test]
    #[ignore = "skeleton: daemon/supervisor"]
    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    fn a_child_past_the_maximum_is_refused() {
        todo!("skeleton: daemon/supervisor")
    }

    #[test]
    #[ignore = "skeleton: daemon/supervisor"]
    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    fn a_panicked_child_restarts_after_its_backoff() {
        todo!("skeleton: daemon/supervisor")
    }
}
