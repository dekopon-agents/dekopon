use std::future::Future;

use super::{GenerationDigest, WatchSet};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct GroupId(u8);

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

#[derive(Debug, Eq, PartialEq)]
pub enum Reconciled {
    Applied { generation: GenerationDigest },
    Unchanged,
    Refused,
}

pub trait Reconcile: Send + 'static {
    type Exit: std::error::Error + Send + 'static;

    fn input(&self) -> Input;

    fn watch_set(&self) -> WatchSet;

    fn reconcile(
        &mut self,
        cause: Cause,
    ) -> impl Future<Output = Result<Reconciled, Self::Exit>> + Send;
}
