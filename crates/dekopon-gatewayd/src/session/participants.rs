use super::*;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

const MAX_LEGS: usize = 8;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Mapping {
    Pending,
    Unmapped,
    Mapped,
}

impl Mapping {
    pub(super) fn for_turn(route: &BoundRoute, message: &InboundMessage) -> Self {
        let shared = match route.memory {
            MemoryPolicy::OneShot => true,
            MemoryPolicy::Persistent(window) => match window.scope {
                MemoryScope::SharedConversation => true,
                MemoryScope::PrivateConversation => false,
            },
        };
        if shared
            && match message.conversation.kind {
                ConversationKind::Channel | ConversationKind::Thread => true,
                ConversationKind::DirectMessage | ConversationKind::GroupDirectMessage => false,
            }
            && chat_transport(message)
        {
            Self::Pending
        } else {
            Self::Unmapped
        }
    }
}

fn chat_transport(message: &InboundMessage) -> bool {
    use dekopon_broker_protocol::ChatTransportKind;
    match message.transport_kind {
        ChatTransportKind::Slack
        | ChatTransportKind::Discord
        | ChatTransportKind::Telegram
        | ChatTransportKind::Whatsapp
        | ChatTransportKind::Local => true,
        ChatTransportKind::Github => false,
    }
}

pub(super) fn native_chat(message: &InboundMessage) -> bool {
    chat_transport(message)
        && match message.message_id {
            MessageId::Native(_) => true,
            MessageId::Github(_)
            | MessageId::Wake { .. }
            | MessageId::Job { .. }
            | MessageId::Follow(_) => false,
        }
}

impl SessionGate {
    pub(crate) fn resolve_mapping(
        &self,
        key: &AdmissionKey,
        cancellation: &SessionCancellation,
        mapped: bool,
    ) {
        let mut entries = self.in_flight.lock();
        if let Some(Holder::Session(running)) = entries.get_mut(key)
            && Arc::ptr_eq(&running.cancellation.state, &cancellation.state)
            && running.mapping == Mapping::Pending
        {
            running.mapping = if mapped {
                Mapping::Mapped
            } else {
                Mapping::Unmapped
            };
            let mut remaining = VecDeque::new();
            while let Some(queued) = running.follow_ups.pop_front() {
                if native_chat(&queued.message) && queued.message.subject != running.subject {
                    if mapped
                        && running.cancellation.state.load(Ordering::Acquire) == SESSION_RUNNING
                    {
                        running.steers.push_back(queued.message);
                        queued.receipts.finish(MessageOutcome::Steered);
                        continue;
                    }
                    record_admission(&queued.message, "queued", remaining.len() + 1, None);
                }
                remaining.push_back(queued);
            }
            running.follow_ups = remaining;
            running
                .steers
                .make_contiguous()
                .sort_by_key(|message| message.received_at);
        }
    }
}

pub(super) struct Authorized {
    pub message: InboundMessage,
    pub principal: Option<PrincipalId>,
    pub join: Option<String>,
}

pub(super) enum Batch {
    Steers(Vec<Authorized>),
    Sealed(Seal),
}

pub(super) struct Request {
    seal: bool,
    reply: oneshot::Sender<Batch>,
}

pub(super) struct Drain {
    requests: mpsc::Sender<Request>,
    runtime: tokio::runtime::Handle,
}

impl Drain {
    pub(super) fn take(&self, seal: bool) -> Batch {
        self.runtime.block_on(async {
            let (reply, result) = oneshot::channel();
            if self.requests.send(Request { seal, reply }).await.is_err() {
                return Batch::Sealed(Seal::Stopped);
            }
            result.await.unwrap_or(Batch::Sealed(Seal::Stopped))
        })
    }
}

pub(super) fn channel(mapped: bool) -> (Option<Drain>, Option<mpsc::Receiver<Request>>) {
    if !mapped {
        return (None, None);
    }
    let (requests, receiver) = mpsc::channel(1);
    (
        Some(Drain {
            requests,
            runtime: tokio::runtime::Handle::current(),
        }),
        Some(receiver),
    )
}

pub(super) struct Inputs {
    pub broker: ResolvedBroker,
    pub route: BoundRoute,
    pub gate: SessionGate,
    pub key: AdmissionKey,
    pub cancellation: SessionCancellation,
    pub asker: ExternalSubject,
    pub leg: Arc<BrokerLeg>,
    pub calls: CallBudget,
    pub progress: Arc<dyn ProgressSink>,
}

pub(super) struct Worker {
    tasks: JoinSet<()>,
    stop: Option<oneshot::Sender<()>>,
}

impl Worker {
    pub(super) fn start(requests: Option<mpsc::Receiver<Request>>, inputs: Inputs) -> Self {
        let mut tasks = JoinSet::new();
        let stop = requests.map(|mut requests| {
            let (stop, stopped) = oneshot::channel();
            tasks.spawn(
                async move {
                    let cancellation = inputs.cancellation.clone();
                    let run = async move {
                        let mut legs = vec![(inputs.asker.clone(), Arc::clone(&inputs.leg))];
                        let mut joined = BTreeSet::from([inputs.asker.clone()]);
                        while let Some(request) = requests.recv().await {
                            let batch =
                                authorize(&inputs, &mut legs, &mut joined, request.seal).await;
                            if request.reply.send(batch).is_err() {
                                break;
                            }
                        }
                    };
                    tokio::select! {
                        () = run => {},
                        () = cancellation.cancelled() => {},
                        _ = stopped => {},
                    }
                }
                .instrument(tracing::Span::current()),
            );
            stop
        });
        Self { tasks, stop }
    }

    pub(super) async fn finish(mut self) {
        if let Some(stop) = self.stop.take() {
            if stop.send(()).is_err() {
                tracing::debug!(
                    event = "gateway_participant_worker_ended",
                    "gateway.participant.worker.ended"
                );
            }
        }
        while let Some(result) = self.tasks.join_next().await {
            if let Err(error) = result {
                tracing::error!(event = "gateway_participant_task_failed", error = %error,
                    "gateway.participant.task.failed");
            }
        }
    }
}

struct InFlight<'a> {
    inputs: &'a Inputs,
    count: usize,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        let mut entries = self.inputs.gate.in_flight.lock();
        if let Some(Holder::Session(running)) = entries.get_mut(&self.inputs.key)
            && Arc::ptr_eq(&running.cancellation.state, &self.inputs.cancellation.state)
        {
            running.authorizing -= self.count;
        }
    }
}

async fn authorize(
    inputs: &Inputs,
    legs: &mut Vec<(ExternalSubject, Arc<BrokerLeg>)>,
    joined: &mut BTreeSet<ExternalSubject>,
    seal: bool,
) -> Batch {
    let messages = {
        let mut entries = inputs.gate.in_flight.lock();
        match entries.get_mut(&inputs.key) {
            Some(Holder::Session(running)) => {
                if running.cancellation.is_cancelled() {
                    return Batch::Sealed(Seal::Stopped);
                }
                if seal && running.steers.is_empty() {
                    return Batch::Sealed(seal_locked(&entries, &inputs.key, &inputs.cancellation));
                }
                running.cancellation.rearm_model();
                let messages = running.steers.drain(..).collect::<Vec<_>>();
                running.authorizing += messages.len();
                messages
            }
            Some(Holder::Probe) | None => return Batch::Sealed(Seal::Stopped),
        }
    };
    let _in_flight = InFlight {
        inputs,
        count: messages.len(),
    };
    let mut ready = Vec::new();
    for message in messages {
        let existing = legs.iter().find(|(subject, _)| *subject == message.subject);
        let leg = match existing {
            Some((_, leg)) => Arc::clone(leg),
            None => match open(inputs, &message).await {
                Ok(leg) if !leg.granted().is_empty() && leg.principal().is_some() => Arc::new(leg),
                Ok(_) => {
                    record_admission(&message, "unauthorized", 0, Some("empty-grant"));
                    continue;
                }
                Err(error) => {
                    record_admission(&message, "unauthorized", 0, Some(refusal_reason(&error)));
                    continue;
                }
            },
        };
        if inputs.cancellation.is_cancelled() {
            return Batch::Sealed(Seal::Stopped);
        }
        let principal = leg.principal().cloned();
        let join = if joined.insert(message.subject.clone()) {
            Some(join_line(&leg))
        } else {
            None
        };
        if message.subject != inputs.asker {
            let overflow = existing.is_none() && legs.len() == MAX_LEGS;
            record_admission(
                &message,
                "steered",
                0,
                overflow.then_some("participant-limit"),
            );
            if existing.is_none() && !overflow {
                legs.push((message.subject.clone(), Arc::clone(&leg)));
            }
        }
        ready.push(Authorized {
            message,
            principal,
            join,
        });
    }
    Batch::Steers(ready)
}

async fn open(inputs: &Inputs, message: &InboundMessage) -> Result<BrokerLeg, SessionError> {
    let claim = chat_claim(&inputs.route, message)?;
    let leg = BrokerLeg::connect(inputs.broker.leg_client()?, Some(claim))
        .instrument(tracing::info_span!("gateway.participant.lookup",
            turn.owner = %inputs.asker, subject = %message.subject))
        .await?;
    Ok(leg
        .with_cancel_signal(inputs.cancellation.signal())
        .with_progress(Arc::clone(&inputs.progress), inputs.calls.clone()))
}

fn refusal_reason(error: &SessionError) -> &'static str {
    match error {
        SessionError::BrokerLeg(BrokerLegError::Client(ClientError::SessionRefused(refusal))) => {
            match refusal {
                SessionRefusal::UnmappedSubject => "unmapped-subject",
                SessionRefusal::AgentDenied { .. } => "agent-denied",
                SessionRefusal::Other => "attestation-refused",
            }
        }
        _ => error.category(),
    }
}

pub(super) fn join_line(leg: &BrokerLeg) -> String {
    let mut providers = BTreeMap::<String, BTreeSet<String>>::new();
    for capability in leg.effective_capabilities() {
        providers.entry(capability.provider).or_default().insert(
            if capability.effect == "read-only" {
                "read"
            } else {
                "write"
            }
            .to_owned(),
        );
    }
    let grants = providers
        .into_iter()
        .map(|(provider, effects)| {
            format!(
                "{provider}: {}",
                effects.into_iter().collect::<Vec<_>>().join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    bound_inbound(&format!(
        "[gateway: participant joined; channel-downscoped capabilities: {grants}]"
    ))
}

pub(super) fn with_join(prompt: String, join: Option<&str>) -> String {
    match (join, prompt.split_once('\n')) {
        (Some(join), Some((header, text))) => bound_inbound(&format!("{header}\n{join}\n{text}")),
        (Some(_), None) | (None, _) => prompt,
    }
}
