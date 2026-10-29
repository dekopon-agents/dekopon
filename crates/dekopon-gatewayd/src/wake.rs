use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use dekopon_agent::{
    BrokerLeg, current_trace_parent,
    wake::{WakeId, WakeRefusal, WakeRegistrar, WakeRequest, WakeSummary},
};
use dekopon_broker_protocol::{
    Attestation, ChatScopeClaim, ChatTransportKind, Conversation, TraceParent, Trigger,
};
use dekopon_core::{AgentId, ExternalSubject, TransportId};
use dekopon_process::ExitCode;
use dekopon_process::Limits as ShellLimits;
use dekopon_shell::{CapabilityInvoker, Interpreter, ScriptOutcome};

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

pub(crate) fn space_wake(message: &InboundMessage) -> bool {
    message.subject.kind() == dekopon_core::PrincipalKind::Space
        && match message.message_id {
            MessageId::Wake { .. } => true,
            MessageId::Native(_)
            | MessageId::Github(_)
            | MessageId::Follow(_)
            | MessageId::Job { .. } => false,
        }
}

pub(crate) struct ReviewSubscriptions {
    pub(crate) anchor: Anchor,
    pub(crate) store: Arc<WakeStore>,
    pub(crate) links: crate::pull_request_links::PullRequestLinkContext,
}

impl ReviewSubscriptions {
    pub(crate) fn reviewed(
        &self,
        capability: &str,
        input: &serde_json::Value,
        result: dekopon_shell::CapabilityCallResult,
    ) -> dekopon_shell::CapabilityCallResult {
        use dekopon_shell::CapabilityCallResult;
        if capability != "gh.pull-request.comment"
            || !match result {
                CapabilityCallResult::Succeeded | CapabilityCallResult::SucceededWithStderr(_) => {
                    true
                }
                CapabilityCallResult::Failed { .. }
                | CapabilityCallResult::Denied { .. }
                | CapabilityCallResult::Exited { .. }
                | CapabilityCallResult::NotFound => false,
            }
        {
            return result;
        }
        for target in &self.links.targets {
            let (owner, repo) = target.coordinates();
            if input
                .get("owner")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|actual| actual.eq_ignore_ascii_case(owner))
                && input
                    .get("repo")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(repo))
                && input.get("number").and_then(serde_json::Value::as_u64)
                    == Some(u64::from(target.number))
                && let Err(error) =
                    self.store
                        .subscribe(self.anchor.clone(), target.clone(), SystemTime::now())
            {
                tracing::info!(
                    audit.event = "gateway.admission",
                    admission.trigger = "pr-subscription",
                    subscription.operation = "ignored",
                    reason = error.telemetry_kind(),
                    "gateway.admission"
                );
                let note = format!(
                    "The GitHub review succeeded, but its PR subscription was not saved: {error}"
                );
                return match result {
                    CapabilityCallResult::Succeeded => {
                        CapabilityCallResult::SucceededWithStderr(note)
                    }
                    CapabilityCallResult::SucceededWithStderr(stderr) => {
                        CapabilityCallResult::SucceededWithStderr(format!("{stderr}\n{note}"))
                    }
                    CapabilityCallResult::Failed { .. }
                    | CapabilityCallResult::Denied { .. }
                    | CapabilityCallResult::Exited { .. }
                    | CapabilityCallResult::NotFound => result,
                };
            }
        }
        result
    }
}

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
    pub(crate) fn as_space(&self) -> Option<Self> {
        use dekopon_broker_protocol::ConversationKind;
        match self.conversation.kind {
            ConversationKind::Channel | ConversationKind::Thread => {}
            ConversationKind::DirectMessage | ConversationKind::GroupDirectMessage => return None,
        }
        let service = match self.kind {
            ChatTransportKind::Slack => "slack",
            ChatTransportKind::Fauxlack => "fauxlack",
            ChatTransportKind::Discord
            | ChatTransportKind::Telegram
            | ChatTransportKind::Whatsapp
            | ChatTransportKind::Github
            | ChatTransportKind::Local => return None,
        };
        let mut anchor = self.clone();
        anchor.subject = format!(
            "{service}.{}.{}",
            self.conversation.container.as_deref()?,
            self.conversation.id
        )
        .parse()
        .ok()?;
        Some(anchor)
    }

    pub(crate) fn same_channel(&self, other: &Self) -> bool {
        self.agent == other.agent
            && self.transport == other.transport
            && self.subject == other.subject
            && self.conversation.container == other.conversation.container
            && self.conversation.id == other.conversation.id
    }

    pub(crate) fn same_thread(&self, other: &Self) -> bool {
        self.same_channel(other) && self.conversation.thread == other.conversation.thread
    }
    /// A local reply target is a connection number that a restart hands to another client.
    pub(crate) fn from_inbound(message: &InboundMessage, agent: &AgentId) -> Option<Self> {
        if matches!(message.reply, ReplyTarget::Local { .. }) {
            return None;
        }
        Self::for_job(message, agent)
    }

    /// A job ends inside the process that started it, so a local connection number is still the
    /// client it names.
    pub(crate) fn for_job(message: &InboundMessage, agent: &AgentId) -> Option<Self> {
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
                participant: None,
                github: None,
                transport: self.transport.clone(),
                kind: self.kind,
                conversation: self.conversation.clone(),
                delivery: None,
                trigger,
            },
        )
    }

    pub(crate) fn job_inbound(
        &self,
        id: dekopon_process::JobId,
        text: String,
        job: Option<TraceParent>,
    ) -> InboundMessage {
        self.inbound_message(
            MessageId::Job {
                id,
                agent: self.agent.clone(),
            },
            text,
            job,
        )
    }

    pub(crate) fn inbound(&self, id: WakeId, text: String, notice: String) -> InboundMessage {
        self.inbound_message(MessageId::Wake { id, notice }, text, self.scheduled_in)
    }

    fn inbound_message(
        &self,
        message_id: MessageId,
        text: String,
        parent: Option<TraceParent>,
    ) -> InboundMessage {
        let span = receive_span(self.kind);
        if let Some(parent) = parent {
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
            pull_request_links: None,
            transport: self.transport.to_string(),
            transport_kind: self.kind,
            subject: self.subject.clone(),
            conversation: self.conversation.clone(),
            message_id,
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
            folded: Vec::new(),
        }
    }

    fn probe_leg(
        &self,
        broker: &ResolvedBroker,
        runtime: &tokio::runtime::Handle,
    ) -> Option<BrokerLeg> {
        let client = broker
            .leg_client()
            .inspect_err(|error| {
                tracing::warn!(
                    event = "gateway_wake_probe_unavailable",
                    error = %error,
                    "gateway.wake.probe.unavailable"
                );
            })
            .ok()?;
        runtime
            .block_on(BrokerLeg::connect(client, Some(self.claim(Trigger::Probe))))
            .inspect_err(|error| {
                tracing::warn!(
                    event = "gateway_wake_probe_unavailable",
                    error = %error,
                    "gateway.wake.probe.unavailable"
                );
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
    #[cfg(test)]
    pub(crate) fn scripted_for_test(script: &str) -> Self {
        Self {
            script: script.to_owned(),
            prev: String::new(),
        }
    }

    pub(crate) fn baseline(
        script: String,
        leg: &dyn CapabilityInvoker,
        shell: &Interpreter,
    ) -> Result<Self, WakeRefusal> {
        match Verdict::from(shell.run(&script, leg)) {
            Verdict::Fire { output } | Verdict::Wait { output } => Ok(Self {
                script,
                prev: output,
            }),
            Verdict::Failed { exit, output } => Err(WakeRefusal::Broken {
                exit: exit.get(),
                output,
            }),
        }
    }

    pub(crate) fn tick(&self, leg: &dyn CapabilityInvoker, shell: &Interpreter) -> Verdict {
        Verdict::from(shell.run_with_prev(&self.script, &self.prev, leg))
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
    shell: Interpreter,
    started: SystemTime,
    space_access: bool,
    job_control: Option<(Arc<crate::jobs::Jobs>, ShellLimits)>,
}

impl SessionWakes {
    pub(crate) fn new(
        anchor: Anchor,
        store: Arc<WakeStore>,
        broker: ResolvedBroker,
        runtime: tokio::runtime::Handle,
        shell: Interpreter,
        job_control: Option<(Arc<crate::jobs::Jobs>, ShellLimits)>,
    ) -> Self {
        Self {
            job_control,
            bounds: store.bounds(),
            anchor,
            store,
            broker,
            runtime,
            shell,
            started: SystemTime::now(),
            space_access: false,
        }
    }

    pub(crate) fn with_space_access(mut self, allowed: bool) -> Self {
        self.space_access = allowed;
        self
    }

    fn space_anchor(&self) -> Result<Anchor, WakeRefusal> {
        self.space_access
            .then(|| self.anchor.as_space())
            .flatten()
            .ok_or(WakeRefusal::MappedChannelRequired)
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
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
    fn list_subscriptions(&self) -> Result<Vec<WakeSummary>, WakeRefusal> {
        self.store
            .list_subscriptions(&self.space_anchor()?, SystemTime::now())
    }

    fn cancel_subscription(&self, id: WakeId) -> Result<(), WakeRefusal> {
        self.store.cancel_subscription(&self.space_anchor()?, id)
    }
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
                if every < self.bounds.min_interval || every > until {
                    return Err(WakeRefusal::Interval {
                        minimum: self.bounds.min_interval,
                    });
                }
                let mut leg = self
                    .anchor
                    .probe_leg(&self.broker, &self.runtime)
                    .ok_or(WakeRefusal::Unavailable)?;
                if let Some((jobs, limits)) = self.job_control.as_ref() {
                    leg = leg.with_job_control(Arc::new(crate::jobs::JobContext::from_probe(
                        Arc::clone(jobs),
                        self.anchor.clone(),
                        self.broker.clone(),
                        self.runtime.clone(),
                        *limits,
                    )));
                }
                let probe = Probe::baseline(script, &leg, &self.shell)?;
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
            target: "dekopon_gatewayd::audit",
            {
                audit.event = "gateway.wake.scheduled",
                wake.id = %summary.id,
                wake.watch = summary.watch.is_some(),
                subject = %self.anchor.subject,
            },
            "gateway.wake.scheduled"
        );
        Ok(summary)
    }

    fn list(&self) -> Vec<WakeSummary> {
        self.store.list(&self.anchor.subject, SystemTime::now())
    }

    fn cancel(&self, id: WakeId) -> Result<(), WakeRefusal> {
        self.store.cancel(&self.anchor.subject, id)?;
        tracing::info!(event = "gateway_wake_cancelled", wake.id = %id, "gateway.wake.cancelled");
        Ok(())
    }
}

/// Runs on a blocking thread: the probe leg and the interpreter both block.
pub(crate) fn run_tick(
    tick: Tick,
    store: &WakeStore,
    broker: &ResolvedBroker,
    runtime: &tokio::runtime::Handle,
    shell: Option<Interpreter>,
    job_control: Option<(Arc<crate::jobs::Jobs>, ShellLimits)>,
) -> Option<Fired> {
    let verdict = match shell {
        None => Verdict::Failed {
            exit: ExitCode::DENIED,
            output: "[gateway: no route answers this chat as this agent with wakes on]".to_owned(),
        },
        Some(shell) => match tick.anchor().probe_leg(broker, runtime) {
            Some(mut leg) => {
                if let Some((jobs, job_limits)) = job_control {
                    leg = leg.with_job_control(Arc::new(crate::jobs::JobContext::from_probe(
                        jobs,
                        tick.anchor().clone(),
                        broker.clone(),
                        runtime.clone(),
                        job_limits,
                    )));
                }
                tick.run(&leg, &shell)
            }
            None => Verdict::Failed {
                exit: ExitCode::DENIED,
                output: "[gateway: the broker could not be reached to run the probe]".to_owned(),
            },
        },
    };
    let failed = matches!(verdict, Verdict::Failed { .. });
    match tick.resolve(store, verdict, SystemTime::now()) {
        Ok(Resolved::Rearmed | Resolved::Gone) => None,
        Ok(Resolved::Fired(fired)) => {
            if failed {
                tracing::warn!(
                    event = "gateway_wake_tick_failed",
                    wake.id = %fired.id(),
                    "gateway.wake.tick.failed"
                );
            }
            Some(*fired)
        }
        Err(error) => {
            tracing::error!(
                event = "gateway_wake_store_failed",
                error = %error,
                "gateway.wake.store.failed"
            );
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
