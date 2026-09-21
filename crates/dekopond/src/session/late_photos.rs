//! Late WhatsApp references are conversation content, never another prompt-loop execution.
use super::*;
use crate::{
    conversation::{ConversationInput, LateAssetRefusal},
    transport::ReplyTarget,
};
use dekopon_broker_protocol::ChatTransportKind;

const RETAINED_REPLY: &str = "Additional photo references received during the request are now in this conversation's temporary inventory. They have not been downloaded by this intake step. Would you like another version including the additional photos?";
const FAILED_RETAINED_REPLY: &str = "Additional photo references received during the request are now in this conversation's temporary inventory. They have not been downloaded by this intake step. The request did not complete successfully; send a new request if you want to use these photos.";
const EXPIRED_REPLY: &str = "Additional photos arrived during the request, but their references are no longer available in this conversation. Please send them again with your next request.";
pub(super) const REFUSED_REPLY: &str = "The additional photos were not retained for this request. Please send them again with your next request.";

#[derive(Clone)]
pub(crate) struct LatePhotos {
    inner: Arc<Mutex<State>>,
    cancellation: SessionCancellation,
}

// Never print scope coordinates or cached grant material as Debug metadata.
impl std::fmt::Debug for LatePhotos {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LatePhotos").finish_non_exhaustive()
    }
}

struct Scope {
    input: ConversationInput,
    access: AssetAccess,
    cache_key: String,
}

#[derive(Clone, Copy)]
enum Ending {
    Running,
    Answered,
    Failed,
}

enum RegistrationRefusal {
    Scope,
    InputLimit,
    RetentionDisabled,
    Conversation(LateAssetRefusal),
}

impl RegistrationRefusal {
    const fn label(&self) -> &'static str {
        match self {
            Self::Scope => "scope-mismatch",
            Self::InputLimit => "input-limit",
            Self::RetentionDisabled => "retention-disabled",
            Self::Conversation(reason) => reason.label(),
        }
    }
}

struct State {
    route_key: String,
    subject: dekopon_core::ExternalSubject,
    conversation: ConversationKey,
    native_conversation: dekopon_broker_protocol::Conversation,
    pending_notice: bool,
    intake_stopped: bool,
    reply: ReplyTarget,
    persistent: bool,
    transport_kind: ChatTransportKind,
    scope: Option<Scope>,
    ids: Vec<u64>,
    ending: Ending,
}

/// A missing old interval is a refusal, not evidence that this receipt arrived while idle.
#[derive(Clone, Debug)]
pub(crate) enum LatePhotoReceipt {
    Run(LatePhotos),
    HistoryUnavailable,
}

impl LatePhotoReceipt {
    pub(super) fn is_stopped(&self) -> bool {
        match self {
            Self::Run(run) => {
                run.cancellation.is_cancelled()
                    || run.inner.lock().expect("late photo state").intake_stopped
            }
            Self::HistoryUnavailable => false,
        }
    }

    pub(crate) fn same_batch(left: Option<&Self>, right: Option<&Self>) -> bool {
        match (left, right) {
            (None, None) => true,
            (Some(Self::Run(left)), Some(Self::Run(right))) => {
                Arc::ptr_eq(&left.inner, &right.inner)
            }
            (Some(Self::HistoryUnavailable), Some(Self::HistoryUnavailable)) => true,
            _ => false,
        }
    }

    pub(super) async fn retain(
        &self,
        runner: &SessionRunner,
        route: &BoundRoute,
        message: &InboundMessage,
        driver: &Arc<dyn ChatDriver>,
        intake: &LateIntake,
    ) -> &'static str {
        let operation = async {
            match self {
                Self::Run(run) => run.retain(runner, route, message, driver, intake).await,
                Self::HistoryUnavailable => {
                    answer(driver, message, REFUSED_REPLY).await;
                    "late-history-unavailable"
                }
            }
        };
        let outcome = tokio::select! {
            biased;
            () = intake.control.cancellation.cancelled() => "stopped",
            () = async {
                match self {
                    Self::Run(run) => run.cancellation.cancelled().await,
                    Self::HistoryUnavailable => std::future::pending().await,
                }
            } => "stopped",
            outcome = operation => outcome,
        };
        if intake.claim_completion() {
            outcome
        } else {
            intake.stopped(driver, message).await
        }
    }
}

/// One entry per admitted metadata operation; permits bound both entries and broker connections.
#[derive(Clone, Default)]
pub(super) struct LateIntakes {
    entries: Arc<Mutex<Vec<Arc<IntakeControl>>>>,
}

struct IntakeControl {
    key: ActiveSessionKey,
    run: Option<LatePhotos>,
    subject: dekopon_core::ExternalSubject,
    cancellation: SessionCancellation,
    gate: Mutex<()>,
    announce_stop: std::sync::atomic::AtomicBool,
}

pub(super) struct LateIntake {
    control: Arc<IntakeControl>,
    registry: LateIntakes,
    _permit: OwnedSemaphorePermit,
}

impl LateIntakes {
    pub(super) fn register(
        &self,
        gate: &SessionGate,
        message: &InboundMessage,
    ) -> Option<LateIntake> {
        let permit = Arc::clone(&gate.late_permits).try_acquire_owned().ok()?;
        let control = Arc::new(IntakeControl {
            run: match &message.late_photos {
                Some(LatePhotoReceipt::Run(run)) => Some(run.clone()),
                _ => None,
            },
            key: (message.transport.clone(), message.conversation.key()),
            subject: message.subject.clone(),
            cancellation: SessionCancellation::new(),
            gate: Mutex::new(()),
            announce_stop: std::sync::atomic::AtomicBool::new(false),
        });
        self.entries
            .lock()
            .expect("late intake registry")
            .push(Arc::clone(&control));
        Some(LateIntake {
            control,
            registry: self.clone(),
            _permit: permit,
        })
    }

    pub(super) fn cancel(
        &self,
        request: &CancelRequest,
        execution: CancelOutcome,
    ) -> CancelOutcome {
        let entries = self.entries.lock().expect("late intake registry");
        let mut already_cancelled = false;
        let mut completing = false;
        let mut cancelled = false;
        let mut announce = !matches!(
            execution,
            CancelOutcome::Cancelled | CancelOutcome::AlreadyCancelled
        );
        for control in entries.iter().filter(|control| {
            control.key.0 == request.transport
                && control.key.1 == request.conversation_id
                && control.subject.canonical() == request.subject
        }) {
            // Publication, terminal arbitration and Stop share one linearization boundary.
            let _gate = control.gate.lock().expect("late intake cancellation gate");
            if control
                .cancellation
                .cancel(CancelSource::User { via: request.via })
            {
                if let Some(run) = &control.run {
                    run.inner.lock().expect("late photo state").intake_stopped = true;
                }
                control.announce_stop.store(announce, Ordering::Release);
                cancelled = true;
                announce = false;
            } else if control.cancellation.is_cancelled() {
                already_cancelled = true;
            } else {
                completing = true;
            }
        }
        if matches!(
            execution,
            CancelOutcome::Cancelled | CancelOutcome::AlreadyCancelled
        ) {
            execution
        } else if cancelled {
            CancelOutcome::Cancelled
        } else if already_cancelled {
            CancelOutcome::AlreadyCancelled
        } else if completing {
            CancelOutcome::Completing
        } else {
            execution
        }
    }
}

impl Drop for LateIntake {
    fn drop(&mut self) {
        self.registry
            .entries
            .lock()
            .expect("late intake registry")
            .retain(|entry| !Arc::ptr_eq(entry, &self.control));
    }
}

impl LateIntake {
    fn claim_completion(&self) -> bool {
        let _gate = self
            .control
            .gate
            .lock()
            .expect("late intake cancellation gate");
        self.control.cancellation.claim_completion()
    }

    async fn stopped(
        &self,
        driver: &Arc<dyn ChatDriver>,
        message: &InboundMessage,
    ) -> &'static str {
        if self.control.announce_stop.swap(false, Ordering::AcqRel) {
            answer(driver, message, STOPPED_REPLY).await;
        }
        "stopped"
    }
}

struct CompletedSession {
    key: ActiveSessionKey,
    session: ActiveSession,
    ended_at: tokio::time::Instant,
}

pub(super) struct RecentSessions {
    capacity: usize,
    completed: std::collections::VecDeque<CompletedSession>,
    unavailable_through: Option<tokio::time::Instant>,
}

impl RecentSessions {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            completed: std::collections::VecDeque::new(),
            unavailable_through: None,
        }
    }

    pub(super) fn complete(&mut self, key: ActiveSessionKey, session: ActiveSession) {
        if !session.late_photos.tracks_receipts() {
            return;
        }
        self.completed.push_back(CompletedSession {
            key,
            session,
            ended_at: tokio::time::Instant::now(),
        });
        while self.completed.len() > self.capacity {
            if let Some(evicted) = self.completed.pop_front() {
                self.unavailable_through = Some(evicted.ended_at);
            }
        }
    }
}

impl ActiveSessions {
    pub(crate) fn late_photos(
        &self,
        route: &BoundRoute,
        message: &InboundMessage,
    ) -> Option<LatePhotoReceipt> {
        if message.transport_kind != ChatTransportKind::Whatsapp
            || route.memory.window().is_none()
            || !message.text.trim().is_empty()
            || message.assets.is_empty()
            || !message
                .assets
                .iter()
                .all(|asset| matches!(asset.mime.as_str(), "image/png" | "image/jpeg"))
        {
            return None;
        }
        let key = (message.transport.clone(), message.conversation.key());
        // Registration/removal and recent insertion share this lock. Receipt-time association
        // cannot observe the gap between an active entry and its completed interval.
        let entries = self.entries.lock().expect("active session registry");
        if let Some(active) = entries.get(&key)
            && message.received_at >= active.started_at
            && active.late_photos.matches(route, message)
        {
            return Some(LatePhotoReceipt::Run(active.late_photos.clone()));
        }
        let recent = self.recent.lock().expect("recent session registry");
        if let Some(completed) = recent.completed.iter().rev().find(|completed| {
            completed.key == key
                && message.received_at >= completed.session.started_at
                && message.received_at <= completed.ended_at
                && completed.session.late_photos.matches(route, message)
        }) {
            return Some(LatePhotoReceipt::Run(completed.session.late_photos.clone()));
        }
        recent
            .unavailable_through
            .filter(|watermark| message.received_at <= *watermark)
            .map(|_| LatePhotoReceipt::HistoryUnavailable)
    }
}

impl LatePhotos {
    pub(super) fn new(
        route: &BoundRoute,
        message: &InboundMessage,
        cancellation: SessionCancellation,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(State {
                route_key: route.cache_key.clone(),
                subject: message.subject.clone(),
                conversation: conversation_key(route, message),
                native_conversation: message.conversation.clone(),
                pending_notice: false,
                intake_stopped: false,
                reply: message.reply.clone(),
                persistent: route.memory.window().is_some(),
                transport_kind: message.transport_kind,
                scope: None,
                ids: Vec::new(),
                ending: Ending::Running,
            })),
            cancellation,
        }
    }

    fn tracks_receipts(&self) -> bool {
        let state = self.inner.lock().expect("late photo state");
        state.persistent && state.transport_kind == ChatTransportKind::Whatsapp
    }

    fn matches(&self, route: &BoundRoute, message: &InboundMessage) -> bool {
        let state = self.inner.lock().expect("late photo state");
        state.subject == message.subject
            && state.native_conversation == message.conversation
            && state.conversation == conversation_key(route, message)
            && state.persistent
            && state.route_key == route.cache_key
            && state.reply == message.reply
    }

    pub(super) fn authorized(
        &self,
        input: ConversationInput,
        access: AssetAccess,
        cache_key: String,
    ) {
        self.inner.lock().expect("late photo state").scope = Some(Scope {
            input,
            access,
            cache_key,
        });
    }

    pub(super) async fn retain(
        &self,
        runner: &SessionRunner,
        route: &BoundRoute,
        message: &InboundMessage,
        driver: &Arc<dyn ChatDriver>,
        intake: &LateIntake,
    ) -> &'static str {
        if self.cancellation.is_cancelled()
            || intake.control.cancellation.is_cancelled()
            || self.inner.lock().expect("late photo state").intake_stopped
        {
            return "stopped";
        }
        let leg = match connect(runner, route, message).await {
            Ok(leg) => leg,
            Err(error) => {
                tracing::warn!(
                    event = "gateway_late_photos_refused",
                    reason = "authorization",
                    category = error.category()
                );
                if !self.cancellation.is_cancelled() {
                    answer(driver, message, REFUSED_REPLY).await;
                }
                return "late-unauthorized";
            }
        };
        let granted = leg.granted();
        let result = {
            let _gate = intake
                .control
                .gate
                .lock()
                .expect("late intake cancellation gate");
            let mut state = self.inner.lock().expect("late photo state");
            if self.cancellation.is_cancelled()
                || intake.control.cancellation.is_cancelled()
                || state.intake_stopped
            {
                return "stopped";
            }
            Self::register(&mut state, runner, route, message, &granted)
        };
        match result {
            Ok(Ending::Running) => "late-retained",
            Ok(ending) => {
                if self.cancellation.is_cancelled() {
                    return "stopped";
                }
                if answer(driver, message, notice(ending)).await {
                    self.remember_notice(&runner.conversations, notice(ending));
                    "late-acknowledged"
                } else {
                    "late-reply-failed"
                }
            }
            Err(reason) => {
                tracing::info!(
                    event = "gateway_late_photos_refused",
                    reason = reason.label()
                );
                if !self.cancellation.is_cancelled() {
                    answer(driver, message, REFUSED_REPLY).await;
                }
                "late-refused"
            }
        }
    }

    fn register(
        state: &mut State,
        runner: &SessionRunner,
        route: &BoundRoute,
        message: &InboundMessage,
        granted: &[String],
    ) -> Result<Ending, RegistrationRefusal> {
        let scope = state.scope.as_ref().ok_or(RegistrationRefusal::Scope)?;
        let window = route.memory.window().ok_or(RegistrationRefusal::Scope)?;
        if state.native_conversation != message.conversation
            || state.subject != message.subject
            || state.conversation != conversation_key(route, message)
            || message.transport_kind != ChatTransportKind::Whatsapp
            || state.route_key != route.cache_key
            || state.reply != message.reply
        {
            return Err(RegistrationRefusal::Scope);
        }
        if !message.text.trim().is_empty()
            || message.asset_overflow
            || message.assets.is_empty()
            || message.assets.len() > asset::MAX_ASSETS_PER_CONVERSATION
            || message.assets.iter().any(|asset| {
                !matches!(asset.mime.as_str(), "image/png" | "image/jpeg")
                    || asset.size.is_some_and(|size| {
                        size > crate::transport::whatsapp::MAX_IMAGE_BYTES as u64
                    })
            })
        {
            return Err(RegistrationRefusal::InputLimit);
        }
        if !runner.assets.retention_enabled() {
            return Err(RegistrationRefusal::RetentionDisabled);
        }
        let ids = runner
            .conversations
            .retain_late_assets(&scope.input, granted, window, &scope.cache_key, || {
                let registered = runner.assets.assets_for_access(
                    &scope.access,
                    message.assets.clone(),
                    route.model.accepts_images(),
                    Instant::now(),
                );
                let retained: Vec<_> = registered
                    .arrived
                    .into_iter()
                    .filter(|id| registered.inventory.iter().any(|asset| asset.id == *id))
                    .collect();
                (!retained.is_empty()).then_some(retained)
            })
            .map_err(RegistrationRefusal::Conversation)?;
        tracing::info!(event = "gateway_late_photos_retained", asset.ids = ?ids);
        if matches!(state.ending, Ending::Running) {
            state.pending_notice = true;
            state.ids.extend(ids);
            // Inventory eviction also removes old notice IDs; tracking never exceeds that table.
            state.ids.retain(|id| {
                runner
                    .assets
                    .get_access(&scope.access, *id, Instant::now())
                    .is_some()
            });
        }
        Ok(state.ending)
    }

    pub(super) fn remember_notice(&self, conversations: &ConversationStore, notice: &'static str) {
        let state = self.inner.lock().expect("late photo state");
        if let Some(scope) = &state.scope {
            conversations.remember_gateway_notice(&scope.input, notice);
        }
    }

    pub(super) fn finish(&self, assets: &AssetStore, succeeded: bool) -> Option<&'static str> {
        let mut state = self.inner.lock().expect("late photo state");
        state.ending = if succeeded {
            Ending::Answered
        } else {
            Ending::Failed
        };
        let ids = std::mem::take(&mut state.ids);
        let pending_notice = std::mem::take(&mut state.pending_notice);
        if self.cancellation.is_cancelled() || state.intake_stopped {
            return None;
        }
        let scope = state.scope.as_ref()?;
        if !pending_notice {
            return None;
        }
        Some(
            if ids.iter().any(|id| {
                assets
                    .get_access(&scope.access, *id, Instant::now())
                    .is_some()
            }) {
                notice(state.ending)
            } else {
                EXPIRED_REPLY
            },
        )
    }
}

fn notice(ending: Ending) -> &'static str {
    match ending {
        Ending::Running | Ending::Answered => RETAINED_REPLY,
        Ending::Failed => FAILED_RETAINED_REPLY,
    }
}

pub(super) fn append_notice(text: &str, notice: Option<&str>) -> String {
    let Some(notice) = notice else {
        return bound_outbound(text);
    };
    // The shared truncator retains both ends; the fixed notice fits wholly in its tail budget.
    bound_outbound(&format!("{text}\n\n{notice}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_photos_notice_survives_a_maximum_length_and_one_byte_over_answer() {
        for length in [
            crate::transport::MAX_OUTBOUND_TEXT_BYTES,
            crate::transport::MAX_OUTBOUND_TEXT_BYTES + 1,
        ] {
            let text = append_notice(&"x".repeat(length), Some(RETAINED_REPLY));
            assert!(text.len() <= crate::transport::MAX_OUTBOUND_TEXT_BYTES);
            assert!(text.ends_with(RETAINED_REPLY));
            assert!(text.contains("truncated by the gateway"));
        }
        let text = append_notice(
            &"🍊".repeat(crate::transport::MAX_OUTBOUND_TEXT_BYTES),
            Some(FAILED_RETAINED_REPLY),
        );
        assert!(text.len() <= crate::transport::MAX_OUTBOUND_TEXT_BYTES);
        assert!(text.ends_with(FAILED_RETAINED_REPLY));
    }
    #[tokio::test]
    async fn late_photos_completion_and_stop_elect_exactly_one_owner_for_cancelled_collection() {
        use crate::collection::{Collector, Offered};
        use dekopon_broker_protocol::{Conversation, ConversationKind};
        use dekopon_test_support::RecordingDriver;
        for completion_wins in [false, true] {
            let subject = dekopon_core::ExternalSubject::whatsapp("16034700182").unwrap();
            let native = Conversation {
                kind: ConversationKind::DirectMessage,
                container: Some("123:456".into()),
                id: "16034700182".into(),
                thread: None,
            };
            let reply = ReplyTarget::WhatsApp {
                recipient: "16034700182".into(),
            };
            let run = LatePhotos {
                inner: Arc::new(Mutex::new(State {
                    route_key: "test-route".into(),
                    subject: subject.clone(),
                    conversation: ConversationKey::private(
                        &"reviewer".parse().unwrap(),
                        "dev",
                        &native.key(),
                        &subject,
                    ),
                    native_conversation: native.clone(),
                    pending_notice: false,
                    intake_stopped: false,
                    reply: reply.clone(),
                    persistent: true,
                    transport_kind: ChatTransportKind::Whatsapp,
                    scope: None,
                    ids: Vec::new(),
                    ending: Ending::Answered,
                })),
                cancellation: SessionCancellation::new(),
            };
            assert!(run.cancellation.claim_completion());
            let message = InboundMessage {
                transport: "dev".into(),
                transport_kind: ChatTransportKind::Whatsapp,
                subject: subject.clone(),
                conversation: native.clone(),
                message_id: "wamid.test".into(),
                text: String::new(),
                assets: vec![asset::PendingAsset {
                    name: "photo.png".into(),
                    mime: "image/png".into(),
                    size: None,
                    source: None,
                }],
                asset_overflow: false,
                addressed: None,
                thread_continuation: None,
                reply,
                liveness: None,
                receive_span: tracing::Span::none(),
                received_at: tokio::time::Instant::now(),
                native_group: None,
                constituents: Vec::new(),
                late_photos: Some(LatePhotoReceipt::Run(run.clone())),
            };
            let config = serde_json::from_value(serde_json::json!({"kind":"whatsappCloudApi", "name":"dev", "appSecretEnv":"APP", "verifyTokenEnv":"VERIFY", "accessTokenEnv":"ACCESS", "bind":"127.0.0.1:9080", "callbackPath":"/wa", "wabaId":"123", "phoneNumberId":"456", "graphApiVersion":"v25.0"})).unwrap();
            let mut collector = Collector::new(&[config], 1);
            assert!(matches!(
                collector.offer(0, message.clone()),
                Offered::Pending
            ));
            let active = ActiveSessions::new(1);
            let intake = active
                .intakes
                .register(&SessionGate::new(1), &message)
                .unwrap();
            if completion_wins {
                assert!(intake.claim_completion());
            }
            // Keep the finalized control registered to expose the original check-before-Drop gap.
            let stop = CancelRequest {
                transport: "dev".into(),
                conversation_id: native.key(),
                subject: subject.canonical(),
                via: dekopon_agent::CancelVia::StopReply,
            };
            assert!(collector.cancel(&stop));
            let outcome = active.cancel(&stop);
            let driver = Arc::new(RecordingDriver::default());
            let courier = Arc::clone(&driver) as Arc<dyn ChatDriver>;
            if completion_wins {
                assert_eq!(outcome, CancelOutcome::Completing);
                assert!(!run.inner.lock().unwrap().intake_stopped);
                assert!(!intake.control.announce_stop.load(Ordering::Acquire));
                // Dispatch owns the removed batch's stopped ending when intake is completing.
                assert!(answer(&courier, &message, STOPPED_REPLY).await);
                assert!(intake.claim_completion());
            } else {
                assert_eq!(outcome, CancelOutcome::Cancelled);
                assert!(run.inner.lock().unwrap().intake_stopped);
                assert!(!intake.claim_completion());
                intake.stopped(&courier, &message).await;
            }
            assert_eq!(driver.replies(), [STOPPED_REPLY]);
            assert!(collector.deadline().is_none());
        }
    }
}
