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
pub struct ChildExit<K, E> {
    pub child: K,
    pub outcome: TaskOutcome<E>,
}

#[derive(Debug)]
pub struct Supervisor<K, E> {
    children: JoinSet<Result<(), E>>,
    names: HashMap<Id, (K, OnShutdown, AbortHandle)>,
    shutdown: watch::Sender<bool>,
    max_children: NonZeroUsize,
}

impl<K: Copy + Send + 'static, E: Send + 'static> Supervisor<K, E> {
    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    #[must_use]
    pub fn new(max_children: NonZeroUsize) -> Self {
        let _bound = max_children;
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    pub fn add<F, Fut>(
        &mut self,
        child_key: K,
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
            child_key,
            on_shutdown,
            child,
        );
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    pub async fn run(&mut self) -> ChildExit<K, E> {
        let _children = (&mut self.children, &mut self.names);
        todo!("skeleton: daemon/supervisor")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    pub async fn shutdown(self, grace: ShutdownGrace) -> Vec<ChildExit<K, E>> {
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
    fn an_aborted_child_exit_keeps_its_key() {
        todo!("skeleton: daemon/supervisor")
    }
}
