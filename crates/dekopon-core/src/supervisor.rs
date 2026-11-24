use std::{collections::HashMap, future::Future, num::NonZeroUsize, time::Duration};

use thiserror::Error;
use tokio::{
    sync::watch,
    task::{AbortHandle, Id, JoinSet},
};

use crate::TaskOutcome;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnShutdown {
    AbortAfterGrace,
    Finish,
}

#[derive(Clone, Copy, Debug)]
pub struct ShutdownGrace {
    pub abortable: Duration,
    pub hard_stop: Duration,
}

#[derive(Clone, Debug)]
pub struct Shutdown(watch::Receiver<bool>);

impl Shutdown {
    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    pub async fn requested(&mut self) {
        let _receiver = &mut self.0;
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    #[must_use]
    pub fn is_requested(&self) -> bool {
        let _receiver = &self.0;
        todo!("skeleton: daemon/supervisor")
    }
}

#[derive(Debug)]
#[must_use]
pub struct ChildExit<E> {
    pub child: ChildName,
    pub outcome: TaskOutcome<E>,
}

// Owner: the daemon's run loop; bound: `max_children`, one task per child and no restarts (a
// child that recovers, like a transport reader, does it inside its own future); no queue;
// shutdown: `shutdown` signals every child, aborts the AbortAfterGrace ones still running after
// `abortable` and the Finish ones still running at `hard_stop`.
#[derive(Debug)]
pub struct Supervisor<E> {
    children: JoinSet<Result<(), E>>,
    names: HashMap<Id, (ChildName, OnShutdown, AbortHandle)>,
    shutdown: watch::Sender<bool>,
    max_children: NonZeroUsize,
}

impl<E: Send + 'static> Supervisor<E> {
    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    #[must_use]
    pub fn new(max_children: NonZeroUsize) -> Self {
        let _bound = max_children;
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    pub fn add<F, Fut>(
        &mut self,
        name: ChildName,
        on_shutdown: OnShutdown,
        child: F,
    ) -> Result<(), SupervisorError>
    where
        F: FnOnce(Shutdown) -> Fut,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
    {
        let _registration = (
            &mut self.children,
            self.max_children,
            name,
            on_shutdown,
            child,
        );
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    pub async fn run(&mut self) -> ChildExit<E> {
        let _children = (&mut self.children, &mut self.names);
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    pub async fn shutdown(self, grace: ShutdownGrace) -> Vec<ChildExit<E>> {
        let _shutdown = (self.shutdown, self.children, self.names, grace);
        todo!("skeleton: daemon/supervisor")
    }
}

#[cfg(test)]
mod tests {
    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    #[test]
    #[ignore = "skeleton: daemon/supervisor"]
    fn shutdown_aborts_an_abortable_child_that_outlives_the_grace() {
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    #[test]
    #[ignore = "skeleton: daemon/supervisor"]
    fn shutdown_waits_for_a_finish_child_past_the_grace() {
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    #[test]
    #[ignore = "skeleton: daemon/supervisor"]
    fn a_child_started_after_shutdown_sees_it_requested() {
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
    fn a_panicked_child_ends_run_as_panicked() {
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    #[test]
    #[ignore = "skeleton: daemon/supervisor"]
    fn a_failed_child_ends_run_with_its_typed_cause() {
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    #[test]
    #[ignore = "skeleton: daemon/supervisor"]
    fn a_child_that_returns_ok_ends_run_as_completed() {
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    #[test]
    #[ignore = "skeleton: daemon/supervisor"]
    fn a_finish_child_still_running_at_the_hard_stop_is_aborted() {
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    #[test]
    #[ignore = "skeleton: daemon/supervisor"]
    fn an_aborted_child_exit_keeps_its_name() {
        todo!("skeleton: daemon/supervisor")
    }
}
