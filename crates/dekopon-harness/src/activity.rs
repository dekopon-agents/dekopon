//! Cosmetic, non-durable observations of actual capability submissions, never authority/evidence.

use crate::{bootstrap::CapabilitySnapshot, history::ExecutionOutcome};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::sync::Notify;

/// A bounded operator-authored label. No description, argument, result or capability name is used.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityLabel(String);
impl ActivityLabel {
    /// Strip controls and Unicode directional/format controls, then bound on UTF-8 boundaries.
    pub fn sanitized(text: &str) -> Self {
        let mut label = String::new();
        for c in text.chars().filter(|c| !c.is_control() && !matches!(c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')) {
            if label.len() + c.len_utf8() > 80 { break; }
            label.push(c);
        }
        let label = label.trim();
        Self(
            if label.is_empty() {
                "Running capability"
            } else {
                label
            }
            .to_owned(),
        )
    }
    /// Plain text only; renderers must disable/escape their platform's markup and link parsing.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Default for ActivityLabel {
    fn default() -> Self {
        Self("Running capability".into())
    }
}

/// Submission is not authorization; completion describes the host's observation only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivityPhase {
    Submitted,
    Finished,
}

/// Private job coordinates plus a safe label. Never stored in history, replay or checkpoints.
#[derive(Clone, Debug)]
pub struct ActivityEvent {
    pub job: String,
    pub generation: String,
    pub sequence: u64,
    pub operation: u32,
    pub phase: ActivityPhase,
    pub label: ActivityLabel,
    pub outcome: Option<ExecutionOutcome>,
}

#[derive(Default)]
struct Shared {
    queue: Mutex<VecDeque<ActivityEvent>>,
    sealed: AtomicBool,
    sequence: AtomicU64,
    changed: Notify,
}
/// One generation's lossy queue32, with a separate undroppable seal. Emission never waits for I/O.
#[derive(Clone, Default)]
pub struct ActivityPublisher(Arc<Shared>);
impl ActivityPublisher {
    /// Prevent all subsequent publication independently of queue pressure.
    pub fn seal(&self) {
        self.0.sealed.store(true, Ordering::Release);
        self.0.changed.notify_one();
    }
    /// Wake a consumer without waiting in the synchronous runtime.
    pub async fn changed(&self) {
        self.0.changed.notified().await;
    }
    /// Coalesce queued activity to the latest observation.
    pub fn latest(&self) -> Option<ActivityEvent> {
        let Ok(mut queue) = self.0.queue.try_lock() else {
            return None;
        };
        let newest = queue.pop_back();
        queue.clear();
        newest
    }
    pub(crate) fn bind(
        &self,
        job: String,
        labels: &BTreeMap<String, ActivityLabel>,
        capabilities: &CapabilitySnapshot,
    ) -> ActivityEmitter {
        // Intersect even trusted mappings with the same fresh snapshot used before inference.
        ActivityEmitter {
            publisher: self.clone(),
            generation: crate::checkpoint::opaque_id(),
            job,
            labels: labels
                .iter()
                .filter(|(id, _)| capabilities.contains(id))
                .take(256)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }
}

pub(crate) struct ActivityEmitter {
    publisher: ActivityPublisher,
    job: String,
    generation: String,
    labels: BTreeMap<String, ActivityLabel>,
}
impl ActivityEmitter {
    pub(crate) fn emit(
        &self,
        operation: u32,
        capability: &str,
        phase: ActivityPhase,
        outcome: Option<ExecutionOutcome>,
    ) {
        let shared = &self.publisher.0;
        if shared.sealed.load(Ordering::Acquire) {
            return;
        }
        let Ok(mut queue) = shared.queue.try_lock() else {
            return;
        };
        let sequence = shared.sequence.fetch_add(1, Ordering::Relaxed);
        let mut event = ActivityEvent {
            job: self.job.clone(),
            generation: self.generation.clone(),
            sequence,
            operation,
            phase,
            label: self.labels.get(capability).cloned().unwrap_or_default(),
            outcome,
        };
        if queue.len() == 32 {
            queue.clear();
            event.label = ActivityLabel::default();
        }
        queue.push_back(event);
        drop(queue);
        shared.changed.notify_one();
    }
}

#[cfg(test)]
mod tests;
