use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use axum::{
    Extension, Router,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::post,
};
use dekopon_broker_protocol::{ChatTransportKind, Conversation, ConversationKind};
use dekopon_core::{AttestedSubject, Redacted};
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

const CALLBACK: &str = "/webhooks/github";
const RECENT_DELIVERIES: usize = 1024;
const QUEUE: usize = 64;
const REFUSAL_WINDOW: Duration = Duration::from_secs(60);

mod facts;
use dekopon_core::github::{GithubBatch, GithubEvent as TypedGithubEvent, GithubWord};
use facts::parse;

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
    accepted: Option<Arc<Mutex<SeenIds>>>,
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
    pub(crate) fn split(self, count: usize) -> (Vec<Self>, impl Future<Output = ()> + Send) {
        let mut tickets = Vec::with_capacity(count);
        let mut receivers = Vec::with_capacity(count);
        for _ in 0..count {
            let (reply, receiver) = oneshot::channel();
            tickets.push(Self {
                delivery: self.delivery.clone(),
                accepted: None,
                reply: Some(reply),
            });
            receivers.push(receiver);
        }
        let answer = async move {
            let mut answer = Acked::Queued;
            for result in futures_util::future::join_all(receivers).await {
                match result {
                    Ok(Acked::Admitted | Acked::Queued | Acked::Ignored) => {}
                    Ok(Acked::Busy) | Err(_) => answer = Acked::Busy,
                }
            }
            self.answer(answer);
        };
        (tickets, answer)
    }

    pub(crate) fn answer(mut self, answer: Acked) {
        let commit = match answer {
            Acked::Admitted | Acked::Queued | Acked::Ignored => true,
            Acked::Busy => false,
        };
        if let Some(accepted) = &self.accepted {
            accepted.lock().answer(&self.delivery, commit);
        }
        if let Some(reply) = self.reply.take() {
            reply.send(answer).unwrap_or_else(|_receiver_closed| {
                tracing::debug!(
                    event = "gateway_github_webhook_refused",
                    reason = "receiver-closed",
                    "gateway.github.webhook.refused"
                )
            });
        }
    }
}

impl Drop for AdmissionTicket {
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() {
            if let Some(accepted) = &self.accepted {
                accepted.lock().answer(&self.delivery, false);
            }
            reply.send(Acked::Busy).unwrap_or_else(|_receiver_closed| {
                tracing::debug!(
                    event = "gateway_github_webhook_refused",
                    reason = "receiver-closed",
                    "gateway.github.webhook.refused"
                )
            });
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

async fn receive(
    State(state): State<StateData>,
    Extension(deadline): Extension<webhook::RequestDeadline>,
    request: Request,
) -> Response {
    let Ok(_permit) = Arc::clone(&state.concurrent).try_acquire_owned() else {
        return refuse(&state, Refusal::Saturated, StatusCode::SERVICE_UNAVAILABLE);
    };
    let received = receive_span(ChatTransportKind::Github);
    match tokio::time::timeout_at(
        deadline.0,
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
    let delivery = header(headers, "x-github-delivery")
        .filter(|value| dekopon_core::github::GithubDeliveryId::new(value).is_ok());
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
    if let Some(reason) = repository_ignore_reason(&root) {
        ignored(state, &delivery, reason);
        return webhook::content_free(StatusCode::OK);
    }
    let Some(entry) = parse(&root, event_kind, &delivery) else {
        return refuse(state, Refusal::Malformed, StatusCode::BAD_REQUEST);
    };
    let Some((owner, repo)) = entry.repository.full_name.as_str().split_once('/') else {
        return refuse(state, Refusal::Malformed, StatusCode::BAD_REQUEST);
    };
    let Ok(subject) =
        AttestedSubject::github_repository(owner, repo, u64::from(entry.repository.id))
    else {
        return refuse(state, Refusal::Malformed, StatusCode::BAD_REQUEST);
    };
    let conversation = Conversation {
        kind: ConversationKind::Thread,
        container: Some(owner.to_owned()),
        id: format!("{owner}.{repo}"),
        thread: Some(entry.thread().to_string()),
    };
    if !state
        .generation
        .admits_github(&state.name, &conversation, subject.name(), event_kind)
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
        control_text: None,
        pull_request_links: None,
        transport: state.name.clone(),
        transport_kind: ChatTransportKind::Github,
        subject,
        conversation,
        message_id: MessageId::Github(Box::new(GithubBatch::new(entry))),
        text: String::new(),
        assets: Vec::new(),
        asset_overflow: false,
        input_refusal: None,
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
    let (permit, ticket, receiver) = {
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
            accepted: Some(Arc::clone(&state.accepted)),
            reply: Some(reply),
        };
        (permit, ticket, receiver)
    };
    receipt.record("message.id", &delivery);
    record_conversation(&receipt, &message.conversation);
    // A closed receiver can drop the ticket synchronously, and its Drop reacquires accepted.
    permit.send(TransportEvent::Delivery(Box::new(message), ticket));
    match receiver.await {
        Ok(Acked::Admitted | Acked::Queued | Acked::Ignored) => {
            webhook::content_free(StatusCode::OK)
        }
        Ok(Acked::Busy) | Err(_) => {
            refuse(state, Refusal::Saturated, StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

fn repository_ignore_reason(root: &Value) -> Option<&'static str> {
    (!root.get("repository").is_some_and(Value::is_object)).then_some("no-repository")
}

fn ignored(state: &StateData, delivery: &str, reason: &str) {
    tracing::debug!(
        event = "gateway_github_ignored",
        reason,
        "gateway.github.ignored"
    );
    dekopon_telemetry::with_event_string_array("github.deliveries", &[delivery], || {
        tracing::info!(target: "dekopon_gatewayd::audit", { audit.event = "gateway.admission",
            outcome = "ignored", transport = %state.name, queue.depth = 0 }, "gateway.admission");
    });
}

fn eligible(entry: &TypedGithubEvent) -> bool {
    match entry.event {
        GithubEvent::PullRequest => matches!(
            entry.action.as_ref().map(GithubWord::as_str),
            Some("opened" | "reopened" | "synchronize" | "closed")
        ),
        GithubEvent::WorkflowRun => {
            entry.action.as_ref().map(GithubWord::as_str) == Some("completed")
                && matches!(
                    entry.author_set.conclusion.as_ref().map(GithubWord::as_str),
                    Some("success" | "failure")
                )
        }
        GithubEvent::BranchProtectionConfiguration
        | GithubEvent::BranchProtectionRule
        | GithubEvent::CheckRun
        | GithubEvent::CheckSuite
        | GithubEvent::CodeScanningAlert
        | GithubEvent::CommitComment
        | GithubEvent::Create
        | GithubEvent::CustomPropertyValues
        | GithubEvent::Delete
        | GithubEvent::DependabotAlert
        | GithubEvent::DeployKey
        | GithubEvent::Deployment
        | GithubEvent::DeploymentProtectionRule
        | GithubEvent::DeploymentReview
        | GithubEvent::DeploymentStatus
        | GithubEvent::Discussion
        | GithubEvent::DiscussionComment
        | GithubEvent::Fork
        | GithubEvent::Gollum
        | GithubEvent::IssueComment
        | GithubEvent::IssueDependencies
        | GithubEvent::Issues
        | GithubEvent::Label
        | GithubEvent::Member
        | GithubEvent::MergeGroup
        | GithubEvent::Meta
        | GithubEvent::Milestone
        | GithubEvent::Package
        | GithubEvent::PageBuild
        | GithubEvent::ProjectCard
        | GithubEvent::ProjectColumn
        | GithubEvent::Project
        | GithubEvent::Public
        | GithubEvent::PullRequestReview
        | GithubEvent::PullRequestReviewComment
        | GithubEvent::PullRequestReviewThread
        | GithubEvent::Push
        | GithubEvent::RegistryPackage
        | GithubEvent::Release
        | GithubEvent::Repository
        | GithubEvent::RepositoryAdvisory
        | GithubEvent::RepositoryDispatch
        | GithubEvent::RepositoryImport
        | GithubEvent::RepositoryRuleset
        | GithubEvent::RepositoryVulnerabilityAlert
        | GithubEvent::SecretScanningAlert
        | GithubEvent::SecretScanningAlertLocation
        | GithubEvent::SecretScanningScan
        | GithubEvent::SecurityAndAnalysis
        | GithubEvent::Star
        | GithubEvent::Status
        | GithubEvent::SubIssues
        | GithubEvent::TeamAdd
        | GithubEvent::Watch
        | GithubEvent::WorkflowDispatch
        | GithubEvent::WorkflowJob => true,
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
    #[cfg(test)]
    pub(crate) fn request_for_test(
        &self,
        request: Request,
    ) -> impl Future<Output = Response> + use<> {
        let state = self.state.clone();
        async move {
            let deadline = webhook::RequestDeadline(
                tokio::time::Instant::now() + webhook::WEBHOOK_REQUEST_TIMEOUT,
            );
            receive(State(state), Extension(deadline), request).await
        }
    }

    #[cfg(test)]
    pub(crate) fn try_delivery_for_test(
        &mut self,
    ) -> Result<TransportEvent, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }

    #[cfg(test)]
    pub(crate) async fn delivery_for_test(&mut self) -> TransportEvent {
        self.receiver.recv().await.expect("signed delivery")
    }

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
pub(crate) mod tests {
    use super::*;
    use axum::body::Body;
    use serde_json::json;

    pub(super) fn state() -> (StateData, mpsc::Receiver<TransportEvent>) {
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

    async fn routed_state() -> (StateData, mpsc::Receiver<TransportEvent>) {
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
        doc["routes"][0]["githubEvents"] = json!(["pull_request", "workflow_run"]);
        let resolved = crate::tests::load(directory.path(), &doc)
            .await
            .expect("config");
        let catalog = dekopon_config::LocalCatalog::load(directory.path().join("dekopon.yaml"))
            .expect("catalog");
        let routes = crate::routes::RoutingTable::bind(&resolved, &catalog).expect("route");
        let (mut state, receiver) = state();
        state.generation = CurrentGeneration::boot(Arc::new(routes), Arc::default());
        (state, receiver)
    }

    pub(crate) fn signed(body: &[u8], secret: &[u8], guid: &str, event: &str) -> Request {
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

    pub(crate) fn pr() -> Value {
        json!({ "repository": {"id": 10, "owner": {"login": "Owner"}, "name": "Repo"}, "sender": {"id": 1,"login":"owner","type":"User"},
            "action": "opened", "number": 7, "pull_request": {"number":7,"user":{"id":1},"author_association":"OWNER","labels":[],"merged":false,"draft":false,"state":"open","base":{"ref":"main","sha":"b".repeat(40),"repo":{"id":10}},"head":{"ref":"feature","sha":"a".repeat(40),"repo":{"id":10,"full_name":"owner/repo"}} } })
    }

    pub(crate) fn sample(event: &str) -> Value {
        let mut body = pr();
        body["action"] = json!(if event == "workflow_run" {
            "completed"
        } else {
            "opened"
        });
        body["issue"] = json!({"number":7,"user":{"id":1},"author_association":"OWNER","labels":[],"assignees":[],"state":"open"});
        body["parent_issue"] = body["issue"].clone();
        body["blocked_issue"] = body["issue"].clone();
        body["parent_issue_repo"] = json!({"id":10});
        body["blocked_issue_repo"] = json!({"id":10});
        body["comment"] = json!({"id":2,"author_association":"OWNER"});
        body["review"] = json!({"id":2,"author_association":"OWNER","state":"approved","commit_id":"a".repeat(40)});
        body["discussion"] = json!({"number":7,"author_association":"OWNER","state":"open","category":{"slug":"q-a"}});
        body["ref"] = json!(if event == "push" {
            "refs/heads/main"
        } else {
            "main"
        });
        body["ref_type"] = json!("branch");
        body["before"] = json!("a".repeat(40));
        body["after"] = json!("b".repeat(40));
        body["created"] = json!(false);
        body["deleted"] = json!(false);
        body["forced"] = json!(false);
        body["commits"] = json!([]);
        body["workflow_run"] = json!({"id":3,"run_number":1,"run_attempt":1,"event":"push","status":"completed","head_sha":"a".repeat(40),"conclusion":"success","pull_requests":[]});
        body["workflow_job"] = json!({"id":4,"run_id":3,"run_attempt":1,"status":"completed","head_sha":"a".repeat(40)});
        body["check_run"] = json!({"id":3,"app":{"id":4},"check_suite":{"id":5},"status":"completed","head_sha":"a".repeat(40),"pull_requests":[]});
        body["check_suite"] = json!({"id":5,"app":{"id":4},"status":"completed","head_sha":"a".repeat(40),"pull_requests":[]});
        body["id"] = json!(3);
        body["sha"] = json!("a".repeat(40));
        body["state"] = json!("success");
        body["release"] = json!({"id":3,"tag_name":"v1","draft":false,"prerelease":false});
        body["deployment"] = json!({"id":3,"ref":"main","sha":"a".repeat(40)});
        body["deployment_status"] = json!({"state":"success"});
        body["alert"] = json!({"number":3,"state":"open","ghsa_id":"GHSA-1234"});
        body["repository_advisory"] = body["alert"].clone();
        body["label"] = json!({"id":3,"name":"ready"});
        body["milestone"] = json!({"id":3,"number":1,"state":"open"});
        body["merge_group"] =
            json!({"head_sha":"a".repeat(40),"base_sha":"b".repeat(40),"base_ref":"main"});
        body["package"] = json!({"id":3,"package_type":"CONTAINER"});
        body["registry_package"] = body["package"].clone();
        body["member"] = json!({"id":3});
        body
    }

    fn review_payload(family: GithubEvent) -> Value {
        let name = serde_json::to_value(family).unwrap();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/github-reviews")
            .join(format!("{}.json", name.as_str().unwrap()));
        serde_json::from_slice(&std::fs::read(path).expect("documented webhook fixture"))
            .expect("documented webhook payload")
    }

    #[test]
    fn ingress_review_families_derive_merged_from_the_simple_pull_request() {
        let mut observed = Vec::new();
        for family in [
            GithubEvent::PullRequestReview,
            GithubEvent::PullRequestReviewComment,
            GithubEvent::PullRequestReviewThread,
        ] {
            let mut body = review_payload(family);
            assert!(body["pull_request"].get("merged").is_none());
            for (merged_at, merged) in [(Value::Null, false), (json!("2026-10-29T04:15:00Z"), true)]
            {
                body["pull_request"]["merged_at"] = merged_at;
                observed.push((
                    family,
                    merged,
                    parse(&body, family, "review").map(|event| {
                        let value = serde_json::to_value(event).unwrap();
                        value["pullRequest"]["merged"].clone()
                    }),
                ));
            }
            body["pull_request"]["merged"] = json!(false);
            assert_eq!(
                serde_json::to_value(parse(&body, family, "explicit").unwrap()).unwrap()["pullRequest"]
                    ["merged"],
                json!(false)
            );
            body["pull_request"]["merged"] = Value::Null;
            assert!(parse(&body, family, "invalid").is_none());
            body["pull_request"]
                .as_object_mut()
                .unwrap()
                .remove("merged");
            body["pull_request"]
                .as_object_mut()
                .unwrap()
                .remove("merged_at");
            assert!(parse(&body, family, "missing").is_none());
        }
        assert!(
            observed
                .iter()
                .all(|(_, expected, actual)| actual.as_ref() == Some(&json!(expected))),
            "{observed:?}"
        );
    }

    #[tokio::test]
    async fn ingress_unlisted_review_is_acknowledged_without_queueing() {
        let (state, mut receiver) = routed_state().await;
        let (capture, _guard) = crate::tests::capture_spans();
        let mut body = review_payload(GithubEvent::PullRequestReview);
        body["repository"]["owner"]["login"] = json!("owner");
        body["repository"]["name"] = json!("repo");
        let body = serde_json::to_vec(&body).unwrap();
        assert_eq!(
            process(
                &state,
                signed(&body, b"secret", "review", "pull_request_review"),
                &receive_span(ChatTransportKind::Github)
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            capture
                .events_where(
                    "dekopon_gatewayd",
                    &[
                        ("event", "gateway_github_ignored"),
                        ("reason", "not-listed")
                    ]
                )
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn ingress_out_of_grammar_repository_is_ignored_without_refusal() {
        let (state, mut receiver) = routed_state().await;
        let (capture, _guard) = crate::tests::capture_spans();
        let mut body = pr();
        body["repository"]["name"] = json!("x.github.io");
        let body = serde_json::to_vec(&body).unwrap();
        assert_eq!(
            process(
                &state,
                signed(&body, b"secret", "dotted", "pull_request"),
                &receive_span(ChatTransportKind::Github)
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert!(receiver.try_recv().is_err());
        assert!(
            capture
                .events_where(
                    "dekopon_gatewayd",
                    &[("event", "gateway_github_webhook_refused")]
                )
                .is_empty()
        );
        assert_eq!(
            capture
                .events_where(
                    "dekopon_gatewayd",
                    &[
                        ("event", "gateway_github_ignored"),
                        ("reason", "not-listed")
                    ]
                )
                .len(),
            1
        );
        for name in [Value::Null, json!(7)] {
            let mut missing = pr();
            missing["repository"]["name"] = name;
            let body = serde_json::to_vec(&missing).unwrap();
            assert_eq!(
                process(
                    &state,
                    signed(&body, b"secret", "missing-name", "pull_request"),
                    &receive_span(ChatTransportKind::Github)
                )
                .await
                .status(),
                StatusCode::BAD_REQUEST
            );
        }
        let mut missing = pr();
        missing["repository"]
            .as_object_mut()
            .unwrap()
            .remove("name");
        let body = serde_json::to_vec(&missing).unwrap();
        assert_eq!(
            process(
                &state,
                signed(&body, b"secret", "absent-name", "pull_request"),
                &receive_span(ChatTransportKind::Github)
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn delayed_headers_still_receive_503_when_admission_times_out() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let (state, mut receiver) = routed_state().await;
        let (capture, _guard) = crate::tests::capture_spans();
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe");
        let address = probe.local_addr().expect("address");
        drop(probe);
        let router = Router::new()
            .route(CALLBACK, post(receive))
            .with_state(state);
        let mut server = tokio::task::JoinSet::new();
        webhook::listen(address, router, "github".into(), &mut server)
            .await
            .expect("listen");
        let mut socket = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect");
        tokio::time::sleep(Duration::from_secs(2)).await;
        let body = serde_json::to_vec(&pr()).expect("body");
        let request = signed(&body, b"secret", "delayed-headers", "pull_request");
        let mut headers = format!(
            "POST {CALLBACK} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (name, value) in request.headers() {
            headers.push_str(&format!("{name}: {}\r\n", value.to_str().expect("header")));
        }
        headers.push_str("\r\n");
        socket.write_all(headers.as_bytes()).await.expect("headers");
        socket.write_all(&body).await.expect("body");
        let TransportEvent::Delivery(_, ticket) = receiver.recv().await.expect("delivery") else {
            panic!("admission ticket")
        };
        let mut response = String::new();
        tokio::time::timeout(
            webhook::WEBHOOK_REQUEST_TIMEOUT + Duration::from_secs(2),
            socket.read_to_string(&mut response),
        )
        .await
        .expect("bounded response")
        .expect("read response");
        assert_eq!(
            response.lines().next(),
            Some("HTTP/1.1 503 Service Unavailable")
        );
        assert_eq!(
            capture
                .events_where(
                    "dekopon_gatewayd",
                    &[
                        ("event", "gateway_github_webhook_refused"),
                        ("reason", "timeout")
                    ]
                )
                .len(),
            1
        );
        ticket.answer(Acked::Admitted);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn pooled_webhook_requests_reconnect_and_receive_timeout_responses() {
        let (state, mut receiver) = routed_state().await;
        let (capture, _guard) = crate::tests::capture_spans();
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe");
        let address = probe.local_addr().expect("address");
        drop(probe);
        let router = Router::new()
            .route(CALLBACK, post(receive))
            .with_state(state);
        let mut server = tokio::task::JoinSet::new();
        webhook::listen(address, router, "github".into(), &mut server)
            .await
            .expect("listen");
        let client = reqwest::Client::builder()
            .http1_only()
            .pool_max_idle_per_host(1)
            .timeout(Duration::from_secs(8))
            .build()
            .expect("client");
        let url = format!("http://{address}{CALLBACK}");
        let ping = signed(b"{}", b"secret", "pooled-ping", "ping");
        let first = client
            .post(&url)
            .headers(ping.headers().clone())
            .body("{}")
            .send()
            .await
            .expect("ping response");
        assert_eq!(first.status(), StatusCode::OK);
        let close = first.headers().get(axum::http::header::CONNECTION).cloned();
        first.bytes().await.expect("drain first response");
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let body = serde_json::to_vec(&pr()).expect("body");
        let request = signed(&body, b"secret", "pooled-second", "pull_request");
        let send = client
            .post(&url)
            .headers(request.headers().clone())
            .body(body)
            .send();
        let (response, event) = tokio::time::timeout(Duration::from_secs(8), async {
            tokio::join!(send, receiver.recv())
        })
        .await
        .expect("bounded second request");
        let TransportEvent::Delivery(_, ticket) = event.expect("delivery") else {
            panic!("admission ticket")
        };
        assert_eq!(
            response
                .expect("second response, not silent close")
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            close.as_ref().map(|value| value.as_bytes()),
            Some(b"close".as_slice())
        );
        assert_eq!(
            capture
                .events_where(
                    "dekopon_gatewayd",
                    &[
                        ("event", "gateway_github_webhook_refused"),
                        ("reason", "timeout")
                    ]
                )
                .len(),
            1
        );
        ticket.answer(Acked::Admitted);
        server.shutdown().await;
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
        let entry = parse(&pr(), GithubEvent::PullRequest, "guid").expect("claim");
        assert_eq!(entry.repository.full_name.as_str(), "owner/repo");
        assert_eq!(entry.thread().to_string(), "7");
        assert!(eligible(&entry));
        assert!(
            GithubBatch::new(entry)
                .note()
                .starts_with("[gateway: unverified")
        );
        let mut ignored = pr();
        ignored["action"] = json!("edited");
        assert!(!eligible(
            &parse(&ignored, GithubEvent::PullRequest, "guid").unwrap()
        ));
        ignored["repository"]["name"] = json!("Dotted.github.io");
        assert_eq!(
            parse(&ignored, GithubEvent::PullRequest, "guid")
                .expect("a dotted repository is admitted")
                .repository
                .full_name
                .as_str(),
            "owner/dotted.github.io"
        );
    }

    #[tokio::test]
    async fn missing_run_identity_is_malformed_even_for_an_ignored_action() {
        let (state, mut receiver) = state();
        let mut body = sample("workflow_run");
        body["action"] = json!("requested");
        let mut run = body["workflow_run"].clone();
        run["conclusion"] = Value::Null;
        body.as_object_mut().unwrap().remove("workflow_run");
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
        body["workflow_run"] = run.clone();
        body["workflow_run"].as_object_mut().unwrap().remove("id");
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
        body["workflow_run"] = run.clone();
        body["workflow_run"]
            .as_object_mut()
            .unwrap()
            .remove("head_sha");
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
        body["workflow_run"] = run;
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
        let (mut state, _unused) = routed_state().await;
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
        let first_span = receive_span(ChatTransportKind::Github);
        let first = process(
            &state,
            signed(&body, b"secret", "first", "pull_request"),
            &first_span,
        );
        tokio::pin!(first);
        let event = tokio::select! { biased; _ = &mut first => panic!("must wait for admission"), event = receiver.recv() => event.unwrap() };
        assert_eq!(count(), (1, 1));
        assert_eq!(
            process(
                &state,
                signed(&body, b"secret", "first", "pull_request"),
                &receive_span(ChatTransportKind::Github)
            )
            .await
            .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        let TransportEvent::Delivery(_, ticket) = event else {
            panic!("delivery ticket")
        };
        ticket.answer(Acked::Admitted);
        assert_eq!(first.await.status(), StatusCode::OK);
        assert_eq!(
            process(
                &state,
                signed(&body, b"secret", "first", "pull_request"),
                &receive_span(ChatTransportKind::Github)
            )
            .await
            .status(),
            StatusCode::OK
        );
        let queued_span = receive_span(ChatTransportKind::Github);
        let queued = process(
            &state,
            signed(&body, b"secret", "queued", "pull_request"),
            &queued_span,
        );
        tokio::pin!(queued);
        assert!(futures_util::poll!(&mut queued).is_pending());
        assert_eq!(count(), (2, 2));
        assert_eq!(
            process(
                &state,
                signed(&body, b"secret", "retry", "pull_request"),
                &receive_span(ChatTransportKind::Github)
            )
            .await
            .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(count(), (2, 2), "a full queue records no accepted identity");
        let TransportEvent::Delivery(_, ticket) = receiver.try_recv().unwrap() else {
            panic!("queued ticket")
        };
        ticket.answer(Acked::Ignored);
        assert_eq!(queued.await.status(), StatusCode::OK);
        let retry_span = receive_span(ChatTransportKind::Github);
        let retry = process(
            &state,
            signed(&body, b"secret", "retry", "pull_request"),
            &retry_span,
        );
        tokio::pin!(retry);
        let event = tokio::select! { biased; _ = &mut retry => panic!("must wait"), event = receiver.recv() => event.unwrap() };
        let TransportEvent::Delivery(_, ticket) = event else {
            panic!("delivery ticket")
        };
        ticket.answer(Acked::Busy);
        assert_eq!(retry.await.status(), StatusCode::SERVICE_UNAVAILABLE);
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

#[cfg(test)]
mod typed_fact_tests {
    use super::tests::{pr, signed};
    use super::*;
    use serde_json::json;

    async fn broker_round_trip(event: TypedGithubEvent) -> Value {
        use dekopon_broker_protocol::{
            Attestation, BrokerRequest, ChatScopeClaim, DeliveryIdentity, FrameLimits,
            RequestEnvelope, Trigger, read_frame, write_frame,
        };
        use dekopon_core::github::GithubBatch;

        let claim = Attestation::for_chat(
            dekopon_core::AttestedSubject::github_repository("owner", "repo", 1).unwrap(),
            "reviewer".parse().unwrap(),
            ChatScopeClaim {
                participant: None,
                transport: "github".parse().unwrap(),
                kind: ChatTransportKind::Github,
                conversation: Conversation {
                    kind: ConversationKind::Thread,
                    container: Some("owner".into()),
                    id: "owner.repo".into(),
                    thread: Some(event.thread().to_string()),
                },
                delivery: Some(DeliveryIdentity::Github {
                    delivery: "ref-delivery".into(),
                }),
                trigger: Trigger::Message,
                github: Some(GithubBatch::new(event)),
            },
        );
        let request = RequestEnvelope::capabilities(Some(claim));
        let (mut sender, mut receiver) = tokio::io::duplex(8192);
        let (sent, received) = tokio::join!(
            write_frame(&mut sender, &request, FrameLimits::default()),
            read_frame::<_, RequestEnvelope>(&mut receiver, FrameLimits::default()),
        );
        sent.unwrap();
        let BrokerRequest::Capabilities {
            attestation: Some(claim),
        } = received.unwrap().request
        else {
            panic!("broker capabilities claim");
        };
        let scope = claim.scope.unwrap();
        let github = scope.github.unwrap();
        serde_json::to_value(github.lead()).unwrap()
    }

    #[tokio::test]
    async fn git_ref_names_survive_the_gateway_to_broker_round_trip() {
        for (family, payload_path, raw, fact_path, expected) in [
            (
                "push",
                "/ref",
                "refs/heads/refs/heads/main",
                "/push/ref",
                "refs/heads/main",
            ),
            (
                "push",
                "/ref",
                "refs/tags/refs/tags/v1",
                "/push/ref",
                "refs/tags/v1",
            ),
            (
                "create",
                "/ref",
                "refs/heads/main",
                "/gitRef/name",
                "refs/heads/main",
            ),
            (
                "pull_request",
                "/pull_request/base/ref",
                "refs/heads/main",
                "/pullRequest/baseRef",
                "refs/heads/main",
            ),
            (
                "release",
                "/release/tag_name",
                "refs/tags/v1",
                "/release/tag",
                "refs/tags/v1",
            ),
            (
                "deployment",
                "/deployment/environment",
                "refs/heads/staging",
                "/authorSet/environment",
                "refs/heads/staging",
            ),
            (
                "workflow_dispatch",
                "/ref",
                "refs/heads/refs/heads/main",
                "/gitRef/name",
                "refs/heads/main",
            ),
            (
                "merge_group",
                "/merge_group/base_ref",
                "refs/heads/refs/heads/main",
                "/mergeGroup/baseRef",
                "refs/heads/main",
            ),
        ] {
            let mut body = super::tests::sample(family);
            if family == "deployment" {
                body["deployment"]["environment"] = json!("staging");
            }
            body.pointer_mut(payload_path)
                .unwrap()
                .clone_from(&json!(raw));
            let event = parse(
                &body,
                serde_json::from_value(json!(family)).unwrap(),
                "ref-delivery",
            )
            .unwrap();
            let gateway = serde_json::to_value(&event).unwrap();
            assert_eq!(
                gateway.pointer(fact_path).unwrap(),
                &json!(expected),
                "gateway {family}"
            );
            let github = broker_round_trip(event).await;
            assert_eq!(github, gateway, "broker {family}");
        }
    }

    #[tokio::test]
    async fn an_unknown_field_is_ignored_and_only_a_required_fact_out_of_bound_is_malformed() {
        let mut body = pr();
        body["unknown"] = json!({"nested":"ignored"});
        body["pull_request"]["unknown"] = json!(true);
        body["pull_request"]["labels"] = json!([{"name":"x".repeat(51)},{"name":"🍋".repeat(50)}]);
        let event = parse(&body, GithubEvent::PullRequest, "delivery-1").unwrap();
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["pullRequest"]["labels"], json!(["🍋".repeat(50)]));
        body["pull_request"]["head"]["sha"] = json!("a".repeat(41));
        assert!(parse(&body, GithubEvent::PullRequest, "delivery-1").is_none());
        let (state, _receiver) = super::tests::state();
        let malformed = signed(
            &serde_json::to_vec(&body).unwrap(),
            b"secret",
            "delivery-1",
            "pull_request",
        );
        assert_eq!(
            process(&state, malformed, &receive_span(ChatTransportKind::Github))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );

        for (family, key) in [
            (GithubEvent::Discussion, "discussion"),
            (GithubEvent::DiscussionComment, "discussion_comment"),
        ] {
            let mut body = pr();
            body["discussion"] = json!({"number":7,"author_association":"OWNER","state":"open","category":{"slug":"推荐项目"}});
            body["comment"] = json!({"id":2,"author_association":"OWNER"});
            let event = parse(&body, family, key).unwrap();
            assert!(
                serde_json::to_value(event).unwrap()["authorSet"]
                    .get("category")
                    .is_none()
            );
        }
        for action in ["deploy staging".to_owned(), "x".repeat(65)] {
            let mut body = pr();
            body["action"] = json!(action);
            assert!(
                serde_json::to_value(
                    parse(&body, GithubEvent::RepositoryDispatch, "dispatch").unwrap()
                )
                .unwrap()
                .get("action")
                .is_none()
            );
        }
        let mut body = pr();
        body["action"] = json!("deploy:migrations");
        body["deployment"] = json!({"id":3,"ref":"main","sha":"a".repeat(40),"environment":"staging environment","task":"deploy:migrations"});
        let event =
            serde_json::to_value(parse(&body, GithubEvent::Deployment, "deployment").unwrap())
                .unwrap();
        assert!(event["authorSet"].get("environment").is_none());
        assert_eq!(event["authorSet"]["task"], json!("deploy:migrations"));
        body["discussion"] = json!({"number":7,"author_association":"OWNER","state":"open","category":{"slug":"q-a"}});
        let event =
            serde_json::to_value(parse(&body, GithubEvent::Discussion, "discussion").unwrap())
                .unwrap();
        assert_eq!(event["discussion"]["authorAssociation"], json!("OWNER"));
        assert_eq!(event["authorSet"]["category"], json!("q-a"));
    }

    #[test]
    fn every_typed_family_parses_its_synthetic_body() {
        for (name, kind) in [
            ("branch_protection_configuration", "none"),
            ("branch_protection_rule", "none"),
            ("check_run", "check"),
            ("check_suite", "check"),
            ("code_scanning_alert", "alert"),
            ("commit_comment", "commitComment"),
            ("create", "ref"),
            ("custom_property_values", "none"),
            ("delete", "ref"),
            ("dependabot_alert", "alert"),
            ("deploy_key", "none"),
            ("deployment", "deployment"),
            ("deployment_protection_rule", "deployment"),
            ("deployment_review", "deployment"),
            ("deployment_status", "deployment"),
            ("discussion", "discussion"),
            ("discussion_comment", "discussionComment"),
            ("fork", "none"),
            ("gollum", "none"),
            ("issue_comment", "issueComment"),
            ("issue_dependencies", "issue"),
            ("issues", "issue"),
            ("label", "label"),
            ("member", "member"),
            ("merge_group", "mergeGroup"),
            ("meta", "none"),
            ("milestone", "milestone"),
            ("package", "package"),
            ("page_build", "none"),
            ("project_card", "none"),
            ("project_column", "none"),
            ("project", "none"),
            ("public", "none"),
            ("pull_request", "pullRequest"),
            ("pull_request_review", "review"),
            ("pull_request_review_comment", "reviewComment"),
            ("pull_request_review_thread", "pullRequest"),
            ("push", "push"),
            ("registry_package", "package"),
            ("release", "release"),
            ("repository", "none"),
            ("repository_advisory", "alert"),
            ("repository_dispatch", "none"),
            ("repository_import", "none"),
            ("repository_ruleset", "none"),
            ("repository_vulnerability_alert", "alert"),
            ("secret_scanning_alert", "alert"),
            ("secret_scanning_alert_location", "alert"),
            ("secret_scanning_scan", "none"),
            ("security_and_analysis", "none"),
            ("star", "none"),
            ("status", "check"),
            ("sub_issues", "issue"),
            ("team_add", "none"),
            ("watch", "none"),
            ("workflow_dispatch", "ref"),
            ("workflow_job", "run"),
            ("workflow_run", "run"),
        ] {
            let event: GithubEvent = serde_json::from_value(json!(name)).unwrap();
            let body = super::tests::sample(name);
            let facts = parse(&body, event, "synthetic").expect(name);
            let value = serde_json::to_value(facts).unwrap();
            assert_eq!(value["kind"], json!(kind), "{name}");
            assert_eq!(value["event"], json!(name));
        }
    }

    #[test]
    fn foreign_issue_and_pull_request_numbers_never_become_facts_or_threads() {
        for (name, action, repo, issue) in [
            (
                "sub_issues",
                "parent_issue_added",
                "parent_issue_repo",
                "parent_issue",
            ),
            (
                "issue_dependencies",
                "blocking_added",
                "blocked_issue_repo",
                "blocked_issue",
            ),
        ] {
            let mut body = super::tests::sample(name);
            body["action"] = json!(action);
            body[repo]["id"] = json!(20);
            body[issue]["number"] = json!(987);
            let event = parse(
                &body,
                serde_json::from_value(json!(name)).unwrap(),
                "foreign",
            )
            .unwrap();
            assert_eq!(event.thread().to_string(), "repository");
            let value = serde_json::to_value(event).unwrap();
            assert!(value.get("number").is_none());
            assert!(value.get("issue").is_none());
        }
        for (name, key) in [
            ("workflow_run", "workflow_run"),
            ("check_run", "check_run"),
            ("check_suite", "check_suite"),
        ] {
            let mut body = super::tests::sample(name);
            body[key]["pull_requests"] = json!([{"number":987,"base":{"repo":{"id":20}}},{"number":7,"base":{"repo":{"id":10}}}]);
            let event = parse(
                &body,
                serde_json::from_value(json!(name)).unwrap(),
                "foreign",
            )
            .unwrap();
            assert_eq!(event.thread().to_string(), "7");
            let value = serde_json::to_value(event).unwrap();
            let target = if name == "workflow_run" {
                "run"
            } else {
                "check"
            };
            assert_eq!(value[target]["pullRequests"], json!([7]));
        }
    }

    #[tokio::test]
    async fn the_redelivery_bodies_parse_to_their_facts() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/typed-events-deliveries");
        let mut count = 0;
        for file in std::fs::read_dir(dir).expect("pinned redeliveries") {
            let path = file.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let delivery = path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .split('-')
                .next()
                .unwrap();
            let fixture: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let family: GithubEvent = serde_json::from_value(fixture["event"].clone()).unwrap();
            let body = &fixture["body"];
            let request = signed(
                &serde_json::to_vec(body).unwrap(),
                b"test-secret",
                delivery,
                fixture["event"].as_str().unwrap(),
            );
            let verified = match webhook::verified_body(request, b"test-secret").await {
                Ok(bytes) => bytes,
                Err(_) => panic!("test-secret signature must verify"),
            };
            assert_eq!(serde_json::from_slice::<Value>(&verified).unwrap(), *body);
            let event = parse(body, family, delivery).expect("real delivery parses");
            let thread = event.thread().to_string();
            let value = serde_json::to_value(event).unwrap();
            assert_eq!(value["event"], fixture["event"]);
            if family == GithubEvent::PullRequest {
                assert_eq!(thread, body["number"].to_string());
                assert_eq!(
                    value["pullRequest"]["baseSha"],
                    body["pull_request"]["base"]["sha"]
                );
                assert_eq!(
                    value["authorSet"]["headSha"],
                    body["pull_request"]["head"]["sha"]
                );
            } else {
                let expected = if body["workflow_run"]["pull_requests"]
                    .as_array()
                    .unwrap()
                    .len()
                    == 1
                {
                    "780"
                } else {
                    "repository"
                };
                assert_eq!(thread, expected);
                assert_eq!(value["run"]["id"], body["workflow_run"]["id"]);
                assert_eq!(
                    value["authorSet"]["runStatus"],
                    body["workflow_run"]["status"]
                );
            }
            count += 1;
        }
        assert_eq!(count, 13);
    }
}
