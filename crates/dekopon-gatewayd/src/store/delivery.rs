use serde::{Deserialize, Serialize};

use super::record::{ConversationKey, WakeRow};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct DeliveryId(u64);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct PendingDelivery {
    pub(crate) id: DeliveryId,
    pub(crate) wake: WakeRow,
    pub(crate) due_ms: u64,
    pub(crate) attempts: u32,
}

#[must_use = "a delivery stays pending and replays at boot until its acknowledgement commits"]
pub(crate) struct DeliveryAck {
    pub(crate) conversation: ConversationKey,
    pub(crate) id: DeliveryId,
}

impl PendingDelivery {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn handled(&self, conversation: ConversationKey) -> DeliveryAck {
        todo!()
    }
}
