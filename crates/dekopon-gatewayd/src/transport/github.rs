use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use axum::{
    Router,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::post,
};
use dekopon_broker_protocol::{ChatTransportKind, Conversation, ConversationKind};
use dekopon_core::{ExternalSubject, Redacted};
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tracing::Instrument as _;

use super::{
    ChatDriver, ChatTransport, InboundMessage, MessageId, OutboundReply, ReplyTarget, SeenIds,
    TransportError, TransportEvent, TransportIdentity, receive_span, record_conversation,
    webhook::{self, VerifyFailure},
    whatsapp::WhatsappTransport,
};
use crate::{config::GithubEvent, gateway::CurrentGeneration};
use dekopon_core::github::{GithubId, GithubThread, thread};
use std::collections::VecDeque;

const CALLBACK: &str = "/webhooks/github";
const RECENT_DELIVERIES: usize = 1024;
const QUEUE: usize = 64;
const REFUSAL_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GithubEntry {
    pub delivery: String,
    pub event: GithubEvent,
    pub action: Option<String>,
    pub actor: GithubId,
    pub thread: GithubThread,
    pub head_sha: Option<String>,
    pub run: Option<GithubId>,
    pub conclusion: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GithubBatch(VecDeque<GithubEntry>);

impl GithubBatch {
    pub(crate) fn new(entry: GithubEntry) -> Self {
        Self(VecDeque::from([entry]))
    }
    pub(crate) fn lead(&self) -> &GithubEntry {
        &self.0[0]
    }
    pub(crate) fn deliveries(&self) -> Vec<&str> {
        self.0.iter().map(|e| e.delivery.as_str()).collect()
    }
    pub(crate) fn append(&mut self, other: &mut Self) -> bool {
        if self.0.len() + other.0.len() > 16 || self.lead().actor != other.lead().actor {
            return false;
        }
        self.0.append(&mut other.0);
        true
    }
    pub(crate) fn note(&self, subject: &ExternalSubject) -> String {
        let mut lines = Vec::with_capacity(self.0.len() + 1);
        for entry in &self.0 {
            let event = serde_json::to_value(entry.event)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            let mut line = format!(
                "[gateway: unverified GitHub {event} claim] repository {}/{}; action {}; {}; delivery {}.",
                subject.tenant().unwrap_or_default(),
                subject.subject(),
                entry.action.as_deref().unwrap_or("none"),
                entry.thread,
                entry.delivery
            );
            if let Some(sha) = &entry.head_sha {
                line.push_str(&format!(" head {sha};"));
            }
            if let Some(run) = entry.run {
                line.push_str(&format!(" run {run}; associated PR {};", entry.thread));
            }
            if let Some(conclusion) = &entry.conclusion {
                line.push_str(&format!(" conclusion {conclusion};"));
            }
            lines.push(line);
        }
        lines.push("Read current checks and head through a broker-granted capability before acting. No associated PR means report only.".to_owned());
        lines.join("\n")
    }
}

pub(super) enum Reservation {
    New,
    Reserved,
    Committed,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Acked {
    Admitted,
    Queued,
    Ignored,
    Busy,
}

pub(crate) struct AdmissionTicket {
    delivery: String,
    accepted: Arc<Mutex<SeenIds>>,
    reply: Option<oneshot::Sender<Acked>>,
}

impl std::fmt::Debug for AdmissionTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmissionTicket")
            .field("delivery", &self.delivery)
            .finish_non_exhaustive()
    }
}

impl AdmissionTicket {
    pub(crate) fn answer(mut self, answer: Acked) {
        let commit = match answer {
            Acked::Admitted | Acked::Queued | Acked::Ignored => true,
            Acked::Busy => false,
        };
        self.accepted.lock().answer(&self.delivery, commit);
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(answer);
        }
    }
}

impl Drop for AdmissionTicket {
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() {
            self.accepted.lock().answer(&self.delivery, false);
            let _ = reply.send(Acked::Busy);
        }
    }
}

#[derive(Clone, Copy)]
enum Refusal {
    Headers,
    Unsigned,
    Signature,
    Oversize,
    Malformed,
    Saturated,
    Timeout,
}
impl Refusal {
    const fn index(self) -> usize {
        match self {
            Self::Headers => 0,
            Self::Unsigned => 1,
            Self::Signature => 2,
            Self::Oversize => 3,
            Self::Malformed => 4,
            Self::Saturated => 5,
            Self::Timeout => 6,
        }
    }
    const fn reason(self) -> &'static str {
        match self {
            Self::Headers => "headers",
            Self::Unsigned => "unsigned",
            Self::Signature => "signature",
            Self::Oversize => "oversize",
            Self::Malformed => "malformed",
            Self::Saturated => "saturated",
            Self::Timeout => "timeout",
        }
    }
}

struct RefusalLog {
    windows: [Option<Instant>; 7],
    suppressed: [u64; 7],
}
impl RefusalLog {
    const fn new() -> Self {
        Self {
            windows: [None; 7],
            suppressed: [0; 7],
        }
    }
    fn admit(&mut self, reason: Refusal) -> Option<u64> {
        let index = reason.index();
        let now = Instant::now();
        if self.windows[index].is_some_and(|last| now.duration_since(last) < REFUSAL_WINDOW) {
            self.suppressed[index] = self.suppressed[index].saturating_add(1);
            return None;
        }
        self.windows[index] = Some(now);
        Some(std::mem::take(&mut self.suppressed[index]))
    }
}

#[derive(Clone)]
struct StateData {
    name: String,
    secret: Arc<Redacted<Vec<u8>>>,
    generation: CurrentGeneration,
    sender: mpsc::Sender<TransportEvent>,
    // Serializes the acceptance decision with the only in-process dedupe owner.
    accepted: Arc<Mutex<SeenIds>>,
    refusals: Arc<Mutex<RefusalLog>>,
    concurrent: Arc<Semaphore>,
}

fn refuse(state: &StateData, reason: Refusal, status: StatusCode) -> Response {
    if let Some(suppressed) = state.refusals.lock().admit(reason) {
        tracing::warn!(event = "gateway_github_webhook_refused", transport = %state.name,
            reason = reason.reason(), status = status.as_u16(), suppressed, "gateway.github.webhook.refused");
    }
    webhook::content_free(status)
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn guid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'-' | b'_' | b':')
        })
}

async fn receive(State(state): State<StateData>, request: Request) -> Response {
    let Ok(_permit) = Arc::clone(&state.concurrent).try_acquire_owned() else {
        return refuse(&state, Refusal::Saturated, StatusCode::SERVICE_UNAVAILABLE);
    };
    let received = receive_span(ChatTransportKind::Github);
    match tokio::time::timeout(
        webhook::WEBHOOK_REQUEST_TIMEOUT,
        process(&state, request, &received).instrument(received.clone()),
    )
    .await
    {
        Ok(response) => response,
        Err(_) => refuse(&state, Refusal::Timeout, StatusCode::SERVICE_UNAVAILABLE),
    }
}

async fn process(state: &StateData, request: Request, received: &tracing::Span) -> Response {
    let headers = request.headers();
    if !webhook::headers_bounded(headers) {
        return refuse(state, Refusal::Headers, StatusCode::BAD_REQUEST);
    }
    let delivery = header(headers, "x-github-delivery").filter(|value| guid(value));
    let event = header(headers, "x-github-event")
        .filter(|value| !value.is_empty() && value.len() <= 64 && value.is_ascii());
    let (Some(delivery), Some(event)) = (delivery, event) else {
        return refuse(state, Refusal::Headers, StatusCode::BAD_REQUEST);
    };
    let delivery = delivery.to_owned();
    let event = event.to_owned();
    let body = match webhook::verified_body(request, state.secret.expose()).await {
        Ok(body) => body,
        Err(VerifyFailure::Headers) => {
            return refuse(state, Refusal::Headers, StatusCode::BAD_REQUEST);
        }
        Err(VerifyFailure::Unsigned) => {
            return refuse(state, Refusal::Unsigned, StatusCode::UNAUTHORIZED);
        }
        Err(VerifyFailure::Oversize) => {
            return refuse(state, Refusal::Oversize, StatusCode::PAYLOAD_TOO_LARGE);
        }
        Err(VerifyFailure::Signature) => {
            return refuse(state, Refusal::Signature, StatusCode::UNAUTHORIZED);
        }
    };
    let Ok(root) = serde_json::from_slice::<Value>(&body) else {
        return refuse(state, Refusal::Malformed, StatusCode::BAD_REQUEST);
    };
    let event_kind = match serde_json::from_value::<GithubEvent>(Value::String(event.clone())) {
        Ok(event) => event,
        Err(_) => {
            let reason = if matches!(
                event.as_str(),
                "ping"
                    | "installation"
                    | "installation_repositories"
                    | "installation_target"
                    | "github_app_authorization"
                    | "marketplace_purchase"
                    | "organization"
                    | "org_block"
                    | "membership"
                    | "team"
                    | "sponsorship"
                    | "personal_access_token_request"
                    | "security_advisory"
                    | "custom_property"
                    | "projects_v2"
                    | "projects_v2_item"
                    | "projects_v2_status_update"
            ) {
                "no-repository"
            } else {
                "unknown-event"
            };
            ignored(state, &delivery, reason);
            return webhook::content_free(StatusCode::OK);
        }
    };
    if !root.get("repository").is_some_and(Value::is_object) {
        ignored(state, &delivery, "no-repository");
        return webhook::content_free(StatusCode::OK);
    }
    let Some((subject, conversation, entry)) = parse(&root, event_kind, &delivery) else {
        return refuse(state, Refusal::Malformed, StatusCode::BAD_REQUEST);
    };
    if !state
        .generation
        .admits_github(&state.name, &conversation, &subject, event_kind)
    {
        ignored(state, &delivery, "not-listed");
        return webhook::content_free(StatusCode::OK);
    }
    if !eligible(&entry) {
        ignored(state, &delivery, "action-filtered");
        return webhook::content_free(StatusCode::OK);
    }
    let receipt = received.in_scope(|| receive_span(ChatTransportKind::Github));
    let message = InboundMessage {
        transport: state.name.clone(),
        transport_kind: ChatTransportKind::Github,
        subject,
        conversation,
        message_id: MessageId::Github(Box::new(GithubBatch::new(entry))),
        text: String::new(),
        assets: Vec::new(),
        asset_overflow: false,
        addressed: Some(true),
        thread_continuation: None,
        reply: ReplyTarget::GithubTrace,
        liveness: None,
        receive_span: receipt.clone(),
        received_at: tokio::time::Instant::now(),
        native_group: None,
        constituents: Vec::new(),
        folded: Vec::new(),
    };
    let receiver = {
        let mut accepted = state.accepted.lock();
        match accepted.reserve(&delivery) {
            Reservation::Committed => return webhook::content_free(StatusCode::OK),
            Reservation::Reserved => {
                return refuse(state, Refusal::Saturated, StatusCode::SERVICE_UNAVAILABLE);
            }
            Reservation::New => {}
        }
        let Ok(permit) = state.sender.try_reserve() else {
            accepted.answer(&delivery, false);
            return refuse(state, Refusal::Saturated, StatusCode::SERVICE_UNAVAILABLE);
        };
        let (reply, receiver) = oneshot::channel();
        let ticket = AdmissionTicket {
            delivery: delivery.clone(),
            accepted: Arc::clone(&state.accepted),
            reply: Some(reply),
        };
        receipt.record("message.id", &delivery);
        record_conversation(&receipt, &message.conversation);
        permit.send(TransportEvent::Delivery(Box::new(message), ticket));
        receiver
    };
    match tokio::time::timeout(webhook::WEBHOOK_REQUEST_TIMEOUT, receiver).await {
        Ok(Ok(Acked::Admitted | Acked::Queued | Acked::Ignored)) => {
            webhook::content_free(StatusCode::OK)
        }
        Ok(Ok(Acked::Busy) | Err(_)) => {
            refuse(state, Refusal::Saturated, StatusCode::SERVICE_UNAVAILABLE)
        }
        Err(_) => refuse(state, Refusal::Timeout, StatusCode::SERVICE_UNAVAILABLE),
    }
}

fn ignored(state: &StateData, delivery: &str, reason: &str) {
    tracing::debug!(
        event = "gateway_github_ignored",
        reason,
        "gateway.github.ignored"
    );
    tracing::info!(target: "dekopon_gatewayd::audit", { audit.event = "gateway.admission",
        outcome = "ignored", transport = %state.name, github.deliveries = &[delivery],
        queue.depth = 0 }, "gateway.admission");
}

fn field<'a>(root: &'a Value, path: &str, max: usize) -> Option<&'a str> {
    root.pointer(path)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= max)
}

fn parse(
    root: &Value,
    event: GithubEvent,
    delivery: &str,
) -> Option<(ExternalSubject, Conversation, GithubEntry)> {
    let owner = field(root, "/repository/owner/login", 39)?;
    let repo = field(root, "/repository/name", 100)?;
    let subject = ExternalSubject::github(owner, repo).ok()?;
    let thread = thread(event, root).ok()?;
    let conversation = Conversation {
        kind: ConversationKind::Thread,
        container: Some(subject.tenant()?.to_owned()),
        id: format!("{}.{}", subject.tenant()?, subject.subject()),
        thread: Some(thread.to_string()),
    };
    let actor = GithubId::new(root.pointer("/sender/id")?.as_u64()?).ok()?;
    let action = field(root, "/action", 64)
        .filter(|v| {
            v.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
        })
        .map(str::to_owned);
    let (head_sha, run, conclusion) = match event {
        GithubEvent::PullRequest => (
            Some(field(root, "/pull_request/head/sha", 128)?.to_owned()),
            None,
            None,
        ),
        GithubEvent::WorkflowRun => (
            Some(field(root, "/workflow_run/head_sha", 128)?.to_owned()),
            Some(GithubId::new(root.pointer("/workflow_run/id")?.as_u64()?).ok()?),
            field(root, "/workflow_run/conclusion", 64).map(str::to_owned),
        ),
        _ => (None, None, None),
    };
    Some((
        subject,
        conversation,
        GithubEntry {
            delivery: delivery.to_owned(),
            event,
            action,
            actor,
            thread,
            head_sha,
            run,
            conclusion,
        },
    ))
}

fn eligible(entry: &GithubEntry) -> bool {
    match entry.event {
        GithubEvent::PullRequest => matches!(
            entry.action.as_deref(),
            Some("opened" | "reopened" | "synchronize")
        ),
        GithubEvent::WorkflowRun => {
            entry.action.as_deref() == Some("completed")
                && matches!(entry.conclusion.as_deref(), Some("success" | "failure"))
        }
        _ => true,
    }
}

pub(crate) struct GithubTransport {
    name: String,
    bind: std::net::SocketAddr,
    state: StateData,
    receiver: mpsc::Receiver<TransportEvent>,
    whatsapp: Option<WhatsappTransport>,
    pending_sibling_connect: bool,
    server: tokio::task::JoinSet<()>,
}

impl GithubTransport {
    pub(crate) fn new(
        name: String,
        bind: std::net::SocketAddr,
        secret: String,
        generation: CurrentGeneration,
        whatsapp: Option<WhatsappTransport>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(QUEUE);
        let pending_sibling_connect = whatsapp.is_some();
        Self {
            name: name.clone(),
            bind,
            state: StateData {
                name,
                secret: Arc::new(Redacted::new(secret.into_bytes())),
                generation,
                sender,
                accepted: Arc::new(Mutex::new(SeenIds::new(RECENT_DELIVERIES))),
                refusals: Arc::new(Mutex::new(RefusalLog::new())),
                concurrent: Arc::new(Semaphore::new(webhook::MAX_WEBHOOK_CONCURRENCY)),
            },
            receiver,
            whatsapp,
            pending_sibling_connect,
            server: tokio::task::JoinSet::new(),
        }
    }
}

struct GithubDriver;
#[async_trait]
impl ChatDriver for GithubDriver {
    async fn reply(
        &self,
        _target: &ReplyTarget,
        _reply: OutboundReply,
    ) -> Result<(), TransportError> {
        Ok(())
    }
}

impl ChatTransport for GithubTransport {
    fn name(&self) -> &str {
        &self.name
    }
    fn connect(&mut self) -> BoxFuture<'_, Result<TransportIdentity, TransportError>> {
        Box::pin(async move {
            let mut router = Router::new()
                .route(CALLBACK, post(receive))
                .with_state(self.state.clone());
            if let Some(whatsapp) = &self.whatsapp {
                router = router.merge(whatsapp.router());
            }
            router = router
                .fallback(|| async { webhook::content_free(StatusCode::NOT_FOUND) })
                .method_not_allowed_fallback(|| async {
                    webhook::content_free(StatusCode::METHOD_NOT_ALLOWED)
                });
            webhook::listen(self.bind, router, self.name.clone(), &mut self.server).await?;
            self.pending_sibling_connect = self.whatsapp.is_some();
            Ok(TransportIdentity::default())
        })
    }
    fn next(&mut self) -> BoxFuture<'_, Result<TransportEvent, TransportError>> {
        Box::pin(async move {
            if self.server.is_empty() {
                return Err(TransportError::Closed);
            }
            if self.pending_sibling_connect {
                self.pending_sibling_connect = false;
                if let Some(whatsapp) = &self.whatsapp {
                    return Ok(TransportEvent::Connected {
                        name: whatsapp.shared_name().to_owned(),
                        identity: TransportIdentity::default(),
                    });
                }
            }
            if let Some(whatsapp) = &mut self.whatsapp {
                tokio::select! {
                    event = whatsapp.shared_next() => event,
                    message = self.receiver.recv() => message.ok_or(TransportError::Closed),
                    _ = self.server.join_next() => Err(TransportError::Closed),
                }
            } else {
                tokio::select! {
                    message = self.receiver.recv() => message.ok_or(TransportError::Closed),
                    _ = self.server.join_next() => Err(TransportError::Closed),
                }
            }
        })
    }
    fn driver(&self) -> Arc<dyn ChatDriver> {
        Arc::new(GithubDriver)
    }
    fn sibling_driver(&self) -> Option<(String, Arc<dyn ChatDriver>)> {
        self.whatsapp
            .as_ref()
            .map(|wa| (wa.shared_name().to_owned(), wa.shared_driver()))
    }
    fn sibling_fetcher(&self) -> Option<(String, Arc<dyn super::AssetFetcher>)> {
        self.whatsapp.as_ref().and_then(|wa| {
            wa.asset_fetcher()
                .map(|fetcher| (wa.shared_name().to_owned(), fetcher))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use serde_json::json;

    fn state() -> (StateData, mpsc::Receiver<TransportEvent>) {
        let (sender, receiver) = mpsc::channel(1);
        (
            StateData {
                name: "github".into(),
                secret: Arc::new(Redacted::new(b"secret".to_vec())),
                generation: CurrentGeneration::boot(Arc::default(), Arc::default()),
                sender,
                accepted: Arc::new(Mutex::new(SeenIds::new(RECENT_DELIVERIES))),
                refusals: Arc::new(Mutex::new(RefusalLog::new())),
                concurrent: Arc::new(Semaphore::new(1)),
            },
            receiver,
        )
    }

    fn signed(body: &[u8], secret: &[u8], guid: &str, event: &str) -> Request {
        let digest: String = webhook::hmac_sha256(secret, body)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Request::builder()
            .method("POST")
            .header("x-hub-signature-256", format!("sha256={digest}"))
            .header("x-github-delivery", guid)
            .header("x-github-event", event)
            .body(Body::from(body.to_vec()))
            .expect("request")
    }

    fn pr() -> Value {
        json!({ "repository": {"owner": {"login": "Owner"}, "name": "Repo"},
            "action": "opened", "number": 7, "pull_request": { "head": {"sha": "abc123"} } })
    }

    #[tokio::test]
    async fn mismatched_bytes_and_wrong_secret_refuse_before_json_parsing() {
        let (state, mut receiver) = state();
        let broken = b"{not json";
        let wrong_key = process(
            &state,
            signed(broken, b"wrong", "guid", "pull_request"),
            &receive_span(ChatTransportKind::Github),
        )
        .await;
        assert_eq!(wrong_key.status(), StatusCode::UNAUTHORIZED);
        let body = serde_json::to_vec(&pr()).expect("json");
        let mut altered = body.clone();
        altered.push(b' ');
        let signature = webhook::hmac_sha256(b"secret", &body);
        let signature: String = signature.iter().map(|byte| format!("{byte:02x}")).collect();
        let request = Request::builder()
            .header("x-hub-signature-256", format!("sha256={signature}"))
            .header("x-github-delivery", "guid")
            .header("x-github-event", "pull_request")
            .body(Body::from(altered))
            .expect("request");
        assert_eq!(
            process(&state, request, &receive_span(ChatTransportKind::Github))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn duplicated_signatures_and_sha1_only_refuse_without_an_identity() {
        let (state, mut receiver) = state();
        let body = serde_json::to_vec(&pr()).expect("json");
        let digest: String = webhook::hmac_sha256(b"secret", &body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let duplicate = Request::builder()
            .header("x-hub-signature-256", format!("sha256={digest}"))
            .header("x-hub-signature-256", format!("sha256={digest}"))
            .header("x-github-delivery", "guid")
            .header("x-github-event", "pull_request")
            .body(Body::from(body.clone()))
            .expect("request");
        assert_eq!(
            process(&state, duplicate, &receive_span(ChatTransportKind::Github))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let sha1 = Request::builder()
            .header("x-hub-signature", "sha1=whatever")
            .header("x-github-delivery", "guid")
            .header("x-github-event", "pull_request")
            .body(Body::from(body))
            .expect("request");
        assert_eq!(
            process(&state, sha1, &receive_span(ChatTransportKind::Github))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn signed_repository_alone_selects_the_subject_and_eligible_action() {
        let (subject, conversation, kind, text) =
            parse(&pr(), "pull_request", "guid").expect("claim");
        assert_eq!(subject.canonical(), "github.owner.repo");
        assert_eq!(conversation.key(), "owner.repo:guid");
        assert_eq!(kind, GithubEvent::PullRequest);
        assert!(text.expect("eligible").starts_with("[gateway: unverified"));
        let mut other = pr();
        other["repository"]["owner"]["login"] = json!("another");
        assert_eq!(
            parse(&other, "pull_request", "guid")
                .expect("signed other repo")
                .0
                .canonical(),
            "github.another.repo"
        );
        let mut ignored = pr();
        ignored["action"] = json!("closed");
        assert!(
            parse(&ignored, "pull_request", "guid")
                .expect("valid action")
                .3
                .is_none()
        );
        ignored["repository"]["name"] = json!("dotted.repo");
        assert!(parse(&ignored, "pull_request", "guid").is_none());
        let run = json!({ "repository": {"owner": {"login": "Owner"}, "name": "Repo"},
            "action": "completed", "workflow_run": {"id": 5, "head_sha": "abc", "conclusion": "failure"}});
        assert!(
            parse(&run, "workflow_run", "guid")
                .expect("report-only run")
                .3
                .expect("eligible")
                .contains("associated PR None")
        );
        let mut pending = run;
        pending["action"] = json!("requested");
        pending["workflow_run"]["conclusion"] = Value::Null;
        assert!(
            parse(&pending, "workflow_run", "guid")
                .expect("ignored signed run")
                .3
                .is_none()
        );
    }

    #[tokio::test]
    async fn missing_run_identity_is_malformed_even_for_an_ignored_action() {
        let (state, mut receiver) = state();
        let mut body = json!({"repository":{"owner":{"login":"Owner"},"name":"Repo"},
            "action":"requested"});
        let missing = serde_json::to_vec(&body).expect("signed JSON");
        assert_eq!(
            process(
                &state,
                signed(&missing, b"secret", "no-run", "workflow_run"),
                &receive_span(ChatTransportKind::Github)
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
        body["workflow_run"] = json!({"head_sha":"abc", "conclusion":null});
        let missing_id = serde_json::to_vec(&body).expect("signed JSON");
        assert_eq!(
            process(
                &state,
                signed(&missing_id, b"secret", "no-id", "workflow_run"),
                &receive_span(ChatTransportKind::Github)
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
        body["workflow_run"] = json!({"id":5, "conclusion":null});
        let missing_sha = serde_json::to_vec(&body).expect("signed JSON");
        assert_eq!(
            process(
                &state,
                signed(&missing_sha, b"secret", "no-sha", "workflow_run"),
                &receive_span(ChatTransportKind::Github)
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
        body["workflow_run"]["head_sha"] = json!("abc");
        let pending = serde_json::to_vec(&body).expect("signed JSON");
        assert_eq!(
            process(
                &state,
                signed(&pending, b"secret", "pending", "workflow_run"),
                &receive_span(ChatTransportKind::Github)
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert!(
            receiver.try_recv().is_err(),
            "no unselected run starts a turn"
        );
    }

    #[tokio::test]
    async fn duplicate_and_queue_full_never_record_unaccepted_identity() {
        use dekopon_test_support::Record;
        let directory = crate::tests::temporary();
        std::fs::write(
            directory.path().join("dekopon.yaml"),
            crate::tests::catalog_text(true, Some("reasoning")),
        )
        .expect("catalog");
        let mut doc = crate::tests::document(directory.path());
        doc["transports"][0] = json!({"kind":"githubWebhook", "name":"github",
            "bind":"127.0.0.1:9080", "secretEnv":"PATH"});
        doc["routes"][0]["transport"] = json!("github");
        doc["routes"][0]["memory"] = json!({"mode":"oneShot"});
        doc["routes"][0]["conversation"] = json!({"kind":["thread"],
            "container":"owner", "ids":["owner.repo"]});
        doc["routes"][0]["githubEvents"] = json!(["pull_request"]);
        let resolved = crate::tests::load(directory.path(), &doc)
            .await
            .expect("config");
        let catalog = dekopon_config::LocalCatalog::load(directory.path().join("dekopon.yaml"))
            .expect("catalog");
        let routes = crate::routes::RoutingTable::bind(&resolved, &catalog).expect("route");
        let (mut state, _unused) = state();
        state.generation = CurrentGeneration::boot(Arc::new(routes), Arc::default());
        let (sender, mut receiver) = mpsc::channel(1);
        state.sender = sender;
        let (capture, _guard) = crate::tests::capture_spans();
        let body = serde_json::to_vec(&pr()).expect("signed JSON");
        let count = || {
            let records = capture.records();
            (
                records
                    .iter()
                    .filter(|record| {
                        matches!(record, Record::Span { .. })
                            && record.field("message.id").is_some()
                    })
                    .count(),
                records
                    .iter()
                    .filter(|record| {
                        matches!(record, Record::Span { .. })
                            && record.field("conversation.thread").is_some()
                    })
                    .count(),
            )
        };
        for (guid, status, expected) in [
            ("first", StatusCode::OK, (1, 1)),
            ("first", StatusCode::OK, (1, 1)),
            ("retry", StatusCode::SERVICE_UNAVAILABLE, (1, 1)),
        ] {
            assert_eq!(
                process(
                    &state,
                    signed(&body, b"secret", guid, "pull_request"),
                    &receive_span(ChatTransportKind::Github)
                )
                .await
                .status(),
                status
            );
            assert_eq!(
                count(),
                expected,
                "only successfully accepted deliveries record identity"
            );
        }
        let _accepted = receiver.try_recv().expect("free one slot");
        assert_eq!(
            process(
                &state,
                signed(&body, b"secret", "retry", "pull_request"),
                &receive_span(ChatTransportKind::Github)
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            count(),
            (2, 2),
            "the retry records identity only after acceptance"
        );
    }

    #[tokio::test]
    async fn refusal_records_only_typed_reason_status_and_transport() {
        let (capture, _guard) = crate::tests::capture_spans();
        let (state, mut receiver) = state();
        let body = br#"{"repository":{"owner":{"login":"SENTINEL_REPO"}},"guid":"SENTINEL_GUID"}"#;
        let response = process(
            &state,
            signed(body, b"wrong", "sentinel-guid", "pull_request"),
            &receive_span(ChatTransportKind::Github),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let records = capture.events_where(
            "dekopon_gatewayd",
            &[
                ("event", "gateway_github_webhook_refused"),
                ("reason", "signature"),
                ("transport", "github"),
                ("status", "401"),
            ],
        );
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].field("suppressed"), Some("0"));
        for forbidden in ["body", "signature", "repository", "guid"] {
            assert!(records[0].field(forbidden).is_none());
        }
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn header_guid_and_event_bounds_refuse_before_a_turn() {
        let (state, mut receiver) = state();
        let body = serde_json::to_vec(&pr()).expect("json");
        for (guid, event) in [
            ("A", "pull_request"),
            ("", "pull_request"),
            ("guid", ""),
            (
                "guid",
                "a-long-event-name-over-sixty-four-characters-that-cannot-fit-in-header",
            ),
        ] {
            assert_eq!(
                process(
                    &state,
                    signed(&body, b"secret", guid, event),
                    &receive_span(ChatTransportKind::Github)
                )
                .await
                .status(),
                StatusCode::BAD_REQUEST
            );
        }
        assert!(receiver.try_recv().is_err());
    }
}
