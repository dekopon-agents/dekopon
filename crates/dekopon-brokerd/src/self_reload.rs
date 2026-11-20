#![expect(dead_code, reason = "skeleton: daemon/broker-self-reload")]

use std::sync::Arc;

use dekopon_core::reload::{Cause, Input, Reconcile, Reconciled, WatchSet};
use tokio::sync::watch;

use crate::{
    BrokerdError, Candidate, Coordination, lock_watch::LockGroup, signer_reload::SignerGroup,
};

pub(crate) enum BrokerGroup {
    Config(ConfigGroup),
    Lock(LockGroup),
    Signer(SignerGroup),
}

pub(crate) struct ConfigGroup {
    coordination: Arc<Coordination>,
}

impl Reconcile for BrokerGroup {
    type Exit = BrokerdError;

    #[expect(clippy::todo, reason = "skeleton: daemon/broker-self-reload")]
    fn input(&self) -> Input {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/broker-self-reload")]
    fn watch_set(&self) -> WatchSet {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/broker-self-reload"
    )]
    async fn reconcile(&mut self, cause: Cause) -> Result<Reconciled, BrokerdError> {
        todo!()
    }
}

impl Coordination {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/broker-self-reload"
    )]
    async fn reload(&self, cause: Cause) -> Result<Reconciled, BrokerdError> {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/broker-self-reload")]
    fn watch_set(&self) -> WatchSet {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/broker-self-reload")]
    pub(crate) fn commits(&self) -> watch::Receiver<Arc<Candidate>> {
        todo!()
    }
}
