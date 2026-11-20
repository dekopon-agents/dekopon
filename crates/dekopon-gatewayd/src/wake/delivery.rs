#![expect(
    dead_code,
    reason = "skeleton: state/durable-store moves the wake store onto the durable store"
)]

use std::sync::Arc;

use dekopon_agent::wake::WakeId;

use super::{Fired, WakeStore};
use crate::{
    config::WakeBounds,
    store::{ConversationKey, DurableStore, StoreError, WakeRow},
};

#[must_use = "a delivery stays pending and replays at boot until its ticket is acknowledged"]
pub(crate) struct DeliveryTicket {
    conversation: ConversationKey,
    wake: WakeId,
}

#[must_use = "a fired wake is delivered by starting its session and acknowledging the ticket"]
pub(crate) struct Delivery {
    pub(crate) fired: Fired,
    pub(crate) ticket: DeliveryTicket,
}

pub(crate) enum TickEnd {
    Resolved(Option<Box<Delivery>>),
    Panicked { wake: WakeId },
}

impl DeliveryTicket {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(super) fn issue(conversation: ConversationKey, wake: WakeId) -> Self {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: state/durable-store")]
    pub(crate) fn wake(&self) -> WakeId {
        todo!()
    }
}

impl WakeStore {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn restored(
        store: Arc<DurableStore>,
        rows: Vec<WakeRow>,
        bounds: WakeBounds,
    ) -> (Self, Vec<Delivery>) {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn acknowledge(&self, ticket: DeliveryTicket) -> Result<(), StoreError> {
        todo!()
    }
}
