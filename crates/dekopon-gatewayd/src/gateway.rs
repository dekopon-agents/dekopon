use std::{
    collections::BTreeMap,
    future::Future,
    sync::Arc,
    time::{Duration, SystemTime},
};

use arc_swap::ArcSwap;
use dekopon_broker_protocol::{BrokerClient, ConversationKind, GenerationOutcome};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::{sync::mpsc, task::JoinSet, time::timeout};
use tracing::Instrument as _;

use crate::{
    Candidate, ServeOutcome, cancel_session, collection, config, follow, jobs, observe_session,
    routes::RoutingTable,
    session::{self, CancelOutcome, SessionRunner},
    spawn_tick,
    transport::{
        self, CancelRequest, ChatDriver, InboundMessage, MessageId, TransportEvent,
        TransportIdentity,
    },
    via_label, wake,
};

pub(crate) struct Gateway {
    runner: Arc<SessionRunner>,
    active: Arc<ArcSwap<ServingGeneration>>,
    reload: Option<ReloadState>,
    identities: Arc<BTreeMap<String, TransportIdentity>>,
    drivers: Arc<BTreeMap<String, Arc<dyn ChatDriver>>>,
}

pub(crate) struct ServingGeneration {
    routes: Arc<RoutingTable>,
    stop_words: Arc<Vec<String>>,
    _sources: Option<Arc<config::GatewaySources>>,
    _digest: String,
    _broker_digest: String,
}

struct ReloadState {
    active: Arc<ArcSwap<ServingGeneration>>,
    path: PathBuf,
    config: config::ResolvedConfig,
    sources: Arc<config::GatewaySources>,
    broker_digest: String,
    gateway_digest: String,
    pending: Option<(String, String)>,
}

fn canonical_yaml(value: &serde_yaml::Value) -> Result<Vec<u8>, serde_json::Error> {
    fn canonical_value(value: &serde_yaml::Value) -> Result<serde_json::Value, serde_json::Error> {
        match value {
            serde_yaml::Value::Mapping(mapping) => {
                let fields = mapping
                    .iter()
                    .map(|(key, value)| {
                        Ok((
                            match key.as_str() {
                                Some(key) => key.to_owned(),
                                None => serde_json::to_string(key)?,
                            },
                            canonical_value(value)?,
                        ))
                    })
                    .collect::<Result<BTreeMap<_, _>, serde_json::Error>>()?;
                serde_json::to_value(fields)
            }
            serde_yaml::Value::Sequence(items) => {
                let items = items
                    .iter()
                    .map(canonical_value)
                    .collect::<Result<Vec<_>, _>>()?;
                serde_json::to_value(items)
            }
            serde_yaml::Value::Tagged(tagged) => {
                serde_json::to_value((tagged.tag.to_string(), canonical_value(&tagged.value)?))
            }
            serde_yaml::Value::Null
            | serde_yaml::Value::Bool(_)
            | serde_yaml::Value::Number(_)
            | serde_yaml::Value::String(_) => serde_json::to_value(value),
        }
    }
    serde_json::to_vec(&canonical_value(value)?)
}

fn serving_digest(
    candidate: &Candidate,
    broker_digest: &str,
) -> Result<String, config::ConfigError> {
    let mut hasher = Sha256::new();
    hasher.update((broker_digest.len() as u64).to_be_bytes());
    hasher.update(broker_digest.as_bytes());
    let mapping = config::merged_mapping(&candidate.sources.config)?;
    let encoded = canonical_yaml(&serde_yaml::Value::Mapping(mapping))
        .map_err(|_error| config::ConfigError::UnclassifiedSource)?;
    hasher.update((encoded.len() as u64).to_be_bytes());
    hasher.update(encoded);
    for (path, bytes) in &candidate.sources.catalog.0 {
        let effective = match (
            path == &candidate.config.catalog_path,
            path.extension().and_then(|extension| extension.to_str()),
        ) {
            (true, Some("yaml" | "yml" | "json")) => {
                let value: serde_yaml::Value = serde_yaml::from_slice(bytes)
                    .map_err(|_error| config::ConfigError::UnclassifiedSource)?;
                canonical_yaml(&value).map_err(|_error| config::ConfigError::UnclassifiedSource)?
            }
            _ => bytes.clone(),
        };
        hasher.update((path.as_os_str().len() as u64).to_be_bytes());
        hasher.update(path.as_os_str().as_encoded_bytes());
        hasher.update((effective.len() as u64).to_be_bytes());
        hasher.update(effective);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

impl Gateway {
    pub(crate) fn new(
        runner: Arc<SessionRunner>,
        routes: Arc<RoutingTable>,
        identities: Arc<BTreeMap<String, TransportIdentity>>,
        drivers: Arc<BTreeMap<String, Arc<dyn ChatDriver>>>,
        stop_words: Arc<Vec<String>>,
    ) -> Self {
        Self {
            runner,
            active: Arc::new(ArcSwap::from_pointee(ServingGeneration {
                routes,
                stop_words,
                _sources: None,
                _digest: String::new(),
                _broker_digest: String::new(),
            })),
            reload: None,
            identities,
            drivers,
        }
    }

    pub(crate) fn with_reload(
        mut self,
        path: PathBuf,
        candidate: Candidate,
        broker_digest: String,
    ) -> Result<Self, config::ConfigError> {
        let gateway_digest = serving_digest(&candidate, &broker_digest)?;
        let sources = Arc::new(candidate.sources);
        self.active.store(Arc::new(ServingGeneration {
            routes: Arc::new(candidate.routes),
            stop_words: Arc::new(candidate.config.stop_words.clone()),
            _sources: Some(Arc::clone(&sources)),
            _digest: gateway_digest.clone(),
            _broker_digest: broker_digest.clone(),
        }));
        self.reload = Some(ReloadState {
            active: Arc::clone(&self.active),
            path,
            config: candidate.config,
            sources,
            broker_digest,
            gateway_digest,
            pending: None,
        });
        Ok(self)
    }

    #[cfg(test)]
    pub(crate) async fn reconcile(&mut self) {
        if let Some(reload) = self.reload.as_mut() {
            reload.reconcile().await;
        }
    }

    #[cfg(test)]
    pub(crate) fn route_for_test(
        &self,
        message: &InboundMessage,
    ) -> Option<crate::routes::BoundRoute> {
        self.active.load().routes.route(message).cloned()
    }

    #[cfg(test)]
    pub(crate) fn digest_for_test(&self) -> String {
        self.active.load()._digest.clone()
    }

    #[cfg(test)]
    pub(crate) fn broker_digest_for_test(&self) -> String {
        self.active.load()._broker_digest.clone()
    }

    #[cfg(test)]
    pub(crate) fn generation_for_test(&self) -> Arc<ServingGeneration> {
        self.active.load_full()
    }

    #[cfg(test)]
    pub(crate) fn start_with_generation_for_test(
        &self,
        sessions: &mut JoinSet<()>,
        generation: &ServingGeneration,
        message: InboundMessage,
    ) {
        self.start_session(sessions, generation, message);
    }

    #[cfg(test)]
    pub(crate) fn snapshot_for_test(
        &self,
        message: &InboundMessage,
    ) -> Option<(crate::routes::BoundRoute, Arc<Vec<String>>)> {
        let active = self.active.load();
        Some((
            active.routes.route(message)?.clone(),
            Arc::clone(&active.stop_words),
        ))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one bounded serving select owns all event sources and shutdown"
    )]
    pub(crate) async fn serve<F>(
        mut self,
        mut receiver: mpsc::Receiver<TransportEvent>,
        shutdown: F,
        grace: Duration,
        mut collector: collection::Collector,
    ) -> ServeOutcome
    where
        F: Future<Output = ()> + Send,
    {
        let mut sessions = JoinSet::new();
        let mut reloading = JoinSet::new();
        if let Some(reload) = self.reload.take() {
            reloading.spawn(reload.run());
        }
        let mut follow_pending = follow::Debounce::default();
        let mut ticks = JoinSet::new();
        let mut wakes = self.runner.wakes.clone();
        let mut notices = self.runner.jobs.take_notices();
        tokio::pin!(shutdown);
        let mut outcome = ServeOutcome::Shutdown;

        loop {
            while let Some(result) = sessions.try_join_next() {
                observe_session(result);
            }
            let deadline = collector.deadline();
            let follow_deadline = follow_pending.deadline();
            let wake_deadline = wakes.as_ref().and_then(|store| store.next_at()).map(|at| {
                tokio::time::Instant::now()
                    + at.duration_since(SystemTime::now()).unwrap_or_default()
            });
            tokio::select! {
                biased;
                () = &mut shutdown => break,
                () = async {
                    match wake_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => {
                    let Some(store) = wakes.clone() else { continue };
                    let reading = Arc::clone(&store);
                    let due = tokio::task::spawn_blocking(move || reading.take_due(SystemTime::now())).await;
                    match due {
                        Ok(Ok(due)) => {
                            for fired in due.fired {
                                self.start_wake(&mut sessions, fired);
                            }
                            for tick in due.ticks {
                                spawn_tick(&self.runner, &self.active.load().routes, &store, &mut ticks, tick);
                            }
                        }
                        Ok(Err(error)) => {
                            tracing::error!(event = "gateway_wake_store_failed", error = %error, wakes = "stopped");
                            wakes = None;
                        }
                        Err(_) => {
                            tracing::error!(event = "gateway_wake_store_failed", wakes = "stopped");
                            wakes = None;
                        }
                    }
                },
                Some(result) = ticks.join_next() => {
                    match result {
                        Ok(Some(fired)) => self.start_wake(&mut sessions, fired),
                        Ok(None) => {}
                        Err(_) => tracing::error!(event = "gateway_wake_task_failed"),
                    }
                },
                () = async {
                    match follow_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => {
                    for message in follow_pending.due(tokio::time::Instant::now()) {
                        let generation = self.active.load();
                        if let Some(route) = generation.routes.route(&message)
                            && let Some(table) = self.runner.follow.as_ref()
                        {
                            let key = follow::follow_key(&route.transport, route.agent.as_str(), &route.conversation, &message.conversation.key());
                            if matches!(table.ticket(&key, SystemTime::now()), follow::FollowTicket::Active { .. }) {
                                self.start_session(&mut sessions, &generation, message);
                            }
                        }
                    }
                },
                () = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => {
                    for message in collector.take_due(tokio::time::Instant::now()) {
                        self.start_session(&mut sessions, &self.active.load(), message);
                    }
                },
                Some(message) = notices.recv() => {
                    let MessageId::Job { agent, .. } = &message.message_id else {
                        continue;
                    };
                    let generation = self.active.load();
                    let answering = generation.routes.route(&message).is_some_and(|route| {
                        route.job_timeout.is_some() && &route.agent == agent
                    });
                    if answering && self.drivers.contains_key(&message.transport) {
                        self.start_session(&mut sessions, &generation, message);
                    } else if let Some(notice) = jobs::Notice::of(&message) {
                        notice.record("dropped");
                    }
                },
                event = receiver.recv() => {
                    let Some(event) = event else {
                        outcome = ServeOutcome::TransportsLost;
                        break;
                    };
                    match event {
                        TransportEvent::Connected { name, identity } => {
                            Arc::make_mut(&mut self.identities).insert(name, identity);
                        }
                        TransportEvent::Message(message) => {
                            let received = message.receive_span.clone();
                            received.in_scope(|| {
                                self.dispatch(
                                    &mut sessions,
                                    &mut collector,
                                    &mut follow_pending,
                                    *message,
                                );
                            });
                        }
                        TransportEvent::CancelRequested(request) => {
                            if collector.cancel(&request) {
                                tracing::info!(event = "gateway_input_stop_requested", transport = %request.transport, via = via_label(request.via));
                            }
                            cancel_session(&self.runner, &request);
                        },
                    }
                }
            }
        }

        reloading.shutdown().await;
        drop(notices);
        collector.shutdown();
        drop(follow_pending);
        self.runner.jobs.seal();

        // A tick that fires here has already retired its row, so its wake must still start.
        if timeout(grace, async {
            while let Some(result) = ticks.join_next().await {
                if let Ok(Some(fired)) = result {
                    self.start_wake(&mut sessions, fired);
                }
            }
            while let Some(result) = sessions.join_next().await {
                observe_session(result);
            }
        })
        .await
        .is_err()
        {
            tracing::warn!(event = "gateway_sessions_abandoned");
            // A blocking probe cannot be aborted; the runtime's shutdown timeout bounds it.
            ticks.detach_all();
            sessions.abort_all();
            while sessions.join_next().await.is_some() {}
        }
        outcome
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one dispatch entry owns stop handling, addressing, and bounded collection"
    )]
    pub(crate) fn dispatch(
        &self,
        sessions: &mut JoinSet<()>,
        collector: &mut collection::Collector,
        follow_pending: &mut follow::Debounce,
        message: InboundMessage,
    ) {
        // Checked before the route lookup and the addressed filter: a reload can unroute a conversation
        // whose person still owns jobs, and a channel stop word like the bot mention plus stop would
        // otherwise be dropped as unaddressed; only this sender's running or pending work may be stopped.
        let generation = self.active.load();
        if transport::is_stop_word(
            self.identities.get(&message.transport),
            &message.text,
            &generation.stop_words,
        ) {
            let mut banished = false;
            if let Some(route) = generation.routes.route(&message)
                && route.follow
                && let Some(table) = self.runner.follow.as_ref()
            {
                let key = follow::follow_key(
                    &route.transport,
                    route.agent.as_str(),
                    &route.conversation,
                    &message.conversation.key(),
                );
                follow_pending.remove(&key);
                if matches!(
                    table.ticket(&key, SystemTime::now()),
                    follow::FollowTicket::Active { .. }
                ) {
                    table.suspend(&key);
                    banished = true;
                    let table = Arc::clone(table);
                    let index = route.route_index;
                    sessions.spawn(async move {
                        match tokio::task::spawn_blocking(move || table.leave(&key)).await {
                            Ok(Ok(())) => tracing::info!(
                                audit.event = "gateway.follow",
                                operation = "banished",
                                route = index
                            ),
                            _ => tracing::warn!(
                                audit.event = "gateway.follow",
                                operation = "refused",
                                route = index,
                                reason = "persistence"
                            ),
                        }
                    });
                }
            }
            let request = CancelRequest {
                transport: message.transport.clone(),
                conversation_id: message.conversation.key(),
                subject: message.subject.canonical(),
                via: dekopon_agent::CancelVia::StopReply,
            };
            let stopped = self.runner.gate.cancel(&request);
            let pending_cancelled = collector.cancel(&request) || stopped.dropped;
            let active_outcome = stopped.running;
            let jobs_stopped = self.runner.jobs.cancel_owner(&request);
            if pending_cancelled {
                if matches!(
                    active_outcome,
                    CancelOutcome::NoSession
                        | CancelOutcome::OtherSubject
                        | CancelOutcome::Completing
                ) && let Some(driver) = self.drivers.get(&message.transport).cloned()
                    && let Some(reply) = self.runner.gate.refusal()
                {
                    let receipt = message.receive_span.clone();
                    sessions.spawn(
                        async move {
                            let _reply = reply;
                            session::answer(&driver, &message, session::STOPPED_REPLY).await;
                        }
                        .instrument(receipt),
                    );
                }
                return;
            }
            match active_outcome {
                CancelOutcome::Cancelled => {
                    tracing::info!(
                        event = "gateway_session_stop_requested",
                        transport = %request.transport,
                        via = "stop-reply"
                    );
                    return;
                }
                outcome @ (CancelOutcome::AlreadyCancelled | CancelOutcome::Completing) => {
                    tracing::debug!(
                        event = "gateway_session_stop_ignored",
                        transport = %request.transport,
                        reason = outcome.ignored_reason()
                    );
                    return;
                }
                CancelOutcome::NoSession | CancelOutcome::OtherSubject => {}
            }
            if jobs_stopped {
                if let Some(driver) = self.drivers.get(&message.transport).cloned()
                    && let Some(reply) = self.runner.gate.refusal()
                {
                    let receipt = message.receive_span.clone();
                    sessions.spawn(
                        async move {
                            let _reply = reply;
                            session::answer(&driver, &message, session::STOPPED_REPLY).await;
                        }
                        .instrument(receipt),
                    );
                }
                return;
            }
            if banished {
                return;
            }
        }
        let Some((route_id, _)) = generation.routes.route_index(&message) else {
            tracing::debug!(
                event = "gateway_message_ignored",
                transport = %message.transport,
                reason = "unrouted",
                conversation.kind = message.conversation.kind.as_str(),
                conversation.container = message.conversation.container.as_deref().unwrap_or_default()
            );
            return;
        };
        let addressed = message.addressed.unwrap_or_else(|| {
            self.identities
                .get(&message.transport)
                .is_some_and(|identity| identity.is_addressed(&message.text))
        });
        let inherited_thread = message
            .thread_continuation
            .as_ref()
            .is_some_and(|continuation| continuation.inherited);
        if addressed
            && let Some(route) = generation.routes.route(&message)
            && route.follow
        {
            let key = follow::follow_key(
                &route.transport,
                route.agent.as_str(),
                &route.conversation,
                &message.conversation.key(),
            );
            follow_pending.cancel(&key, &message.subject);
        }
        if message.conversation.kind != ConversationKind::DirectMessage
            && !addressed
            && !inherited_thread
            && !collector.is_native_continuation(route_id, &message)
        {
            if let Some(route) = generation.routes.route(&message)
                && route.follow
                && let Some(table) = self.runner.follow.as_ref()
            {
                let key = follow::follow_key(
                    &route.transport,
                    route.agent.as_str(),
                    &route.conversation,
                    &message.conversation.key(),
                );
                if matches!(
                    table.ticket(&key, SystemTime::now()),
                    follow::FollowTicket::Active { .. }
                ) {
                    if !follow_pending.offer(key, message, tokio::time::Instant::now()) {
                        tracing::info!(
                            audit.event = "gateway.follow",
                            operation = "refused",
                            route = route_id,
                            reason = "capacity"
                        );
                    }
                    return;
                }
            }
            tracing::debug!(
                event = "gateway_message_ignored",
                transport = %message.transport,
                reason = "not-addressed"
            );
            return;
        }
        match collector.offer(route_id, message) {
            collection::Offered::Pending => {}
            collection::Offered::Immediate(message) => {
                self.start_session(sessions, &generation, message);
            }
            collection::Offered::Refused(message, reason) => {
                collection::disposition(&message.receive_span, reason);
                if let Some(driver) = self.drivers.get(&message.transport).cloned()
                    && let Some(permit) = self.runner.gate.refusal()
                {
                    let receipt = message.receive_span.clone();
                    sessions.spawn(async move {
                    let _permit = permit;
                    let reply = match reason {
                        "collection-full" => "Busy collecting other requests. Please try again shortly.",
                        "incompatible-group" => "Another media group is still being collected. Please retry this group separately.",
                        "deadline-overflow" => "Input refused: the configured media collection deadline cannot be represented.",
                        _ => "Input refused: too many attachments or messages, or too much text. Please send a smaller request.",
                    };
                    session::answer(&driver, &message, reply).await;
                }.instrument(receipt));
                }
            }
        }
    }

    fn start_session(
        &self,
        sessions: &mut JoinSet<()>,
        generation: &ServingGeneration,
        message: InboundMessage,
    ) {
        let Some(route) = generation.routes.route(&message) else {
            collection::disposition(&message.receive_span, "route-lost");
            return;
        };
        let Some(driver) = self.drivers.get(&message.transport).cloned() else {
            message.receive_span.in_scope(|| {
                tracing::error!(event = "gateway_message_ignored", reason = "no-driver")
            });
            collection::disposition(&message.receive_span, "no-driver");
            return;
        };
        let active = Arc::clone(&self.active);
        sessions.spawn(session::run_session_resolving(
            Arc::clone(&self.runner),
            route.clone(),
            message,
            driver,
            move |message, _queued| active.load().routes.route(message).cloned(),
        ));
    }

    /// A wake whose route no longer answers as its agent is delivered as its note, so the person still
    /// hears what they asked to be reminded of.
    fn start_wake(&self, sessions: &mut JoinSet<()>, fired: wake::Fired) {
        let agent = fired.agent().clone();
        let id = fired.id();
        let message = fired.into_inbound();
        let generation = self.active.load();
        let answering = generation
            .routes
            .route(&message)
            .is_some_and(|route| route.wakes && route.agent == agent);
        let receipt = message.receive_span.clone();
        receipt.in_scope(|| tracing::info!(event = "gateway_wake_fired", wake.id = %id));
        if answering {
            self.start_session(sessions, &generation, message);
            return;
        }
        receipt.in_scope(|| tracing::warn!(event = "gateway_wake_orphaned", wake.id = %id));
        if let Some(driver) = self.drivers.get(&message.transport).cloned()
            && let MessageId::Wake { notice, .. } = &message.message_id
        {
            let notice = notice.clone();
            sessions.spawn(
                async move {
                    session::answer(&driver, &message, &notice).await;
                }
                .instrument(receipt),
            );
        }
    }
}

impl ReloadState {
    async fn run(mut self) {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.reset();
        loop {
            tick.tick().await;
            self.reconcile().await;
        }
    }

    async fn reconcile(&mut self) {
        let (candidate, report) = crate::load_candidate_against(
            &self.path,
            None,
            config::LoadMode::Boot,
            None,
            Some((&self.config, &self.sources)),
        )
        .await;
        let Some(candidate) = candidate else {
            tracing::warn!(
                event = "gateway_reload_refused",
                applied = false,
                problems = report.problems.len()
            );
            return;
        };
        let broker = &candidate.config.broker;
        let Ok(client) = BrokerClient::new(&broker.socket_path, broker.server_uid, broker.frame)
        else {
            tracing::warn!(event = "gateway_reload_monitor_failed");
            return;
        };
        if let Some((pending_broker, pending_gateway)) = self.pending.clone() {
            let (observed, outcome) = match client.observe_generation().await {
                Ok(observation) => observation,
                Err(_) => {
                    tracing::warn!(event = "gateway_reload_monitor_failed");
                    return;
                }
            };
            if outcome != GenerationOutcome::Active {
                tracing::warn!(event = "gateway_reload_monitor_failed");
                return;
            }
            let Ok(digest) = serving_digest(&candidate, &pending_broker) else {
                return;
            };
            if observed == pending_broker && digest == pending_gateway {
                self.settle(candidate, observed, digest);
                return;
            }
            self.broker_digest = observed;
            self.pending = None;
        }
        let expected_broker = self.broker_digest.clone();
        let (broker_digest, outcome) =
            match client.prepare_generation(expected_broker.clone()).await {
                Ok(result) => result,
                Err(_) => {
                    tracing::warn!(event = "gateway_reload_monitor_failed");
                    return;
                }
            };
        let Ok(digest) = serving_digest(&candidate, &broker_digest) else {
            return;
        };
        match outcome {
            GenerationOutcome::Active if broker_digest == expected_broker => {
                self.settle(candidate, broker_digest, digest);
                return;
            }
            GenerationOutcome::Prepared => {}
            GenerationOutcome::Refused => {
                self.broker_digest = broker_digest;
                tracing::warn!(
                    event = "gateway_reload_refused",
                    applied = false,
                    reason = "broker-prepare"
                );
                return;
            }
            GenerationOutcome::Active | GenerationOutcome::Applied | GenerationOutcome::Aborted => {
                tracing::warn!(
                    event = "gateway_reload_refused",
                    applied = false,
                    reason = "broker-prepare"
                );
                return;
            }
        }
        let confirmed = match client.commit_generation(broker_digest.clone()).await {
            Ok(GenerationOutcome::Applied) => true,
            Ok(_) => false,
            Err(_) => matches!(client.observe_generation().await,
                Ok((observed, GenerationOutcome::Active)) if observed == broker_digest),
        };
        if !confirmed {
            self.pending = Some((broker_digest, digest));
            tracing::warn!(
                event = "gateway_reload_refused",
                applied = false,
                reason = "broker-confirmation"
            );
            return;
        }
        self.settle(candidate, broker_digest, digest);
    }

    fn settle(&mut self, candidate: Candidate, broker_digest: String, digest: String) {
        self.pending = None;
        if digest == self.gateway_digest {
            self.broker_digest = broker_digest;
            return;
        }
        let sources = Arc::new(candidate.sources);
        self.active.store(Arc::new(ServingGeneration {
            routes: Arc::new(candidate.routes),
            stop_words: Arc::new(candidate.config.stop_words.clone()),
            _sources: Some(Arc::clone(&sources)),
            _digest: digest.clone(),
            _broker_digest: broker_digest.clone(),
        }));
        self.config = candidate.config;
        self.sources = sources;
        self.broker_digest = broker_digest;
        tracing::info!(event = "gateway_reload_applied", generation.digest = %digest);
        self.gateway_digest = digest;
    }
}
