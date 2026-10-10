use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
    time::Duration,
};

use dekopon_core::Redacted;
use dekopon_model_token_governor::{Budget, MeterSpec, Metering, Tokens, UnixMillis};
use dekopon_test_support::{CODEX_RESPONSES_TWO_DELTAS, OPENAI_CHAT_COMPLETIONS_TWO_DELTAS};
use parking_lot::Mutex;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};

use crate::{
    Grant, MAX_BODY_BYTES, ModelProxy, ProxyModel, SESSION_HEADER, SUBJECT_HEADER, Upstream,
};

const SUBJECT: &str = "dekopon:gylmar-vm";

enum Step {
    Write(Vec<u8>),
    Wait(Duration),
}

/// A loopback upstream that records each request and answers with scripted writes and pauses.
struct FakeUpstream {
    url: String,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl FakeUpstream {
    async fn start(steps: Vec<Step>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let request = read_request(&mut stream).await;
            recorded.lock().push(request);
            for step in steps {
                match step {
                    Step::Write(bytes) => {
                        if stream.write_all(&bytes).await.is_err() {
                            return;
                        }
                        let _flushed = stream.flush().await;
                    }
                    Step::Wait(duration) => tokio::time::sleep(duration).await,
                }
            }
        });
        Self { url, requests }
    }

    fn request(&self) -> String {
        String::from_utf8(self.requests.lock()[0].clone()).unwrap()
    }

    fn body(&self) -> String {
        let request = self.request();
        request.split_once("\r\n\r\n").unwrap().1.to_owned()
    }
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = stream.read(&mut buffer).await.unwrap();
        request.extend_from_slice(&buffer[..read]);
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            let length = dekopon_test_support::content_length(&request[..end]);
            if request.len() >= end + 4 + length || read == 0 {
                return request;
            }
        }
        if read == 0 {
            return request;
        }
    }
}

fn sse_head() -> Step {
    Step::Write(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n"
            .to_vec(),
    )
}

fn chunk(bytes: &[u8]) -> Step {
    let mut out = format!("{:x}\r\n", bytes.len()).into_bytes();
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n");
    Step::Write(out)
}

fn end() -> Step {
    Step::Write(b"0\r\n\r\n".to_vec())
}

fn json(status: &str, body: &str) -> Step {
    Step::Write(
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes(),
    )
}

fn metering(limit: u64) -> Arc<Metering> {
    Arc::new(Metering::new(
        vec![Budget::new(
            "gylmar".parse().unwrap(),
            None,
            &[MeterSpec::Rolling {
                limit: Tokens(limit),
                period: Duration::from_secs(5 * 3_600),
            }],
            UnixMillis::now(),
        )],
        Metering::system_clock(),
    ))
}

fn used(metering: &Metering) -> u64 {
    metering.statuses(&"gylmar".parse().unwrap()).unwrap()[0]
        .1
        .used
        .0
}

fn models(
    endpoint: &str,
    codex: Option<Arc<dekopon_model::chatgpt::CredentialFile>>,
) -> Vec<ProxyModel> {
    let mut models = vec![
        ProxyModel {
            name: "claude-opus".to_owned(),
            wire_model: "claude-opus-4-1".to_owned(),
            backend: "anthropic",
            reserve: Tokens(10),
            upstream: Upstream::Anthropic {
                endpoint: endpoint.to_owned(),
                api_key: Redacted::new("sk-ant-proxy".to_owned()),
            },
        },
        ProxyModel {
            name: "glm-flash".to_owned(),
            wire_model: "z-ai/glm-4.5-air".to_owned(),
            backend: "openrouter",
            reserve: Tokens(10),
            upstream: Upstream::OpenRouter {
                endpoint: endpoint.to_owned(),
                api_key: Redacted::new("sk-or-proxy".to_owned()),
            },
        },
    ];
    if let Some(credential) = codex {
        models.push(ProxyModel {
            name: "astra".to_owned(),
            wire_model: "gpt-5-codex".to_owned(),
            backend: "codex",
            reserve: Tokens(10),
            upstream: Upstream::Codex {
                endpoint: endpoint.to_owned(),
                credential,
            },
        });
    }
    models
}

struct Running {
    url: String,
    metering: Arc<Metering>,
    client: reqwest::Client,
}

async fn proxy_with(
    endpoint: &str,
    limit: u64,
    codex: Option<Arc<dekopon_model::chatgpt::CredentialFile>>,
    ping: Duration,
) -> Running {
    let metering = metering(limit);
    let guests = HashMap::from([(
        SUBJECT.to_owned(),
        Grant {
            agent: "gylmar".parse().unwrap(),
            models: BTreeSet::from([
                "claude-opus".to_owned(),
                "glm-flash".to_owned(),
                "astra".to_owned(),
            ]),
        },
    )]);
    let proxy = ModelProxy::new(models(endpoint, codex), guests, Arc::clone(&metering))
        .unwrap()
        .with_timing(ping, Duration::from_secs(30));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = Arc::new(proxy).router();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Running {
        url,
        metering,
        client: reqwest::Client::new(),
    }
}

async fn proxy(endpoint: &str, limit: u64) -> Running {
    proxy_with(endpoint, limit, None, Duration::from_secs(20)).await
}

impl Running {
    fn post(&self, path: &str, body: impl Into<reqwest::Body>) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{path}", self.url))
            .header(SUBJECT_HEADER, SUBJECT)
            .header("content-type", "application/json")
            .body(body)
    }
}

#[derive(Clone, Default)]
struct MeterRecords(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for MeterRecords {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{}={value:?} ", field.name()));
            }
        }
        if event.metadata().target() == "meter" {
            let mut fields = Fields(String::new());
            event.record(&mut fields);
            self.0.lock().push(fields.0);
        }
    }
}

fn capture() -> (MeterRecords, tracing::subscriber::DefaultGuard) {
    use tracing_subscriber::layer::SubscriberExt as _;
    let records = MeterRecords::default();
    let guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(records.clone()));
    (records, guard)
}

type Fields = std::collections::BTreeMap<String, String>;

#[derive(Clone, Default)]
struct CallSpans(Arc<Mutex<HashMap<tracing::span::Id, Fields>>>);

struct SpanFields<'a>(&'a mut Fields);

impl tracing::field::Visit for SpanFields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CallSpans {
    fn on_new_span(
        &self,
        attributes: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if attributes.metadata().name() == "model.proxy.call" {
            let mut fields = Fields::new();
            attributes.record(&mut SpanFields(&mut fields));
            self.0.lock().insert(id.clone(), fields);
        }
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if let Some(fields) = self.0.lock().get_mut(id) {
            values.record(&mut SpanFields(fields));
        }
    }
}

impl CallSpans {
    fn only(&self) -> Fields {
        let spans = self.0.lock();
        assert_eq!(spans.len(), 1, "{spans:?}");
        spans.values().next().unwrap().clone()
    }
}

fn capture_spans() -> (CallSpans, tracing::subscriber::DefaultGuard) {
    use tracing_subscriber::layer::SubscriberExt as _;
    let spans = CallSpans::default();
    let guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(spans.clone()));
    (spans, guard)
}

const USAGE_ANSWER: &str = r#"{"id":"x","usage":{"prompt_tokens":5,"completion_tokens":2}}"#;
const CHAT_CALL: &str = r#"{"model":"glm-flash","messages":[],"stream":false}"#;

#[tokio::test]
async fn a_proxied_call_records_subject_session_model_and_outcome() {
    let (spans, _guard) = capture_spans();
    let upstream = FakeUpstream::start(vec![json("200 OK", USAGE_ANSWER)]).await;
    let running = proxy(&upstream.url, 100_000).await;
    let response = running
        .post("/v1/chat/completions", CHAT_CALL)
        .header(SESSION_HEADER, " vm-session-7 ")
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), USAGE_ANSWER);
    assert!(!upstream.request().contains("vm-session-7"));
    let expected = [
        ("vm.subject", SUBJECT),
        ("vm.session", "vm-session-7"),
        ("agent", "gylmar"),
        ("model.name", "glm-flash"),
        ("usage.input_tokens", "5"),
        ("usage.output_tokens", "2"),
        ("outcome", "succeeded"),
    ]
    .map(|(name, value)| (name.to_owned(), value.to_owned()));
    assert_eq!(spans.only(), Fields::from(expected));
}

#[tokio::test]
async fn a_call_without_a_session_header_still_records_its_subject() {
    let (spans, _guard) = capture_spans();
    let upstream = FakeUpstream::start(vec![json("200 OK", USAGE_ANSWER)]).await;
    let running = proxy(&upstream.url, 100_000).await;
    let response = running.post("/v1/chat/completions", CHAT_CALL).send().await;
    assert_eq!(response.unwrap().text().await.unwrap(), USAGE_ANSWER);
    let span = spans.only();
    assert_eq!(span["vm.subject"], SUBJECT);
    assert_eq!(span["vm.session"], "");
}

#[tokio::test]
async fn an_oversized_session_header_is_recorded_empty() {
    let (spans, _guard) = capture_spans();
    let upstream = FakeUpstream::start(vec![json("200 OK", USAGE_ANSWER)]).await;
    let running = proxy(&upstream.url, 100_000).await;
    let response = running
        .post("/v1/chat/completions", CHAT_CALL)
        .header(SESSION_HEADER, "s".repeat(129))
        .send()
        .await;
    assert_eq!(response.unwrap().status(), 200);
    let span = spans.only();
    assert_eq!(span["vm.session"], "");
    assert_eq!(span["outcome"], "succeeded");
}

#[tokio::test]
async fn a_refused_call_records_its_outcome_on_the_span() {
    let (spans, _guard) = capture_spans();
    let running = proxy("http://127.0.0.1:9", 100_000).await;
    let response = running
        .post("/v1/chat/completions", r#"{"model":"claude-opus"}"#)
        .header(SESSION_HEADER, "vm-session-7")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    let expected = [
        ("vm.subject", SUBJECT),
        ("vm.session", "vm-session-7"),
        ("agent", "gylmar"),
        ("usage.input_tokens", "0"),
        ("usage.output_tokens", "0"),
        ("outcome", "refused"),
    ]
    .map(|(name, value)| (name.to_owned(), value.to_owned()));
    assert_eq!(spans.only(), Fields::from(expected));
}

#[tokio::test]
async fn a_forwarded_request_changes_only_the_model_and_the_credential() {
    let upstream = FakeUpstream::start(vec![json(
        "200 OK",
        r#"{"id":"x","usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
    )])
    .await;
    let running = proxy(&upstream.url, 100_000).await;
    let body = "{\"model\": \"glm-flash\",\n \"messages\":[{\"role\":\"user\",\"content\":\"hi \\u00e9\"}], \"stream\":false}";
    let response = running
        .post("/v1/chat/completions", body)
        .header("authorization", "Bearer guest-token")
        .header("x-api-key", "guest-key")
        .header("x-request-id", "req-7")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"id":"x","usage":{"prompt_tokens":5,"completion_tokens":2}}"#
    );
    assert_eq!(
        upstream.body(),
        "{\"model\": \"z-ai/glm-4.5-air\",\n \"messages\":[{\"role\":\"user\",\"content\":\"hi \\u00e9\"}], \"stream\":false}"
    );
    let request = upstream.request().to_ascii_lowercase();
    assert!(
        request.starts_with("post /v1/chat/completions "),
        "{request}"
    );
    assert!(
        request.contains("authorization: bearer sk-or-proxy"),
        "{request}"
    );
    assert!(request.contains("x-request-id: req-7"), "{request}");
    assert!(
        !request.contains("guest-token") && !request.contains("guest-key"),
        "{request}"
    );
    assert!(!request.contains(SUBJECT), "{request}");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(used(&running.metering), 7);
}

#[tokio::test]
async fn a_codex_request_is_forced_unstored_and_must_stream() {
    let directory = tempfile::TempDir::new().unwrap();
    let path = directory.path().join("auth.json");
    std::fs::write(&path, r#"{"version":1,"access":"synthetic-access","refresh":"synthetic","expiresAt":18446744073709551615,"accountId":"acct-1"}"#).unwrap();
    let credential = Arc::new(
        dekopon_model::chatgpt::CredentialFile::open(&path, Duration::from_secs(2)).unwrap(),
    );
    let mut steps = vec![Step::Write(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n".to_vec(),
    )];
    steps.push(Step::Write(CODEX_RESPONSES_TWO_DELTAS.as_bytes().to_vec()));
    let upstream = FakeUpstream::start(steps).await;
    let running = proxy_with(
        &upstream.url,
        100_000,
        Some(credential),
        Duration::from_secs(20),
    )
    .await;

    let refused = running
        .post("/v1/responses", r#"{"model":"astra","input":[]}"#)
        .send()
        .await
        .unwrap();
    let message = sandbox_refusal(refused, "/v1/responses", 400, "invalid_request").await;
    assert!(message.contains("Set `stream` to true"), "{message}");

    let streamed = running
        .post(
            "/v1/responses",
            r#"{"model":"astra","store":true,"stream":true,"input":[]}"#,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(streamed.status(), 200);
    assert_eq!(streamed.text().await.unwrap(), CODEX_RESPONSES_TWO_DELTAS);
    assert_eq!(
        upstream.body(),
        r#"{"model":"gpt-5-codex","store":false,"stream":true,"input":[]}"#
    );
    let request = upstream.request().to_ascii_lowercase();
    assert!(
        request.contains("authorization: bearer synthetic-access"),
        "{request}"
    );
    assert!(request.contains("chatgpt-account-id: acct-1"), "{request}");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(used(&running.metering), 150);
}

fn refusal_shape<'a>(dialect_path: &str, body: &'a serde_json::Value) -> Option<&'a str> {
    match dialect_path {
        "/v1/messages" => {
            assert_eq!(body["type"], "error");
            body["error"]["type"].as_str()
        }
        _ => body["error"]["code"].as_str(),
    }
}

#[tokio::test]
async fn a_refusal_is_each_dialects_throttling_error_with_retry_after() {
    let running = proxy("http://127.0.0.1:9", 2_000).await;
    let spent = running
        .metering
        .admit(
            dekopon_model_token_governor::Call {
                agent: "gylmar".parse().unwrap(),
                model: "glm-flash".to_owned(),
                backend: "openrouter",
                via: dekopon_model_token_governor::Via::Agent,
            },
            dekopon_model_token_governor::Estimate {
                input: Tokens(1_990),
                output_reserve: Tokens(0),
            },
        )
        .unwrap();
    spent.settle(dekopon_model_token_governor::Outcome::Failed);
    for (path, model, expected) in [
        ("/v1/messages", "claude-opus", "rate_limit_error"),
        ("/v1/chat/completions", "glm-flash", "rate_limit_exceeded"),
    ] {
        let response = running
            .post(
                path,
                format!(r#"{{"model":"{model}","stream":true,"messages":[]}}"#),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 429, "{path}");
        let retry: u64 = response.headers()["retry-after"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(retry > 60, "{path}: {retry}");
        let body: serde_json::Value = json_body(response).await;
        assert_eq!(refusal_shape(path, &body), Some(expected), "{body}");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(
            message.starts_with("dekopon sandbox: The agent gylmar is at 99% of its token budget"),
            "{message}"
        );
    }
    let huge = format!(
        r#"{{"model":"claude-opus","messages":[{{"role":"user","content":"{}"}}]}}"#,
        "x".repeat(20_000)
    );
    let response = running.post("/v1/messages", huge).send().await.unwrap();
    assert_eq!(response.status(), 400);
    assert!(response.headers().get("retry-after").is_none());
    let body: serde_json::Value = json_body(response).await;
    assert_eq!(body["error"]["type"], "invalid_request_error");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.starts_with("dekopon sandbox: "), "{message}");
    assert!(!message.contains("prompt is too long"), "{message}");
    assert!(message.contains("can't run as is"), "{message}");
}

/// Asserts the dialect's error shape, the status and the sandbox prefix, and returns the message.
async fn sandbox_refusal(
    response: reqwest::Response,
    path: &str,
    status: u16,
    kind: &str,
) -> String {
    assert_eq!(response.status(), status, "{path}");
    let body = json_body(response).await;
    if path.starts_with("/v1/messages") {
        assert_eq!(body["type"], "error", "{body}");
        assert_eq!(body["error"]["type"], kind, "{body}");
    } else {
        assert_eq!(body["error"]["code"], kind, "{body}");
        assert!(body["error"]["type"].is_string(), "{body}");
        assert!(body["error"]["param"].is_null(), "{body}");
    }
    let message = body["error"]["message"].as_str().unwrap().to_owned();
    assert!(message.starts_with("dekopon sandbox: "), "{message}");
    message
}

#[tokio::test]
async fn an_unknown_subject_and_an_ungranted_model_are_sandbox_permission_errors() {
    let running = proxy("http://127.0.0.1:9", 100_000).await;
    let response = running
        .client
        .post(format!("{}/v1/messages", running.url))
        .header(SUBJECT_HEADER, "dekopon:stranger-vm")
        .body(r#"{"model":"claude-opus"}"#)
        .send()
        .await
        .unwrap();
    let message = sandbox_refusal(response, "/v1/messages", 403, "permission_error").await;
    assert!(message.contains("granted no model"), "{message}");
    let long = "terra".repeat(10_000);
    for (path, model, kind, expected) in [
        (
            "/v1/messages",
            long.as_str(),
            "permission_error",
            "configured models: `claude-opus`. Set the model to one of them.",
        ),
        (
            "/v1/messages",
            "glm-flash",
            "permission_error",
            "configured models: `claude-opus`.",
        ),
        (
            "/v1/chat/completions",
            "claude-opus",
            "permission_denied",
            "configured models: `glm-flash`.",
        ),
        (
            "/v1/responses",
            "astra",
            "permission_denied",
            "none is served on this path: `claude-opus` on `/v1/messages`, `glm-flash` on `/v1/chat/completions`.",
        ),
    ] {
        let response = running
            .post(path, format!(r#"{{"model":"{model}"}}"#))
            .send()
            .await
            .unwrap();
        let message = sandbox_refusal(response, path, 403, kind).await;
        assert!(message.contains(expected), "{message}");
        assert!(!message.contains("terra"), "{message}");
    }
}

#[tokio::test]
async fn every_malformed_request_is_a_sandbox_invalid_request() {
    let running = proxy("http://127.0.0.1:9", 100_000).await;
    for (path, body, kind, expected) in [
        (
            "/v1/messages",
            "{",
            "invalid_request_error",
            "must be a JSON object",
        ),
        (
            "/v1/chat/completions",
            "[1]",
            "invalid_request",
            "must be a JSON object",
        ),
        (
            "/v1/messages",
            r#"{"messages":[]}"#,
            "invalid_request_error",
            "must name a model. Set `model` to one of this agent's configured models: `astra`, `claude-opus`, `glm-flash`.",
        ),
        (
            "/v1/chat/completions",
            r#"{"model":"glm-flash","stream":true,"stream":false}"#,
            "invalid_request",
            "repeats the top-level key `stream`; send it once.",
        ),
        (
            "/v1/chat/completions",
            r#"{"model":"glm-flash","models":["x/y"]}"#,
            "invalid_request",
            "OpenRouter fallback routing (`models` / `route`) is not allowed",
        ),
        (
            "/v1/chat/completions",
            r#"{"model":"glm-flash","route":"fallback"}"#,
            "invalid_request",
            "Remove the field.",
        ),
    ] {
        let response = running.post(path, body).send().await.unwrap();
        let message = sandbox_refusal(response, path, 400, kind).await;
        assert!(message.contains(expected), "{message}");
        assert!(
            !message.contains("x/y") && !message.contains("fallback\""),
            "{message}"
        );
    }
}

#[tokio::test]
async fn a_body_over_the_cap_is_refused_with_413() {
    let running = proxy("http://127.0.0.1:9", 100_000).await;
    let response = running
        .post(
            "/v1/chat/completions",
            vec![b' '; MAX_BODY_BYTES + 1024 * 1024],
        )
        .send()
        .await
        .unwrap();
    let message = sandbox_refusal(response, "/v1/chat/completions", 413, "request_too_large").await;
    assert!(message.contains("at most 8 MiB"), "{message}");
}

#[test]
fn a_cap_sized_body_of_deeply_nested_arrays_is_peeked() {
    let nested = format!("{}{}", "[".repeat(120), "]".repeat(120));
    let mut body = String::from(r#"{"model":"astra","messages":["#);
    while body.len() + nested.len() + 64 < MAX_BODY_BYTES {
        body.push_str(&nested);
        body.push(',');
    }
    body.push_str(r#"{"type":"image"}]}"#);
    let peek = dekopon_model::wire::RequestPeek::of(body.as_bytes()).unwrap();
    assert_eq!(peek.model, "astra");
    assert_eq!(peek.images, 1);
}

#[tokio::test]
async fn an_upstream_error_status_is_charged_nothing() {
    let (records, _guard) = capture();
    let overloaded =
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
    let upstream = FakeUpstream::start(vec![Step::Write(
        format!(
            "HTTP/1.1 529 Overloaded\r\ncontent-type: application/json\r\nretry-after: 7\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{overloaded}",
            overloaded.len()
        )
        .into_bytes(),
    )])
    .await;
    let running = proxy(&upstream.url, 100_000).await;
    let response = running
        .post(
            "/v1/messages",
            r#"{"model":"claude-opus","stream":true,"messages":[]}"#,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 529);
    assert_eq!(response.headers()["retry-after"], "7");
    assert_eq!(response.text().await.unwrap(), overloaded);
    assert_eq!(used(&running.metering), 0);
    let records = records.0.lock().clone();
    assert_eq!(records.len(), 1, "{records:?}");
    for field in [
        "outcome=\"failed\"",
        "usage.input_tokens=0 ",
        "usage.output_tokens=0 ",
    ] {
        assert!(records[0].contains(field), "{field}: {records:?}");
    }
}

#[tokio::test]
async fn count_tokens_is_forwarded_and_never_charged() {
    let (records, _guard) = capture();
    let upstream = FakeUpstream::start(vec![json("200 OK", r#"{"input_tokens":2095}"#)]).await;
    let running = proxy(&upstream.url, 100_000).await;
    let response = running
        .post(
            "/v1/messages/count_tokens",
            r#"{"model":"claude-opus","messages":[]}"#,
        )
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), r#"{"input_tokens":2095}"#);
    let request = upstream.request().to_ascii_lowercase();
    assert!(
        request.starts_with("post /v1/messages/count_tokens "),
        "{request}"
    );
    assert!(request.contains("x-api-key: sk-ant-proxy"), "{request}");
    assert!(
        request.contains("anthropic-version: 2023-06-01"),
        "{request}"
    );
    assert_eq!(used(&running.metering), 0);
    assert!(records.0.lock().is_empty());
}

#[tokio::test]
async fn a_silent_upstream_stream_carries_pings() {
    let upstream = FakeUpstream::start(vec![
        sse_head(),
        chunk(
            OPENAI_CHAT_COMPLETIONS_TWO_DELTAS
                .split_inclusive("\n\n")
                .next()
                .unwrap()
                .as_bytes(),
        ),
        Step::Wait(Duration::from_millis(400)),
        chunk(
            OPENAI_CHAT_COMPLETIONS_TWO_DELTAS
                .split_once("\n\n")
                .unwrap()
                .1
                .as_bytes(),
        ),
        end(),
    ])
    .await;
    let running = proxy_with(&upstream.url, 100_000, None, Duration::from_millis(100)).await;
    let text = running
        .post(
            "/v1/chat/completions",
            r#"{"model":"glm-flash","stream":true,"messages":[]}"#,
        )
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(text.matches(": ping\n\n").count() >= 2, "{text}");
    assert_eq!(
        text.replace(": ping\n\n", ""),
        OPENAI_CHAT_COMPLETIONS_TWO_DELTAS
    );
}

#[tokio::test]
async fn a_client_disconnect_mid_stream_charges_what_was_observed_once() {
    let (records, _guard) = capture();
    let first = OPENAI_CHAT_COMPLETIONS_TWO_DELTAS
        .split_inclusive("\n\n")
        .take(3)
        .collect::<String>();
    let upstream = FakeUpstream::start(vec![
        sse_head(),
        chunk(first.as_bytes()),
        Step::Wait(Duration::from_secs(10)),
    ])
    .await;
    let running = proxy_with(&upstream.url, 100_000, None, Duration::from_millis(50)).await;
    let body = r#"{"model":"glm-flash","stream":true,"messages":[]}"#;
    let mut response = running
        .post("/v1/chat/completions", body)
        .send()
        .await
        .unwrap();
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains("hello.") {
        seen.extend_from_slice(&response.chunk().await.unwrap().unwrap());
    }
    drop(response);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while records.0.lock().is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let records = records.0.lock().clone();
    assert_eq!(records.len(), 1, "{records:?}");
    assert!(records[0].contains("outcome=\"cancelled\""), "{records:?}");
    assert!(records[0].contains("meter.via=\"proxy\""), "{records:?}");
    let input = Tokens::from_bytes(body.replace("glm-flash", "z-ai/glm-4.5-air").len()).0;
    assert_eq!(
        used(&running.metering),
        input + Tokens::from_bytes("Echoed hello.".len()).0
    );
}

async fn json_body(response: reqwest::Response) -> serde_json::Value {
    serde_json::from_slice(&response.bytes().await.unwrap()).unwrap()
}
