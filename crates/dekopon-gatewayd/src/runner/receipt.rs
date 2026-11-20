use serde::Serialize;

use super::IntentId;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ReceiptOutcome {
    Ok,
    Refused,
    Failed,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum ReceiptOutput {
    Inline(String),
    Oversize {
        oversize: bool,
        bytes: u64,
        sha256: String,
    },
}

#[derive(Debug, Serialize)]
pub(crate) struct Receipt {
    pub(crate) intent: IntentId,
    pub(crate) capability: String,
    pub(crate) outcome: ReceiptOutcome,
    pub(crate) output: ReceiptOutput,
}

#[derive(Default)]
pub(crate) struct ReceiptBatches {
    batches: Vec<Vec<u8>>,
    open: Vec<u8>,
}

impl ReceiptOutput {
    #[expect(clippy::todo, unused_variables, reason = "skeleton: rpi/tick-runner")]
    pub(crate) fn of(output: &[u8]) -> Self {
        todo!()
    }
}

impl ReceiptBatches {
    #[expect(clippy::todo, unused_variables, reason = "skeleton: rpi/tick-runner")]
    pub(crate) fn push(&mut self, receipt: &Receipt) {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: rpi/tick-runner")]
    pub(crate) fn finish(self) -> Vec<Vec<u8>> {
        todo!()
    }
}
