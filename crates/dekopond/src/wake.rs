use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use dekopon_agent::{
    BrokerLeg, current_trace_parent,
    wake::{WakeId, WakeRefusal, WakeRegistrar, WakeRequest, WakeSummary},
};
use dekopon_broker_protocol::{
    Attestation, BrokerClient, ChatScopeClaim, ChatTransportKind, Conversation, TraceParent,
    Trigger,
};
use dekopon_core::{AgentId, ExternalSubject, TransportId};
use dekopon_shell::{
    CapabilityInvoker, ExitCode, Interpreter, Limits as ShellLimits, ScriptOutcome,
};

use crate::{
    config::{ResolvedBroker, WakeBounds},
    transport::{InboundMessage, MessageId, ReplyTarget, receive_span},
};

pub(crate) mod store;

pub use store::WakeStoreError;
pub(crate) use store::{Fired, Resolved, Tick, WakeStore};

const MAX_NOTE_BYTES: usize = 2 * 1024;
const MAX_PROBE_OUTPUT_BYTES: usize = 8 * 1024;
// Meta refuses free-form business-initiated text outside the 24-hour customer-service window.
const WHATSAPP_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug)]
pub(crate) struct Anchor {
    transport: TransportId,
    kind: ChatTransportKind,
    subject: ExternalSubject,
    conversation: Conversation,
    reply: ReplyTarget,
    agent: AgentId,
    scheduled_in: Option<TraceParent>,
}

impl Anchor {
    pub(crate) fn from_inbound(message: &InboundMessage, agent: &AgentId) -> Option<Self> {
        Some(Self {
            transport: message.transport.parse().ok()?,
            kind: message.transport_kind,
            subject: message.subject.clone(),
            conversation: message.conversation.clone(),
            reply: message.reply.clone(),
            agent: agent.clone(),
            scheduled_in: current_trace_parent(),
        })
    }

    pub(crate) const fn subject(&self) -> &ExternalSubject {
        &self.subject
    }

    pub(crate) const fn agent(&self) -> &AgentId {
        &self.agent
    }

    pub(crate) const fn conversation(&self) -> &Conversation {
        &self.conversation
    }

    pub(crate) const fn transport(&self) -> &TransportId {
        &self.transport
    }

    pub(crate) fn claim(&self, trigger: Trigger) -> Attestation {
        Attestation::for_chat(
            self.subject.clone(),
            self.agent.clone(),
            ChatScopeClaim {
                transport: self.transport.clone(),
                kind: self.kind,
                conversation: self.conversation.clone(),
                trigger,
            },
        )
    }

    pub(crate) fn inbound(&self, id: WakeId, text: String) -> InboundMessage {
        let span = receive_span(self.kind);
        if let Some(parent) = self.scheduled_in {
            dekopon_telemetry::link_remote(
                &span,
                dekopon_telemetry::TraceContextParts {
                    trace_id: parent.trace_id(),
                    span_id: parent.parent_id(),
                    flags: parent.flags(),
                },
            );
        }
        InboundMessage {
            transport: self.transport.to_string(),
            transport_kind: self.kind,
            subject: self.subject.clone(),
            conversation: self.conversation.clone(),
            message_id: MessageId::Wake(id),
            text,
            assets: Vec::new(),
            asset_overflow: false,
            addressed: Some(true),
            thread_continuation: None,
            reply: self.reply.clone(),
            liveness: None,
            receive_span: span,
            received_at: tokio::time::Instant::now(),
            native_group: None,
            constituents: Vec::new(),
            late_photos: None,
        }
    }

    fn probe_leg(
        &self,
        broker: &ResolvedBroker,
        runtime: &tokio::runtime::Handle,
    ) -> Option<BrokerLeg> {
        let client = BrokerClient::new(&broker.socket_path, broker.server_uid, broker.frame)
            .inspect_err(|error| {
                tracing::warn!(event = "gateway_wake_probe_unavailable", error = %error);
            })
            .ok()?;
        runtime
            .block_on(BrokerLeg::connect(client, Some(self.claim(Trigger::Probe))))
            .inspect_err(|error| {
                tracing::warn!(event = "gateway_wake_probe_unavailable", error = %error);
            })
            .ok()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Probe {
    script: String,
    prev: String,
}

impl Probe {
    pub(crate) fn baseline(
        script: String,
        leg: &dyn CapabilityInvoker,
        limits: ShellLimits,
    ) -> Result<(Self, String), WakeRefusal> {
        match Verdict::from(Interpreter::new(limits).run(&script, leg)) {
            Verdict::Fire { output } | Verdict::Wait { output } => Ok((
                Self {
                    script,
                    prev: output.clone(),
                },
                output,
            )),
            Verdict::Failed { exit, output } => Err(WakeRefusal::Broken {
                exit: exit.get(),
                output,
            }),
        }
    }

    pub(crate) fn tick(&self, leg: &dyn CapabilityInvoker, limits: ShellLimits) -> Verdict {
        Verdict::from(Interpreter::new(limits).run_with_prev(&self.script, &self.prev, leg))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Verdict {
    Fire { output: String },
    Wait { output: String },
    Failed { exit: ExitCode, output: String },
}

impl From<ScriptOutcome> for Verdict {
    fn from(outcome: ScriptOutcome) -> Self {
        if outcome.truncated || outcome.output.len() > MAX_PROBE_OUTPUT_BYTES {
            return Self::Failed {
                exit: outcome.exit_code,
                output: format!(
                    "[gateway: the probe printed more than {MAX_PROBE_OUTPUT_BYTES} bytes; print less]"
                ),
            };
        }
        match outcome.exit_code {
            ExitCode::SUCCESS => Self::Fire {
                output: outcome.output,
            },
            ExitCode::FAILURE => Self::Wait {
                output: outcome.output,
            },
            exit => Self::Failed {
                exit,
                output: outcome.output,
            },
        }
    }
}

pub(crate) struct SessionWakes {
    anchor: Anchor,
    store: Arc<WakeStore>,
    bounds: WakeBounds,
    broker: ResolvedBroker,
    runtime: tokio::runtime::Handle,
    limits: ShellLimits,
    started: SystemTime,
}

impl SessionWakes {
    pub(crate) fn new(
        anchor: Anchor,
        store: Arc<WakeStore>,
        broker: ResolvedBroker,
        runtime: tokio::runtime::Handle,
        limits: ShellLimits,
    ) -> Self {
        Self {
            bounds: store.bounds(),
            anchor,
            store,
            broker,
            runtime,
            limits,
            started: SystemTime::now(),
        }
    }

    fn check(&self, note: &str, span: Duration) -> Result<(), WakeRefusal> {
        if note.len() > MAX_NOTE_BYTES {
            return Err(WakeRefusal::NoteTooLong {
                maximum: MAX_NOTE_BYTES,
            });
        }
        let maximum = match self.anchor.kind {
            ChatTransportKind::Whatsapp => self
                .bounds
                .max_horizon
                .min(WHATSAPP_WINDOW.saturating_sub(self.started.elapsed().unwrap_or_default())),
            _ => self.bounds.max_horizon,
        };
        if span > maximum {
            return Err(WakeRefusal::Horizon { maximum });
        }
        Ok(())
    }
}

impl WakeRegistrar for SessionWakes {
    fn schedule(&self, request: WakeRequest) -> Result<WakeSummary, WakeRefusal> {
        let now = SystemTime::now();
        let summary = match request {
            WakeRequest::Once { note, after } => {
                self.check(&note, after)?;
                self.store.register(
                    self.anchor.clone(),
                    note,
                    now,
                    store::Schedule::Once { after },
                )?
            }
            WakeRequest::Watch {
                note,
                script,
                every,
                until,
            } => {
                self.check(&note, until)?;
                if every < self.bounds.min_interval {
                    return Err(WakeRefusal::Interval {
                        minimum: self.bounds.min_interval,
                    });
                }
                let leg = self
                    .anchor
                    .probe_leg(&self.broker, &self.runtime)
                    .ok_or(WakeRefusal::Unavailable)?;
                let (probe, _) = Probe::baseline(script, &leg, self.limits)?;
                self.store.register(
                    self.anchor.clone(),
                    note,
                    now,
                    store::Schedule::Watch {
                        probe,
                        every,
                        until,
                    },
                )?
            }
        };
        tracing::info!(
            target: "dekopond::audit",
            {
                audit.event = "gateway.wake.scheduled",
                wake.id = %summary.id,
                wake.watch = summary.watch.is_some(),
                subject = %self.anchor.subject,
            },
            "wake scheduled"
        );
        Ok(summary)
    }

    fn list(&self) -> Vec<WakeSummary> {
        self.store.list(&self.anchor.subject, SystemTime::now())
    }

    fn cancel(&self, id: WakeId) -> Result<(), WakeRefusal> {
        self.store.cancel(&self.anchor.subject, id)?;
        tracing::info!(event = "gateway_wake_cancelled", wake.id = %id);
        Ok(())
    }
}

/// Runs on a blocking thread: the probe leg and the interpreter both block.
pub(crate) fn run_tick(
    tick: Tick,
    store: &WakeStore,
    broker: &ResolvedBroker,
    runtime: &tokio::runtime::Handle,
    limits: Option<ShellLimits>,
) -> Option<Fired> {
    let verdict = match (limits, tick.anchor().probe_leg(broker, runtime)) {
        (Some(limits), Some(leg)) => tick.run(&leg, limits),
        (None, _) => Verdict::Failed {
            exit: ExitCode::DENIED,
            output: "[gateway: no route answers this chat as this agent with wakes on]".to_owned(),
        },
        (Some(_), None) => Verdict::Failed {
            exit: ExitCode::DENIED,
            output: "[gateway: the broker could not be reached to run the probe]".to_owned(),
        },
    };
    let failed = matches!(verdict, Verdict::Failed { .. });
    match tick.resolve(store, verdict, SystemTime::now()) {
        Ok(Resolved::Rearmed | Resolved::Gone) => None,
        Ok(Resolved::Fired(fired)) => {
            if failed {
                tracing::warn!(event = "gateway_wake_tick_failed", wake.id = %fired.id());
            }
            Some(*fired)
        }
        Err(error) => {
            tracing::error!(event = "gateway_wake_store_failed", error = %error);
            None
        }
    }
}

pub(crate) fn fired_text(id: WakeId, note: &str, reason: &str, output: Option<&str>) -> String {
    let mut text =
        format!("[gateway: wake {id} {reason}. Your note from when it was scheduled:]\n{note}");
    if let Some(output) = output {
        text.push_str("\n[gateway: the probe printed:]\n");
        text.push_str(output);
    }
    text
}
