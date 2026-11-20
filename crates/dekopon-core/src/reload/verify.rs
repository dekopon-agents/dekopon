use std::{future::Future, time::Duration};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerificationWindow {
    reads: [Duration; 3],
    cap: Duration,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Settle<T> {
    Pending,
    Settled(T),
}

impl VerificationWindow {
    #[expect(clippy::todo, reason = "skeleton: daemon/watcher-core")]
    pub const fn provider_lock() -> Self {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/watcher-core"
    )]
    pub async fn verify<R, F, T, S, V>(&self, read: R, settle: S) -> Option<V>
    where
        R: FnMut() -> F + Send,
        F: Future<Output = T> + Send,
        S: FnMut(T) -> Settle<V> + Send,
    {
        todo!()
    }
}
