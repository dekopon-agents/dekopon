#![expect(dead_code, reason = "skeleton: daemon/broker-self-reload")]

use std::sync::Arc;

use dekopon_core::reload::{
    Cause, CommitReceiver, Input, Reconcile, Reconciled, ReloadError, WatchSet,
};
use tokio::task::JoinSet;

use crate::{BrokerdError, Coordination, lock_watch::LockGroup, signer_reload::SignerGroup};

pub(crate) enum BrokerGroup {
    Config(ConfigGroup),
    Lock(LockGroup),
    Signer(SignerGroup),
}

pub(crate) enum BrokerStop {
    LockChanged,
    Failed(BrokerdError),
}

impl From<ReloadError> for BrokerStop {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/broker-self-reload"
    )]
    fn from(error: ReloadError) -> Self {
        todo!()
    }
}

pub(crate) struct ConfigGroup {
    coordination: Arc<Coordination>,
}

impl Reconcile for BrokerGroup {
    type Exit = BrokerStop;

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
    async fn reconcile(&mut self, cause: Cause) -> Result<Reconciled, BrokerStop> {
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
    pub(crate) fn commits(&self) -> CommitReceiver {
        todo!()
    }
}

#[expect(clippy::todo, unused_variables, reason = "skeleton: daemon/supervisor")]
pub(crate) async fn reload_ended(task: &mut JoinSet<BrokerStop>) -> Result<(), BrokerdError> {
    todo!()
}

#[cfg(test)]
#[expect(
    clippy::todo,
    unused_variables,
    reason = "skeleton: daemon/broker-self-reload"
)]
pub(crate) fn injected_watcher(
    sender: tokio::sync::oneshot::Sender<dekopon_core::reload::Injector>,
) -> impl FnOnce(
    dekopon_core::reload::ReloadSettings,
    Vec<BrokerGroup>,
) -> Result<dekopon_core::reload::Watcher<BrokerGroup>, ReloadError> {
    move |settings, groups| todo!()
}
