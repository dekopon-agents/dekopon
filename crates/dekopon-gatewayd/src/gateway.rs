use std::{
    collections::BTreeMap,
    future::Future,
    sync::Arc,
    time::{Duration, SystemTime},
};

use arc_swap::ArcSwap;
use dekopon_broker_protocol::{BrokerClient, Conversation, ConversationKind, GenerationOutcome};
use dekopon_core::{ExternalSubject, MAX_RENDERED_PROBLEMS, bounded_problems, error_chain};
use dekopon_model_proxy::{CurrentProxy, ModelProxy};
use dekopon_model_token_governor::Metering;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::{sync::mpsc, task::JoinSet, time::timeout};
use tracing::Instrument as _;

use crate::{
    Candidate, GatewaydError, ServeOutcome, cancel_session,
    collection::{self, RouteIdentity},
    config::{self, GithubEvent},
    follow, jobs, observe_session,
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
    pub(crate) routes: Arc<RoutingTable>,
    stop_words: Arc<Vec<String>>,
    proxy: Option<Arc<ModelProxy>>,
    _sources: Option<Arc<config::GatewaySources>>,
    _digest: String,
    _broker_digest: String,
}

#[derive(Clone)]
pub(crate) struct CurrentGeneration(
    Arc<ArcSwap<ServingGeneration>>,
    Arc<std::sync::OnceLock<Option<Arc<follow::FollowTable>>>>,
);

impl CurrentGeneration {
    pub(crate) fn load(&self) -> Arc<ServingGeneration> {
        self.0.load_full()
    }
    pub(crate) fn boot(routes: Arc<RoutingTable>, stop_words: Arc<Vec<String>>) -> Self {
        Self(
            Arc::new(ArcSwap::from_pointee(ServingGeneration {
                routes,
                stop_words,
                proxy: None,
                _sources: None,
                _digest: String::new(),
                _broker_digest: String::new(),
            })),
            Arc::default(),
        )
    }

    pub(crate) fn set_follow(&self, table: Option<Arc<follow::FollowTable>>) {
        self.1.get_or_init(|| table);
    }

    pub(crate) fn follow_table(&self) -> Option<&Arc<follow::FollowTable>> {
        self.1.get().and_then(Option::as_ref)
    }

    pub(crate) fn admits_github(
        &self,
        transport: &str,
        conversation: &Conversation,
        subject: &ExternalSubject,
        event: GithubEvent,
    ) -> bool {
        self.0
            .load()
            .routes
            .github_route(transport, conversation, subject, event)
            .is_some()
    }
}

impl CurrentProxy for CurrentGeneration {
    fn current(&self) -> Option<Arc<ModelProxy>> {
        self.0.load().proxy.clone()
    }
}

pub(crate) struct ResidentModels {
    pub(crate) credentials: Arc<session::ConfiguredModels>,
    pub(crate) metering: Arc<Metering>,
    pub(crate) proxy: Option<Arc<ModelProxy>>,
}

struct ReloadState {
    active: Arc<ArcSwap<ServingGeneration>>,
    path: PathBuf,
    config: config::ResolvedConfig,
    sources: Arc<config::GatewaySources>,
    broker_digest: String,
    gateway_digest: String,
    credentials: Arc<session::ConfiguredModels>,
    metering: Arc<Metering>,
    pending: Option<(String, String)>,
    unsettled: Option<config::RawCandidate>,
}

enum Unsettled {
    Monitor,
    Refused(&'static str),
    Proxy(crate::proxy::ProxyStartError),
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
    for (path, digest) in &candidate.sources.ca_bundles {
        hasher.update(path.as_os_str().as_encoded_bytes());
        hasher.update(digest.bytes());
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
impl ServingGeneration {
    pub(crate) fn routes_for_test(&self) -> &RoutingTable {
        &self.routes
    }
}

impl Gateway {
    #[cfg(test)]
    pub(crate) fn new(
        runner: Arc<SessionRunner>,
        routes: Arc<RoutingTable>,
        identities: Arc<BTreeMap<String, TransportIdentity>>,
        drivers: Arc<BTreeMap<String, Arc<dyn ChatDriver>>>,
        stop_words: Arc<Vec<String>>,
    ) -> Self {
        Self::serving(
            runner,
            CurrentGeneration::boot(routes, stop_words),
            identities,
            drivers,
        )
    }

    pub(crate) fn serving(
        runner: Arc<SessionRunner>,
        generation: CurrentGeneration,
        identities: Arc<BTreeMap<String, TransportIdentity>>,
        drivers: Arc<BTreeMap<String, Arc<dyn ChatDriver>>>,
    ) -> Self {
        Self {
            runner,
            active: generation.0,
            reload: None,
            identities,
            drivers,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_reload(
        self,
        path: PathBuf,
        candidate: Candidate,
        broker_digest: String,
    ) -> Result<Self, config::ConfigError> {
        self.with_reload_owners(
            path,
            candidate,
            broker_digest,
            ResidentModels {
                credentials: Arc::new(session::ConfiguredModels::default()),
                metering: Arc::new(Metering::new(Vec::new(), Metering::system_clock())),
                proxy: None,
            },
        )
    }

    pub(crate) fn with_reload_owners(
        mut self,
        path: PathBuf,
        candidate: Candidate,
        broker_digest: String,
        owners: ResidentModels,
    ) -> Result<Self, config::ConfigError> {
        let gateway_digest = serving_digest(&candidate, &broker_digest)?;
        owners
            .credentials
            .set_active_models(&candidate.config.models);
        let sources = Arc::new(candidate.sources);
        self.active.store(Arc::new(ServingGeneration {
            routes: Arc::new(candidate.routes),
            stop_words: Arc::new(candidate.config.stop_words.clone()),
            proxy: owners.proxy,
            _sources: Some(Arc::clone(&sources)),
            _digest: gateway_digest.clone(),
            _broker_digest: broker_digest.clone(),
        }));
        candidate.config.emit_effective_indicators();
        self.reload = Some(ReloadState {
            active: Arc::clone(&self.active),
            path,
            config: candidate.config,
            sources,
            broker_digest,
            gateway_digest,
            credentials: owners.credentials,
            metering: owners.metering,
            pending: None,
            unsettled: None,
        });
        Ok(self)
    }

    pub(crate) fn proxy_router(&self) -> axum::Router {
        ModelProxy::router_current(CurrentGeneration(Arc::clone(&self.active), Arc::default()))
    }

    #[cfg(test)]
    pub(crate) async fn reconcile(&mut self) {
        if let Some(reload) = self.reload.as_mut() {
            reload.reconcile().await;
        }
    }

    #[cfg(test)]
    pub(crate) fn runner_for_test(&self) -> &Arc<SessionRunner> {
        &self.runner
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
                                drop(spawn_tick(&self.runner, &self.active.load().routes, &store, &mut ticks, tick));
                            }
                        }
                        Ok(Err(error)) => {
                            tracing::error!(event = "gateway_wake_store_failed", error = %error, wakes = "stopped", "gateway.wake.store.failed");
                            wakes = None;
                        }
                        Err(_) => {
                            tracing::error!(
                                event = "gateway_wake_store_failed",
                                wakes = "stopped",
                                "gateway.wake.store.failed"
                            );
                            wakes = None;
                        }
                    }
                },
                Some(result) = ticks.join_next() => {
                    match result {
                        Ok(Some(fired)) => self.start_wake(&mut sessions, fired),
                        Ok(None) => {}
                        Err(_) => tracing::error!(
                            event = "gateway_wake_task_failed",
                            "gateway.wake.task.failed"
                        ),
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
                            && route.follow
                            && let Some(table) = self.runner.follow.as_ref()
                        {
                            let key = follow::follow_key(&route.transport, route.agent.as_str(), &route.conversation, &message.conversation.key());
                            match table.ticket(&key, SystemTime::now()) {
                                follow::FollowTicket::Active { .. } => self.start_session(&mut sessions, &generation, message),
                                follow::FollowTicket::Lapsed => message.receive_span.in_scope(|| self.leave_follow(&mut sessions, &mut follow_pending, key, route.route_index, follow::LeaveCause::Lapse)),
                                follow::FollowTicket::Absent => {},
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
                    for (identity, message) in collector.take_due_routed(tokio::time::Instant::now()) {
                        let generation = self.active.load();
                        let enqueued = generation.routes.route(&message)
                            .is_some_and(|route| RouteIdentity::of(route, &message) == identity);
                        if enqueued {
                            self.start_session(&mut sessions, &generation, message);
                        } else {
                            for receipt in &message.constituents {
                                collection::disposition(receipt, "route-lost");
                            }
                        }
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
                        TransportEvent::Delivery(message, ticket) => {
                            let received = message.receive_span.clone();
                            received.in_scope(|| {
                                self.dispatch_delivery(&mut sessions, *message, ticket);
                            });
                        }
                        TransportEvent::FollowLeave { transport, conversation, received } => {
                            let generation = self.active.load();
                            for route in generation.routes.follow_routes(&transport, &conversation) {
                                let key = follow::follow_key(&route.transport, route.agent.as_str(), &route.conversation, &conversation.key());
                                received.in_scope(|| self.leave_follow(&mut sessions, &mut follow_pending, key, route.route_index, follow::LeaveCause::Reaction));
                            }
                        }
                        TransportEvent::CancelRequested(request) => {
                            if collector.cancel(&request) {
                                tracing::info!(event = "gateway_input_stop_requested", transport = %request.transport, via = via_label(request.via), "gateway.input.stop.requested");
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
            tracing::warn!(
                event = "gateway_sessions_abandoned",
                "gateway.sessions.abandoned"
            );
            sessions.abort_all();
            while let Some(result) = sessions.join_next().await {
                observe_session(result);
            }
            while let Some(result) = ticks.join_next().await {
                if let Ok(Some(fired)) = result {
                    self.start_wake(&mut sessions, fired);
                }
            }
            while let Some(result) = sessions.join_next().await {
                observe_session(result);
            }
        }
        if let Some(summaries) = &self.runner.summaries {
            summaries.stop();
        }
        self.runner.thread_owner.shutdown().await;
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
        let follow_mute = message.transport_kind
            == dekopon_broker_protocol::ChatTransportKind::Slack
            && generation
                .routes
                .route(&message)
                .is_some_and(|route| route.follow)
            && transport::is_stop_word(
                self.identities.get(&message.transport),
                &message.text,
                &["mute".to_owned()],
            );
        if follow_mute
            || transport::is_stop_word(
                self.identities.get(&message.transport),
                &message.text,
                &generation.stop_words,
            )
        {
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
                banished = table.ticket(&key, SystemTime::now()) != follow::FollowTicket::Absent;
                self.leave_follow(
                    sessions,
                    follow_pending,
                    key,
                    route.route_index,
                    if follow_mute {
                        follow::LeaveCause::Mute
                    } else {
                        follow::LeaveCause::StopWord
                    },
                );
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
                        via = "stop-reply",
                        "gateway.session.stop.requested"
                    );
                    return;
                }
                outcome @ (CancelOutcome::AlreadyCancelled | CancelOutcome::Completing) => {
                    tracing::debug!(
                        event = "gateway_session_stop_ignored",
                        transport = %request.transport,
                        reason = outcome.ignored_reason(),
                        "gateway.session.stop.ignored"
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
        let Some((route_id, route)) = generation.routes.route_index(&message) else {
            tracing::debug!(
                event = "gateway_message_ignored",
                transport = %message.transport,
                reason = "unrouted",
                conversation.kind = message.conversation.kind.as_str(),
                conversation.container = message.conversation.container.as_deref().unwrap_or_default(),
                "gateway.message.ignored"
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
            && !(route.ambient.pull_request_links && message.pull_request_links.is_some())
            && ((route.follow
                && message.transport_kind == dekopon_broker_protocol::ChatTransportKind::Slack)
                || (!inherited_thread
                    && !collector
                        .is_native_continuation(&RouteIdentity::of(route, &message), &message)))
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
                match table.ticket(&key, SystemTime::now()) {
                    follow::FollowTicket::Active { .. } => {
                        if !follow_pending.offer(key, message, tokio::time::Instant::now()) {
                            tracing::info!(
                                audit.event = "gateway.follow",
                                operation = "refused",
                                route = route_id,
                                reason = "capacity",
                                "gateway.follow"
                            );
                        }
                        return;
                    }
                    follow::FollowTicket::Lapsed => {
                        self.leave_follow(
                            sessions,
                            follow_pending,
                            key,
                            route_id,
                            follow::LeaveCause::Lapse,
                        );
                        return;
                    }
                    follow::FollowTicket::Absent => {}
                }
            }
            tracing::debug!(
                event = "gateway_message_ignored",
                transport = %message.transport,
                reason = "not-addressed",
                "gateway.message.ignored"
            );
            return;
        }
        match collector.offer(RouteIdentity::of(route, &message), message) {
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

    fn leave_follow(
        &self,
        sessions: &mut JoinSet<()>,
        pending: &mut follow::Debounce,
        key: String,
        route: usize,
        cause: follow::LeaveCause,
    ) {
        pending.remove(&key);
        let Some(table) = self.runner.follow.as_ref() else {
            return;
        };
        if !table.suspend(&key) {
            return;
        }
        tracing::info!(
            audit.event = "gateway.follow",
            operation = "banished",
            cause = cause.label(),
            route,
            "gateway.follow"
        );
        let table = Arc::clone(table);
        sessions.spawn(async move {
            match tokio::task::spawn_blocking(move || table.leave(&key)).await {
                Ok(Ok(())) => {}
                _ => tracing::warn!(
                    audit.event = "gateway.follow",
                    operation = "refused",
                    route,
                    reason = "persistence",
                    "gateway.follow"
                ),
            }
        });
    }

    pub(crate) fn dispatch_delivery(
        &self,
        sessions: &mut JoinSet<()>,
        message: InboundMessage,
        ticket: crate::transport::github::AdmissionTicket,
    ) {
        let generation = self.active.load();
        let listed = match &message.message_id {
            MessageId::Github(batch) => generation
                .routes
                .github_route(
                    &message.transport,
                    &message.conversation,
                    &message.subject,
                    batch.lead().event,
                )
                .is_some(),
            MessageId::Native(_)
            | MessageId::Follow(_)
            | MessageId::Wake { .. }
            | MessageId::Job { .. } => false,
        };
        if !listed
            || !self.drivers.contains_key(&message.transport)
            || message.addressed == Some(false)
        {
            session::record_admission(&message, "ignored", 0, None);
            ticket.answer(crate::transport::github::Acked::Ignored);
            return;
        }
        if let (Some(store), MessageId::Github(batch)) = (&self.runner.wakes, &message.message_id)
            && let Some(route) = generation.routes.route(&message)
            && matches!(
                batch.lead().action.as_ref().map(|action| action.as_str()),
                Some("synchronize" | "closed")
            )
            && store.matches_subscription(&route.agent, batch.lead())
        {
            let Some(permit) = self
                .runner
                .gate
                .admit_probe((message.transport.clone(), message.conversation.key()))
            else {
                session::record_admission(&message, "busy", 0, Some("subscription-admission"));
                ticket.answer(transport::github::Acked::Busy);
                return;
            };
            let runner = Arc::clone(&self.runner);
            let active = Arc::clone(&self.active);
            let drivers = Arc::clone(&self.drivers);
            let store = Arc::clone(store);
            let route = route.clone();
            let event = batch.lead().clone();
            let receipt = message.receive_span.clone();
            let span = receipt.clone();
            sessions.spawn(async move {
                match session::connect(&runner, &route, &message).await {
                    Ok(leg) => drop(leg),
                    Err(error) => {
                        tracing::info!(audit.event = "gateway.admission", admission.trigger = "pr-subscription", subscription.operation = "ignored", reason = "repository-admission", error = %error, "gateway.admission");
                        ticket.answer(transport::github::Acked::Ignored);
                        return;
                    }
                }
                let generation = active.load_full();
                if !generation.routes.github_route(&message.transport, &message.conversation, &message.subject, event.event).is_some_and(|current| current.agent == route.agent) {
                    session::record_admission(&message, "ignored", 0, Some("subscription-route-lost"));
                    ticket.answer(transport::github::Acked::Ignored);
                    return;
                }
                let agent = route.agent.clone();
                let closed = event.action.as_ref().is_some_and(|action| action.as_str() == "closed");
                let store_span = tracing::Span::current();
                let delivery = tokio::task::spawn_blocking(move || store_span.in_scope(|| store.deliver_subscriptions(&agent, &event, SystemTime::now()))).await;
                let delivery = match delivery {
                    Ok(Ok(delivery)) => delivery,
                    Ok(Err(error)) => {
                        tracing::error!(event = "gateway_wake_store_failed", error = %error, "gateway.wake.store.failed");
                        ticket.answer(transport::github::Acked::Busy);
                        return;
                    }
                    Err(error) => {
                        tracing::error!(event = "gateway_wake_store_failed", error = %error, "gateway.wake.store.failed");
                        ticket.answer(transport::github::Acked::Busy);
                        return;
                    }
                };
                drop(permit);
                if !delivery.matched && closed
                {
                    session::record_admission(&message, "ignored", 0, None);
                    ticket.answer(transport::github::Acked::Ignored);
                    return;
                }
                if !delivery.matched {
                    run_routed_delivery(runner, active, drivers, message, Some(ticket)).await;
                    return;
                }
                let (tickets, answer) = ticket.split(delivery.fired.len());
                let wakes = delivery.fired.into_iter().zip(tickets).map(|(fired, ticket)| {
                    let runner = Arc::clone(&runner);
                    let active = Arc::clone(&active);
                    let drivers = Arc::clone(&drivers);
                    let receipt = &receipt;
                    async move {
                        let agent = fired.agent().clone();
                        let wake = fired.into_inbound();
                        dekopon_telemetry::link_span(&wake.receive_span, receipt);
                        let generation = active.load_full();
                        if !generation.routes.route(&wake).is_some_and(|route| route.wakes && route.agent == agent) {
                            session::record_admission(&wake, "ignored", 0, Some("subscription-route-lost"));
                            ticket.answer(transport::github::Acked::Ignored);
                            return;
                        }
                        run_routed_delivery(runner, active, drivers, wake, Some(ticket)).await;
                    }
                });
                tokio::join!(answer, futures_util::future::join_all(wakes));
            }.instrument(span));
            return;
        }
        if let MessageId::Github(batch) = &message.message_id
            && batch.lead().event == GithubEvent::PullRequest
            && batch
                .lead()
                .action
                .as_ref()
                .is_some_and(|action| action.as_str() == "closed")
        {
            session::record_admission(&message, "ignored", 0, None);
            ticket.answer(transport::github::Acked::Ignored);
            return;
        }
        self.start_session_with_ticket(sessions, &generation, message, Some(ticket));
    }

    fn start_session(
        &self,
        sessions: &mut JoinSet<()>,
        generation: &ServingGeneration,
        message: InboundMessage,
    ) {
        self.start_session_with_ticket(sessions, generation, message, None);
    }

    fn start_session_with_ticket(
        &self,
        sessions: &mut JoinSet<()>,
        generation: &ServingGeneration,
        message: InboundMessage,
        ticket: Option<crate::transport::github::AdmissionTicket>,
    ) {
        let Some(route) = generation.routes.route(&message) else {
            collection::disposition(&message.receive_span, "route-lost");
            return;
        };
        let Some(driver) = self.drivers.get(&message.transport).cloned() else {
            message.receive_span.in_scope(|| {
                tracing::error!(
                    event = "gateway_message_ignored",
                    reason = "no-driver",
                    "gateway.message.ignored"
                )
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
            (generation._digest.clone(), ticket),
            move |message, queued| {
                let generation = active.load();
                let route = generation.routes.route(message)?;
                (message.constituents.is_empty()
                    || RouteIdentity::of(route, message) == RouteIdentity::of(queued, message))
                .then(|| (route.clone(), generation._digest.clone()))
            },
        ));
    }

    fn start_wake(&self, sessions: &mut JoinSet<()>, fired: wake::Fired) {
        let space = fired.is_space();
        let agent = fired.agent().clone();
        let id = fired.id();
        let message = fired.into_inbound();
        let generation = self.active.load();
        let answering = generation
            .routes
            .route(&message)
            .is_some_and(|route| route.wakes && route.agent == agent);
        let receipt = message.receive_span.clone();
        receipt.in_scope(
            || tracing::info!(event = "gateway_wake_fired", wake.id = %id, "gateway.wake.fired"),
        );
        if answering {
            self.start_session(sessions, &generation, message);
            return;
        }
        receipt.in_scope(|| {
            tracing::warn!(
            event = "gateway_wake_orphaned",
            wake.id = %id,
            "gateway.wake.orphaned")
        });
        if !space
            && let Some(driver) = self.drivers.get(&message.transport).cloned()
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

async fn run_routed_delivery(
    runner: Arc<SessionRunner>,
    active: Arc<ArcSwap<ServingGeneration>>,
    drivers: Arc<BTreeMap<String, Arc<dyn ChatDriver>>>,
    message: InboundMessage,
    ticket: Option<transport::github::AdmissionTicket>,
) {
    let generation = active.load_full();
    let Some(route) = generation.routes.route(&message).cloned() else {
        session::record_admission(&message, "ignored", 0, Some("route-lost"));
        if let Some(ticket) = ticket {
            ticket.answer(transport::github::Acked::Ignored);
        }
        return;
    };
    let Some(driver) = drivers.get(&message.transport).cloned() else {
        session::record_admission(&message, "ignored", 0, Some("no-driver"));
        if let Some(ticket) = ticket {
            ticket.answer(transport::github::Acked::Ignored);
        }
        return;
    };
    let agent = route.agent.clone();
    session::run_session_resolving(
        runner,
        route,
        message,
        driver,
        (generation._digest.clone(), ticket),
        move |message, queued| {
            let generation = active.load();
            let route = generation.routes.route(message)?;
            (!wake::space_wake(message) || route.wakes && route.agent == agent).then_some(())?;
            (message.constituents.is_empty()
                || RouteIdentity::of(route, message) == RouteIdentity::of(queued, message))
            .then(|| (route.clone(), generation._digest.clone()))
        },
    )
    .await;
}

fn reload_reason(problem: &GatewaydError) -> &'static str {
    match problem {
        GatewaydError::Config(config::ConfigError::RestartRequired { .. }) => "restart-required",
        GatewaydError::Config(_) => "configuration",
        GatewaydError::Catalog(_) => "catalog",
        GatewaydError::Route(_) => "route",
        GatewaydError::Probe(_) => "vendor-probe",
        GatewaydError::Startup { .. } => "credential-or-transport",
        GatewaydError::BrokerProbe(_)
        | GatewaydError::BrokerGeneration
        | GatewaydError::TransportConnect { .. }
        | GatewaydError::TransportTask(_)
        | GatewaydError::TransportsLost
        | GatewaydError::AssetStore { .. }
        | GatewaydError::Journal { .. }
        | GatewaydError::CacheKeySecret(_)
        | GatewaydError::WakeStore(_)
        | GatewaydError::Proxy(_)
        | GatewaydError::Problems { .. } => "preparation",
    }
}

pub(crate) fn reload_problem(problem: &GatewaydError) -> String {
    match problem {
        GatewaydError::Config(
            error @ (config::ConfigError::Decode { .. }
            | config::ConfigError::DecodeFragment { .. }),
        ) => format!("{problem}: {error}"),
        GatewaydError::Catalog(_) => problem.to_string(),
        GatewaydError::Proxy(crate::proxy::ProxyStartError::Codex { model, source }) => format!(
            "proxy model {model:?} credential could not be opened ({})",
            source.kind().as_str()
        ),
        GatewaydError::Probe(probe) => {
            use dekopon_model::error::{AuthError, InferenceError, RateLimitError};
            let (kind, status) = match &probe.source {
                crate::ProbeError::Credential(_) => ("credential", None),
                crate::ProbeError::InvalidToolCall | crate::ProbeError::MalformedResponse(_) => {
                    ("protocol", Some(200))
                }
                crate::ProbeError::Vendor(error) => {
                    let status = match error {
                        InferenceError::Provider(failure)
                        | InferenceError::Authentication(AuthError::Provider(failure))
                        | InferenceError::RateLimited(RateLimitError(failure)) => failure.status,
                        InferenceError::InvalidRequest(_)
                        | InferenceError::Transport(_)
                        | InferenceError::Protocol(_)
                        | InferenceError::Attachment(_)
                        | InferenceError::Authentication(AuthError::Credential(_))
                        | InferenceError::DeadlineExceeded
                        | InferenceError::Cancelled
                        | InferenceError::OverBudget(_) => None,
                    };
                    (error.kind().as_str(), status)
                }
            };
            let status = status.map_or_else(String::new, |status| format!("; HTTP {status}"));
            format!(
                "model {} probe failed ({kind}{status}), routes {}",
                probe.model,
                probe.routes.join(", ")
            )
        }
        GatewaydError::Config(_)
        | GatewaydError::Route(_)
        | GatewaydError::Startup { .. }
        | GatewaydError::BrokerProbe(_)
        | GatewaydError::BrokerGeneration
        | GatewaydError::TransportConnect { .. }
        | GatewaydError::TransportTask(_)
        | GatewaydError::TransportsLost
        | GatewaydError::AssetStore { .. }
        | GatewaydError::Journal { .. }
        | GatewaydError::CacheKeySecret(_)
        | GatewaydError::WakeStore(_)
        | GatewaydError::Proxy(_)
        | GatewaydError::Problems { .. } => error_chain(problem),
    }
}

fn reload_entries(problem: &GatewaydError) -> Vec<(&'static str, String)> {
    match problem {
        GatewaydError::Config(config::ConfigError::Invalid { path, problems }) => problems
            .iter()
            .map(|item| ("configuration", format!("{}: {}", path.display(), item)))
            .collect(),
        GatewaydError::Route(crate::routes::RouteError { problems }) => problems
            .iter()
            .map(|item| ("route", item.to_string()))
            .collect(),
        GatewaydError::Config(_)
        | GatewaydError::Catalog(_)
        | GatewaydError::Startup { .. }
        | GatewaydError::BrokerProbe(_)
        | GatewaydError::BrokerGeneration
        | GatewaydError::TransportConnect { .. }
        | GatewaydError::TransportTask(_)
        | GatewaydError::TransportsLost
        | GatewaydError::AssetStore { .. }
        | GatewaydError::Journal { .. }
        | GatewaydError::CacheKeySecret(_)
        | GatewaydError::WakeStore(_)
        | GatewaydError::Proxy(_)
        | GatewaydError::Probe(_)
        | GatewaydError::Problems { .. } => vec![(reload_reason(problem), reload_problem(problem))],
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
            Some((&self.config, &self.sources, &self.active.load_full().routes)),
        )
        .await;
        let Some(candidate) = candidate else {
            let entries = report
                .problems
                .iter()
                .flat_map(reload_entries)
                .collect::<Vec<_>>();
            let error = bounded_problems(entries.iter().map(|(_, reason)| reason.clone()));
            if self.unreported(report.raw.refusing(&error)) {
                let categories = entries
                    .iter()
                    .take(MAX_RENDERED_PROBLEMS)
                    .map(|(category, _)| *category)
                    .collect::<Vec<_>>();
                tracing::warn!(
                    event = "gateway_reload_refused",
                    applied = false,
                    problems = entries.len(),
                    categories = %categories.join(","),
                    error = %error,
                    "gateway.reload.refused"
                );
            }
            return;
        };
        let raw = report.raw;
        if let Err(unsettled) = self.activate(candidate).await {
            let identity = match &unsettled {
                Unsettled::Monitor => "monitor",
                Unsettled::Refused(reason) => reason,
                Unsettled::Proxy(_) => "proxy-model",
            };
            if !self.unreported(raw.refusing(identity)) {
                return;
            }
            match unsettled {
                Unsettled::Monitor => tracing::warn!(
                    event = "gateway_reload_monitor_failed",
                    "gateway.reload.monitor.failed"
                ),
                Unsettled::Refused(reason) => {
                    tracing::warn!(
                        event = "gateway_reload_refused",
                        applied = false,
                        reason,
                        "gateway.reload.refused"
                    );
                }
                Unsettled::Proxy(error) => tracing::warn!(
                    event = "gateway_reload_refused",
                    applied = false,
                    reason = "proxy-model",
                    error = %reload_problem(&GatewaydError::Proxy(error)),
                    "gateway.reload.refused"
                ),
            }
        }
    }

    fn unreported(&mut self, raw: config::RawCandidate) -> bool {
        self.unsettled.replace(raw) != Some(raw)
    }

    async fn activate(&mut self, candidate: Candidate) -> Result<(), Unsettled> {
        let refreshed_proxy = if candidate.config.models != self.config.models
            || candidate.routes.guests() != self.active.load().routes.guests()
        {
            match (
                self.active.load().proxy.is_some(),
                candidate.config.proxy.as_ref(),
            ) {
                (true, Some(_)) => match crate::proxy::build(
                    candidate.routes.guests(),
                    &candidate.config.models,
                    &self.credentials,
                    &self.metering,
                ) {
                    Ok(proxy) => Some(proxy),
                    Err(error) => return Err(Unsettled::Proxy(error)),
                },
                _ => None,
            }
        } else {
            None
        };
        let broker = &candidate.config.broker;
        let Ok(client) = BrokerClient::new(&broker.socket_path, broker.server_uid, broker.frame)
        else {
            return Err(Unsettled::Monitor);
        };
        if let Some((pending_broker, pending_gateway)) = self.pending.clone() {
            let (observed, outcome) = match client.observe_generation().await {
                Ok(observation) => observation,
                Err(_) => return Err(Unsettled::Monitor),
            };
            if outcome != GenerationOutcome::Active {
                return Err(Unsettled::Monitor);
            }
            let Ok(digest) = serving_digest(&candidate, &pending_broker) else {
                return Ok(());
            };
            if observed == pending_broker && digest == pending_gateway {
                self.settle(candidate, observed, digest, refreshed_proxy);
                return Ok(());
            }
            self.broker_digest = observed;
            self.pending = None;
        }
        let expected_broker = self.broker_digest.clone();
        let (broker_digest, outcome) =
            match client.prepare_generation(expected_broker.clone()).await {
                Ok(result) => result,
                Err(_) => return Err(Unsettled::Monitor),
            };
        let Ok(digest) = serving_digest(&candidate, &broker_digest) else {
            return Ok(());
        };
        match outcome {
            GenerationOutcome::Active if broker_digest == expected_broker => {
                self.settle(candidate, broker_digest, digest, refreshed_proxy);
                return Ok(());
            }
            GenerationOutcome::Prepared => {}
            GenerationOutcome::Refused => {
                self.broker_digest = broker_digest;
                return Err(Unsettled::Refused("broker-prepare"));
            }
            GenerationOutcome::Active | GenerationOutcome::Applied | GenerationOutcome::Aborted => {
                return Err(Unsettled::Refused("broker-prepare"));
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
            return Err(Unsettled::Refused("broker-confirmation"));
        }
        self.settle(candidate, broker_digest, digest, refreshed_proxy);
        Ok(())
    }

    fn settle(
        &mut self,
        candidate: Candidate,
        broker_digest: String,
        digest: String,
        refreshed_proxy: Option<Arc<ModelProxy>>,
    ) {
        self.pending = None;
        let repaired = self.unsettled.take().is_some();
        if digest == self.gateway_digest {
            self.broker_digest = broker_digest;
            if repaired {
                tracing::info!(
                    event = "gateway_reload_applied",
                    generation.digest = %digest,
                    category.count = 0,
                    categories = "",
                    "gateway.reload.applied"
                );
            }
            return;
        }
        let categories = self
            .sources
            .changes(&candidate.sources)
            .map(|changes| changes.swap)
            .unwrap_or_default();
        let sources = Arc::new(candidate.sources);
        let proxy = refreshed_proxy.or_else(|| self.active.load().proxy.clone());
        self.active.store(Arc::new(ServingGeneration {
            routes: Arc::new(candidate.routes),
            stop_words: Arc::new(candidate.config.stop_words.clone()),
            proxy,
            _sources: Some(Arc::clone(&sources)),
            _digest: digest.clone(),
            _broker_digest: broker_digest.clone(),
        }));
        candidate.config.emit_effective_indicators();
        self.credentials.set_active_models(&candidate.config.models);
        self.config = candidate.config;
        self.sources = sources;
        self.broker_digest = broker_digest;
        tracing::info!(
            event = "gateway_reload_applied",
            generation.digest = %digest,
            category.count = categories.len(),
            categories = %categories.join(","),
            "gateway.reload.applied"
        );
        self.gateway_digest = digest;
        self.credentials.retire();
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use dekopon_core::Redacted;
    use dekopon_model_proxy::{Grant, Granted, ProxyModel, Upstream};
    use dekopon_model_token_governor::Tokens;
    use dekopon_test_support::LoopbackServer;
    use std::collections::{BTreeSet, HashMap};

    #[test]
    fn proxy_tls_source_bytes_never_enter_a_flattened_reload_record() {
        const SENTINEL: &str = "SENTINEL";
        let path = PathBuf::from("gateway.yaml");
        let (capture, _guard) = crate::tests::capture_spans();
        for (index, source) in [
            rustls::pki_types::pem::Error::MissingSectionEnd {
                end_marker: b"PRIVATE KEY----- SENTINEL".to_vec(),
            },
            rustls::pki_types::pem::Error::Base64Decode(SENTINEL.to_owned()),
        ]
        .into_iter()
        .enumerate()
        {
            let problem = GatewaydError::Config(config::ConfigError::Invalid {
                path: path.clone(),
                problems: vec![config::ConfigProblem::Proxy(
                    crate::proxy::ProxyProblem::Tls(dekopon_model_proxy::tls::TlsError::Read {
                        path: path.clone(),
                        source,
                    }),
                )],
            });
            let entries = reload_entries(&problem);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].0, "configuration");
            let expected = format!("{}: proxy TLS files are unusable", path.display());
            assert_eq!(entries[0].1, expected);
            tracing::warn!(event = "gateway_reload_refused", applied = false, error = %entries[0].1);
            let refused =
                capture.events_where("dekopon_gatewayd", &[("event", "gateway_reload_refused")]);
            assert_eq!(refused.len(), index + 1);
            let reason = refused[index].field("error").unwrap();
            assert_eq!(reason, expected);
            assert!(!reason.contains(SENTINEL));
            assert!(!reason.contains("83, 69, 78"));
        }
    }

    #[test]
    fn probe_reload_record_omits_vendor_model_but_boot_retains_its_diagnostic() {
        use crate::{ProbeError, ProbeProblem};
        use dekopon_model::error::{InferenceError, ProtocolFailure};

        const SENTINEL: &str = "configured-vendor-model-secret-sentinel";
        let problem = GatewaydError::Probe(Box::new(ProbeProblem {
            model: "router".to_owned(),
            vendor_model: SENTINEL.to_owned(),
            file: "gateway.yaml".into(),
            routes: Vec::new(),
            source: ProbeError::Vendor(InferenceError::Protocol(ProtocolFailure::NoChoices)),
        }));
        assert!(
            error_chain(&problem).contains(SENTINEL),
            "boot output is unchanged"
        );
        let (capture, _guard) = crate::tests::capture_spans();
        let reason = reload_problem(&problem);
        tracing::warn!(event = "gateway_reload_refused", applied = false, error = %reason);
        let refused =
            capture.events_where("dekopon_gatewayd", &[("event", "gateway_reload_refused")]);
        assert_eq!(refused.len(), 1);
        let error = refused[0].field("error").unwrap();
        assert!(error.contains("router"), "{error}");
        assert!(error.contains("probe failed"), "{error}");
        assert!(error.contains("protocol"), "{error}");
        assert!(!error.contains(SENTINEL), "{error}");
    }

    #[tokio::test]
    async fn one_published_snapshot_selects_the_proxy_for_each_request() {
        let answer = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
        let first = LoopbackServer::once(answer);
        let second = LoopbackServer::once(answer);
        let meter = Arc::new(Metering::new(Vec::new(), Metering::system_clock()));
        let build = |server: &LoopbackServer| {
            Arc::new(
                ModelProxy::new(
                    vec![ProxyModel {
                        name: "fixture".into(),
                        wire_model: "upstream".into(),
                        backend: "openrouter",
                        reserve: Tokens(0),
                        upstream: Upstream::OpenRouter {
                            endpoint: server.url(),
                            api_key: Redacted::new("synthetic-key".into()),
                            settings: Vec::new(),
                        },
                    }],
                    HashMap::from([(
                        "vm".into(),
                        Grant {
                            agent: "reviewer".parse().unwrap(),
                            models: Granted::Listed(BTreeSet::from(["fixture".into()])),
                        },
                    )]),
                    Arc::clone(&meter),
                )
                .unwrap(),
            )
        };
        let old_proxy = build(&first);
        let active = Arc::new(ArcSwap::from_pointee(ServingGeneration {
            routes: Arc::new(RoutingTable::default()),
            stop_words: Arc::new(Vec::new()),
            proxy: Some(Arc::clone(&old_proxy)),
            _sources: None,
            _digest: "old".into(),
            _broker_digest: "old".into(),
        }));
        let previous = active.load_full();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let routed = Arc::clone(&active);
        let serving = tokio::spawn(async move {
            axum::serve(
                listener,
                ModelProxy::router_current(CurrentGeneration(routed, Arc::default())),
            )
            .await
            .unwrap()
        });
        let client = reqwest::Client::new();
        let send = || {
            client
                .post(format!("http://{address}/v1/chat/completions"))
                .header("x-dekopon-vm-subject", "vm")
                .header("content-type", "application/json")
                .body(r#"{"model":"fixture","messages":[],"stream":false}"#)
        };
        assert_eq!(send().send().await.unwrap().status(), 200);
        assert!(Arc::ptr_eq(previous.proxy.as_ref().unwrap(), &old_proxy));
        let next = build(&second);
        active.store(Arc::new(ServingGeneration {
            routes: Arc::clone(&previous.routes),
            stop_words: Arc::clone(&previous.stop_words),
            proxy: Some(Arc::clone(&next)),
            _sources: None,
            _digest: "new".into(),
            _broker_digest: "new".into(),
        }));
        assert_eq!(active.load()._digest, "new");
        assert!(Arc::ptr_eq(previous.proxy.as_ref().unwrap(), &old_proxy));
        assert!(Arc::ptr_eq(active.load().proxy.as_ref().unwrap(), &next));
        assert_eq!(send().send().await.unwrap().status(), 200);
        assert!(
            first
                .request_text()
                .starts_with("POST /v1/chat/completions HTTP/1.1")
        );
        assert!(
            second
                .request_text()
                .starts_with("POST /v1/chat/completions HTTP/1.1")
        );
        serving.abort();
    }
}
