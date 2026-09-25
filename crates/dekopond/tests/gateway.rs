#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "tests spawn, join and drain freely"
)]
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::{
    fs,
    io::{BufRead as _, BufReader, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    os::unix::{fs::PermissionsExt as _, net::UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use dekopon_test_support::{CaptureLayer, Record};
use serde_json::{Value, json};
use tokio::sync::{MutexGuard, oneshot};
use tracing_subscriber::layer::SubscriberExt as _;

const MAPPED_SUBJECT: &str = "tel.16034700182";
const OTHER_MAPPED_SUBJECT: &str = "tel.16035550100";
const UNMAPPED_SUBJECT: &str = "tel.19999999999";
const MAPPED_PRINCIPAL: &str = "cpetersen";
const OTHER_MAPPED_PRINCIPAL: &str = "jortega";
const GATEWAY_PRINCIPAL: &str = "dekopond-gateway";
const AGENT: &str = "chat-agent";

fn provider(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(format!("examples/providers/{name}-provider.wasm"))
}

fn temporary() -> tempfile::TempDir {
    let parent = std::env::temp_dir()
        .canonicalize()
        .expect("canonical temporary parent");
    let directory = tempfile::Builder::new()
        .tempdir_in(parent)
        .expect("temporary directory");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("private temporary directory");
    directory
}

fn write_owner_only(path: &Path, contents: &[u8]) {
    fs::write(path, contents).expect("fixture writes");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("fixture is owner-only");
}

fn broker_policies() -> String {
    [
        (MAPPED_PRINCIPAL, "first"),
        (OTHER_MAPPED_PRINCIPAL, "second"),
    ]
    .into_iter()
    .map(|(principal, suffix)| {
        format!(
            r#"
@id("chat-agent-session-{suffix}")
permit(principal == Dekopon::Principal::"{principal}",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"{AGENT}")
when {{ context has via && context.via == "{GATEWAY_PRINCIPAL}" }};

@id("chat-agent-probe-{suffix}")
permit(principal == Dekopon::Principal::"{principal}",
       action == Dekopon::Action::"cli-probe.upper",
       resource == Dekopon::Provider::"cli-probe")
when {{ context has via && context.via == "{GATEWAY_PRINCIPAL}"
     && context has agent && context.agent == "{AGENT}" }};

@id("chat-agent-memory-{suffix}")
permit(principal == Dekopon::Principal::"{principal}",
       action in [Dekopon::Action::"memory.chat.record",
                  Dekopon::Action::"memory.chat.recent",
                  Dekopon::Action::"memory.chat.search"],
       resource == Dekopon::Provider::"memory-chat")
when {{ context has via && context.via == "{GATEWAY_PRINCIPAL}"
     && context has agent && context.agent == "{AGENT}"
     && context has transportKind && context.transportKind == "local"
     && context has transport && context.transport == "dev"
     && context has conversation && context.conversation.id == "dev" }};
"#
        )
    })
    .collect()
}

fn broker_config(directory: &Path, uid: u32) -> Value {
    let mut storage = serde_json::to_value(dekopon_storage_host::StorageLimits::default())
        .expect("storage limits serialize");
    let fields = storage.as_object_mut().expect("storage limits object");
    fields.insert(
        "rootPath".to_owned(),
        json!(directory.join("provider-storage")),
    );
    json!({
        "apiVersion": dekopon_brokerd::CONFIG_API_VERSION,
        "socketPath": directory.join("broker.sock"),
        "brokerPrincipal": "broker-test",
        "policyRevision": "policy-gateway",
        "policiesPath": directory.join("policies.cedar"),
        "providers": [provider("cli-probe"), provider("memory-chat")],
        "identities": [{
            "uid": uid,
            "principal": GATEWAY_PRINCIPAL,
            "actor": {"type": "service", "principal": GATEWAY_PRINCIPAL},
            "attestor": {"namespaces": ["tel"]}
        }],
        "principals": {
            MAPPED_PRINCIPAL: {"subjects": [MAPPED_SUBJECT]},
            OTHER_MAPPED_PRINCIPAL: {"subjects": [OTHER_MAPPED_SUBJECT]}
        },
        "constraintSets": {
            "cli-probe.upper": {
                "provider": "cli-probe", "effect": "read-only", "risk": "Low",
                "constraints": {"timeoutMs": 30_000, "maxOutputBytes": 1_048_576}
            },
            "memory.chat.record": {
                "route": "chatMemoryRecord",
                "provider": "memory-chat", "effect": "local-write", "risk": "Medium",
                "constraints": {
                    "timeoutMs": 30_000, "maxOutputBytes": 131_072,
                    "storage": {"interface":"jsonl","access":"read-write","namespace":"chat"}
                }
            },
            "memory.chat.recent": {
                "route": "chatMemoryRecent",
                "provider": "memory-chat", "effect": "read-only", "risk": "High",
                "constraints": {
                    "timeoutMs": 30_000, "maxOutputBytes": 131_072,
                    "storage": {"interface":"jsonl","access":"read-only","namespace":"chat"}
                }
            },
            "memory.chat.search": {
                "route": "chatMemorySearch",
                "provider": "memory-chat", "effect": "read-only", "risk": "High",
                "constraints": {
                    "timeoutMs": 30_000, "maxOutputBytes": 131_072,
                    "storage": {"interface":"jsonl","access":"read-only","namespace":"chat"}
                }
            }
        },
        "storage": storage,
        "chatMemory": {
            "continuityPolicy": "authority-bound",
            "enabledAgents": [AGENT],
            "maxLookbackTurns": 200,
            "maxRecentTurns": 20,
            "maxSearchResults": 20,
            "maxQueryBytes": 256,
            "maxResultBytes": 65_536,
            "maxTurnBytes": 32_768,
            "maxDedupRecords": 16_000,
            "maxDedupBytes": 4_194_304,
            "compactionTargetBytes": 8_388_608,
            "compactionThresholdBytes": 12_582_912
        }
    })
}

fn catalog_text() -> String {
    format!(
        "apiVersion: dekopon.dev/v1alpha1\n\
         kind: Agent\n\
         metadata:\n  name: {AGENT}\n\
         spec:\n  \
         description: Answers chat questions under broker authority\n  \
         enabled: true\n  \
         instructions: Answer in one short sentence. You have no authority of your own.\n  \
         modelClass: reasoning\n"
    )
}

fn gateway_config_with(
    directory: &Path,
    uid: u32,
    model_endpoint: &str,
    conversation_scope: Option<&str>,
    progress_detail: &str,
    stream: bool,
) -> Value {
    let mut config = json!({
        "apiVersion": dekopond::CONFIG_API_VERSION,
        "catalogPath": directory.join("dekopon.yaml"),
        "broker": {
            "socketPath": directory.join("broker.sock"),
            "serverUid": uid
        },
        "transports": [
            {
                "name": "dev",
                "kind": "local",
                "socketPath": directory.join("dev.sock"),
                "liveness": {
                    "mode": "native",
                    "progress": "message",
                    "stream": stream,
                    "cancelButton": progress_detail != "off" || stream,
                    "keepAlive": {"atSeconds": [15, 45], "everySeconds": 60, "max": 10}
                }
            }
        ],
        "stopWords": ["stop", "cancel"],
        "models": [{
            "name": "mock",
            "kind": "openaiCompatible",
            "endpoint": model_endpoint,
            "model": "test-model",
            "timeoutMs": 30_000,
            "classes": ["reasoning"]
        }],
        "routes": [{
            "transport": "dev",
            "conversation": {"kind": ["directMessage"]},
            "agent": AGENT,
            "limits": {"maxSteps": 4, "maxCapabilityCalls": 4},
            "progressDetail": progress_detail,
            "memory": {"mode": "persistent"}
        }],
        "sessions": {"maxConcurrent": 2},
        "shutdownGraceMs": 30_000
    });
    if let Some(scope) = conversation_scope {
        config["routes"][0]["conversation"] = json!({"kind": "any"});
        config["routes"][0]["memory"]["scope"] = json!(scope);
    }
    config
}

fn bash_tool_call(id: &str, script: &str) -> Value {
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": "bash",
                        "arguments": json!({ "script": script }).to_string()
                    }
                }]
            }
        }]
    })
}

fn final_answer(text: &str) -> Value {
    json!({
        "choices": [{
            "message": { "role": "assistant", "content": text, "tool_calls": [] }
        }]
    })
}

fn spawn_model(
    responses: Vec<Value>,
    first_answer_delay: Duration,
    first_answer_hold: Option<ModelHold>,
) -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("mock model binds");
    let address = listener.local_addr().expect("mock model address");
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&requests);
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&bodies);
    thread::spawn(move || {
        for (index, response) in responses.into_iter().enumerate() {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            counted.fetch_add(1, Ordering::SeqCst);
            let Some(request) = read_request(&mut stream) else {
                return;
            };
            recorded
                .lock()
                .expect("recorded model requests")
                .push(request);
            if index == 0 {
                thread::sleep(first_answer_delay);
                if let Some(hold) = &first_answer_hold {
                    hold.wait();
                }
            }
            respond(stream, &response);
        }
    });
    (format!("http://{address}/v1"), requests, bodies)
}

fn read_request(stream: &mut TcpStream) -> Option<Value> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("mock read timeout configures");
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    let header_end = loop {
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).ok()?;
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while bytes.len() - header_end < content_length {
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    serde_json::from_slice(&bytes[header_end..]).ok()
}

fn respond(mut stream: TcpStream, body: &Value) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"
    )
    .expect("response headers write");
    for chunk in chunks(body) {
        write!(stream, "data: {chunk}\n\n").expect("chunk writes");
        stream.flush().expect("chunk flushes");
    }
    stream
        .write_all(b"data: [DONE]\n\n")
        .expect("sentinel writes");
    stream.flush().expect("response flushes");
}

fn chunks(body: &Value) -> Vec<Value> {
    let message = &body["choices"][0]["message"];
    // The initial role chunk carries content null, which an accumulator must not treat as an empty
    // answer; usage stays null on every chunk too.
    let mut chunks = vec![delta_chunk(
        json!({"role": "assistant", "content": null}),
        None,
    )];
    if let Some(text) = message["content"].as_str() {
        let split = text
            .char_indices()
            .nth(text.chars().count() / 2)
            .map_or(text.len(), |(index, _)| index);
        chunks.push(delta_chunk(json!({"content": &text[..split]}), None));
        chunks.push(delta_chunk(json!({"content": &text[split..]}), None));
        chunks.push(delta_chunk(json!({}), Some("stop")));
    }
    for (index, call) in message["tool_calls"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        chunks.push(delta_chunk(
            json!({"tool_calls": [{
                "index": index,
                "id": call["id"],
                "type": "function",
                "function": {"name": call["function"]["name"], "arguments": ""}
            }]}),
            None,
        ));
        chunks.push(delta_chunk(
            json!({"tool_calls": [{
                "index": index,
                "function": {"arguments": call["function"]["arguments"]}
            }]}),
            None,
        ));
        chunks.push(delta_chunk(json!({}), Some("tool_calls")));
    }
    chunks.push(json!({
        "id": "chatcmpl-gateway",
        "object": "chat.completion.chunk",
        "choices": [],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    }));
    chunks
}

fn delta_chunk(delta: Value, finish_reason: Option<&str>) -> Value {
    json!({
        "id": "chatcmpl-gateway",
        "object": "chat.completion.chunk",
        "model": "test-model",
        "usage": null,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]
    })
}

async fn wait_for_socket<T: std::fmt::Debug>(path: &Path, task: &mut tokio::task::JoinHandle<T>) {
    for _ in 0..3_000 {
        if fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o077 == 0)
        {
            return;
        }
        if task.is_finished() {
            panic!(
                "fixture exited before binding {}: {:?}",
                path.display(),
                task.await
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("socket at {} did not become owner-only", path.display());
}

fn ask(socket: &Path, subject: &str, text: &str) -> String {
    let lines = ask_lines(socket, subject, text);
    lines
        .last()
        .and_then(|line| line["reply"].as_str())
        .expect("the reply carries text")
        .to_owned()
}

fn ask_when_idle(socket: &Path, subject: &str, text: &str) -> String {
    for _ in 0..3_000 {
        let reply = ask(socket, subject, text);
        if reply != "I'm busy — try again shortly." {
            return reply;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the prior gateway session did not release admission within thirty seconds");
}

struct Audit {
    capture: &'static CaptureLayer,
    _exclusive: MutexGuard<'static, ()>,
}

impl Audit {
    async fn exclusive() -> Self {
        static EXCLUSIVE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        static CAPTURE: OnceLock<CaptureLayer> = OnceLock::new();
        let exclusive = EXCLUSIVE.lock().await;
        let capture = CAPTURE.get_or_init(|| {
            let capture = CaptureLayer::workspace();
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry().with(capture.clone()),
            )
            .expect("this test binary installs its dispatcher once");
            capture
        });
        capture.clear();
        Self {
            capture,
            _exclusive: exclusive,
        }
    }

    fn records(&self) -> Vec<String> {
        self.capture
            .events()
            .into_iter()
            .map(|(fields, _)| fields)
            .collect()
    }

    /// The gateway's own receipt record carries the inbound text by design, but broker records must
    /// never contain any of it.
    fn broker_records(&self) -> Vec<String> {
        self.capture
            .records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Event { target, fields, .. } if target.starts_with("dekopon_broker") => {
                    Some(fields)
                }
                Record::Event { .. } | Record::Span { .. } => None,
            })
            .collect()
    }

    fn progress_records(&self) -> Vec<(String, Vec<&'static str>)> {
        self.capture
            .records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Event { fields, scope, .. }
                    if has(&fields, "audit.event", "\"gateway.progress\"") =>
                {
                    Some((fields, scope))
                }
                Record::Event { .. } | Record::Span { .. } => None,
            })
            .collect()
    }

    fn find(&self, event: &str, fields: &[(&str, &str)]) -> Option<String> {
        let event = format!("\"{event}\"");
        self.records().into_iter().find(|record| {
            has(record, "audit.event", &event)
                && fields
                    .iter()
                    .all(|(field, value)| has(record, field, value))
        })
    }

    async fn wait_for_gateway_event(&self, event: &str) -> bool {
        let rendered = format!("\"{event}\"");
        for _ in 0..3_000 {
            if self
                .records()
                .iter()
                .any(|record| has(record, "event", &rendered))
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    async fn wait_for_memory_record(&self) {
        for _ in 0..3_000 {
            if self
                .find(
                    "broker.execution",
                    &[
                        ("capability.id", "memory.chat.record"),
                        ("outcome", "Succeeded"),
                    ],
                )
                .is_some()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the post-acceptance memory record did not complete within thirty seconds");
    }
}

fn has(record: &str, field: &str, value: &str) -> bool {
    let rendered = format!(" {field}={value}");
    record.match_indices(&rendered).any(|(start, _)| {
        record[start + rendered.len()..]
            .chars()
            .next()
            .is_none_or(|next| next == ' ')
    })
}

fn field<'a>(record: &'a str, name: &str) -> Option<&'a str> {
    let rendered = format!(" {name}=");
    let start = record.find(&rendered)? + rendered.len();
    let rest = &record[start..];
    let end = rest.find(' ').unwrap_or(rest.len());
    Some(rest[..end].trim_matches('"'))
}

fn scripted_deltas(responses: &[Value]) -> usize {
    responses
        .iter()
        .flat_map(chunks)
        .filter(|chunk| {
            chunk["choices"][0]["delta"]["content"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        })
        .count()
}

struct Fixture {
    directory: tempfile::TempDir,
    broker: tokio::task::JoinHandle<Result<(), dekopon_brokerd::BrokerdError>>,
    stop_broker: oneshot::Sender<()>,
    gateway: tokio::task::JoinHandle<Result<(), dekopond::DekopondError>>,
    stop_gateway: oneshot::Sender<()>,
    model_requests: Arc<AtomicUsize>,
    model_prompts: Arc<Mutex<Vec<Value>>>,
}

impl Fixture {
    fn socket(&self) -> PathBuf {
        self.directory.path().join("dev.sock")
    }

    fn prompt(&self, index: usize) -> Vec<(String, String)> {
        let prompts = self.model_prompts.lock().expect("recorded model requests");
        let request = prompts
            .get(index)
            .unwrap_or_else(|| panic!("the model received at least {} requests", index + 1));
        request["messages"]
            .as_array()
            .expect("a chat-completions request carries messages")
            .iter()
            .map(|message| {
                (
                    message["role"].as_str().unwrap_or_default().to_owned(),
                    message["content"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect()
    }

    #[allow(
        clippy::let_underscore_must_use,
        reason = "a shutdown oneshot fails only when the daemon already exited, and the join \
                  below is what decides whether it exited cleanly"
    )]
    async fn shutdown(self) -> tempfile::TempDir {
        let _ = self.stop_gateway.send(());
        self.gateway
            .await
            .expect("gateway task exits")
            .expect("gateway stops cleanly");
        let _ = self.stop_broker.send(());
        self.broker
            .await
            .expect("broker task exits")
            .expect("broker stops cleanly");
        self.directory
    }
}

async fn boot(responses: Vec<Value>) -> Fixture {
    boot_in_with_scope(temporary(), responses, None, "plain", true).await
}

async fn boot_progress(responses: Vec<Value>) -> Fixture {
    boot_in_with_scope(temporary(), responses, None, "plain", false).await
}

async fn boot_ticking(responses: Vec<Value>, keep_alive: Value, model_delay: Duration) -> Fixture {
    boot_with(
        temporary(),
        responses,
        None,
        "plain",
        false,
        &Timing {
            keep_alive: Some(keep_alive),
            model_delay,
            model_hold: None,
        },
    )
    .await
}

async fn boot_held(responses: Vec<Value>, hold: &ModelHold) -> Fixture {
    boot_with(
        temporary(),
        responses,
        None,
        "plain",
        true,
        &Timing {
            model_hold: Some(hold.clone()),
            ..Timing::default()
        },
    )
    .await
}

async fn boot_rendering(responses: Vec<Value>, progress_detail: &str) -> Fixture {
    boot_in_with_scope(temporary(), responses, None, progress_detail, false).await
}

async fn boot_shared(responses: Vec<Value>) -> Fixture {
    boot_in_with_scope(
        temporary(),
        responses,
        Some("sharedConversation"),
        "plain",
        true,
    )
    .await
}

async fn boot_in(directory: tempfile::TempDir, responses: Vec<Value>) -> Fixture {
    boot_in_with_scope(directory, responses, None, "plain", true).await
}

/// Timing this race is unreliable, since an instant answer can finish before a second caller's line
/// is written; the hold makes mid-run a fact, not a bet on scheduling.
#[derive(Clone)]
struct ModelHold(Arc<(Mutex<bool>, Condvar)>);

impl ModelHold {
    fn new() -> Self {
        Self(Arc::new((Mutex::new(false), Condvar::new())))
    }

    fn wait(&self) {
        let (released, ready) = &*self.0;
        let mut released = released.lock().expect("the model hold locks");
        while !*released {
            released = ready.wait(released).expect("the model hold locks");
        }
    }

    fn release(&self) {
        let (released, ready) = &*self.0;
        *released.lock().expect("the model hold locks") = true;
        ready.notify_all();
    }
}

#[derive(Default)]
struct Timing {
    keep_alive: Option<Value>,
    model_delay: Duration,
    model_hold: Option<ModelHold>,
}

async fn boot_in_with_scope(
    directory: tempfile::TempDir,
    responses: Vec<Value>,
    conversation_scope: Option<&str>,
    progress_detail: &str,
    stream: bool,
) -> Fixture {
    boot_with(
        directory,
        responses,
        conversation_scope,
        progress_detail,
        stream,
        &Timing::default(),
    )
    .await
}

async fn boot_with(
    directory: tempfile::TempDir,
    responses: Vec<Value>,
    conversation_scope: Option<&str>,
    progress_detail: &str,
    stream: bool,
    timing: &Timing,
) -> Fixture {
    let uid = dekopon_brokerd::current_uid();

    let broker_path = directory.path().join("broker.json");
    write_owner_only(
        &directory.path().join("policies.cedar"),
        broker_policies().as_bytes(),
    );
    write_owner_only(
        &broker_path,
        &serde_json::to_vec(&broker_config(directory.path(), uid))
            .expect("broker config serializes"),
    );
    let (stop_broker, broker_stopped) = oneshot::channel::<()>();
    let mut broker = tokio::spawn(dekopon_brokerd::run(broker_path, async move {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "a dropped sender means the fixture went away, which is the same instruction \
                      to shut down as a delivered one"
        )]
        let _ = broker_stopped.await;
    }));
    wait_for_socket(&directory.path().join("broker.sock"), &mut broker).await;

    write_owner_only(
        &directory.path().join("dekopon.yaml"),
        catalog_text().as_bytes(),
    );
    let (endpoint, model_requests, model_prompts) =
        spawn_model(responses, timing.model_delay, timing.model_hold.clone());
    let gateway_path = directory.path().join("dekopond.json");
    let mut config = gateway_config_with(
        directory.path(),
        uid,
        &endpoint,
        conversation_scope,
        progress_detail,
        stream,
    );
    if let Some(keep_alive) = &timing.keep_alive {
        config["transports"][0]["liveness"]["keepAlive"] = keep_alive.clone();
    }
    write_owner_only(
        &gateway_path,
        &serde_json::to_vec(&config).expect("gateway config serializes"),
    );
    let (stop_gateway, gateway_stopped) = oneshot::channel::<()>();
    let mut gateway = tokio::spawn(dekopond::run(gateway_path, async move {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "a dropped sender means the fixture went away, which is the same instruction \
                      to shut down as a delivered one"
        )]
        let _ = gateway_stopped.await;
    }));
    wait_for_socket(&directory.path().join("dev.sock"), &mut gateway).await;

    Fixture {
        directory,
        broker,
        stop_broker,
        gateway,
        stop_gateway,
        model_requests,
        model_prompts,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_message_reaches_a_provider_under_the_senders_own_principal() {
    let audit = Audit::exclusive().await;
    let fixture = boot(vec![
        bash_tool_call("call-1", "probe upper --text hi | jq -r .text"),
        final_answer("The capability upper-cased hi."),
    ])
    .await;

    let socket = fixture.socket();
    let reply = tokio::task::spawn_blocking(move || ask(&socket, MAPPED_SUBJECT, "say hi"))
        .await
        .expect("the request completes");
    assert_eq!(reply, "The capability upper-cased hi.");
    assert_eq!(fixture.model_requests.load(Ordering::SeqCst), 2);
    let tool_output = fixture
        .prompt(1)
        .into_iter()
        .find_map(|(role, content)| (role == "tool").then_some(content))
        .expect("second model turn carries the command output");
    assert!(
        tool_output.lines().any(|line| line == "HI"),
        "the provider ran and upper-cased its text: {tool_output}"
    );

    let _directory = fixture.shutdown().await;

    let principal = format!("{MAPPED_PRINCIPAL:?}");
    let via = format!("{GATEWAY_PRINCIPAL:?}");
    let subject = format!("{MAPPED_SUBJECT:?}");
    let agent = format!("{AGENT:?}");
    audit
        .find(
            "broker.execution",
            &[
                ("capability.id", "cli-probe.upper"),
                ("principal", &principal),
                ("via", &via),
                ("subject", &subject),
                ("actor.kind", "\"agent\""),
                ("actor.id", &agent),
                ("outcome", "Succeeded"),
            ],
        )
        .unwrap_or_else(|| {
            panic!(
                "an execution record names the sender: {:#?}",
                audit.records()
            )
        });

    audit
        .find(
            "broker.decision",
            &[
                ("decision.allowed", "true"),
                ("principal", &principal),
                ("via", &via),
            ],
        )
        .unwrap_or_else(|| panic!("a decision record exists: {:#?}", audit.records()));
    let records = audit.broker_records().concat();
    assert!(!records.contains("say hi"), "{records}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_persistent_route_answers_a_follow_up_with_the_exchange_before_it() {
    let audit = Audit::exclusive().await;
    let fixture = boot(vec![
        final_answer("Two things broke."),
        final_answer("The second one was the database."),
    ])
    .await;

    let socket = fixture.socket();
    let asked = socket.clone();
    let first = tokio::task::spawn_blocking(move || ask(&asked, MAPPED_SUBJECT, "what broke?"))
        .await
        .expect("the first request completes");
    assert_eq!(first, "Two things broke.");
    audit.wait_for_memory_record().await;
    let second = tokio::task::spawn_blocking(move || {
        ask_when_idle(&socket, MAPPED_SUBJECT, "and the second one?")
    })
    .await
    .expect("the follow-up completes");
    assert_eq!(second, "The second one was the database.");

    let follow_up = fixture.prompt(1);
    assert_eq!(
        follow_up
            .iter()
            .map(|(role, content)| (role.as_str(), content.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (
                "system",
                concat!(
                    "Answer in one short sentence. You have no authority of your own.\n\n",
                    "Durable chat memory is available on demand. Use `memory recent --last N` or ",
                    "`memory search --query TEXT`. Searches inspect at most 200 prior turns. Do not ",
                    "claim recall without retrieving it.\n\n",
                    "[Gateway assets: this reply adapter accepts any concrete syntactically valid media type (no wildcards). ",
                    "Plan a converter for other formats; attaching retains a file but only a separately authorized asset.send delivers it. ",
                    "References use chat-asset:<N>, never data URLs.]"
                )
            ),
            ("user", "what broke?"),
            ("assistant", "Two things broke."),
            ("user", "and the second one?"),
        ]
    );

    let _directory = fixture.shutdown().await;
    let records = audit.broker_records().concat();
    assert!(!records.contains("what broke"), "{records}");
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_shared_scope_replays_attributed_history_across_two_principals() {
    let audit = Audit::exclusive().await;
    let fixture = boot_shared(vec![
        final_answer("Two things broke."),
        bash_tool_call(
            "shared-participant-probe",
            "probe upper --text second-participant | jq -r .text",
        ),
        final_answer("The second one was the database."),
    ])
    .await;

    let socket = fixture.socket();
    let asked = socket.clone();
    let first = tokio::task::spawn_blocking(move || ask(&asked, MAPPED_SUBJECT, "what broke?"))
        .await
        .expect("the first participant's request completes");
    assert_eq!(first, "Two things broke.");
    audit.wait_for_memory_record().await;

    let second = tokio::task::spawn_blocking(move || {
        ask_when_idle(&socket, OTHER_MAPPED_SUBJECT, "and the second one?")
    })
    .await
    .expect("the second participant's request completes");
    assert_eq!(second, "The second one was the database.");

    let first_prompt =
        format!("[gateway: authenticated participant: {MAPPED_SUBJECT}]\nwhat broke?");
    let second_prompt = format!(
        "[gateway: authenticated participant: {OTHER_MAPPED_SUBJECT}]\nand the second one?"
    );
    assert_eq!(
        fixture.prompt(1),
        vec![
            (
                "system".to_owned(),
                concat!(
                    "Answer in one short sentence. You have no authority of your own.\n\n",
                    "Durable chat memory is available on demand. Use `memory recent --last N` or ",
                    "`memory search --query TEXT`. Searches inspect at most 200 prior turns. Do not ",
                    "claim recall without retrieving it.\n\n",
                    "[Gateway assets: this reply adapter accepts any concrete syntactically valid media type (no wildcards). ",
                    "Plan a converter for other formats; attaching retains a file but only a separately authorized asset.send delivers it. ",
                    "References use chat-asset:<N>, never data URLs.]"
                )
                .to_owned(),
            ),
            ("user".to_owned(), first_prompt),
            ("assistant".to_owned(), "Two things broke.".to_owned()),
            ("user".to_owned(), second_prompt),
        ],
        "the second authenticated principal receives one shared, provenance-labelled transcript"
    );
    let tool_output = fixture
        .prompt(2)
        .into_iter()
        .find_map(|(role, content)| (role == "tool").then_some(content))
        .expect("the second participant's follow-up turn carries the command output");
    assert!(
        tool_output.lines().any(|line| line == "SECOND-PARTICIPANT"),
        "the second participant's command ran and upper-cased its text: {tool_output}"
    );

    let _directory = fixture.shutdown().await;
    assert!(
        audit
            .find(
                "broker.decision",
                &[("principal", &format!("{OTHER_MAPPED_PRINCIPAL:?}"))]
            )
            .is_some(),
        "the second participant still receives an independent broker decision: {:#?}",
        audit.records()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn durable_recent_retrieves_the_accepted_turn_after_broker_and_gateway_restart() {
    let audit = Audit::exclusive().await;
    let fixture = boot(vec![final_answer("The retained answer.")]).await;
    let socket = fixture.socket();
    let first = tokio::task::spawn_blocking(move || ask(&socket, MAPPED_SUBJECT, "remember this"))
        .await
        .expect("first request completes");
    assert_eq!(first, "The retained answer.");
    audit.wait_for_memory_record().await;
    let directory = fixture.shutdown().await;

    let fixture = boot_in(
        directory,
        vec![
            bash_tool_call(
                "memory-1",
                "memory recent --last 1 | jq -r '.turns[0].assistant'",
            ),
            final_answer("I retrieved the retained answer."),
        ],
    )
    .await;
    let socket = fixture.socket();
    let second = tokio::task::spawn_blocking(move || {
        ask(
            &socket,
            MAPPED_SUBJECT,
            "retrieve the prior accepted answer",
        )
    })
    .await
    .expect("post-restart request completes");
    assert_eq!(second, "I retrieved the retained answer.");
    assert_eq!(fixture.model_requests.load(Ordering::SeqCst), 2);
    let tool_output = fixture
        .prompt(1)
        .into_iter()
        .find_map(|(role, content)| (role == "tool").then_some(content))
        .expect("second model turn carries memory command output");
    assert!(
        tool_output.contains("The retained answer."),
        "{tool_output}"
    );
    let _directory = fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unmapped_subject_is_refused_before_a_model_is_ever_asked() {
    let audit = Audit::exclusive().await;
    let fixture = boot(vec![final_answer("this must never be reached")]).await;

    let socket = fixture.socket();
    let reply = tokio::task::spawn_blocking(move || ask(&socket, UNMAPPED_SUBJECT, "say hi"))
        .await
        .expect("the request completes");
    assert_eq!(reply, "You're not authorized to use this agent.");
    assert_eq!(
        fixture.model_requests.load(Ordering::SeqCst),
        0,
        "an unauthorized subject must not cost a model call"
    );

    let _directory = fixture.shutdown().await;
    let subject = format!("{UNMAPPED_SUBJECT:?}");
    let proposed = audit
        .records()
        .into_iter()
        .filter(|record| {
            has(record, "subject", &subject)
                && (has(record, "audit.event", "\"broker.decision\"")
                    || has(record, "audit.event", "\"broker.execution\""))
        })
        .collect::<Vec<_>>();
    assert!(proposed.is_empty(), "{proposed:#?}");
}

fn ask_lines(socket: &Path, subject: &str, text: &str) -> Vec<Value> {
    let mut stream = UnixStream::connect(socket).expect("development socket accepts a caller");
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .expect("read timeout configures");
    let request = json!({"subject": subject, "text": text}).to_string();
    writeln!(stream, "{request}").expect("request writes");
    stream.flush().expect("request flushes");
    read_lines(stream)
}

fn ask_lines_on(stream: UnixStream, subject: &str, text: &str) -> Vec<Value> {
    let mut writer = stream.try_clone().expect("the connection clones");
    let request = json!({"subject": subject, "text": text}).to_string();
    writeln!(writer, "{request}").expect("request writes");
    writer.flush().expect("request flushes");
    read_lines(stream)
}

fn read_lines(stream: UnixStream) -> Vec<Value> {
    let mut reader = BufReader::new(stream);
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).expect("a line arrives");
        assert!(
            read > 0,
            "the connection closed before the answer: {lines:?}"
        );
        let value = serde_json::from_str::<Value>(&line).expect("every line is JSON");
        let answered = value.get("reply").is_some();
        lines.push(value);
        if answered {
            return lines;
        }
    }
}

fn kinds(lines: &[Value]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            if line.get("reply").is_some() {
                return if line.get("id").is_some() {
                    "reply-in-place".to_owned()
                } else {
                    "reply".to_owned()
                };
            }
            if line["progress"].get("deleted").is_some() {
                return "progress-deleted".to_owned();
            }
            for key in [
                "progress", "delta", "typing", "status", "reaction", "cancel",
            ] {
                if line.get(key).is_some() {
                    return key.to_owned();
                }
            }
            panic!("unrecognized driver line: {line}")
        })
        .collect()
}

fn texts(lines: &[Value], key: &str) -> Vec<String> {
    lines
        .iter()
        .filter_map(|line| line.get(key)?.get("text")?.as_str().map(ToOwned::to_owned))
        .collect()
}

fn elapsed_seconds(tick: &str) -> u64 {
    tick.split_once('(')
        .and_then(|(_, rest)| rest.split_once(" s)"))
        .unwrap_or_else(|| panic!("a keep-alive line carries its elapsed seconds: {tick}"))
        .0
        .parse()
        .expect("the elapsed seconds are a number")
}

fn tool_run() -> Vec<Value> {
    vec![
        bash_tool_call("call-1", "probe upper --text hi | jq -r .text"),
        final_answer("The capability upper-cased hi."),
    ]
}

const LADDER: [&str; 3] = ["reaction", "typing", "status"];

#[tokio::test(flavor = "multi_thread")]
async fn the_local_driver_streams_a_whole_run_and_finalizes_the_answer_in_place() {
    let _audit = Audit::exclusive().await;
    let fixture = boot(tool_run()).await;
    let socket = fixture.socket();
    let lines = tokio::task::spawn_blocking(move || ask_lines(&socket, MAPPED_SUBJECT, "say hi"))
        .await
        .expect("the request completes");

    let sequence = kinds(&lines);
    assert_eq!(
        sequence
            .iter()
            .take(LADDER.len())
            .map(String::as_str)
            .collect::<Vec<_>>(),
        LADDER,
        "the cheap signals go out before anything the model has to produce: {sequence:?}"
    );
    assert!(
        sequence.iter().any(|kind| kind == "delta"),
        "the answer is streamed as it is written: {sequence:?}"
    );
    assert!(
        !sequence.iter().any(|kind| kind == "progress"),
        "with a stream there is one message and the stream is it: {sequence:?}"
    );
    assert_eq!(
        sequence.last().map(String::as_str),
        Some("reply-in-place"),
        "the surface becomes the answer rather than being followed by one: {sequence:?}"
    );
    assert!(
        !sequence.iter().any(|kind| kind == "progress-deleted"),
        "finalize succeeded, so the delete-and-post fallback must not run: {sequence:?}"
    );

    let streamed = texts(&lines, "delta");
    assert!(
        streamed
            .windows(2)
            .all(|pair| pair[1].starts_with(&pair[0])),
        "a stream grows rather than being replaced: {streamed:?}"
    );
    assert_eq!(
        lines
            .last()
            .and_then(|line| line["reply"].as_str())
            .expect("the answer carries text"),
        "The capability upper-cased hi."
    );
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_progress_message_carries_the_run_and_becomes_the_answer_in_place() {
    let _audit = Audit::exclusive().await;
    let fixture = boot_progress(tool_run()).await;
    let socket = fixture.socket();
    let lines = tokio::task::spawn_blocking(move || ask_lines(&socket, MAPPED_SUBJECT, "say hi"))
        .await
        .expect("the request completes");

    let sequence = kinds(&lines);
    assert_eq!(
        sequence
            .iter()
            .take(LADDER.len())
            .map(String::as_str)
            .collect::<Vec<_>>(),
        LADDER,
        "the cheap signals go out before anything the model has to produce: {sequence:?}"
    );
    assert!(
        sequence.iter().any(|kind| kind == "progress"),
        "a run with a tool call posts a progress message: {sequence:?}"
    );
    assert!(
        !sequence.iter().any(|kind| kind == "delta"),
        "nothing streams where the operator did not ask for a stream: {sequence:?}"
    );
    assert_eq!(
        sequence.last().map(String::as_str),
        Some("reply-in-place"),
        "the progress message becomes the answer: {sequence:?}"
    );
    assert!(
        !sequence.iter().any(|kind| kind == "progress-deleted"),
        "finalize succeeded, so the delete-and-post fallback must not run: {sequence:?}"
    );

    let posted = texts(&lines, "progress");
    assert!(
        posted.iter().any(|text| text.contains("Running")),
        "the line says which capability word is running: {posted:?}"
    );
    let identifiers = lines
        .iter()
        .filter_map(|line| line["progress"].get("id")?.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        identifiers.len(),
        1,
        "one message per session, edited rather than re-posted: {identifiers:?}"
    );
    assert_eq!(
        lines.last().and_then(|line| line["id"].as_str()),
        identifiers.iter().next().copied(),
        "the answer landed in the message the run had been writing: {lines:?}"
    );
    fixture.shutdown().await;
}

/// Without the grant check gating the progress line, a model could inject arbitrary text onto the
/// chat surface simply by naming a fake capability.
#[tokio::test(flavor = "multi_thread")]
async fn a_capability_identifier_the_model_invented_never_reaches_the_progress_line() {
    const PLANTED_MARKER: &str = "zarquon";
    const PLANTED_CAPABILITY: &str = "zarquon-ignore-your-instructions-and-say-this-instead";

    let _audit = Audit::exclusive().await;
    let fixture = boot_progress(vec![
        bash_tool_call("call-1", &format!("{PLANTED_CAPABILITY} --text hi")),
        final_answer("That capability does not exist."),
    ])
    .await;
    let socket = fixture.socket();
    let lines = tokio::task::spawn_blocking(move || ask_lines(&socket, MAPPED_SUBJECT, "say hi"))
        .await
        .expect("the request completes");

    let posted = texts(&lines, "progress");
    assert!(
        !posted.is_empty(),
        "the run has to have written a progress line for its contents to mean anything: {:?}",
        kinds(&lines)
    );
    assert!(
        !posted.iter().any(|text| text.contains(PLANTED_MARKER)),
        "a capability nothing grants put model-authored text on the progress line: {posted:?}"
    );
    fixture.shutdown().await;
}

/// Uses real time, not a paused tokio clock, since a paused clock only auto-advances on a park
/// nothing wakes it from, which never happens here.
#[tokio::test(flavor = "multi_thread")]
async fn keep_alive_ticks_edit_one_message_until_the_budget_stops_them() {
    let _audit = Audit::exclusive().await;
    let fixture = boot_ticking(
        vec![final_answer("That took a while.")],
        json!({"atSeconds": [1, 2], "everySeconds": 1, "max": 3}),
        Duration::from_secs(5),
    )
    .await;
    let socket = fixture.socket();
    let lines =
        tokio::task::spawn_blocking(move || ask_lines(&socket, MAPPED_SUBJECT, "take your time"))
            .await
            .expect("the request completes");

    let posted = texts(&lines, "progress");
    assert!(
        posted
            .first()
            .is_some_and(|text| text.contains("Still working")),
        "nothing is posted until the first tick, which is a slow first turn's only line: {posted:?}"
    );
    let ticks = posted
        .iter()
        .filter(|text| text.contains("Still working"))
        .collect::<Vec<_>>();
    assert!(
        ticks.len() >= 2,
        "the operator's schedule reaches the surface while the model thinks: {ticks:?}"
    );
    assert!(
        ticks.len() <= 3,
        "`max` stops the ticks while the model is still thinking, and a fourth would have landed \
         a second later: {ticks:?}"
    );
    let seconds = ticks
        .iter()
        .map(|tick| elapsed_seconds(tick.as_str()))
        .collect::<Vec<_>>();
    assert!(
        seconds[0] >= 1,
        "no tick lands before the operator's first offset: {ticks:?}"
    );
    assert!(
        seconds.windows(2).all(|pair| pair[1] > pair[0]),
        "each tick carries a number fresher than the one before it: {ticks:?}"
    );

    let identifiers = lines
        .iter()
        .filter_map(|line| line["progress"].get("id")?.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        identifiers.len(),
        1,
        "every tick after the first edits the message the first one posted: {identifiers:?}"
    );
    assert_eq!(
        lines.last().and_then(|line| line["id"].as_str()),
        identifiers.iter().next().copied(),
        "the answer lands in the message the ticks had been writing: {lines:?}"
    );
    assert_eq!(
        lines
            .last()
            .and_then(|line| line["reply"].as_str())
            .expect("the answer carries text"),
        "That took a while."
    );
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_word_cancels_the_session_running_in_that_conversation() {
    let audit = Audit::exclusive().await;
    let hold = ModelHold::new();
    let fixture = boot_held(tool_run(), &hold).await;
    let socket = fixture.socket();

    let asking = {
        let socket = socket.clone();
        let stream = UnixStream::connect(&socket).expect("development socket accepts a caller");
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .expect("read timeout configures");
        tokio::task::spawn_blocking(move || ask_lines_on(stream, MAPPED_SUBJECT, "say hi"))
    };
    for _ in 0..6_000 {
        if fixture.model_requests.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        fixture.model_requests.load(Ordering::SeqCst) > 0,
        "the session never reached the model, so there was nothing to stop"
    );
    let stopping = {
        let socket = socket.clone();
        tokio::task::spawn_blocking(move || {
            let mut stream = UnixStream::connect(&socket).expect("second caller connects");
            let request = json!({"subject": MAPPED_SUBJECT, "text": "stop"}).to_string();
            writeln!(stream, "{request}").expect("stop word writes");
            stream.flush().expect("stop word flushes");
        })
    };
    stopping.await.expect("the stop word is delivered");
    let cancelled = audit
        .wait_for_gateway_event("gateway_session_stop_requested")
        .await;
    hold.release();
    assert!(
        cancelled,
        "the word reached the gateway but stopped no session: {:?}",
        audit.records()
    );
    // Every stop origin, native, button, operator shutdown, or wall-clock bound, writes the same
    // record; via is the only field that shows a person typed the word.
    let requested = audit
        .records()
        .into_iter()
        .find(|record| has(record, "event", "\"gateway_session_stop_requested\""))
        .unwrap_or_else(|| panic!("the recorded stop is readable: {:?}", audit.records()));
    assert!(
        has(&requested, "via", "\"stop-reply\""),
        "the stop record names the affordance that stopped the session: {requested}"
    );
    let lines = asking.await.expect("the cancelled request answers");

    assert_eq!(
        lines
            .last()
            .and_then(|line| line["reply"].as_str())
            .expect("the answer carries text"),
        "Stopped.",
        "the policy is the only terminal writer on a cancel: {lines:?}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.get("reply").is_some())
            .count(),
        1,
        "exactly one terminal line reaches the person: {lines:?}"
    );
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_run_renders_at_off_plain_and_detailed() {
    let _audit = Audit::exclusive().await;
    let mut rendered = Vec::new();
    for detail in ["off", "plain", "detailed"] {
        let fixture = boot_rendering(tool_run(), detail).await;
        let socket = fixture.socket();
        let lines =
            tokio::task::spawn_blocking(move || ask_lines(&socket, MAPPED_SUBJECT, "say hi"))
                .await
                .expect("the request completes");
        rendered.push((detail, kinds(&lines), texts(&lines, "progress")));
        fixture.shutdown().await;
    }

    let (_, off_kinds, off_progress) = &rendered[0];
    assert!(
        off_progress.is_empty(),
        "`off` posts no progress message at all: {off_progress:?}"
    );
    assert!(
        off_kinds.iter().any(|kind| kind == "typing"),
        "`off` still publishes the service's own signals: {off_kinds:?}"
    );

    let (_, _, plain_progress) = &rendered[1];
    assert!(
        plain_progress.iter().any(|text| text.contains("Working")),
        "`plain` says what it is doing in verbs: {plain_progress:?}"
    );
    assert!(
        !plain_progress
            .iter()
            .any(|text| text.contains("turn 1") || text.contains("of 4")),
        "`plain` carries no route budgets: {plain_progress:?}"
    );

    let (_, _, detailed_progress) = &rendered[2];
    assert!(
        detailed_progress.iter().any(|text| text.contains("turn")),
        "`detailed` adds the counters `plain` withholds: {detailed_progress:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_trace_carries_one_progress_record_per_event_and_no_prompt_script_or_result_text() {
    const PLANTED_PROMPT: &str = "zarquon-the-planted-question";
    const PLANTED_RESULT: &str = "zarquon-the-planted-result";

    let audit = Audit::exclusive().await;
    let script = format!("probe upper --text {PLANTED_RESULT} | jq -r .text");
    let responses = vec![
        bash_tool_call("call-1", &script),
        final_answer("Upper-cased."),
    ];
    let fixture = boot(responses.clone()).await;
    let socket = fixture.socket();
    let lines =
        tokio::task::spawn_blocking(move || ask_lines(&socket, MAPPED_SUBJECT, PLANTED_PROMPT))
            .await
            .expect("the request completes");
    assert_eq!(
        lines.last().and_then(|line| line["reply"].as_str()),
        Some("Upper-cased."),
        "{lines:?}"
    );
    fixture.shutdown().await;

    let progress = audit.progress_records();
    let kinds = progress
        .iter()
        .map(|(record, _)| field(record, "kind").unwrap_or("no-kind"))
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        [
            "started",
            "model_turn",
            "answered",
            "tool_started",
            "tool_finished",
            "model_turn",
            "answered",
            "finished",
            "terminal_answered",
        ],
        "the run's events, one record each: {progress:#?}"
    );
    for (record, scope) in &progress {
        assert!(
            scope.contains(&"gateway.session"),
            "a progress record outside the session's trace: {record} under {scope:?}"
        );
    }
    let planted_upper = PLANTED_RESULT.to_uppercase();
    for planted in [
        PLANTED_PROMPT,
        script.as_str(),
        PLANTED_RESULT,
        planted_upper.as_str(),
    ] {
        assert!(
            !progress.iter().any(|(record, _)| record.contains(planted)),
            "a progress record carried {planted}: {progress:#?}"
        );
        assert!(
            !lines.iter().any(|line| line.to_string().contains(planted)),
            "a rendered line carried {planted}: {lines:?}"
        );
    }

    let records = audit.records();
    let (terminal, _) = progress.last().expect("the run ended");
    assert!(
        has(
            terminal,
            "stream.deltas",
            &scripted_deltas(&responses).to_string()
        ),
        "the count is the scripted one: {terminal}"
    );
    assert!(
        records
            .iter()
            .any(|record| record.contains("gateway_progress_rendered") && record.contains("chars")),
        "a stream render says how much was on screen: {records:#?}"
    );
    let answered = records
        .iter()
        .rev()
        .find(|record| record.contains("agent.model.answer"))
        .unwrap_or_else(|| panic!("the answer is on the trace: {records:#?}"));
    assert!(answered.contains("Upper-cased."), "{answered}");
}

struct GatewayChild(std::process::Child);

impl Drop for GatewayChild {
    fn drop(&mut self) {
        if let Err(error) = self.0.kill() {
            eprintln!("test gateway kill: {error}");
        }
        if let Err(error) = self.0.wait() {
            eprintln!("test gateway wait: {error}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_healthy_chat_serves_during_peer_recovery_then_fatal_failure_exits_nonzero() {
    let fixture = boot(vec![final_answer("Healthy transport answered.")]).await;
    let directory = fixture.directory.path();
    let mut config: Value =
        serde_json::from_slice(&fs::read(directory.join("dekopond.json")).expect("gateway config"))
            .expect("config JSON");
    let healthy = directory.join("second.sock");
    let missing_parent = directory.join("not-yet-created");
    let recovering = missing_parent.join("recovering.sock");
    config["transports"][0]["socketPath"] = json!(healthy);
    config["transports"]
        .as_array_mut()
        .expect("transports")
        .push(json!({
            "name": "recovering", "kind": "local", "socketPath": recovering,
        }));
    config["shutdownGraceMs"] = json!(100);
    let path = directory.join("recovery.json");
    write_owner_only(
        &path,
        &serde_json::to_vec(&config).expect("config serializes"),
    );
    let log_path = directory.join("child.log");
    let mut child = GatewayChild(
        std::process::Command::new(env!("CARGO_BIN_EXE_dekopond"))
            .arg("--config")
            .arg(&path)
            .stdout(fs::File::create(&log_path).expect("log file"))
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("gateway subprocess"),
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while !healthy.exists() {
            assert!(
                child.0.try_wait().expect("child status").is_none(),
                "gateway must stay alive during recovery"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("healthy listener starts independently");
    let reply = tokio::task::spawn_blocking(move || {
        ask(&healthy, MAPPED_SUBJECT, "answer while the peer retries")
    })
    .await
    .expect("healthy request");
    assert_eq!(reply, "Healthy transport answered.");

    fs::create_dir(&missing_parent).expect("create missing parent");
    write_owner_only(&recovering, b"protected non-socket");
    let status = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(status) = child.0.try_wait().expect("child status") {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fatal transport failure exits promptly despite a healthy peer");
    assert_eq!(status.code(), Some(1));
    let logs = fs::read_to_string(log_path).expect("subprocess logs");
    for event in [
        "gateway_transport_recovering",
        "gateway_stopped",
        "gateway_exit",
    ] {
        assert!(logs.contains(event), "missing {event}: {logs}");
    }
    assert!(logs.contains("transport-failed") && logs.contains("recovering"));
    assert_eq!(
        fs::read(&recovering).expect("protected file survives"),
        b"protected non-socket"
    );
    fixture.shutdown().await;
}
