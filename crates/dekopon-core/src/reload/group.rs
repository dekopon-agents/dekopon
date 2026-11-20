use std::future::Future;

use super::{ReloadError, WatchSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Cause {
    Startup,
    Changed,
    Rescan,
    Hangup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Input {
    Operator,
    Machine,
}

#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reconciled {
    Applied,
    Unchanged,
    Refused,
}

pub trait Reconcile: Send + 'static {
    type Exit: std::error::Error + From<ReloadError> + Send + 'static;

    fn input(&self) -> Input;

    fn watch_set(&self) -> WatchSet;

    // The watcher drops this future at shutdown and at the activation bound, so a group changes
    // what it serves only after its last await.
    fn reconcile(
        &mut self,
        cause: Cause,
    ) -> impl Future<Output = Result<Reconciled, Self::Exit>> + Send;
}
