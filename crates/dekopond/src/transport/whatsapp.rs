//! Meta WhatsApp Cloud API webhook and text reply transport.
//!
//! TLS terminates outside this daemon. The listener verifies Meta's signature over the exact raw
//! body before parsing, claims message IDs in a bounded process-local set, enqueues a whole delivery
//! atomically, and acknowledges before any session or model work begins.

use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use axum::{
    Router,
    body::to_bytes,
    extract::{RawQuery, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use dekopon_broker_protocol::{ChatTransportKind, Conversation, ConversationKind};
use dekopon_core::{ExternalSubject, Redacted};
use futures_util::{StreamExt as _, future::BoxFuture};
use hmac::{Hmac, KeyInit as _, Mac as _};
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tracing::{Instrument as _, Span};

use crate::{
    config::{LivenessMode, LivenessSettings},
    transport::{
        ChatDriver, ChatTransport, InboundMessage, LivenessTarget, OutboundReply, ReplyTarget,
        SeenIds, TextUnit, TransportError, TransportEvent, TransportIdentity, TypingLease,
        bound_inbound, credential_client, receive_span, record_conversation, split_message,
    },
};

const MAX_WEBHOOK_BODY_BYTES: usize = 256 * 1024;
const MAX_WEBHOOK_HEADERS: usize = 32;
const MAX_HEADER_BYTES: usize = 8 * 1024;
/// Hyper's parser buffer also includes the request line; keep it finite above the field ceilings.
const MAX_CONNECTION_BUFFER_BYTES: usize = 16 * 1024;
const MAX_QUERY_BYTES: usize = 2 * 1024;
const MAX_QUERY_VALUE_BYTES: usize = 512;
const MAX_MESSAGES_PER_DELIVERY: usize = 128;
/// Message identifiers one webhook remembers, four times the socket transports' ring.
///
/// Deliberately larger: Slack's and Discord's rings only have to bridge one reconnect, while this
/// is the whole replay defense for a webhook Meta retries for far longer than a delivery burst,
/// and each burst may itself carry [`MAX_MESSAGES_PER_DELIVERY`] identifiers.
const MAX_DEDUP_IDS: usize = 4096;
const MAX_QUEUED_MESSAGES: usize = 512;
const WEBHOOK_QUEUE: usize = 64;
const MAX_WEBHOOK_CONCURRENCY: usize = 16;
const WEBHOOK_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const GRAPH_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Deadline on one cosmetic Graph call. Liveness decorates an answer; it never delays one.
const LIVENESS_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the typing indicator is re-posted while a session runs.
///
/// Meta dismisses it after 25 seconds; this sits under that with room for one slow call. Whether
/// re-posting actually restarts that timer is the open question [`TypingLease::renew`] states.
const TYPING_RENEW_INTERVAL: Duration = Duration::from_secs(20);
const MAX_GRAPH_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_WHATSAPP_TEXT_CHARS: usize = 4096;
/// One refusal reason is reported at most this often, with the count it stands for.
const REFUSAL_LOG_WINDOW: Duration = Duration::from_secs(60);
/// How long the accept loop waits after running out of descriptors or socket buffers.
///
/// Shared with the broker's two accept loops so a wait tuned in one place is not silently
/// different here.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(dekopon_core::ACCEPT_BACKOFF_MS);

pub(crate) struct WhatsappTransport {
    name: String,
    bind: std::net::SocketAddr,
    callback_path: String,
    state: WebhookState,
    receiver: mpsc::Receiver<QueuedDelivery>,
    pending: VecDeque<QueuedDelivery>,
    driver: Arc<WhatsappDriver>,
    server: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Clone)]
struct WebhookState {
    name: String,
    app_secret: Arc<Redacted<Vec<u8>>>,
    verify_token: Arc<Redacted<String>>,
    waba_id: String,
    phone_number_id: String,
    /// `liveness.mode: native` — an inbound message carries coordinates for the typing indicator.
    ///
    /// One decision, because the Cloud API has exactly one liveness surface: there is no message
    /// edit, no stream, and no button inside the 24-hour service window. Configuration refuses
    /// `liveness.stream` and `liveness.cancelButton` on this transport, so neither reaches here;
    /// `liveness.progress: message` is accepted and does nothing at run time, because the driver
    /// answers `None` for the progress surface rather than switching on a flag.
    native: bool,
    sender: mpsc::Sender<QueuedDelivery>,
    dedup: Arc<Mutex<ClaimedIds>>,
    refusals: Arc<Mutex<RefusalLog>>,
    queue_capacity: Arc<Semaphore>,
    concurrency: Arc<Semaphore>,
}

/// Why one webhook request was refused, as a stable low-cardinality telemetry reason.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Refusal {
    /// No usable `X-Hub-Signature-256`, so there was nothing to verify.
    Unsigned,
    /// A signature that does not match the app secret over these exact bytes.
    Signature,
    /// Headers or a body past the bounds this listener accepts.
    Oversize,
    /// Signed, but not a delivery this transport can read.
    Malformed,
    /// Concurrency or queue capacity is spent; Meta should redeliver.
    Saturated,
    /// The request did not finish inside the handler deadline.
    Timeout,
    /// A subscription-verification challenge that did not prove the verify token.
    Verification,
    /// Internal state this handler needs is unusable.
    Unavailable,
}

impl Refusal {
    const COUNT: usize = 8;

    const fn index(self) -> usize {
        match self {
            Self::Unsigned => 0,
            Self::Signature => 1,
            Self::Oversize => 2,
            Self::Malformed => 3,
            Self::Saturated => 4,
            Self::Timeout => 5,
            Self::Verification => 6,
            Self::Unavailable => 7,
        }
    }

    const fn reason(self) -> &'static str {
        match self {
            Self::Unsigned => "unsigned",
            Self::Signature => "signature",
            Self::Oversize => "oversize",
            Self::Malformed => "malformed",
            Self::Saturated => "saturated",
            Self::Timeout => "timeout",
            Self::Verification => "verification",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Per-reason emission windows, so a refusal always leaves a trace and never a flood.
///
/// The callback is this daemon's only public surface, so an unauthenticated stranger decides how
/// often refusals happen. Emitting one line each would let that stranger write unbounded volume
/// into a shared log sink; emitting none is how a wrong app secret becomes invisible. One line per
/// reason per window, carrying how many it stands for, is both.
struct RefusalLog {
    windows: [Option<Instant>; Refusal::COUNT],
    suppressed: [u64; Refusal::COUNT],
}

impl RefusalLog {
    const fn new() -> Self {
        Self {
            windows: [None; Refusal::COUNT],
            suppressed: [0; Refusal::COUNT],
        }
    }

    /// Reports how many earlier refusals of this reason an emission now stands for, or nothing when
    /// this reason has already been reported inside the current window.
    fn admit(&mut self, refusal: Refusal, now: Instant) -> Option<u64> {
        let index = refusal.index();
        let due =
            self.windows[index].is_none_or(|last| now.duration_since(last) >= REFUSAL_LOG_WINDOW);
        if !due {
            self.suppressed[index] = self.suppressed[index].saturating_add(1);
            return None;
        }
        self.windows[index] = Some(now);
        Some(std::mem::take(&mut self.suppressed[index]))
    }
}

/// Answers one refused request, naming its reason once per window and nothing about its content.
fn refuse(state: &WebhookState, refusal: Refusal, status: StatusCode) -> Response {
    // A poisoned refusal log must not silence the refusal it exists to record.
    let admitted = match state.refusals.lock() {
        Ok(mut log) => log.admit(refusal, Instant::now()),
        Err(_) => Some(0),
    };
    if let Some(suppressed) = admitted {
        tracing::warn!(
            event = "gateway_whatsapp_webhook_refused",
            transport = %state.name,
            reason = refusal.reason(),
            status = status.as_u16(),
            suppressed,
        );
    }
    content_free(status)
}

struct QueuedDelivery {
    messages: VecDeque<InboundMessage>,
    _capacity: OwnedSemaphorePermit,
}

/// The webhook's redelivery ring, with the claim/release the acknowledgment needs.
///
/// A webhook answers Meta before the work is queued, so an identifier is *claimed* on arrival and
/// released again if the queue refuses it. The ring underneath is the one Slack and Discord use.
struct ClaimedIds(SeenIds);

impl ClaimedIds {
    fn new() -> Self {
        Self(SeenIds::new(MAX_DEDUP_IDS))
    }

    fn claim(&mut self, messages: Vec<InboundMessage>) -> Vec<InboundMessage> {
        let mut accepted = Vec::with_capacity(messages.len());
        for message in messages {
            if self.0.insert(message.message_id.clone()) {
                accepted.push(message);
            }
        }
        accepted
    }

    /// Undoes a claim that never reached the queue, so Meta's redelivery is accepted rather than
    /// silently acknowledged as a duplicate of work nothing is doing.
    fn release(&mut self, claimed: &[String]) {
        for id in claimed {
            self.0.remove(id);
        }
    }
}

impl WhatsappTransport {
    /// Every credential arrives non-empty: [`crate::transport::read_credential`] refuses an
    /// exported-but-blank variable, because an empty app secret is an HMAC key anyone can guess.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        name: String,
        bind: std::net::SocketAddr,
        callback_path: String,
        waba_id: String,
        phone_number_id: String,
        graph_api_version: String,
        graph_endpoint: String,
        app_secret: String,
        verify_token: String,
        access_token: String,
        liveness: LivenessSettings,
    ) -> Result<Self, TransportError> {
        let (sender, receiver) = mpsc::channel(WEBHOOK_QUEUE);
        let http = credential_client(GRAPH_REQUEST_TIMEOUT)
            .build()
            .map_err(|source| TransportError::Request(Box::new(source)))?;
        let driver = Arc::new(WhatsappDriver {
            transport: name.clone(),
            endpoint: graph_endpoint,
            version: graph_api_version,
            phone_number_id: phone_number_id.clone(),
            access_token: Redacted::new(access_token),
            http,
        });
        Ok(Self {
            name: name.clone(),
            bind,
            callback_path,
            state: WebhookState {
                name,
                app_secret: Arc::new(Redacted::new(app_secret.into_bytes())),
                verify_token: Arc::new(Redacted::new(verify_token)),
                waba_id,
                phone_number_id,
                native: liveness.mode == LivenessMode::Native,
                sender,
                dedup: Arc::new(Mutex::new(ClaimedIds::new())),
                refusals: Arc::new(Mutex::new(RefusalLog::new())),
                queue_capacity: Arc::new(Semaphore::new(MAX_QUEUED_MESSAGES)),
                concurrency: Arc::new(Semaphore::new(MAX_WEBHOOK_CONCURRENCY)),
            },
            receiver,
            pending: VecDeque::new(),
            driver,
            server: None,
        })
    }
}

impl Drop for WhatsappTransport {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}

impl ChatTransport for WhatsappTransport {
    fn name(&self) -> &str {
        &self.name
    }

    fn connect(&mut self) -> BoxFuture<'_, Result<TransportIdentity, TransportError>> {
        Box::pin(async move {
            if self.server.is_some() {
                return Err(TransportError::Response);
            }
            let listener = tokio::net::TcpListener::bind(self.bind)
                .await
                .map_err(TransportError::Io)?;
            let router = Router::new()
                .route(&self.callback_path, get(verify_subscription))
                .route(&self.callback_path, post(receive_webhook))
                .fallback(|| async { content_free(StatusCode::NOT_FOUND) })
                .method_not_allowed_fallback(|| async {
                    content_free(StatusCode::METHOD_NOT_ALLOWED)
                })
                .with_state(self.state.clone());
            let name = self.name.clone();
            self.server = Some(tokio::spawn(async move {
                let mut connections = tokio::task::JoinSet::new();
                let connection_limit = Arc::new(Semaphore::new(MAX_WEBHOOK_CONCURRENCY));
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let stream = match accepted {
                                Ok((stream, _peer)) => stream,
                                // One failed accept is not a failed listener. Ending the loop here
                                // is permanent — nothing restarts a transport reader — so only a
                                // socket that can never serve again may end it.
                                Err(error) => match classify_accept(&error) {
                                    AcceptFailure::Connection => {
                                        tracing::debug!(
                                            event = "gateway_whatsapp_accept_failed",
                                            transport = %name,
                                            kind = "connection",
                                            error = %error,
                                        );
                                        continue;
                                    }
                                    AcceptFailure::Exhausted => {
                                        tracing::warn!(
                                            event = "gateway_whatsapp_accept_failed",
                                            transport = %name,
                                            kind = "exhausted",
                                            error = %error,
                                        );
                                        tokio::time::sleep(ACCEPT_BACKOFF).await;
                                        continue;
                                    }
                                    AcceptFailure::Fatal => {
                                        tracing::error!(
                                            event = "gateway_whatsapp_listener_stopped",
                                            transport = %name,
                                            error = %error,
                                        );
                                        return;
                                    }
                                },
                            };
                            let Ok(connection_permit) = Arc::clone(&connection_limit).try_acquire_owned() else {
                                // Drop immediately: even slow pre-header clients are concurrency-bound.
                                continue;
                            };
                            let service = TowerToHyperService::new(router.clone());
                            connections.spawn(async move {
                                let _connection_permit = connection_permit;
                                let mut builder = hyper::server::conn::http1::Builder::new();
                                builder
                                    .max_headers(MAX_WEBHOOK_HEADERS)
                                    .max_buf_size(MAX_CONNECTION_BUFFER_BYTES);
                                let connection = builder.serve_connection(TokioIo::new(stream), service);
                                // This includes header parsing, so a slowloris cannot hold a socket
                                // beyond the same hard deadline as a buffered webhook request.
                                #[allow(
                                    clippy::let_underscore_must_use,
                                    reason = "the outcome is that one untrusted client's connection ended, by deadline or by hanging up; the router already recorded whatever it answered"
                                )]
                                let _ = tokio::time::timeout(WEBHOOK_REQUEST_TIMEOUT, connection).await;
                            });
                        }
                        Some(_) = connections.join_next(), if !connections.is_empty() => {}
                    }
                }
            }));
            Ok(TransportIdentity::default())
        })
    }

    fn next(&mut self) -> BoxFuture<'_, Result<TransportEvent, TransportError>> {
        Box::pin(async move {
            enum Source {
                Delivery(QueuedDelivery),
                ListenerStopped,
            }

            loop {
                if let Some(delivery) = self.pending.front_mut() {
                    if let Some(message) = delivery.messages.pop_front() {
                        return Ok(TransportEvent::Message(Box::new(message)));
                    }
                    self.pending.pop_front();
                    continue;
                }

                let source = {
                    let Some(server) = self.server.as_mut() else {
                        return Err(TransportError::Closed);
                    };
                    tokio::select! {
                        delivery = self.receiver.recv() => delivery
                            .map_or(Source::ListenerStopped, Source::Delivery),
                        _ = server => Source::ListenerStopped,
                    }
                };
                match source {
                    Source::Delivery(delivery) => self.pending.push_back(delivery),
                    Source::ListenerStopped => {
                        self.server.take();
                        return Err(TransportError::Closed);
                    }
                }
            }
        })
    }

    /// One handle for replying and for the one liveness surface the Cloud API has.
    ///
    /// What the capability objects advertise is what WhatsApp implements, not what this deployment
    /// asked for: `liveness.mode: off` withholds the target on the inbound message instead, so the
    /// policy has nothing to render against and the transport is reply-only exactly as before.
    fn driver(&self) -> Arc<dyn ChatDriver> {
        Arc::clone(&self.driver) as Arc<dyn ChatDriver>
    }
}

/// What a failed `accept()` says about the listening socket itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AcceptFailure {
    /// One connection died before it was handed over; the listener is unaffected.
    Connection,
    /// Descriptors or socket buffers are spent; the listener works again once one is freed.
    Exhausted,
    /// The listening socket can no longer serve anything.
    Fatal,
}

fn classify_accept(error: &io::Error) -> AcceptFailure {
    match error.kind() {
        io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionRefused
        | io::ErrorKind::Interrupted
        | io::ErrorKind::TimedOut
        | io::ErrorKind::WouldBlock => AcceptFailure::Connection,
        io::ErrorKind::OutOfMemory => AcceptFailure::Exhausted,
        _ => match error.raw_os_error() {
            // Process or system descriptor and buffer exhaustion. A pod under its memory limit with
            // many half-open connections reaches these, and every one of them is temporary.
            Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM) => {
                AcceptFailure::Exhausted
            }
            _ => AcceptFailure::Fatal,
        },
    }
}

async fn verify_subscription(
    State(state): State<WebhookState>,
    RawQuery(query): RawQuery,
) -> Response {
    let Some(query) = query.filter(|query| query.len() <= MAX_QUERY_BYTES) else {
        return refuse(&state, Refusal::Verification, StatusCode::FORBIDDEN);
    };
    let Ok(fields) = parse_query(&query) else {
        return refuse(&state, Refusal::Verification, StatusCode::FORBIDDEN);
    };
    let (Some(mode), Some(token), Some(challenge)) = (
        exactly_one(&fields, "hub.mode"),
        exactly_one(&fields, "hub.verify_token"),
        exactly_one(&fields, "hub.challenge"),
    ) else {
        return refuse(&state, Refusal::Verification, StatusCode::FORBIDDEN);
    };
    if mode != "subscribe"
        || !constant_time_eq(token.as_bytes(), state.verify_token.expose().as_bytes())
    {
        return refuse(&state, Refusal::Verification, StatusCode::FORBIDDEN);
    }
    text_response(StatusCode::OK, challenge.to_owned())
}

async fn receive_webhook(State(state): State<WebhookState>, request: Request) -> Response {
    let Ok(permit) = Arc::clone(&state.concurrency).try_acquire_owned() else {
        return refuse(&state, Refusal::Saturated, StatusCode::SERVICE_UNAVAILABLE);
    };
    // One span per delivery, opened before a byte of the body is read, so the signature check, the
    // dedup claim, and the 200 that acknowledges the delivery are inside the trace of whatever the
    // delivery turns out to carry — including a refusal, which is a receipt with no message.
    let received = receive_span(ChatTransportKind::Whatsapp);
    match tokio::time::timeout(
        WEBHOOK_REQUEST_TIMEOUT,
        process_webhook(&state, request, permit, &received).instrument(received.clone()),
    )
    .await
    {
        Ok(response) => response,
        Err(_) => refuse(&state, Refusal::Timeout, StatusCode::REQUEST_TIMEOUT),
    }
}

/// Verifies, parses, claims, and enqueues one signed delivery.
///
/// Everything from the dedup claim to the enqueue is synchronous on purpose: the caller's deadline
/// can only cancel this at an `.await`, and a cancellation between claiming a message ID and
/// queueing it would drop that message and swallow Meta's redelivery of it.
async fn process_webhook(
    state: &WebhookState,
    request: Request,
    _permit: tokio::sync::OwnedSemaphorePermit,
    received: &Span,
) -> Response {
    if !headers_bounded(request.headers()) {
        return refuse(state, Refusal::Oversize, StatusCode::BAD_REQUEST);
    }
    let Some(signature) = exact_signature(request.headers()) else {
        return refuse(state, Refusal::Unsigned, StatusCode::UNAUTHORIZED);
    };
    if request
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_WEBHOOK_BODY_BYTES)
    {
        return refuse(state, Refusal::Oversize, StatusCode::PAYLOAD_TOO_LARGE);
    }
    let body = match to_bytes(request.into_body(), MAX_WEBHOOK_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => return refuse(state, Refusal::Oversize, StatusCode::PAYLOAD_TOO_LARGE),
    };
    if !valid_hmac_sha256(state.app_secret.expose(), &body, &signature) {
        return refuse(state, Refusal::Signature, StatusCode::UNAUTHORIZED);
    }
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return refuse(state, Refusal::Malformed, StatusCode::BAD_REQUEST),
    };
    let messages = match parse_delivery(state, &value, received) {
        Ok(messages) => messages,
        Err(()) => return refuse(state, Refusal::Malformed, StatusCode::BAD_REQUEST),
    };
    let mut dedup = match state.dedup.lock() {
        Ok(dedup) => dedup,
        Err(_) => {
            return refuse(
                state,
                Refusal::Unavailable,
                StatusCode::INTERNAL_SERVER_ERROR,
            );
        }
    };
    let accepted = dedup.claim(messages);
    if accepted.is_empty() {
        return content_free(StatusCode::OK);
    }
    // Meta sends one message per delivery in practice and the parser tolerates a batch, so the
    // identifier is recorded only when the delivery is the one message this span describes. A
    // batched delivery leaves it unset and parents every message's `gateway.message` here.
    if let [only] = accepted.as_slice() {
        received.record("message.id", only.message_id.as_str());
    }
    let permit_count = u32::try_from(accepted.len()).expect("delivery bound fits u32");
    // Only the claimed IDs are needed to undo the claim, so the messages themselves move into the
    // delivery rather than being held a second time for a path that usually does not run.
    let claimed: Vec<String> = accepted
        .iter()
        .map(|message| message.message_id.clone())
        .collect();
    let Ok(capacity) = Arc::clone(&state.queue_capacity).try_acquire_many_owned(permit_count)
    else {
        dedup.release(&claimed);
        return refuse(state, Refusal::Saturated, StatusCode::SERVICE_UNAVAILABLE);
    };
    let delivery = QueuedDelivery {
        messages: accepted.into(),
        _capacity: capacity,
    };
    if state.sender.try_send(delivery).is_err() {
        dedup.release(&claimed);
        return refuse(state, Refusal::Saturated, StatusCode::SERVICE_UNAVAILABLE);
    }
    content_free(StatusCode::OK)
}

fn parse_delivery(
    state: &WebhookState,
    root: &Value,
    received: &Span,
) -> Result<Vec<InboundMessage>, ()> {
    let object = root.as_object().ok_or(())?;
    if object.get("object").and_then(Value::as_str) != Some("whatsapp_business_account") {
        return Ok(Vec::new());
    }
    let Some(entries) = object.get("entry").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut accepted = Vec::new();
    for entry in entries {
        if entry.get("id").and_then(Value::as_str) != Some(state.waba_id.as_str()) {
            continue;
        }
        let Some(changes) = entry.get("changes").and_then(Value::as_array) else {
            continue;
        };
        for change in changes {
            if change.get("field").and_then(Value::as_str) != Some("messages") {
                continue;
            }
            let value = &change["value"];
            if value.get("messaging_product").and_then(Value::as_str) != Some("whatsapp")
                || value
                    .pointer("/metadata/phone_number_id")
                    .and_then(Value::as_str)
                    != Some(state.phone_number_id.as_str())
            {
                continue;
            }
            let own_number = value
                .pointer("/metadata/display_phone_number")
                .and_then(Value::as_str)
                .map(digits_only);
            let Some(messages) = value.get("messages").and_then(Value::as_array) else {
                continue;
            };
            let contacts = value.get("contacts").and_then(Value::as_array);
            for (index, message) in messages.iter().enumerate() {
                if message.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }
                let (Some(id), Some(sender), Some(text)) = (
                    message.get("id").and_then(Value::as_str),
                    message.get("from").and_then(Value::as_str),
                    message.pointer("/text/body").and_then(Value::as_str),
                ) else {
                    continue;
                };
                // An individual message, positively: the delivery's `contacts` name the human the
                // Cloud API would send a reply to, and on an individual message exactly one of
                // them is `from`. A group payload names the group in `from` — with the human in
                // `group_id`/`participant`, which are kept as the two checks that say so outright
                // — and no contact matches it, so a shape Meta adds later that this gateway cannot
                // answer is dropped rather than answered as if the group were a person.
                let individual = contacts.is_some_and(|contacts| {
                    contacts
                        .iter()
                        .any(|contact| contact.get("wa_id").and_then(Value::as_str) == Some(sender))
                });
                if !individual
                    || message.get("group_id").is_some()
                    || message.get("participant").is_some()
                {
                    // On an event rather than on `received`: one span covers the whole delivery,
                    // so a `drop.reason` recorded there is overwritten by the next message's and
                    // stains the trace of an accepted message beside it.
                    tracing::debug!(
                        event = "gateway_message_ignored",
                        transport = %state.name,
                        reason = "group-unsupported",
                        message.index = index
                    );
                    continue;
                }
                if !canonical_whatsapp_message_id(id)
                    || sender.is_empty()
                    || sender.len() > 64
                    || sender.starts_with('0')
                    || !sender.bytes().all(|byte| byte.is_ascii_digit())
                    || text.trim().is_empty()
                    || own_number.as_deref() == Some(sender)
                {
                    continue;
                }
                let Ok(subject) = ExternalSubject::whatsapp(sender) else {
                    continue;
                };
                if accepted.len() == MAX_MESSAGES_PER_DELIVERY {
                    return Err(());
                }
                // The business account and phone number Meta signed this delivery under are the
                // container; the sender is the conversation, because an individual message has
                // nothing else it could be addressed to.
                let conversation = Conversation {
                    kind: ConversationKind::DirectMessage,
                    container: Some(format!("{}:{}", state.waba_id, state.phone_number_id)),
                    id: sender.to_owned(),
                    thread: None,
                };
                record_conversation(received, &conversation);
                accepted.push(InboundMessage {
                    transport: state.name.clone(),
                    transport_kind: ChatTransportKind::Whatsapp,
                    subject,
                    conversation,
                    message_id: id.to_owned(),
                    text: bound_inbound(text),
                    assets: Vec::new(),
                    addressed: None,
                    thread_continuation: None,
                    reply: ReplyTarget::WhatsApp {
                        recipient: sender.to_owned(),
                    },
                    // The typing indicator is addressed to the message it answers rather than to
                    // the conversation: the one Graph call that shows it also marks that message
                    // read, and the read is what carries the identifier.
                    liveness: state.native.then(|| LivenessTarget::WhatsApp {
                        recipient: sender.to_owned(),
                        inbound_message_id: id.to_owned(),
                    }),
                    receive_span: received.clone(),
                });
            }
        }
    }
    Ok(accepted)
}

fn digits_only(value: &str) -> String {
    value
        .bytes()
        .filter(u8::is_ascii_digit)
        .map(char::from)
        .collect()
}

fn canonical_whatsapp_message_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn headers_bounded(headers: &HeaderMap) -> bool {
    let mut count = 0_usize;
    let mut bytes = 0_usize;
    for (name, value) in headers {
        count = count.saturating_add(1);
        bytes = bytes
            .saturating_add(name.as_str().len())
            .saturating_add(value.as_bytes().len());
        if count > MAX_WEBHOOK_HEADERS || bytes > MAX_HEADER_BYTES {
            return false;
        }
    }
    true
}

fn exact_signature(headers: &HeaderMap) -> Option<[u8; 32]> {
    let values: Vec<_> = headers.get_all("x-hub-signature-256").iter().collect();
    if values.len() != 1 {
        return None;
    }
    let value = values[0].to_str().ok()?.strip_prefix("sha256=")?;
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        digest[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Some(digest)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) fn hmac_sha256(key: &[u8], body: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of every length");
    mac.update(body);
    mac.finalize().into_bytes().into()
}

fn valid_hmac_sha256(key: &[u8], body: &[u8], signature: &[u8]) -> bool {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of every length");
    mac.update(body);
    mac.verify_slice(signature).is_ok()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let maximum = left.len().max(right.len());
    for index in 0..maximum {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

/// Decodes the verification query, refusing any field that could grow the echoed challenge.
///
/// `form_urlencoded` is lossy where the hand-written decoder was strict: a malformed escape or a
/// non-UTF-8 byte survives as literal text or `U+FFFD` instead of failing the parse. Neither can
/// reach the response — such a `hub.verify_token` fails the constant-time comparison and such a
/// `hub.mode` is not `subscribe` — so the refusal happens one step later, still as 403.
fn parse_query(query: &str) -> Result<Vec<(String, String)>, ()> {
    let mut fields = Vec::new();
    for (name, value) in form_urlencoded::parse(query.as_bytes()) {
        if name.len() > MAX_QUERY_VALUE_BYTES || value.len() > MAX_QUERY_VALUE_BYTES {
            return Err(());
        }
        fields.push((name.into_owned(), value.into_owned()));
    }
    Ok(fields)
}

fn exactly_one<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    let mut matches = fields.iter().filter(|(field, _)| field == name);
    let value = matches.next()?.1.as_str();
    matches.next().is_none().then_some(value)
}

fn content_free(status: StatusCode) -> Response {
    text_response(status, String::new())
}

fn text_response(status: StatusCode, body: String) -> Response {
    let mut response = (status, body).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// The answering half of the transport, plus the one liveness surface the Cloud API has.
struct WhatsappDriver {
    transport: String,
    endpoint: String,
    version: String,
    phone_number_id: String,
    access_token: Redacted<String>,
    http: reqwest::Client,
}

impl WhatsappDriver {
    /// The one Graph endpoint this transport posts to, for answers and for liveness alike.
    fn messages_url(&self) -> String {
        format!(
            "{}/{}/{}/messages",
            self.endpoint, self.version, self.phone_number_id
        )
    }

    /// One text message, sent exactly once and never retried.
    async fn send_text(&self, recipient: &str, body: &str) -> Result<(), TransportError> {
        #[allow(
            clippy::map_err_ignore,
            reason = "serializing a serde_json::Value cannot fail: it holds no non-string map keys \
                      and serde_json::Number rejects non-finite floats"
        )]
        let payload = serde_json::to_vec(&json!({
            "messaging_product": "whatsapp",
            "recipient_type": "individual",
            "to": recipient,
            "type": "text",
            "text": { "preview_url": false, "body": body }
        }))
        .map_err(|_| TransportError::Response)?;
        let response = self
            .http
            .post(self.messages_url())
            .bearer_auth(self.access_token.expose())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(payload)
            .send()
            .await
            .map_err(|source| TransportError::Request(Box::new(source)))?;
        let status = response.status();
        let bytes = bounded_response(response).await?;
        if !status.is_success() {
            return Err(TransportError::Service {
                code: format!("http-{}", status.as_u16()),
            });
        }
        let value: Value =
            serde_json::from_slice(&bytes).map_err(TransportError::MalformedResponse)?;
        if value.get("messaging_product").and_then(Value::as_str) != Some("whatsapp") {
            return Err(TransportError::Response);
        }
        if !value
            .pointer("/messages/0/id")
            .and_then(Value::as_str)
            .is_some_and(canonical_whatsapp_message_id)
        {
            return Err(TransportError::Response);
        }
        Ok(())
    }
}

#[async_trait]
impl TypingLease for WhatsappDriver {
    fn renew_every(&self) -> Duration {
        TYPING_RENEW_INTERVAL
    }

    /// Shows the typing indicator by marking the inbound message read, which is one call.
    ///
    /// The Cloud API has no separate typing endpoint: the indicator rides the `status: read`
    /// message-status call as `typing_indicator`, so a run that shows it also delivers the blue
    /// ticks, and a run with liveness off delivers neither. That fusion is Meta's, not a choice
    /// made here.
    ///
    /// **The renewal is unverified against the Graph API.** Meta documents the indicator as
    /// dismissed after 25 seconds or when the reply lands, and documents nothing about re-posting
    /// it; re-posting every 20 seconds is this transport's bet that a second call restarts that
    /// timer. Only a real WhatsApp Business number and a person watching the conversation can
    /// settle it, so the loopback test below pins the *request* and not Meta's behavior. If the
    /// bet is wrong the cost is dead air after 25 seconds, which the design already accepted.
    async fn renew(&self, target: &LivenessTarget) -> Result<(), TransportError> {
        let LivenessTarget::WhatsApp {
            inbound_message_id, ..
        } = target
        else {
            return Err(TransportError::Response);
        };
        #[allow(
            clippy::map_err_ignore,
            reason = "serializing a serde_json::Value cannot fail: it holds no non-string map keys \
                      and serde_json::Number rejects non-finite floats"
        )]
        let payload = serde_json::to_vec(&json!({
            "messaging_product": "whatsapp",
            "status": "read",
            "message_id": inbound_message_id,
            "typing_indicator": { "type": "text" }
        }))
        .map_err(|_| TransportError::Response)?;
        let response = self
            .http
            .post(self.messages_url())
            .bearer_auth(self.access_token.expose())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(payload)
            .timeout(LIVENESS_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|source| TransportError::Request(Box::new(source)))?;
        let status = response.status();
        let bytes = bounded_response(response).await?;
        if !status.is_success() {
            return Err(TransportError::Service {
                code: format!("http-{}", status.as_u16()),
            });
        }
        let value: Value =
            serde_json::from_slice(&bytes).map_err(TransportError::MalformedResponse)?;
        if value.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(TransportError::Response);
        }
        Ok(())
    }
}

/// Typing is the whole of WhatsApp's liveness.
///
/// The other accessors keep the trait's `None`, and that is the honest answer rather than a gap:
/// the Cloud API has no message edit, so there is no progress message and no stream to grow, and
/// an interactive button needs a template outside the 24-hour service window, so there is no
/// cancel control either. Configuration refuses `liveness.stream` and `liveness.cancelButton` on
/// this transport for those reasons, so neither reaches this driver. `liveness.progress: message`
/// is not refused: it resolves like anywhere else and does nothing here, because the surface it
/// asks for is one of the trait defaults this impl leaves alone.
#[async_trait]
impl ChatDriver for WhatsappDriver {
    async fn reply(
        &self,
        target: &ReplyTarget,
        reply: OutboundReply,
    ) -> Result<(), TransportError> {
        let ReplyTarget::WhatsApp { recipient } = target else {
            return Err(TransportError::Response);
        };
        let OutboundReply { text, images } = reply;
        if !images.is_empty() {
            // Configuration refuses provider attachments on a WhatsApp route, so reaching here
            // means the two disagree. Say which one rather than dropping bytes silently:
            // sending an image needs Meta's media upload, which this transport does not have.
            tracing::error!(
                event = "gateway_whatsapp_image_unsupported",
                transport = %self.transport,
            );
            return Err(TransportError::Response);
        }
        let mut accepted = 0_usize;
        for chunk in split_message(&text, MAX_WHATSAPP_TEXT_CHARS, TextUnit::Scalar) {
            match self.send_text(recipient, &chunk).await {
                Ok(()) => accepted += 1,
                // Name the cause here: the session only learns that a split answer arrived in
                // part, and the service code behind that is otherwise discarded.
                Err(error) if accepted > 0 => {
                    tracing::warn!(
                        event = "gateway_whatsapp_reply_partial",
                        category = error.category(),
                        delivered = accepted,
                    );
                    return Err(TransportError::PartialDelivery);
                }
                Err(error) => return Err(error),
            }
        }
        (accepted > 0).then_some(()).ok_or(TransportError::Response)
    }

    fn typing(&self) -> Option<&dyn TypingLease> {
        Some(self)
    }
}

async fn bounded_response(response: reqwest::Response) -> Result<Vec<u8>, TransportError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_GRAPH_RESPONSE_BYTES as u64)
    {
        return Err(TransportError::Response);
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|source| TransportError::Request(Box::new(source)))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_GRAPH_RESPONSE_BYTES {
            return Err(TransportError::Response);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;

    use super::*;

    fn state() -> WebhookState {
        state_and_receiver().0
    }

    fn state_and_receiver() -> (WebhookState, mpsc::Receiver<QueuedDelivery>) {
        let (sender, receiver) = mpsc::channel(4);
        (
            WebhookState {
                name: "wa".to_owned(),
                app_secret: Arc::new(Redacted::new(b"secret".to_vec())),
                verify_token: Arc::new(Redacted::new("verify".to_owned())),
                waba_id: "123".to_owned(),
                phone_number_id: "456".to_owned(),
                native: true,
                sender,
                dedup: Arc::new(Mutex::new(ClaimedIds::new())),
                refusals: Arc::new(Mutex::new(RefusalLog::new())),
                queue_capacity: Arc::new(Semaphore::new(MAX_QUEUED_MESSAGES)),
                concurrency: Arc::new(Semaphore::new(1)),
            },
            receiver,
        )
    }

    /// The receive span the listener would have opened for this delivery.
    fn received() -> Span {
        receive_span(ChatTransportKind::Whatsapp)
    }

    fn signature(secret: &[u8], body: &[u8]) -> String {
        let digest = hmac_sha256(secret, body);
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        format!("sha256={hex}")
    }

    fn signed_request(body: &[u8]) -> Request {
        Request::builder()
            .method("POST")
            .header("x-hub-signature-256", signature(b"secret", body))
            .body(Body::from(body.to_vec()))
            .expect("request")
    }

    #[test]
    fn hmac_matches_a_known_vector_and_uses_exact_raw_bytes() {
        let compact = br#"{"object":"whatsapp_business_account"}"#;
        let spaced = br#"{ "object": "whatsapp_business_account" }"#;
        assert_ne!(
            hmac_sha256(b"secret", compact),
            hmac_sha256(b"secret", spaced)
        );
        assert!(valid_hmac_sha256(
            b"secret",
            compact,
            &hmac_sha256(b"secret", compact),
        ));
        let known = hmac_sha256(b"key", b"The quick brown fox jumps over the lazy dog");
        let known: String = known.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            known,
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[tokio::test]
    async fn subscription_verification_returns_the_exact_challenge() {
        let response = verify_subscription(
            State(state()),
            RawQuery(Some(
                "hub.mode=subscribe&hub.verify_token=verify&hub.challenge=%E2%9C%93".to_owned(),
            )),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = to_bytes(response.into_body(), 16).await.expect("body");
        assert_eq!(&body[..], "✓".as_bytes());

        for query in [
            "hub.mode=other&hub.verify_token=verify&hub.challenge=x",
            "hub.mode=subscribe&hub.challenge=x",
            "hub.mode=subscribe&hub.verify_token=wrong&hub.challenge=x",
            "hub.mode=subscribe&hub.mode=subscribe&hub.verify_token=verify&hub.challenge=x",
            // A malformed escape now survives the parse as literal text; `%zz` is not `subscribe`.
            "hub.mode=%zz&hub.verify_token=verify&hub.challenge=x",
        ] {
            let response =
                verify_subscription(State(state()), RawQuery(Some(query.to_owned()))).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
    }

    #[tokio::test]
    async fn post_verifies_exact_bytes_before_parsing_and_deduplicates() {
        let body = serde_json::to_vec(&json!({
            "object":"whatsapp_business_account",
            "entry":[{"id":"123","changes":[{"field":"messages","value":{
                "messaging_product":"whatsapp","metadata":{"phone_number_id":"456"},
                "contacts":[{"wa_id":"1603"}],
                "messages":[{"id":"same","from":"1603","type":"text","text":{"body":"hello ✓"}}]
            }}]}]
        }))
        .expect("json");
        let (state, mut receiver) = state_and_receiver();
        let first = process_webhook(
            &state,
            signed_request(&body),
            Arc::clone(&state.concurrency)
                .acquire_owned()
                .await
                .expect("permit"),
            &received(),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(receiver.recv().await.expect("batch").messages.len(), 1);

        let duplicate = process_webhook(
            &state,
            signed_request(&body),
            Arc::clone(&state.concurrency)
                .acquire_owned()
                .await
                .expect("permit"),
            &received(),
        )
        .await;
        assert_eq!(duplicate.status(), StatusCode::OK);
        assert!(receiver.try_recv().is_err());

        let mut changed = body.clone();
        changed.push(b' ');
        let wrong_raw_bytes = process_webhook(
            &state,
            Request::builder()
                .method("POST")
                .header("x-hub-signature-256", signature(b"secret", &body))
                .body(Body::from(changed))
                .expect("request"),
            Arc::clone(&state.concurrency)
                .acquire_owned()
                .await
                .expect("permit"),
            &received(),
        )
        .await;
        assert_eq!(wrong_raw_bytes.status(), StatusCode::UNAUTHORIZED);

        let duplicate_signature = process_webhook(
            &state,
            Request::builder()
                .method("POST")
                .header("x-hub-signature-256", signature(b"secret", &body))
                .header("x-hub-signature-256", signature(b"secret", &body))
                .body(Body::from(body.clone()))
                .expect("request"),
            Arc::clone(&state.concurrency)
                .acquire_owned()
                .await
                .expect("permit"),
            &received(),
        )
        .await;
        assert_eq!(duplicate_signature.status(), StatusCode::UNAUTHORIZED);

        for signature_value in [
            None,
            Some("sha1=00"),
            Some("sha256=gg"),
            Some("sha256=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        ] {
            let mut builder = Request::builder().method("POST");
            if let Some(value) = signature_value {
                builder = builder.header("x-hub-signature-256", value);
            }
            let response = process_webhook(
                &state,
                builder.body(Body::from(body.clone())).expect("request"),
                Arc::clone(&state.concurrency)
                    .acquire_owned()
                    .await
                    .expect("permit"),
                &received(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    async fn saturated_message_capacity_returns_retryable_and_rolls_back_the_claim() {
        let body = serde_json::to_vec(&json!({
            "object":"whatsapp_business_account",
            "entry":[{"id":"123","changes":[{"field":"messages","value":{
                "messaging_product":"whatsapp","metadata":{"phone_number_id":"456"},
                "contacts":[{"wa_id":"1603"}],
                "messages":[{"id":"retry-me","from":"1603","type":"text","text":{"body":"hello"}}]
            }}]}]
        }))
        .expect("json");
        let (mut state, mut receiver) = state_and_receiver();
        state.queue_capacity = Arc::new(Semaphore::new(0));
        let saturated = process_webhook(
            &state,
            signed_request(&body),
            Arc::clone(&state.concurrency)
                .acquire_owned()
                .await
                .expect("permit"),
            &received(),
        )
        .await;
        assert_eq!(saturated.status(), StatusCode::SERVICE_UNAVAILABLE);

        state.queue_capacity.add_permits(1);
        let retried = process_webhook(
            &state,
            signed_request(&body),
            Arc::clone(&state.concurrency)
                .acquire_owned()
                .await
                .expect("permit"),
            &received(),
        )
        .await;
        assert_eq!(retried.status(), StatusCode::OK);
        assert_eq!(
            receiver.recv().await.expect("retried batch").messages.len(),
            1
        );
    }

    #[test]
    fn query_requires_exactly_one_of_each_field() {
        let fields = parse_query("hub.mode=subscribe&hub.verify_token=a&hub.challenge=%E2%9C%93")
            .expect("query");
        assert_eq!(exactly_one(&fields, "hub.challenge"), Some("✓"));
        let repeated = parse_query("hub.mode=subscribe&hub.mode=subscribe").expect("query");
        assert_eq!(exactly_one(&repeated, "hub.mode"), None);
        let oversized = format!("hub.challenge={}", "x".repeat(MAX_QUERY_VALUE_BYTES + 1));
        assert!(parse_query(&oversized).is_err());
    }

    #[test]
    fn signed_payload_scope_batches_and_unsupported_messages_are_filtered() {
        let payload = json!({
            "object": "whatsapp_business_account",
            "entry": [
                {"id":"wrong","changes":[{"field":"messages","value":{
                    "messaging_product":"whatsapp","metadata":{"phone_number_id":"456"},
                    "messages":[{"id":"bad","from":"100","type":"text","text":{"body":"no"}}]
                }}]},
                {"id":"123","changes":[
                    {"field":"messages","value":{"messaging_product":"whatsapp",
                      "metadata":{"phone_number_id":"456","display_phone_number":"+1 999"},
                      "statuses":[{"id":"status"}] }},
                    {"field":"messages","value":{"messaging_product":"whatsapp",
                      "metadata":{"phone_number_id":"456","display_phone_number":"+1 999"},
                      "contacts":[{"wa_id":"1603"},{"wa_id":"01603"},{"wa_id":"1999"}],
                      "messages":[
                        {"id":"one","from":"1603","type":"image"},
                        {"id":"bad-sender","from":"01603","type":"text","text":{"body":"ignore"}},
                        {"id":"bad id","from":"1603","type":"text","text":{"body":"ignore"}},
                        {"id":"two","from":"1603","type":"text","text":{"body":"hello"}},
                        {"id":"self","from":"1999","type":"text","text":{"body":"echo"}}
                      ]}}
                ]}
            ]
        });
        let messages = parse_delivery(&state(), &payload, &received()).expect("delivery");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message_id, "two");
        assert_eq!(messages[0].subject.canonical(), "whatsapp.1603");
        assert_eq!(
            messages[0].conversation.container.as_deref(),
            Some("123:456")
        );
        assert_eq!(messages[0].conversation.id, "1603");
        assert_eq!(messages[0].conversation.key(), "1603");
        assert_eq!(
            messages[0].liveness,
            Some(LivenessTarget::WhatsApp {
                recipient: "1603".to_owned(),
                inbound_message_id: "two".to_owned(),
            }),
            "the typing indicator is addressed to the message it answers"
        );
    }

    /// Only a message whose sender is one of the delivery's own contacts is answered.
    ///
    /// Two shapes at once, because the rule is positive rather than a list of things to refuse: a
    /// group payload, where `from` is the group and the contact is the human inside it, and a
    /// delivery naming no contact for its sender at all — a shape this gateway cannot answer even
    /// though it carries neither of the two group keys.
    #[test]
    fn a_message_whose_sender_is_not_a_contact_of_the_delivery_is_dropped() {
        let group = json!({
            "object": "whatsapp_business_account",
            "entry": [{"id":"123","changes":[{"field":"messages","value":{
                "messaging_product":"whatsapp","metadata":{"phone_number_id":"456"},
                "contacts":[{"wa_id":"1603"}],
                "messages":[{
                    "id":"group-one","from":"120363000000000000","group_id":"120363000000000000",
                    "participant":"1603","type":"text","text":{"body":"hello everyone"}
                }]
            }}]}]
        });
        let uncontacted = json!({
            "object": "whatsapp_business_account",
            "entry": [{"id":"123","changes":[{"field":"messages","value":{
                "messaging_product":"whatsapp","metadata":{"phone_number_id":"456"},
                "contacts":[{"wa_id":"1603"}],
                "messages":[{
                    "id":"elsewhere","from":"1700","type":"text","text":{"body":"hello"}
                }]
            }}]}]
        });

        assert!(
            parse_delivery(&state(), &group, &received())
                .expect("delivery")
                .is_empty(),
            "the Cloud API's individual-message path cannot answer a group"
        );
        assert!(
            parse_delivery(&state(), &uncontacted, &received())
                .expect("delivery")
                .is_empty(),
            "a reply has nowhere to go when no contact in the delivery is the sender"
        );
    }

    /// `liveness.mode: off` leaves the transport exactly as reply-only as it was.
    #[test]
    fn liveness_off_withholds_the_target_rather_than_the_capability() {
        let mut state = state();
        state.native = false;
        let payload = json!({
            "object": "whatsapp_business_account",
            "entry": [{"id":"123","changes":[{"field":"messages","value":{
                "messaging_product":"whatsapp","metadata":{"phone_number_id":"456"},
                "contacts":[{"wa_id":"1603"}],
                "messages":[{"id":"one","from":"1603","type":"text","text":{"body":"hello"}}]
            }}]}]
        });
        let messages = parse_delivery(&state, &payload, &received()).expect("delivery");
        assert_eq!(messages.len(), 1);
        assert!(messages[0].liveness.is_none());
    }

    #[test]
    fn dedup_claims_once_and_is_bounded() {
        let messages = parse_delivery(
            &state(),
            &json!({
                "object":"whatsapp_business_account",
                "entry":[{"id":"123","changes":[{"field":"messages","value":{
                    "messaging_product":"whatsapp","metadata":{"phone_number_id":"456"},
                    "contacts":[{"wa_id":"1603"}],
                    "messages":[{"id":"same","from":"1603","type":"text","text":{"body":"hello"}}]
                }}]}]
            }),
            &received(),
        )
        .expect("delivery");
        let mut dedup = ClaimedIds::new();
        assert_eq!(dedup.claim(messages.clone()).len(), 1);
        assert!(dedup.claim(messages).is_empty());
    }

    #[tokio::test]
    async fn loopback_listener_verifies_and_enqueues_before_acknowledging() {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
        let address = probe.local_addr().expect("address");
        drop(probe);
        let mut transport = WhatsappTransport::new(
            "wa".to_owned(),
            address,
            "/wa".to_owned(),
            "123".to_owned(),
            "456".to_owned(),
            "v23.0".to_owned(),
            "http://127.0.0.1:9".to_owned(),
            "secret".to_owned(),
            "verify".to_owned(),
            "access".to_owned(),
            LivenessSettings {
                mode: LivenessMode::Native,
                ..LivenessSettings::default()
            },
        )
        .expect("transport");
        transport.connect().await.expect("connect");
        let client = reqwest::Client::new();
        let challenge = client
            .get(format!(
                "http://{address}/wa?hub.mode=subscribe&hub.verify_token=verify&hub.challenge=exact"
            ))
            .send()
            .await
            .expect("GET");
        assert_eq!(challenge.status(), StatusCode::OK);
        assert_eq!(challenge.text().await.expect("challenge"), "exact");

        let body = serde_json::to_vec(&json!({
            "object":"whatsapp_business_account",
            "entry":[{"id":"123","changes":[{"field":"messages","value":{
                "messaging_product":"whatsapp","metadata":{"phone_number_id":"456"},
                "contacts":[{"wa_id":"1603"}],
                "messages":[{"id":"wamid.loopback","from":"1603","type":"text","text":{"body":"hello"}}]
            }}]}]
        })).expect("body");
        let response = client
            .post(format!("http://{address}/wa"))
            .header("x-hub-signature-256", signature(b"secret", &body))
            .body(body)
            .send()
            .await
            .expect("POST");
        assert_eq!(response.status(), StatusCode::OK);
        let event = tokio::time::timeout(Duration::from_secs(1), transport.next())
            .await
            .expect("queued promptly")
            .expect("event");
        let TransportEvent::Message(message) = event else {
            panic!("message event")
        };
        assert_eq!(message.message_id, "wamid.loopback");
    }

    #[tokio::test]
    async fn listener_failures_surface_and_reconnect_reuses_the_port() {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
        let address = probe.local_addr().expect("address");
        let mut blocked = WhatsappTransport::new(
            "wa".to_owned(),
            address,
            "/wa".to_owned(),
            "123".to_owned(),
            "456".to_owned(),
            "v23.0".to_owned(),
            "http://127.0.0.1:9".to_owned(),
            "secret".to_owned(),
            "verify".to_owned(),
            "access".to_owned(),
            LivenessSettings {
                mode: LivenessMode::Native,
                ..LivenessSettings::default()
            },
        )
        .expect("transport");
        assert!(blocked.connect().await.is_err());
        drop(probe);

        let mut running = WhatsappTransport::new(
            "wa".to_owned(),
            address,
            "/wa".to_owned(),
            "123".to_owned(),
            "456".to_owned(),
            "v23.0".to_owned(),
            "http://127.0.0.1:9".to_owned(),
            "secret".to_owned(),
            "verify".to_owned(),
            "access".to_owned(),
            LivenessSettings {
                mode: LivenessMode::Native,
                ..LivenessSettings::default()
            },
        )
        .expect("transport");
        running.connect().await.expect("listener starts");
        assert!(
            running.connect().await.is_err(),
            "a second accept loop is refused"
        );
        running.server.as_ref().expect("server handle").abort();
        let stopped = tokio::time::timeout(Duration::from_secs(1), running.next())
            .await
            .expect("listener completion is observed");
        assert!(matches!(stopped, Err(TransportError::Closed)));
        running.connect().await.expect("listener reconnects");

        drop(running);
        tokio::task::yield_now().await;
        std::net::TcpListener::bind(address).expect("drop aborts listener and releases port");
    }

    #[tokio::test]
    async fn graph_reply_is_one_exact_bounded_post() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).await.expect("read");
                assert!(read > 0, "complete request");
                request.extend_from_slice(&buffer[..read]);
                if let Some(split) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    let header_end = split + 4;
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .expect("content length");
                    if request.len() >= header_end + length {
                        break;
                    }
                }
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 66\r\nConnection: close\r\n\r\n{\"messaging_product\":\"whatsapp\",\"messages\":[{\"id\":\"wamid.reply\"}]}").await.expect("write");
            request
        });
        let driver = WhatsappDriver {
            transport: "wa".to_owned(),
            endpoint: format!("http://{address}"),
            version: "v23.0".to_owned(),
            phone_number_id: "456".to_owned(),
            access_token: Redacted::new("access-secret".to_owned()),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(2))
                .build()
                .expect("client"),
        };
        driver
            .reply(
                &ReplyTarget::WhatsApp {
                    recipient: "1603".to_owned(),
                },
                OutboundReply::text("hello"),
            )
            .await
            .expect("reply");
        let request = server.await.expect("server");
        let request = String::from_utf8(request).expect("utf8 request");
        assert!(request.starts_with("POST /v23.0/456/messages HTTP/1.1\r\n"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer access-secret\r\n")
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("content-type: application/json\r\n")
        );
        let body = request.split("\r\n\r\n").nth(1).expect("body");
        let body: Value = serde_json::from_str(body).expect("json body");
        assert_eq!(body["to"], "1603");
        assert_eq!(body["text"]["body"], "hello");
        assert_eq!(body["text"]["preview_url"], false);
    }

    /// A 200 that is not JSON is what an interposed proxy or a captive portal returns, and it is
    /// indistinguishable from a missing field unless the parse error survives the call.
    #[tokio::test]
    async fn a_graph_response_that_is_not_json_keeps_the_parse_error() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buffer = [0_u8; 4096];
            let read = stream.read(&mut buffer).await.expect("read");
            assert!(read > 0, "the request arrives");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 32\r\nConnection: close\r\n\r\n<html><body>Forbidden</body></ht",
                )
                .await
                .expect("write");
        });
        let driver = WhatsappDriver {
            transport: "wa".to_owned(),
            endpoint: format!("http://{address}"),
            version: "v23.0".to_owned(),
            phone_number_id: "456".to_owned(),
            access_token: Redacted::new("access-secret".to_owned()),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(2))
                .build()
                .expect("client"),
        };
        let error = driver
            .send_text("1603", "hello")
            .await
            .expect_err("HTML is not a Graph response");
        assert_eq!(error.category(), "malformed-response");
        let source = std::error::Error::source(&error).expect("the parse error is kept");
        let detail = source.to_string();
        assert!(detail.contains("line 1 column 1"), "{detail}");
        server.await.expect("server");
    }

    #[tokio::test]
    async fn outcome_unknown_timeout_is_not_retried() {
        use tokio::io::AsyncReadExt as _;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("one accept");
            let mut bytes = [0_u8; 1024];
            #[allow(
                clippy::let_underscore_must_use,
                reason = "this mock only has to hold the connection past the client's deadline; \
                          whether the request bytes arrived changes nothing about the timeout \
                          under test"
            )]
            let _ = stream.read(&mut bytes).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        let driver = WhatsappDriver {
            transport: "wa".to_owned(),
            endpoint: format!("http://{address}"),
            version: "v23.0".to_owned(),
            phone_number_id: "456".to_owned(),
            access_token: Redacted::new("access-secret".to_owned()),
            http: reqwest::Client::builder()
                .timeout(Duration::from_millis(50))
                .build()
                .expect("client"),
        };
        assert!(
            driver
                .reply(
                    &ReplyTarget::WhatsApp {
                        recipient: "1603".to_owned()
                    },
                    OutboundReply::text("hello"),
                )
                .await
                .is_err()
        );
        server.await.expect("one server task");
        // The mock accepted exactly one connection. `reply` has no retry loop, so an unknown
        // post-transmission outcome cannot create a second visible message.
    }

    /// The one WhatsApp counting unit, exercised through the shared splitter: Meta counts scalars,
    /// so 4,096 crabs are one message here where the same text is two on a UTF-16 ceiling.
    fn split(text: &str) -> Vec<String> {
        split_message(text, MAX_WHATSAPP_TEXT_CHARS, TextUnit::Scalar)
    }

    #[test]
    fn long_answers_split_by_unicode_scalars_without_losing_text() {
        assert_eq!(split(&"🦀".repeat(4096)).len(), 1);

        let answer = format!("BEGIN{}END", "🦀".repeat(4097));
        let chunks = split(&answer);
        assert!(
            chunks.len() > 1,
            "an answer past the ceiling is not one post"
        );
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.chars().count() <= MAX_WHATSAPP_TEXT_CHARS)
        );
        assert_eq!(
            chunks.concat(),
            answer,
            "every scalar the model wrote is still sent"
        );

        let lines = format!("{}\ntail", "x".repeat(MAX_WHATSAPP_TEXT_CHARS - 1));
        let broken = split(&lines);
        assert_eq!(broken.len(), 2);
        assert!(broken[0].ends_with('\n'), "a line boundary is preferred");
        assert_eq!(broken.concat(), lines);

        // One rule for every transport: Meta refuses an empty message body exactly as Slack and
        // Discord do, so an empty answer is a visible placeholder rather than a delivery failure.
        assert_eq!(split(""), vec!["[empty response]".to_owned()]);
    }

    #[tokio::test]
    async fn an_answer_past_the_service_ceiling_is_sent_as_more_than_one_message() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut request = Vec::new();
                let mut buffer = [0_u8; 8192];
                loop {
                    let read = stream.read(&mut buffer).await.expect("read");
                    assert!(read > 0, "complete request");
                    request.extend_from_slice(&buffer[..read]);
                    if let Some(split) = request.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        let header_end = split + 4;
                        let headers = String::from_utf8_lossy(&request[..header_end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .and_then(|value| value.parse::<usize>().ok())
                            })
                            .expect("content length");
                        if request.len() >= header_end + length {
                            let body: Value =
                                serde_json::from_slice(&request[header_end..]).expect("json body");
                            bodies
                                .push(body["text"]["body"].as_str().expect("text body").to_owned());
                            break;
                        }
                    }
                }
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 66\r\nConnection: close\r\n\r\n{\"messaging_product\":\"whatsapp\",\"messages\":[{\"id\":\"wamid.reply\"}]}").await.expect("write");
            }
            bodies
        });
        let driver = WhatsappDriver {
            transport: "wa".to_owned(),
            endpoint: format!("http://{address}"),
            version: "v23.0".to_owned(),
            phone_number_id: "456".to_owned(),
            access_token: Redacted::new("access-secret".to_owned()),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(5))
                .build()
                .expect("client"),
        };
        // The session's own outbound bound is twice the WhatsApp ceiling, so this length is
        // reachable from an ordinary answer rather than only from abuse.
        let answer = format!("BEGIN{}END", "x".repeat(MAX_WHATSAPP_TEXT_CHARS));
        driver
            .reply(
                &ReplyTarget::WhatsApp {
                    recipient: "1603".to_owned(),
                },
                OutboundReply::text(answer.clone()),
            )
            .await
            .expect("reply");
        let bodies = server.await.expect("server");
        assert_eq!(bodies.len(), 2, "one post per service-sized chunk");
        assert_eq!(bodies.concat(), answer, "no part of the answer is dropped");
    }

    #[test]
    fn every_refusal_reason_is_reported_once_per_window_with_its_count() {
        let mut log = RefusalLog::new();
        let start = Instant::now();
        assert_eq!(log.admit(Refusal::Signature, start), Some(0));
        assert_eq!(log.admit(Refusal::Signature, start), None);
        assert_eq!(log.admit(Refusal::Signature, start), None);
        // A different reason is never hidden by a noisy one.
        assert_eq!(log.admit(Refusal::Saturated, start), Some(0));
        assert_eq!(
            log.admit(Refusal::Signature, start + REFUSAL_LOG_WINDOW),
            Some(2),
            "the emission stands for the ones it replaced"
        );
        assert_eq!(
            log.admit(Refusal::Signature, start + REFUSAL_LOG_WINDOW * 2),
            Some(0),
            "the count resets with each emission"
        );

        let mut reasons: Vec<&str> = [
            Refusal::Unsigned,
            Refusal::Signature,
            Refusal::Oversize,
            Refusal::Malformed,
            Refusal::Saturated,
            Refusal::Timeout,
            Refusal::Verification,
            Refusal::Unavailable,
        ]
        .iter()
        .map(|refusal| refusal.reason())
        .collect();
        assert_eq!(reasons.len(), Refusal::COUNT);
        reasons.sort_unstable();
        reasons.dedup();
        assert_eq!(
            reasons.len(),
            Refusal::COUNT,
            "each reason has its own slot and its own name"
        );
    }

    #[test]
    fn only_an_unusable_listening_socket_ends_the_accept_loop() {
        for kind in [
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::Interrupted,
            io::ErrorKind::TimedOut,
            io::ErrorKind::WouldBlock,
        ] {
            assert_eq!(
                classify_accept(&io::Error::from(kind)),
                AcceptFailure::Connection,
                "{kind:?} is one dead connection, not a dead listener"
            );
        }
        for code in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
            assert_eq!(
                classify_accept(&io::Error::from_raw_os_error(code)),
                AcceptFailure::Exhausted,
                "exhaustion recovers once something is freed"
            );
        }
        assert_eq!(
            classify_accept(&io::Error::from_raw_os_error(libc::EBADF)),
            AcceptFailure::Fatal
        );
        assert_eq!(
            classify_accept(&io::Error::from(io::ErrorKind::InvalidInput)),
            AcceptFailure::Fatal
        );
    }

    /// A loopback Graph endpoint that answers `count` requests and hands back what it was sent.
    ///
    /// Hand-rolled like the reply mocks above: what is under test is the exact request Meta
    /// receives, and a real socket carrying real bytes is the only thing that proves one.
    fn graph_mock(
        count: usize,
        status: u16,
        response: &'static str,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let address = listener.local_addr().expect("address");
        listener.set_nonblocking(true).expect("mock is pollable");
        let listener = tokio::net::TcpListener::from_std(listener).expect("mock adopts");
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..count {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let read = stream.read(&mut buffer).await.expect("read");
                    assert!(read > 0, "complete request");
                    request.extend_from_slice(&buffer[..read]);
                    if let Some(split) = request.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        let header_end = split + 4;
                        let headers = String::from_utf8_lossy(&request[..header_end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .and_then(|value| value.parse::<usize>().ok())
                            })
                            .expect("content length");
                        if request.len() >= header_end + length {
                            break;
                        }
                    }
                }
                let reason = if status == 200 { "OK" } else { "Bad Request" };
                let head = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                );
                stream.write_all(head.as_bytes()).await.expect("write head");
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write body");
                requests.push(String::from_utf8(request).expect("utf8 request"));
            }
            requests
        });
        (format!("http://{address}"), server)
    }

    fn driver(endpoint: String) -> WhatsappDriver {
        WhatsappDriver {
            transport: "wa".to_owned(),
            endpoint,
            version: "v23.0".to_owned(),
            phone_number_id: "456".to_owned(),
            access_token: Redacted::new("access-secret".to_owned()),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(5))
                .build()
                .expect("client"),
        }
    }

    fn typing_target() -> LivenessTarget {
        LivenessTarget::WhatsApp {
            recipient: "1603".to_owned(),
            inbound_message_id: "wamid.inbound".to_owned(),
        }
    }

    /// The body of one request, as the Graph endpoint parsed it.
    fn body(request: &str) -> Value {
        serde_json::from_str(request.split("\r\n\r\n").nth(1).expect("body")).expect("json body")
    }

    /// Meta has no typing endpoint: the indicator rides the read receipt, so one call does both.
    #[tokio::test]
    async fn typing_is_the_read_receipt_and_the_indicator_in_one_call() {
        let (endpoint, server) = graph_mock(1, 200, r#"{"success":true}"#);
        let driver = driver(endpoint);
        assert_eq!(driver.renew_every(), Duration::from_secs(20));
        driver
            .renew(&typing_target())
            .await
            .expect("the typing indicator is shown");
        let requests = server.await.expect("server");
        assert!(requests[0].starts_with("POST /v23.0/456/messages HTTP/1.1\r\n"));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("authorization: bearer access-secret\r\n")
        );
        let sent = body(&requests[0]);
        assert_eq!(sent["messaging_product"], "whatsapp");
        assert_eq!(sent["status"], "read");
        assert_eq!(
            sent["message_id"], "wamid.inbound",
            "the read receipt is what carries the identifier, so the indicator rides it"
        );
        assert_eq!(sent["typing_indicator"], json!({ "type": "text" }));
    }

    /// Meta documents no renewal, so what this pins is the request rather than the outcome: every
    /// 20 seconds the same call goes out again, and whether it restarts Meta's 25-second dismissal
    /// is the one thing only a live WhatsApp Business number can answer.
    #[tokio::test]
    async fn a_running_session_re_posts_the_same_indicator_request() {
        let (endpoint, server) = graph_mock(2, 200, r#"{"success":true}"#);
        let driver = driver(endpoint);
        for _ in 0..2 {
            driver
                .renew(&typing_target())
                .await
                .expect("the indicator is re-posted");
        }
        let requests = server.await.expect("server");
        assert_eq!(requests.len(), 2);
        assert_eq!(
            body(&requests[0]),
            body(&requests[1]),
            "renewal is the identical call, not a different one"
        );
    }

    /// A message identifier Meta will not accept is the likely live failure, and it arrives as a
    /// 400 rather than as a body to interpret.
    #[tokio::test]
    async fn a_refused_indicator_surfaces_the_graph_status() {
        let (endpoint, server) = graph_mock(
            1,
            400,
            r#"{"error":{"message":"Unsupported post request","code":100}}"#,
        );
        let driver = driver(endpoint);
        let error = driver
            .renew(&typing_target())
            .await
            .expect_err("a rejected indicator is not a shown one");
        assert!(
            matches!(&error, TransportError::Service { code } if code == "http-400"),
            "{error:?}"
        );
        server.await.expect("server");
    }

    /// A 200 whose body does not say `success` is the Graph API disagreeing with itself; the
    /// indicator was not shown, so the call did not succeed.
    #[tokio::test]
    async fn an_indicator_is_not_shown_unless_graph_says_so() {
        let (endpoint, server) = graph_mock(1, 200, r#"{"messaging_product":"whatsapp"}"#);
        let driver = driver(endpoint);
        let error = driver
            .renew(&typing_target())
            .await
            .expect_err("a 200 without success is not a shown indicator");
        assert!(matches!(&error, TransportError::Response), "{error:?}");
        server.await.expect("server");
    }

    /// `Some` means implemented, so the three surfaces WhatsApp lacks have to answer `None`.
    #[tokio::test]
    async fn whatsapp_offers_typing_and_nothing_else() {
        let driver = driver("http://127.0.0.1:1".to_owned());
        assert!(driver.typing().is_some());
        assert!(
            driver.progress().is_none(),
            "the Cloud API cannot edit a message it sent, so a route that resolved \
             `liveness.progress: message` — which configuration accepts here — gets nothing"
        );
        assert!(
            driver.stream().is_none(),
            "a stream is an edited message, which is the same missing endpoint"
        );
        assert!(
            driver.cancel_button().is_none(),
            "an interactive button needs a template outside the service window"
        );
        assert!(driver.status().is_none());
        assert!(driver.reaction().is_none());
    }
}
