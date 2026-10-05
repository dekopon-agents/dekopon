use std::sync::{
    Arc,
    atomic::{AtomicU32, AtomicU64, Ordering},
};

use dekopon_agent::{ProgressEvent, ProgressSink};
use dekopon_model::ModelText;
use tokio::sync::{mpsc, watch};

use crate::{session::SessionCancellation, transport::StreamedText};

pub(crate) const EVENT_QUEUE: usize = 64;

const NO_TURN: u32 = 0;
const MAX_NOTES_PER_SESSION: u32 = 24;

#[derive(Debug, Default)]
pub(crate) struct ProgressCounters {
    pub dropped: AtomicU32,
    pub notes_dropped: AtomicU32,
    pub deltas: AtomicU32,
    pub note_generation: AtomicU64,
}

pub(crate) struct QueuedEvent {
    pub event: ProgressEvent,
    pub note_generation: u64,
}

pub(crate) struct ProgressAdapter {
    transport: String,
    events: mpsc::Sender<QueuedEvent>,
    text: watch::Sender<StreamedText>,
    turn: AtomicU32,
    notes: AtomicU32,
    counters: Arc<ProgressCounters>,
    cancellation: SessionCancellation,
}

impl ProgressAdapter {
    pub(crate) fn new(
        transport: String,
        events: mpsc::Sender<QueuedEvent>,
        text: watch::Sender<StreamedText>,
        counters: Arc<ProgressCounters>,
        cancellation: SessionCancellation,
    ) -> Self {
        Self {
            transport,
            events,
            text,
            turn: AtomicU32::new(NO_TURN),
            notes: AtomicU32::new(0),
            counters,
            cancellation,
        }
    }
}

impl ProgressSink for ProgressAdapter {
    fn emit(&self, event: ProgressEvent) {
        if matches!(event, ProgressEvent::Note { .. })
            && self
                .notes
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                    (count < MAX_NOTES_PER_SESSION).then_some(count + 1)
                })
                .is_err()
        {
            self.counters.notes_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if let ProgressEvent::TextDelta { turn, text, .. } = &event {
            self.counters.deltas.fetch_add(1, Ordering::Relaxed);
            let restart = self.turn.swap(*turn, Ordering::Relaxed) != *turn;
            self.text.send_modify(|cumulative| {
                if restart {
                    cumulative.text = ModelText::default();
                    cumulative.generation += 1;
                }
                cumulative.text.push(text);
            });
            return;
        }
        let note_generation = if matches!(
            event,
            ProgressEvent::ModelTurn { .. } | ProgressEvent::Steered { .. }
        ) {
            self.counters
                .note_generation
                .fetch_add(1, Ordering::AcqRel)
                .wrapping_add(1)
        } else {
            self.counters.note_generation.load(Ordering::Acquire)
        };
        if matches!(event, ProgressEvent::Steered { .. }) {
            self.text.send_modify(|cumulative| {
                cumulative.text = ModelText::default();
                cumulative.generation += 1;
            });
            self.turn.store(NO_TURN, Ordering::Relaxed);
        }
        // Claimed synchronously here, on the loop's own thread, so a stop word arriving while the
        // session unwinds loses the race instead of overwriting the finished answer with the
        // stopped reply.
        if matches!(event, ProgressEvent::Finished { .. }) {
            #[allow(
                clippy::let_underscore_must_use,
                reason = "the bool says whether this claim closed the race; losing it means a \
                          stop already won, which the policy has already rendered and the \
                          session already reports as cancelled"
            )]
            let _ = self.cancellation.claim_completion();
        }
        record(&event);
        if let Err(error) = self.events.try_send(QueuedEvent {
            event,
            note_generation,
        }) {
            let dropped = self.counters.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::debug!(
                event = "gateway_progress_dropped",
                transport = %self.transport,
                count = dropped,
                reason = match error {
                    mpsc::error::TrySendError::Full(_) => "full",
                    mpsc::error::TrySendError::Closed(_) => "closed",
                }
            );
        }
    }
}

pub(crate) fn record(event: &ProgressEvent) {
    match event {
        ProgressEvent::Started { agent, max_steps } => tracing::info!(
            target: "dekopon_gatewayd::audit",
            { audit.event = "gateway.progress", kind = "started", agent = agent.as_str(), max_steps = *max_steps },
            "gateway progress"
        ),
        ProgressEvent::ModelTurn { turn, of } => tracing::info!(
            target: "dekopon_gatewayd::audit",
            { audit.event = "gateway.progress", kind = "model_turn", turn = *turn, of = *of },
            "gateway progress"
        ),
        ProgressEvent::Steered { turn } => tracing::info!(
            target: "dekopon_gatewayd::audit",
            { audit.event = "gateway.progress", kind = "steered", turn = *turn },
            "gateway progress"
        ),
        ProgressEvent::Note { text, eta } => tracing::info!(
            target: "dekopon_gatewayd::audit",
            { audit.event = "gateway.progress", kind = "note", note = text.as_str(), note.eta_s = eta.map(|eta| eta.as_secs()) },
            "gateway progress"
        ),
        ProgressEvent::TextDelta { .. } => {}
        ProgressEvent::Answered {
            turn,
            tool_calls,
            duration,
            first_delta,
        } => tracing::info!(
            target: "dekopon_gatewayd::audit",
            {
                audit.event = "gateway.progress",
                kind = "answered",
                turn = *turn,
                tool_calls = *tool_calls,
                elapsed_ms = duration.as_millis() as u64,
                first_delta_ms = first_delta.map(|delta| delta.as_millis() as u64),
            },
            "gateway progress"
        ),
        ProgressEvent::ToolStarted {
            word,
            argument_count,
            calls_used,
            calls_max,
        } => tracing::info!(
            target: "dekopon_gatewayd::audit",
            {
                audit.event = "gateway.progress",
                kind = "tool_started",
                word = word.as_str(),
                argument_count = *argument_count,
                calls_used = *calls_used,
                calls_max = *calls_max,
            },
            "gateway progress"
        ),
        ProgressEvent::ToolFinished {
            word,
            outcome,
            duration,
        } => tracing::info!(
            target: "dekopon_gatewayd::audit",
            {
                audit.event = "gateway.progress",
                kind = "tool_finished",
                word = word.as_str(),
                outcome = ?outcome,
                elapsed_ms = duration.as_millis() as u64,
            },
            "gateway progress"
        ),
        ProgressEvent::Attachment {
            index,
            media_type,
            bytes,
        } => tracing::info!(
            target: "dekopon_gatewayd::audit",
            {
                audit.event = "gateway.progress",
                kind = "attachment",
                index = *index,
                media_type = media_type.as_str(),
                bytes = *bytes,
            },
            "gateway progress"
        ),
        ProgressEvent::KeepAlive { elapsed, count } => tracing::info!(
            target: "dekopon_gatewayd::audit",
            {
                audit.event = "gateway.progress",
                kind = "keep_alive",
                elapsed_ms = elapsed.as_millis() as u64,
                count = *count,
            },
            "gateway progress"
        ),
        ProgressEvent::Cancelled { by } => tracing::info!(
            target: "dekopon_gatewayd::audit",
            { audit.event = "gateway.progress", kind = "cancelled", by = ?by },
            "gateway progress"
        ),
        ProgressEvent::Failed { class } => tracing::info!(
            target: "dekopon_gatewayd::audit",
            { audit.event = "gateway.progress", kind = "failed", class = ?class },
            "gateway progress"
        ),
        ProgressEvent::Finished {
            outcome,
            elapsed,
            turns,
            tool_calls,
        } => tracing::info!(
            target: "dekopon_gatewayd::audit",
            {
                audit.event = "gateway.progress",
                kind = "finished",
                outcome = ?outcome,
                elapsed_ms = elapsed.as_millis() as u64,
                turns = *turns,
                tool_calls = *tool_calls,
            },
            "gateway progress"
        ),
    }
}
