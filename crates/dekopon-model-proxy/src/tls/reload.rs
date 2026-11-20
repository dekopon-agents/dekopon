#![expect(dead_code, reason = "skeleton: daemon/reload-consumers")]

use std::{convert::Infallible, sync::Arc};

use arc_swap::ArcSwap;
use dekopon_core::reload::{Cause, Input, Reconcile, Reconciled, WatchSet};
use tokio_rustls::TlsAcceptor;

use super::{Listener, TlsFiles};

pub struct TlsGroup {
    files: TlsFiles,
    identity: String,
    acceptor: Arc<ArcSwap<TlsAcceptor>>,
}

impl Reconcile for TlsGroup {
    type Exit = Infallible;

    #[expect(clippy::todo, reason = "skeleton: daemon/reload-consumers")]
    fn input(&self) -> Input {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/reload-consumers")]
    fn watch_set(&self) -> WatchSet {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/reload-consumers"
    )]
    async fn reconcile(&mut self, cause: Cause) -> Result<Reconciled, Infallible> {
        todo!()
    }
}

impl Listener {
    #[expect(clippy::todo, reason = "skeleton: daemon/reload-consumers")]
    pub fn reload_group(&self) -> TlsGroup {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "skeleton: daemon/reload-consumers"]
    fn the_acceptor_swaps_on_a_tls_volume_change_with_no_per_connection_stat() {}

    #[test]
    #[ignore = "skeleton: daemon/reload-consumers"]
    fn a_refused_bundle_keeps_the_old_acceptor_and_reports_once_per_change() {}

    #[test]
    #[ignore = "skeleton: daemon/reload-consumers"]
    fn a_half_swapped_bundle_is_retried_not_served() {}

    #[test]
    #[ignore = "skeleton: daemon/reload-consumers"]
    fn an_aborted_tls_reload_keeps_the_old_acceptor() {}

    #[test]
    #[ignore = "skeleton: daemon/reload-consumers"]
    fn a_retired_acceptor_drains_with_its_connections() {}

    #[test]
    #[ignore = "skeleton: daemon/reload-consumers"]
    fn a_restarted_proxy_loads_the_mounted_bundle() {}
}
