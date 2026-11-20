#![expect(
    dead_code,
    reason = "skeleton: state/durable-store moves the wake store onto the durable store"
)]

use std::sync::Arc;

use dekopon_agent::wake::WakeId;
use dekopon_process::JobId;

use super::{Fired, WakeStore};
use crate::{
    collection::Dispositions,
    config::WakeBounds,
    store::{ConversationKey, JobRow, Persistence, StoreFailed, WakeRow},
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Delivered {
    Wake(WakeId),
    Job(JobId),
}

#[must_use = "a delivery stays pending and replays at boot until its ticket is settled"]
#[derive(Debug)]
pub(crate) struct DeliveryTicket {
    conversation: ConversationKey,
    delivered: Delivered,
}

#[must_use = "a fired wake is delivered by admitting its turn with the ticket in its dispositions"]
pub(crate) struct Delivery {
    pub(crate) fired: Fired,
    pub(crate) ticket: DeliveryTicket,
}

#[must_use = "an interrupted job is reported by admitting its notice with the ticket in its dispositions"]
pub(crate) struct JobNotice {
    pub(crate) row: JobRow,
    pub(crate) ticket: DeliveryTicket,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryDisposition {
    Handled,
    Orphaned,
    Rejected,
}

pub(crate) struct TickEnd {
    pub(crate) wake: WakeId,
    pub(crate) result: TickResult,
}

pub(crate) enum TickResult {
    Rearmed,
    Fired(Box<Delivery>),
    Gone,
    Panicked,
}

pub(crate) struct Replay {
    pub(crate) wakes: Vec<Delivery>,
    pub(crate) jobs: Vec<JobNotice>,
}

impl DeliveryTicket {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(super) fn issue(conversation: ConversationKey, delivered: Delivered) -> Self {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: state/durable-store")]
    pub(crate) fn delivered(&self) -> Delivered {
        todo!()
    }
}

impl Dispositions {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn with_delivery(self, ticket: DeliveryTicket) -> Self {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: state/durable-store")]
    pub(crate) fn take_delivery(&mut self) -> Option<DeliveryTicket> {
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
        persistence: &Persistence,
        rows: Vec<WakeRow>,
        jobs: Vec<(ConversationKey, JobRow)>,
        bounds: WakeBounds,
    ) -> (Arc<Self>, Replay) {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn settle(
        &self,
        ticket: DeliveryTicket,
        disposition: DeliveryDisposition,
    ) -> Result<(), StoreFailed> {
        todo!()
    }
}
