#![expect(dead_code, reason = "skeleton: daemon/reload-consumers")]

use std::sync::Arc;

use super::CurrentGeneration;
use crate::follow::FollowTable;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Resident {
    TransportConnection,
    SessionPool,
    BrokerConnection,
    TelemetryExporter,
    MeteringStore,
    ProxyListener,
    ReloadBlock,
}

#[expect(
    clippy::todo,
    unused_variables,
    reason = "skeleton: daemon/reload-consumers"
)]
pub(crate) fn resident(category: &str) -> Option<Resident> {
    todo!()
}

impl CurrentGeneration {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/reload-consumers"
    )]
    pub(crate) fn swap_follow(&self, table: Option<Arc<FollowTable>>) {
        todo!()
    }
}
