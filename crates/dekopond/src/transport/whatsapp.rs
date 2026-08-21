//! Meta WhatsApp Cloud API webhook and text reply transport.
//!
//! TLS terminates outside this daemon. The listener verifies Meta's signature over the exact raw
//! body before parsing, claims message IDs in a bounded process-local set, enqueues a whole delivery
//! atomically, and acknowledges before any session or model work begins.

use std::{
    collections::{HashSet, VecDeque},
    io,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::to_bytes,
    extract::{RawQuery, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use dekopon_broker_protocol::ChatTransportKind;
use dekopon_core::{ExternalSubject, Redacted};
use futures_util::{StreamExt as _, future::BoxFuture};
use hmac::{Hmac, KeyInit as _, Mac as _};
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use crate::transport::{
    ChatReplier, ChatTransport, ConversationKind, DeliveryReceipt, InboundMessage, OutboundReply,
    ReplyTarget, TransportError, TransportEvent, TransportIdentity, bound_inbound,
};

const MAX_WEBHOOK_BODY_BYTES: usize = 256 * 1024;
const MAX_WEBHOOK_HEADERS: usize = 32;
const MAX_HEADER_BYTES: usize = 8 * 1024;
/// Hyper's parser buffer also includes the request line; keep it finite above the field ceilings.
const MAX_CONNECTION_BUFFER_BYTES: usize = 16 * 1024;
const MAX_QUERY_BYTES: usize = 2 * 1024;
const MAX_QUERY_VALUE_BYTES: usize = 512;
const MAX_MESSAGES_PER_DELIVERY: usize = 128;
const MAX_DEDUP_IDS: usize = 4096;
const MAX_QUEUED_MESSAGES: usize = 512;
const WEBHOOK_QUEUE: usize = 64;
const MAX_WEBHOOK_CONCURRENCY: usize = 16;
const WEBHOOK_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const GRAPH_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_GRAPH_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_WHATSAPP_TEXT_CHARS: usize = 4096;
/// One refusal reason is reported at most this often, with the count it stands for.
const REFUSAL_LOG_WINDOW: Duration = Duration::from_secs(60);
/// How long the accept loop waits after running out of descriptors or socket buffers.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

pub(crate) struct WhatsappTransport {
    name: String,
    bind: std::net::SocketAddr,
    callback_path: String,
    state: WebhookState,
    receiver: mpsc::Receiver<QueuedDelivery>,
    pending: VecDeque<QueuedDelivery>,
    replier: Arc<WhatsappReplier>,
    server: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Clone)]
struct WebhookState {
    name: String,
    app_secret: Arc<Redacted<Vec<u8>>>,
    verify_token: Arc<Redacted<String>>,
    waba_id: String,
    phone_number_id: String,
    sender: mpsc::Sender<QueuedDelivery>,
    dedup: Arc<Mutex<Dedup>>,
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

struct Dedup {
    ids: HashSet<String>,
    order: VecDeque<String>,
}

impl Dedup {
    fn new() -> Self {
        Self {
            ids: HashSet::with_capacity(MAX_DEDUP_IDS),
            order: VecDeque::with_capacity(MAX_DEDUP_IDS),
        }
    }

    fn claim(&mut self, messages: Vec<InboundMessage>) -> Vec<InboundMessage> {
        let mut accepted = Vec::with_capacity(messages.len());
        for message in messages {
            if self.ids.insert(message.message_id.clone()) {
                self.order.push_back(message.message_id.clone());
                while self.order.len() > MAX_DEDUP_IDS {
                    if let Some(oldest) = self.order.pop_front() {
                        self.ids.remove(&oldest);
                    }
                }
                accepted.push(message);
            }
        }
        accepted
    }

    /// Undoes a claim that never reached the queue, so Meta's redelivery is accepted rather than
    /// silently acknowledged as a duplicate of work nothing is doing.
    fn release(&mut self, claimed: &[String]) {
        for id in claimed {
            self.ids.remove(id);
        }
        self.order.retain(|id| self.ids.contains(id));
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
    ) -> Result<Self, TransportError> {
        let (sender, receiver) = mpsc::channel(WEBHOOK_QUEUE);
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(GRAPH_REQUEST_TIMEOUT)
            .build()
            .map_err(|source| TransportError::Request(Box::new(source)))?;
        let replier = Arc::new(WhatsappReplier {
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
                sender,
                dedup: Arc::new(Mutex::new(Dedup::new())),
                refusals: Arc::new(Mutex::new(RefusalLog::new())),
                queue_capacity: Arc::new(Semaphore::new(MAX_QUEUED_MESSAGES)),
                concurrency: Arc::new(Semaphore::new(MAX_WEBHOOK_CONCURRENCY)),
            },
            receiver,
            pending: VecDeque::new(),
            replier,
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

    fn replier(&self) -> Arc<dyn ChatReplier> {
        Arc::clone(&self.replier) as Arc<dyn ChatReplier>
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
    match tokio::time::timeout(
        WEBHOOK_REQUEST_TIMEOUT,
        process_webhook(&state, request, permit),
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
    let messages = match parse_delivery(state, &value) {
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

fn parse_delivery(state: &WebhookState, root: &Value) -> Result<Vec<InboundMessage>, ()> {
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
            for message in messages {
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
                let conversation =
                    format!("{}:{}:{}", state.waba_id, state.phone_number_id, sender);
                accepted.push(InboundMessage {
                    transport: state.name.clone(),
                    transport_kind: ChatTransportKind::Whatsapp,
                    subject,
                    channel: conversation.clone(),
                    thread: None,
                    conversation_id: conversation,
                    message_id: id.to_owned(),
                    text: bound_inbound(text),
                    assets: Vec::new(),
                    conversation: ConversationKind::DirectMessage,
                    addressed: None,
                    reply: ReplyTarget::WhatsApp {
                        recipient: sender.to_owned(),
                    },
                    activity: None,
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
fn hmac_sha256(key: &[u8], body: &[u8]) -> [u8; 32] {
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

fn parse_query(query: &str) -> Result<Vec<(String, String)>, ()> {
    let mut fields = Vec::new();
    for part in query.split('&') {
        let (name, value) = part.split_once('=').ok_or(())?;
        let name = percent_decode(name)?;
        let value = percent_decode(value)?;
        if name.len() > MAX_QUERY_VALUE_BYTES || value.len() > MAX_QUERY_VALUE_BYTES {
            return Err(());
        }
        fields.push((name, value));
    }
    Ok(fields)
}

fn percent_decode(value: &str) -> Result<String, ()> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => decoded.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                decoded.push(
                    (percent_hex_nibble(bytes[index + 1]).ok_or(())? << 4)
                        | percent_hex_nibble(bytes[index + 2]).ok_or(())?,
                );
                index += 2;
            }
            b'%' => return Err(()),
            byte => decoded.push(byte),
        }
        index += 1;
    }
    String::from_utf8(decoded).map_err(|_| ())
}

fn percent_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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

struct WhatsappReplier {
    transport: String,
    endpoint: String,
    version: String,
    phone_number_id: String,
    access_token: Redacted<String>,
    http: reqwest::Client,
}

impl WhatsappReplier {
    /// One text message, sent exactly once and never retried.
    async fn send_text(&self, recipient: &str, body: &str) -> Result<String, TransportError> {
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
            .post(format!(
                "{}/{}/{}/messages",
                self.endpoint, self.version, self.phone_number_id
            ))
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
                code: status.as_u16().to_string(),
            });
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| TransportError::Response)?;
        if value.get("messaging_product").and_then(Value::as_str) != Some("whatsapp") {
            return Err(TransportError::Response);
        }
        value
            .pointer("/messages/0/id")
            .and_then(Value::as_str)
            .filter(|id| canonical_whatsapp_message_id(id))
            .map(str::to_owned)
            .ok_or(TransportError::Response)
    }
}

impl ChatReplier for WhatsappReplier {
    fn reply(
        &self,
        target: ReplyTarget,
        reply: OutboundReply,
    ) -> BoxFuture<'_, Result<DeliveryReceipt, TransportError>> {
        Box::pin(async move {
            let ReplyTarget::WhatsApp { recipient } = target else {
                return Err(TransportError::Response);
            };
            let OutboundReply { text, image } = reply;
            if image.is_some() {
                // Configuration refuses an image generator on a WhatsApp route, so reaching here
                // means the two disagree. Say which one rather than dropping bytes silently:
                // sending an image needs Meta's media upload, which this transport does not have.
                tracing::error!(
                    event = "gateway_whatsapp_image_unsupported",
                    transport = %self.transport,
                );
                return Err(TransportError::Response);
            }
            let mut accepted = 0_usize;
            let mut last_id = None;
            for chunk in split_message(&text) {
                match self.send_text(&recipient, &chunk).await {
                    Ok(id) => {
                        accepted += 1;
                        last_id = Some(id);
                    }
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
            Ok(DeliveryReceipt::new(
                last_id.ok_or(TransportError::Response)?,
            ))
        })
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

/// Splits an answer into service-sized messages, preferring a line boundary.
///
/// Not truncation: the session's own outbound bound is 8 KiB, which is twice what one WhatsApp text
/// message may carry, so an answer longer than the service ceiling is the ordinary case rather than
/// an abusive one, and dropping its second half would lose the conclusion. Unicode scalar values
/// are what Meta counts.
fn split_message(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        // The byte offset of the scalar just past this chunk's ceiling, or the end of the text.
        let mut end = text[start..]
            .char_indices()
            .nth(MAX_WHATSAPP_TEXT_CHARS)
            .map_or(text.len(), |(index, _)| start + index);
        if end < text.len()
            && let Some(newline) = text[start..end].rfind('\n')
            && newline > 0
        {
            end = start + newline + 1;
        }
        chunks.push(text[start..end].to_owned());
        start = end;
    }
    chunks
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
                sender,
                dedup: Arc::new(Mutex::new(Dedup::new())),
                refusals: Arc::new(Mutex::new(RefusalLog::new())),
                queue_capacity: Arc::new(Semaphore::new(MAX_QUEUED_MESSAGES)),
                concurrency: Arc::new(Semaphore::new(1)),
            },
            receiver,
        )
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
        assert!(parse_query("hub.mode=%zz").is_err());
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
        let messages = parse_delivery(&state(), &payload).expect("delivery");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message_id, "two");
        assert_eq!(messages[0].subject.canonical(), "whatsapp.1603");
        assert_eq!(messages[0].channel, "123:456:1603");
    }

    #[test]
    fn dedup_claims_once_and_is_bounded() {
        let messages = parse_delivery(
            &state(),
            &json!({
                "object":"whatsapp_business_account",
                "entry":[{"id":"123","changes":[{"field":"messages","value":{
                    "messaging_product":"whatsapp","metadata":{"phone_number_id":"456"},
                    "messages":[{"id":"same","from":"1603","type":"text","text":{"body":"hello"}}]
                }}]}]
            }),
        )
        .expect("delivery");
        let mut dedup = Dedup::new();
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
        let replier = WhatsappReplier {
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
        let receipt = replier
            .reply(
                ReplyTarget::WhatsApp {
                    recipient: "1603".to_owned(),
                },
                OutboundReply::text("hello"),
            )
            .await
            .expect("reply");
        assert!(receipt.accepted());
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
            let _ = stream.read(&mut bytes).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        let replier = WhatsappReplier {
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
            replier
                .reply(
                    ReplyTarget::WhatsApp {
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

    #[test]
    fn long_answers_split_by_unicode_scalars_without_losing_text() {
        assert_eq!(split_message(&"🦀".repeat(4096)).len(), 1);

        let answer = format!("BEGIN{}END", "🦀".repeat(4097));
        let chunks = split_message(&answer);
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
        let split = split_message(&lines);
        assert_eq!(split.len(), 2);
        assert!(split[0].ends_with('\n'), "a line boundary is preferred");
        assert_eq!(split.concat(), lines);

        assert_eq!(split_message(""), vec![String::new()]);
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
        let replier = WhatsappReplier {
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
        let receipt = replier
            .reply(
                ReplyTarget::WhatsApp {
                    recipient: "1603".to_owned(),
                },
                OutboundReply::text(answer.clone()),
            )
            .await
            .expect("reply");
        assert!(receipt.accepted());
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
}
