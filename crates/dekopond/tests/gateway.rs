//! End-to-end: a chat message reaches a real broker as an attested proposal, and the audit record
//! names the *sender's* principal rather than the daemon's.
//!
//! Nothing here is stubbed on the authority side. `dekopon-brokerd` runs for real, with its own
//! owner-controlled configuration, the exact fetched echo provider component, an attestor grant,
//! identity mappings, and `via`-scoped rules. The only mock is the model endpoint, because a
//! model is the one participant whose answer must be deterministic for a test to assert on it.
//!
//! The audit records are read the way an operator's log pipeline reads them: as the
//! `dekopon_broker::audit` events the in-process broker emits.

#![cfg(unix)]

use std::{
    fs,
    io::{BufRead as _, BufReader, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    os::unix::{fs::PermissionsExt as _, net::UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use dekopon_test_support::CaptureLayer;
use serde_json::{Value, json};
use tokio::sync::{MutexGuard, oneshot};
use tracing_subscriber::layer::SubscriberExt as _;

/// The canonical subject the broker's owner-controlled configuration maps to a principal.
const MAPPED_SUBJECT: &str = "tel.16034700182";
/// A second mapped subject used to prove explicitly shared transcript behavior.
const OTHER_MAPPED_SUBJECT: &str = "tel.16035550100";
/// A canonical subject nothing maps, which must therefore reach nothing.
const UNMAPPED_SUBJECT: &str = "tel.19999999999";
/// The principal the first mapped subject resolves to, inside the broker and nowhere else.
const MAPPED_PRINCIPAL: &str = "cpetersen";
/// The independent principal the second mapped subject resolves to.
const OTHER_MAPPED_PRINCIPAL: &str = "jortega";
/// The daemon's own peer principal, which is the `via` of every attested decision it makes.
const GATEWAY_PRINCIPAL: &str = "dekopond-gateway";
/// The catalog agent both the route and the attested rule name.
const AGENT: &str = "chat-agent";

fn provider(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(format!("examples/providers/{name}-provider.wasm"))
}

fn echo_provider() -> PathBuf {
    provider("echo")
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

/// The broker's whole authorization surface: both mapped principals may drive `chat-agent` and
/// reach `echo.echo`, but only *via* the gateway that vouched for them.
///
/// The direct twin is deliberately absent. That is the whole point of `via`: configuring a gateway
/// must not widen anything, so the daemon's own peer identity authorizes nothing on its own.
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

@id("chat-agent-echo-{suffix}")
permit(principal == Dekopon::Principal::"{principal}",
       action == Dekopon::Action::"echo.echo",
       resource == Dekopon::Provider::"echo")
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
     && context has channel && context.channel == "dev"
     && context has conversation && context.conversation == "dev" }};
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
    fields.insert(
        "namespaceKeyPath".to_owned(),
        json!(directory.join("storage-key.yaml")),
    );
    json!({
        "apiVersion": dekopon_brokerd::CONFIG_API_VERSION,
        "socketPath": directory.join("broker.sock"),
        "brokerPrincipal": "broker-test",
        "policyRevision": "policy-gateway",
        "policiesPath": directory.join("policies.cedar"),
        "providers": [echo_provider(), provider("memory-chat")],
        "identities": [{
            "uid": uid,
            "principal": GATEWAY_PRINCIPAL,
            "actor": {"type": "service", "principal": GATEWAY_PRINCIPAL},
            "attestor": {
                "namespaces": ["tel"],
                "chatScopes": [{
                    "breadth": "exactConversation",
                    "kind": "local",
                    "transport": "dev",
                    "channel": "dev",
                    "conversation": "dev",
                    "localSubjectService": "tel"
                }]
            }
        }],
        "identityMappings": [
            {"subject": MAPPED_SUBJECT, "principal": MAPPED_PRINCIPAL},
            {"subject": OTHER_MAPPED_SUBJECT, "principal": OTHER_MAPPED_PRINCIPAL}
        ],
        "constraintSets": {
            "echo.echo": {
                "provider": "echo", "effect": "read-only", "risk": "Low",
                "idempotency": "idempotent",
                "constraints": {"timeoutMs": 30_000, "maxOutputBytes": 1_048_576}
            },
            "memory.chat.record": {
                "route": "chatMemoryRecord",
                "provider": "memory-chat", "effect": "local-write", "risk": "Medium",
                "idempotency": "conditional",
                "constraints": {
                    "timeoutMs": 30_000, "maxOutputBytes": 131_072,
                    "storage": {"interface":"jsonl","access":"read-write","namespace":"chat"}
                }
            },
            "memory.chat.recent": {
                "route": "chatMemoryRecent",
                "provider": "memory-chat", "effect": "read-only", "risk": "High",
                "idempotency": "idempotent",
                "constraints": {
                    "timeoutMs": 30_000, "maxOutputBytes": 131_072,
                    "storage": {"interface":"jsonl","access":"read-only","namespace":"chat"}
                }
            },
            "memory.chat.search": {
                "route": "chatMemorySearch",
                "provider": "memory-chat", "effect": "read-only", "risk": "High",
                "idempotency": "idempotent",
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

fn gateway_config(
    directory: &Path,
    uid: u32,
    model_endpoint: &str,
    conversation_scope: Option<&str>,
) -> Value {
    let mut config = json!({
        "apiVersion": dekopond::CONFIG_API_VERSION,
        "catalogPath": directory.join("dekopon.yaml"),
        "broker": {
            "socketPath": directory.join("broker.sock"),
            "serverUid": uid
        },
        "transports": [
            {"name": "dev", "kind": "local", "socketPath": directory.join("dev.sock")}
        ],
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
            "match": {"kind": "directMessage"},
            "agent": AGENT,
            "limits": {"maxSteps": 4, "maxCapabilityCalls": 4},
            // Persistent so one fixture covers both properties: an unauthorized subject is still
            // refused before a model call, and a follow-up still reaches the model with the
            // exchange before it in front of the question.
            "conversation": {"mode": "persistent"}
        }],
        "sessions": {"maxConcurrent": 2},
        "shutdownGraceMs": 30_000
    });
    if let Some(scope) = conversation_scope {
        config["routes"][0]["conversation"]["scope"] = json!(scope);
    }
    config
}

// ---------------------------------------------------------------------------
// Mock model endpoint
// ---------------------------------------------------------------------------

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

/// Serves a fixed script of model responses on loopback, counting and keeping every request.
///
/// The count is the assertion that matters for the refusal case: a gateway that refuses *after*
/// contacting a model has already spent the money the refusal was supposed to save. The bodies are
/// what a conversation assertion needs, since "this message was seeded with the last exchange" is a
/// claim about the message list a request carried and not about how many requests there were.
fn spawn_model(responses: Vec<Value>) -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("mock model binds");
    let address = listener.local_addr().expect("mock model address");
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&requests);
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&bodies);
    thread::spawn(move || {
        for response in responses {
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
    let body = serde_json::to_vec(body).expect("response serializes");
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .expect("response headers write");
    stream.write_all(&body).expect("response body writes");
    stream.flush().expect("response flushes");
}

// ---------------------------------------------------------------------------
// Fixture lifecycle
// ---------------------------------------------------------------------------

/// Waits until a socket exists *and* is owner-only.
///
/// Existence alone is not readiness: both daemons bind and then narrow the mode, and a client that
/// connects inside that window fails its own privacy check.
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

/// Sends one line to the development transport and reads the answer it writes back.
fn ask(socket: &Path, subject: &str, text: &str) -> String {
    let mut stream = UnixStream::connect(socket).expect("development socket accepts a caller");
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .expect("read timeout configures");
    let request = json!({"subject": subject, "channel": "dev", "text": text}).to_string();
    writeln!(stream, "{request}").expect("request writes");
    stream.flush().expect("request flushes");
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("a reply arrives");
    let reply = serde_json::from_str::<Value>(&line).expect("the reply is JSON");
    reply["reply"]
        .as_str()
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

/// The broker's audit records for one test.
///
/// The broker runs in this process, so each `broker.decision` and `broker.execution` record is a
/// `dekopon_broker::audit` event on the one process-wide dispatcher. A test takes
/// [`Audit::exclusive`] before it boots anything and holds it to its end, which is what makes every
/// record in the capture its own.
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
            let capture = CaptureLayer::with_target_prefix("dekopon_broker::audit");
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

    /// Every audit record so far, in order, each rendered as ` field=value…`.
    fn records(&self) -> Vec<String> {
        self.capture
            .events()
            .into_iter()
            .map(|(fields, _)| fields)
            .collect()
    }

    /// The first record of `event` carrying every `(field, value)` pair, as its fields render.
    fn find(&self, event: &str, fields: &[(&str, &str)]) -> Option<String> {
        let event = format!("\"{event}\"");
        self.records().into_iter().find(|record| {
            has(record, "audit.event", &event)
                && fields
                    .iter()
                    .all(|(field, value)| has(record, field, value))
        })
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

/// Whether a rendered record has `field` rendered exactly as `value`.
fn has(record: &str, field: &str, value: &str) -> bool {
    let rendered = format!(" {field}={value}");
    record.match_indices(&rendered).any(|(start, _)| {
        record[start + rendered.len()..]
            .chars()
            .next()
            .is_none_or(|next| next == ' ')
    })
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

    /// One request's message list as `(role, content)` pairs, in the order the model saw them.
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

    /// Stops both daemons and hands back the directory, which provider storage still lives in.
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

/// Boots a real broker and a real gateway against one mock model endpoint.
async fn boot(responses: Vec<Value>) -> Fixture {
    boot_in_with_scope(temporary(), responses, None).await
}

/// Boots the same real pair with the route's shared scope explicitly enabled.
async fn boot_shared(responses: Vec<Value>) -> Fixture {
    boot_in_with_scope(temporary(), responses, Some("sharedConversation")).await
}

/// Reboots both real processes over the same provider-storage directory.
async fn boot_in(directory: tempfile::TempDir, responses: Vec<Value>) -> Fixture {
    boot_in_with_scope(directory, responses, None).await
}

async fn boot_in_with_scope(
    directory: tempfile::TempDir,
    responses: Vec<Value>,
    conversation_scope: Option<&str>,
) -> Fixture {
    let uid = dekopon_brokerd::current_uid();

    let broker_path = directory.path().join("broker.json");
    write_owner_only(
        &directory.path().join("storage-key.yaml"),
        b"apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
    );
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
    let (endpoint, model_requests, model_prompts) = spawn_model(responses);
    let gateway_path = directory.path().join("dekopond.json");
    write_owner_only(
        &gateway_path,
        &serde_json::to_vec(&gateway_config(
            directory.path(),
            uid,
            &endpoint,
            conversation_scope,
        ))
        .expect("gateway config serializes"),
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
    // The property this whole daemon exists to demonstrate: the gateway holds no authority, the
    // broker maps the sender's subject to a principal, and the audit record attributes the effect
    // to that person — not to the process that relayed their message.
    let audit = Audit::exclusive().await;
    let fixture = boot(vec![
        bash_tool_call("call-1", "echo.echo --message hi | jq -r .message"),
        final_answer("The capability echoed hi."),
    ])
    .await;

    let socket = fixture.socket();
    let reply = tokio::task::spawn_blocking(move || ask(&socket, MAPPED_SUBJECT, "say hi"))
        .await
        .expect("the request completes");
    assert_eq!(reply, "The capability echoed hi.");
    assert_eq!(fixture.model_requests.load(Ordering::SeqCst), 2);

    let _directory = fixture.shutdown().await;

    let principal = format!("{MAPPED_PRINCIPAL:?}");
    let via = format!("{GATEWAY_PRINCIPAL:?}");
    let subject = format!("{MAPPED_SUBJECT:?}");
    let agent = format!("{AGENT:?}");
    audit
        .find(
            "broker.execution",
            &[
                ("capability.id", "echo.echo"),
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

    // The decision that authorized it agrees, and the audit carries the subject rather than the
    // message that prompted it.
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
    let records = audit.records().concat();
    assert!(!records.contains("say hi"), "{records}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_persistent_route_answers_a_follow_up_with_the_exchange_before_it() {
    // The daemon assembled from its own configuration, not a hand-built runner: a second message on
    // the same conversation reaches the model with the first exchange in front of the new question.
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
    // Local write+flush acceptance reaches the caller just before the gateway's one bounded
    // post-acceptance record finishes. Wait for its exact audited success rather than sleeping and
    // racing a slow filesystem.
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
                    "claim recall without retrieving it."
                )
            ),
            ("user", "what broke?"),
            ("assistant", "Two things broke."),
            ("user", "and the second one?"),
        ]
    );

    let _directory = fixture.shutdown().await;
    // Conversation text may now be in opaque provider storage, but the broker's audit records
    // still never contain a word of it.
    let records = audit.records().concat();
    assert!(!records.contains("what broke"), "{records}");
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_shared_scope_replays_attributed_history_across_two_principals() {
    let audit = Audit::exclusive().await;
    let fixture = boot_shared(vec![
        final_answer("Two things broke."),
        bash_tool_call(
            "shared-participant-echo",
            "echo.echo --message second-participant | jq -r .message",
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
                    "claim recall without retrieving it."
                )
                .to_owned(),
            ),
            ("user".to_owned(), first_prompt),
            ("assistant".to_owned(), "Two things broke.".to_owned()),
            ("user".to_owned(), second_prompt),
        ],
        "the second authenticated principal receives one shared, provenance-labelled transcript"
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
    // A subject the owner never mapped reaches nothing, and finding that out costs one broker round
    // trip rather than a model session. The mock model would answer if asked; it is never asked.
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
    // A refused capability *listing* is not an invocation, so it produces no decision record; the
    // audit stays empty because nothing was ever proposed.
    assert!(audit.records().is_empty(), "{:#?}", audit.records());
}
