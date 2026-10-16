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

struct ServingGeneration {
    routes: Arc<RoutingTable>,
    stop_words: Arc<Vec<String>>,
    _sources: Option<Arc<config::GatewaySources>>,
    _digest: String,
    _broker_digest: String,
}

struct ReloadState {
    path: PathBuf,
    config: config::ResolvedConfig,
    sources: Arc<config::GatewaySources>,
    broker_digest: String,
    gateway_digest: String,
    pending: Option<(String, String)>,
}

fn serving_digest(candidate: &Candidate) -> Result<String, config::ConfigError> {
    let mut hasher = Sha256::new();
    let mapping = config::merged_mapping(&candidate.sources.config)?;
    let encoded =
        serde_json::to_vec(&mapping).map_err(|_error| config::ConfigError::UnclassifiedSource)?;
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
                serde_json::to_vec(&value)
                    .map_err(|_error| config::ConfigError::UnclassifiedSource)?
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
        let gateway_digest = serving_digest(&candidate)?;
        let sources = Arc::new(candidate.sources);
        self.active.store(Arc::new(ServingGeneration {
            routes: Arc::new(candidate.routes),
            stop_words: Arc::new(candidate.config.stop_words.clone()),
            _sources: Some(Arc::clone(&sources)),
            _digest: gateway_digest.clone(),
            _broker_digest: broker_digest.clone(),
        }));
        self.reload = Some(ReloadState {
            path,
            config: candidate.config,
            sources,
            broker_digest,
            gateway_digest,
            pending: None,
        });
        Ok(self)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one broker-first coordination transaction owns confirmation and publication"
    )]
    pub(crate) async fn reconcile(&mut self) {
        let Some(state) = self.reload.as_ref() else {
            return;
        };
        let path = state.path.clone();
        let (candidate, report) = crate::load_candidate_against(
            &path,
            None,
            config::LoadMode::Boot,
            None,
            Some((&state.config, &state.sources)),
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
        let old_gateway = state.gateway_digest.clone();
        let old_broker = state.broker_digest.clone();
        let pending = state.pending.clone();
        let digest = match serving_digest(&candidate) {
            Ok(digest) => digest,
            Err(_) => return,
        };
        let broker = &candidate.config.broker;
        let Ok(client) = BrokerClient::new(&broker.socket_path, broker.server_uid, broker.frame)
        else {
            tracing::warn!(event = "gateway_reload_monitor_failed");
            return;
        };
        if let Some((pending_broker, pending_gateway)) = &pending {
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
            if observed == *pending_broker && digest == *pending_gateway {
                if candidate
                    .sources
                    .still_current(&path, &candidate.config.catalog_path, crate::current_uid())
                    .await
                    .unwrap_or(false)
                {
                    if digest == old_gateway {
                        if let Some(active) = self.reload.as_mut() {
                            active.broker_digest = observed;
                            active.pending = None;
                        }
                    } else {
                        self.publish(path, candidate, observed, digest);
                    }
                }
                return;
            }
            if observed != old_broker && observed != *pending_broker {
                tracing::warn!(event = "gateway_reload_monitor_failed");
                return;
            }
            let Some(active) = self.reload.as_mut() else {
                return;
            };
            if observed == *pending_broker {
                active.broker_digest = observed;
            }
            active.pending = None;
        }
        let Some(active) = self.reload.as_ref() else {
            return;
        };
        let expected_broker = active.broker_digest.clone();
        let (broker_digest, outcome) =
            match client.prepare_generation(expected_broker.clone()).await {
                Ok(result) => result,
                Err(_) => {
                    tracing::warn!(event = "gateway_reload_monitor_failed");
                    return;
                }
            };
        match outcome {
            GenerationOutcome::Active if broker_digest == expected_broker => {
                if digest != old_gateway {
                    if candidate
                        .sources
                        .still_current(&path, &candidate.config.catalog_path, crate::current_uid())
                        .await
                        .unwrap_or(false)
                    {
                        self.publish(path, candidate, broker_digest, digest);
                    } else {
                        tracing::warn!(
                            event = "gateway_reload_refused",
                            applied = false,
                            reason = "source-changed"
                        );
                    }
                }
                return;
            }
            GenerationOutcome::Prepared => {}
            GenerationOutcome::Active
            | GenerationOutcome::Refused
            | GenerationOutcome::Applied
            | GenerationOutcome::Aborted => {
                tracing::warn!(
                    event = "gateway_reload_refused",
                    applied = false,
                    reason = "broker-prepare"
                );
                return;
            }
        }
        if !candidate
            .sources
            .still_current(&path, &candidate.config.catalog_path, crate::current_uid())
            .await
            .unwrap_or(false)
        {
            if !matches!(
                client.abort_generation(broker_digest).await,
                Ok(GenerationOutcome::Aborted)
            ) {
                tracing::warn!(event = "gateway_reload_monitor_failed");
            }
            tracing::warn!(
                event = "gateway_reload_refused",
                applied = false,
                reason = "source-changed"
            );
            return;
        }
        let confirmed = match client.commit_generation(broker_digest.clone()).await {
            Ok(GenerationOutcome::Applied) => true,
            Ok(_) => false,
            Err(_) => matches!(client.observe_generation().await,
                Ok((observed, GenerationOutcome::Active)) if observed == broker_digest),
        };
        if !confirmed {
            if let Some(active) = self.reload.as_mut() {
                active.pending = Some((broker_digest, digest));
            }
            tracing::warn!(
                event = "gateway_reload_refused",
                applied = false,
                reason = "broker-confirmation"
            );
            return;
        }
        if digest == old_gateway {
            if let Some(active) = self.reload.as_mut() {
                active.broker_digest = broker_digest;
            }
        } else {
            self.publish(path, candidate, broker_digest, digest);
        }
    }

    fn publish(
        &mut self,
        path: PathBuf,
        candidate: Candidate,
        broker_digest: String,
        digest: String,
    ) {
        let sources = Arc::new(candidate.sources);
        self.active.store(Arc::new(ServingGeneration {
            routes: Arc::new(candidate.routes),
            stop_words: Arc::new(candidate.config.stop_words.clone()),
            _sources: Some(Arc::clone(&sources)),
            _digest: digest.clone(),
            _broker_digest: broker_digest.clone(),
        }));
        self.reload = Some(ReloadState {
            path,
            config: candidate.config,
            sources,
            broker_digest,
            gateway_digest: digest.clone(),
            pending: None,
        });
        tracing::info!(event = "gateway_reload_applied", generation.digest = %digest);
    }

    #[cfg(test)]
    pub(crate) fn route_for_test(
        &self,
        message: &InboundMessage,
    ) -> Option<crate::routes::BoundRoute> {
        self.active.load().routes.route(message).cloned()
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
        let mut reload_tick = tokio::time::interval(Duration::from_secs(2));
        reload_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        reload_tick.reset();
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
                _ = reload_tick.tick(), if self.reload.is_some() => self.reconcile().await,
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
                        if let Some(route) = self.active.load().routes.route(&message)
                            && let Some(table) = self.runner.follow.as_ref()
                        {
                            let key = follow::follow_key(&route.transport, route.agent.as_str(), &route.conversation, &message.conversation.key());
                            if matches!(table.ticket(&key, SystemTime::now()), follow::FollowTicket::Active { .. }) {
                                self.start_session(&mut sessions, message);
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
                        self.start_session(&mut sessions, message);
                    }
                },
                Some(message) = notices.recv() => {
                    let MessageId::Job { agent, .. } = &message.message_id else {
                        continue;
                    };
                    let answering = self.active.load().routes.route(&message).is_some_and(|route| {
                        route.job_timeout.is_some() && &route.agent == agent
                    });
                    if answering && self.drivers.contains_key(&message.transport) {
                        self.start_session(&mut sessions, message);
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
            collection::Offered::Immediate(message) => self.start_session(sessions, message),
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

    fn start_session(&self, sessions: &mut JoinSet<()>, message: InboundMessage) {
        let generation = self.active.load();
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
            move |message| active.load().routes.route(message).cloned(),
        ));
    }

    /// A wake whose route no longer answers as its agent is delivered as its note, so the person still
    /// hears what they asked to be reminded of.
    fn start_wake(&self, sessions: &mut JoinSet<()>, fired: wake::Fired) {
        let agent = fired.agent().clone();
        let id = fired.id();
        let message = fired.into_inbound();
        let answering = self
            .active
            .load()
            .routes
            .route(&message)
            .is_some_and(|route| route.wakes && route.agent == agent);
        let receipt = message.receive_span.clone();
        receipt.in_scope(|| tracing::info!(event = "gateway_wake_fired", wake.id = %id));
        if answering {
            self.start_session(sessions, message);
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
