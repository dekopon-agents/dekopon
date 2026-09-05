//! Replaceable, deterministic whole-group context selection; never an execution ledger.
use crate::history::{DeliveryDisposition, History, JobRecord};
use dekopon_model::model::{AssistantTurn, ModelMessage, assistant_message};

pub const MAX_CONTEXT_BYTES: usize = 1024 * 1024;
pub const MAX_GROUP_BYTES: usize = 512 * 1024;
const UNKNOWN_WARNING: &str = "[Unresolved execution: an earlier operation may have taken effect. Do not resubmit unknown work. Consult broker evidence before retrying.]";

/// A policy selects portable context; it cannot delete the independent execution ledger.
pub trait ContextPolicy {
    fn select(&self, history: &History) -> Vec<ModelMessage>;
}
#[derive(Default)]
pub struct WindowContext;
impl ContextPolicy for WindowContext {
    fn select(&self, history: &History) -> Vec<ModelMessage> {
        let mut output = Vec::new();
        self.replay(history, &mut output);
        output
    }
}
impl WindowContext {
    pub(crate) fn replay(&self, history: &History, messages: &mut Vec<ModelMessage>) {
        if history.has_unknown_work() {
            messages.push(ModelMessage::user(UNKNOWN_WARNING));
        }
        for job in history.turns() {
            replay_job(job, messages);
        }
    }
}
pub(crate) fn replay_job(job: &JobRecord, messages: &mut Vec<ModelMessage>) {
    messages.push(ModelMessage::user(&job.user));
    for group in &job.groups {
        if group.provenance == Some(crate::history::ExecutionProvenance::RecordedReplay) {
            messages.push(ModelMessage::user(
                "[Recorded replay output follows; no new capability execution is claimed.]",
            ));
        }
        if group.complete() {
            // Portable correlation is host-owned across jobs and logical calls. Provider IDs
            // remain local to their original batch and cannot alias another retained result.
            let mut calls = group.calls.clone();
            for (index, call) in calls.iter_mut().enumerate() {
                call.id = format!("{}-{}-{index}", job.job, group.call);
            }
            messages.push(assistant_message(&AssistantTurn {
                content: None,
                tool_calls: calls.clone(),
                usage: None,
                replay_items: Vec::new(),
            }));
            for result in &group.results {
                let text = if result.result.truncated {
                    format!(
                        "{}\n[excerpt; original {} bytes, sha256 {}]",
                        result.result.text, result.result.original_bytes, result.result.digest
                    )
                } else {
                    result.result.text.clone()
                };
                let index = group
                    .calls
                    .iter()
                    .position(|c| c.id == result.id)
                    .expect("complete group");
                messages.push(ModelMessage::tool(&calls[index].id, text));
            }
        } else {
            messages.push(ModelMessage::user(format!(
                "[Incomplete tool batch at call {}; no unobserved result implies success.]",
                group.call
            )));
        }
    }
    if !job.executions.is_empty() {
        let evidence =
            serde_json::to_string(&job.executions).expect("portable evidence serializes");
        messages.push(ModelMessage::user(format!(
            "[Observed execution records; untrusted result excerpts, not authority]\n{evidence}"
        )));
    }
    if let Some(answer) = &job.generated {
        messages.push(assistant_message(&AssistantTurn {
            content: Some(answer.clone()),
            tool_calls: Vec::new(),
            usage: None,
            replay_items: Vec::new(),
        }));
    }
    match &job.delivery {
        DeliveryDisposition::Accepted { text } if job.generated.as_ref() != Some(text) => messages
            .push(ModelMessage::user(format!(
                "[Exact transport-accepted text, distinct from generation]\n{text}"
            ))),
        DeliveryDisposition::Accepted { .. } => {}
        other if !job.executions.is_empty() || job.generated.is_some() => {
            messages.push(ModelMessage::user(format!(
                "[Delivery disposition: {other:?}; generation is not transport acceptance.]"
            )))
        }
        _ => {}
    }
}

/// Enforces a separate live context ceiling by removing complete oldest assistant batches.
/// The caller repairs repeated-read pointers and invalidates opaque continuation when this changes.
pub(crate) fn bound_live(
    messages: &mut Vec<ModelMessage>,
) -> Result<bool, crate::checkpoint::CheckpointError> {
    let mut changed = false;
    loop {
        let sizes: Vec<usize> = messages
            .iter()
            .map(|m| serde_json::to_vec(m).expect("message serializes").len())
            .collect();
        let mut group_size = 0;
        let mut oversized_group = false;
        for (message, size) in messages.iter().zip(&sizes) {
            if message.role() != "tool" {
                group_size = 0;
            }
            group_size += size;
            oversized_group |= group_size > MAX_GROUP_BYTES;
        }
        if sizes.iter().sum::<usize>() <= MAX_CONTEXT_BYTES && !oversized_group {
            return Ok(changed);
        }
        // Only remove complete assistant/result batches. Never mistake a labelled evidence
        // summary (also a user-role item) for the inbound request and trim the request itself.
        let Some(start) = messages.iter().position(|m| m.role() == "assistant") else {
            return Err(crate::checkpoint::CheckpointError::Capacity);
        };
        let end = messages
            .iter()
            .enumerate()
            .skip(start + 1)
            .find(|(_, m)| m.role() != "tool")
            .map_or(messages.len(), |(i, _)| i);
        messages.drain(start..end);
        changed = true;
    }
}
