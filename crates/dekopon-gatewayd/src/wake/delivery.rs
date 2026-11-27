use std::{sync::Arc, time::SystemTime};

use dekopon_agent::wake::WakeId;
use dekopon_process::JobId;

use super::{Fired, WakeStore};
use crate::{
    collection::Dispositions,
    store::{ConversationKey, JobRow, StoreFailed},
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Delivered {
    Wake(WakeId),
    Job(JobId),
}

#[must_use = "a delivery stays pending and replays at boot until its ticket is settled"]
pub(crate) struct DeliveryTicket {
    conversation: ConversationKey,
    delivered: Delivered,
    store: Option<Arc<WakeStore>>,
}

impl std::fmt::Debug for DeliveryTicket {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeliveryTicket")
            .field("conversation", &self.conversation)
            .field("delivered", &self.delivered)
            .finish_non_exhaustive()
    }
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

#[must_use = "restored deliveries are admitted when serving starts"]
pub(crate) struct Replay {
    pub(crate) wakes: Vec<Delivery>,
    pub(crate) jobs: Vec<JobNotice>,
}

impl DeliveryTicket {
    pub(super) const fn issue(
        conversation: ConversationKey,
        delivered: Delivered,
        store: Option<Arc<WakeStore>>,
    ) -> Self {
        Self {
            conversation,
            delivered,
            store,
        }
    }

    pub(super) fn settled(mut self) -> (ConversationKey, Delivered) {
        self.store = None;
        (self.conversation.clone(), self.delivered)
    }

    pub(crate) fn settle(mut self, disposition: DeliveryDisposition) -> Result<(), StoreFailed> {
        match self.store.take() {
            Some(store) => store.settle(self, disposition),
            None => Ok(()),
        }
    }
}

impl Drop for DeliveryTicket {
    fn drop(&mut self) {
        if let Some(store) = self.store.take() {
            store.reoffer(self.delivered, SystemTime::now());
        }
    }
}

impl Dispositions {
    pub(crate) fn with_delivery(mut self, ticket: Option<DeliveryTicket>) -> Self {
        self.deliveries.extend(ticket);
        self
    }

    pub(crate) async fn settle(&mut self, disposition: DeliveryDisposition) {
        for ticket in self.deliveries.drain(..) {
            drop(dekopon_core::offload(move || ticket.settle(disposition)).await);
        }
    }
}
