#![expect(dead_code, reason = "skeleton: daemon/watcher-core")]

use dekopon_core::reload::{Cause, Input, Reconcile, Reconciled, WatchSet};
use dekopon_model_proxy::tls::TlsReload;
use tokio::task::JoinSet;

use super::ReloadState;
use crate::{GatewaydError, ServeOutcome};

pub(crate) enum GatewayGroup {
    Config(Box<ReloadState>),
    ProxyTls(TlsReload),
}

impl Reconcile for GatewayGroup {
    type Exit = GatewaydError;

    #[expect(clippy::todo, reason = "skeleton: daemon/watcher-core")]
    fn input(&self) -> Input {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/watcher-core")]
    fn watch_set(&self) -> WatchSet {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/watcher-core"
    )]
    async fn reconcile(&mut self, cause: Cause) -> Result<Reconciled, GatewaydError> {
        todo!()
    }
}

impl ReloadState {
    #[expect(clippy::todo, reason = "skeleton: daemon/watcher-core")]
    fn watch_set(&self) -> WatchSet {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/watcher-core"
    )]
    async fn reconcile_cause(&mut self, cause: Cause) -> Result<Reconciled, GatewaydError> {
        todo!()
    }
}

#[expect(
    clippy::todo,
    unused_variables,
    reason = "skeleton: daemon/reload-consumers"
)]
async fn reconcile_tls(tls: &mut TlsReload, cause: Cause) -> Reconciled {
    todo!()
}

#[expect(clippy::todo, unused_variables, reason = "skeleton: daemon/supervisor")]
async fn reload_ended(task: &mut JoinSet<GatewaydError>) -> ServeOutcome {
    todo!()
}
