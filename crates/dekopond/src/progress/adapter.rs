//! The gateway's [`ProgressSink`]: one trace record per event, then the two channels.
//!
//! The prompt loop is synchronous and runs on a blocking thread, so this implementation never
//! blocks and never awaits. Discrete events go to a bounded queue with `try_send`; the cumulative
//! answer text goes to a watch value, because text is a value the policy reads at its own cadence
//! rather than an event it must not miss.

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use dekopon_agent::{ProgressEvent, ProgressSink};
use dekopon_model::ModelText;
use tokio::sync::{mpsc, watch};

use crate::session::SessionCancellation;

/// Discrete events buffered between the blocking prompt loop and the policy task.
///
/// Bounded because everything that grows needs a bound and an owner. Discrete events are rare —
/// a turn, a tool, an attachment — so overflow means the policy task is wedged behind a service
/// call, and dropping the oldest news is better than blocking the model loop on presentation.
pub(crate) const EVENT_QUEUE: usize = 64;

/// The turn number no delta has carried yet, so the first one always starts a fresh accumulator.
const NO_TURN: u32 = 0;

/// What the adapter counted that the policy renders or records.
#[derive(Debug, Default)]
pub(crate) struct ProgressCounters {
    /// Events the bounded queue could not take.
    pub dropped: AtomicU32,
    /// Text deltas observed, which is what makes "the stream was alive" readable at cancel.
    pub deltas: AtomicU32,
}

/// The gateway's progress sink.
pub(crate) struct ProgressAdapter {
    transport: String,
    events: mpsc::Sender<ProgressEvent>,
    text: watch::Sender<ModelText>,
    /// The turn the watch value belongs to, because each turn's text starts again from nothing.
    turn: AtomicU32,
    counters: Arc<ProgressCounters>,
    /// The race a finished loop closes; see [`ProgressSink::emit`].
    cancellation: SessionCancellation,
}

impl ProgressAdapter {
    pub(crate) fn new(
        transport: String,
        events: mpsc::Sender<ProgressEvent>,
        text: watch::Sender<ModelText>,
        counters: Arc<ProgressCounters>,
        cancellation: SessionCancellation,
    ) -> Self {
        Self {
            transport,
            events,
            text,
            turn: AtomicU32::new(NO_TURN),
            counters,
            cancellation,
        }
    }
}

impl ProgressSink for ProgressAdapter {
    fn emit(&self, event: ProgressEvent) {
        // A text delta is a value rather than an event: it is the newest rendering of one thing,
        // it arrives hundreds of times per turn, and the count is what a trace needs. The loop
        // sends fragments, so the cumulative text is accumulated here, once, rather than by every
        // reader; `stream.deltas` on `prompt.model_turn` and the count on the policy's terminal
        // record are where a reader finds how many there were.
        if let ProgressEvent::TextDelta { turn, text, .. } = &event {
            self.counters.deltas.fetch_add(1, Ordering::Relaxed);
            // A new turn's text starts from nothing: the surface shows what is being written now,
            // not the previous turn's answer with this one appended to it.
            let restart = self.turn.swap(*turn, Ordering::Relaxed) != *turn;
            self.text.send_modify(|cumulative| {
                if restart {
                    *cumulative = ModelText::default();
                }
                cumulative.push(text);
            });
            return;
        }
        // The loop has the answer, so there is nothing left to stop: this claim is what makes a
        // stop word that arrives while the session is still unwinding lose the race instead of
        // replacing the answer on screen with `Stopped.`. It happens here, synchronously on the
        // loop's own thread, because the session cannot claim it until a thread hand-off later.
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
        if let Err(error) = self.events.try_send(event) {
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

/// Writes one event to the message's own trace.
///
/// Metadata only, by construction: every field below is a counter, a duration, an operator or
/// provider word, or a fixed kind. There is no field a prompt, a capability argument, a provider
/// result, or model text could be written into.
pub(crate) fn record(event: &ProgressEvent) {
    match event {
        ProgressEvent::Started { agent, max_steps } => tracing::info!(
            target: "dekopond::audit",
            { audit.event = "gateway.progress", kind = "started", agent = agent.as_str(), max_steps = *max_steps },
            "gateway progress"
        ),
        ProgressEvent::ModelTurn { turn, of } => tracing::info!(
            target: "dekopond::audit",
            { audit.event = "gateway.progress", kind = "model_turn", turn = *turn, of = *of },
            "gateway progress"
        ),
        // Counted rather than recorded; see `emit`.
        ProgressEvent::TextDelta { .. } => {}
        ProgressEvent::Answered {
            turn,
            tool_calls,
            duration,
            first_delta,
        } => tracing::info!(
            target: "dekopond::audit",
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
            target: "dekopond::audit",
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
            target: "dekopond::audit",
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
            target: "dekopond::audit",
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
            target: "dekopond::audit",
            {
                audit.event = "gateway.progress",
                kind = "keep_alive",
                elapsed_ms = elapsed.as_millis() as u64,
                count = *count,
            },
            "gateway progress"
        ),
        ProgressEvent::Cancelled { by } => tracing::info!(
            target: "dekopond::audit",
            { audit.event = "gateway.progress", kind = "cancelled", by = ?by },
            "gateway progress"
        ),
        ProgressEvent::Failed { class } => tracing::info!(
            target: "dekopond::audit",
            { audit.event = "gateway.progress", kind = "failed", class = ?class },
            "gateway progress"
        ),
        ProgressEvent::Finished {
            outcome,
            elapsed,
            turns,
            tool_calls,
        } => tracing::info!(
            target: "dekopond::audit",
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
