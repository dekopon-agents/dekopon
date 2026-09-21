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
const REFUSED_REPLY: &str = "The additional photos were not retained for this request. Please send them again with your next request.";

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
    ) -> &'static str {
        match self {
            Self::Run(run) => run.retain(runner, route, message, driver).await,
            Self::HistoryUnavailable => {
                answer(driver, message, REFUSED_REPLY).await;
                "late-history-unavailable"
            }
        }
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
    ) -> &'static str {
        if self.cancellation.is_cancelled() {
            return "stopped";
        }
        // Separate from execution admission: a busy model cannot exclude content, but neither
        // can a flood of metadata-only inputs create unlimited broker connections.
        let Ok(_permit) = runner.gate.late_permits.try_acquire() else {
            answer(driver, message, REFUSED_REPLY).await;
            return "late-busy";
        };
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
            let mut state = self.inner.lock().expect("late photo state");
            if self.cancellation.is_cancelled() {
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

    pub(super) fn finish(&self, assets: &AssetStore, succeeded: bool) -> Option<String> {
        let mut state = self.inner.lock().expect("late photo state");
        state.ending = if succeeded {
            Ending::Answered
        } else {
            Ending::Failed
        };
        let ids = std::mem::take(&mut state.ids);
        let pending_notice = std::mem::take(&mut state.pending_notice);
        if self.cancellation.is_cancelled() {
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
            }
            .to_owned(),
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
}
