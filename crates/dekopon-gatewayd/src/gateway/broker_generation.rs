#![expect(dead_code, reason = "skeleton: daemon/broker-self-reload")]

use std::sync::Arc;

use arc_swap::ArcSwap;
use dekopon_broker_protocol::BrokerClient;
use dekopon_core::reload::{CommitSignal, GenerationDigest};

use super::ServingGeneration;
use crate::{GatewaydError, config};

#[expect(
    clippy::todo,
    unused_variables,
    reason = "skeleton: daemon/broker-self-reload"
)]
pub(crate) async fn follow(
    client: BrokerClient,
    commits: CommitSignal,
    active: Arc<ArcSwap<ServingGeneration>>,
) -> Result<(), GatewaydError> {
    todo!()
}

impl ServingGeneration {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/broker-self-reload"
    )]
    pub(crate) fn with_broker(
        &self,
        broker: &GenerationDigest,
    ) -> Result<Self, config::ConfigError> {
        todo!()
    }
}
