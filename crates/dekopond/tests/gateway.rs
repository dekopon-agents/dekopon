//! End-to-end: a chat message reaches a real broker as an attested proposal, and the audit record
//! names the *sender's* principal rather than the daemon's.
//!
//! Nothing here is stubbed on the authority side. `dekopon-brokerd` runs for real, with its own
//! owner-controlled configuration, the checked-in echo provider component, an attestor grant, an
//! identity mapping, and one `via`-scoped rule. The only mock is the model endpoint, because a
//! model is the one participant whose answer must be deterministic for a test to assert on it.

#![cfg(unix)]

use std::{
    fs,
    io::{BufRead as _, BufReader, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    os::unix::{fs::PermissionsExt as _, net::UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use serde_json::{Value, json};
use tokio::sync::oneshot;

/// The canonical subject the broker's owner-controlled configuration maps to a principal.
const MAPPED_SUBJECT: &str = "tel.16034700182";
/// A canonical subject nothing maps, which must therefore reach nothing.
const UNMAPPED_SUBJECT: &str = "tel.19999999999";
/// The principal that subject resolves to, inside the broker and nowhere else.
const MAPPED_PRINCIPAL: &str = "cpetersen";
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

/// The broker's whole authorization surface: `cpetersen` may drive `chat-agent` and reach
/// `echo.echo`, but only *via* the gateway that vouched for them.
///
/// The direct twin is deliberately absent. That is the whole point of `via`: configuring a gateway
/// must not widen anything, so the daemon's own peer identity authorizes nothing on its own.
fn broker_policies() -> String {
    format!(
        r#"
@id("chat-agent-session")
permit(principal == Dekopon::Principal::"{MAPPED_PRINCIPAL}",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"{AGENT}")
when {{ context has via && context.via == "{GATEWAY_PRINCIPAL}" }};

@id("chat-agent-echo")
permit(principal == Dekopon::Principal::"{MAPPED_PRINCIPAL}",
       action == Dekopon::Action::"echo.echo",
       resource == Dekopon::Provider::"echo")
when {{ context has via && context.via == "{GATEWAY_PRINCIPAL}"
     && context has agent && context.agent == "{AGENT}" }};

@id("chat-agent-memory")
permit(principal == Dekopon::Principal::"{MAPPED_PRINCIPAL}",
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
        "auditPath": directory.join("audit.jsonl"),
        "checkpointPath": directory.join("checkpoint.json"),
        "checkpointLockPath": directory.join("checkpoint.lock"),
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
            {"subject": MAPPED_SUBJECT, "principal": MAPPED_PRINCIPAL}
        ],
        "constraintSets": {
            "echo.echo": {
                "provider": "echo", "effect": "read-only", "risk": "Low",
                "idempotency": "idempotent",
                "constraints": {"timeoutMs": 30_000, "maxOutputBytes": 1_048_576}
            },
            "memory.chat.record": {
                "provider": "memory-chat", "effect": "local-write", "risk": "Medium",
                "idempotency": "conditional",
                "constraints": {
                    "timeoutMs": 30_000, "maxOutputBytes": 131_072,
                    "storage": {"interface":"jsonl","access":"read-write","namespace":"chat"}
                }
            },
            "memory.chat.recent": {
                "provider": "memory-chat", "effect": "read-only", "risk": "High",
                "idempotency": "idempotent",
                "constraints": {
                    "timeoutMs": 30_000, "maxOutputBytes": 131_072,
                    "storage": {"interface":"jsonl","access":"read-only","namespace":"chat"}
                }
            },
            "memory.chat.search": {
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

fn gateway_config(directory: &Path, uid: u32, model_endpoint: &str) -> Value {
    json!({
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
    })
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

/// Every audit event the broker durably recorded, in order.
fn audit_events(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .expect("durable audit reads")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("audit record is JSON"))
        .map(|record| record["event"].clone())
        .collect()
}

struct Fixture {
    directory: tempfile::TempDir,
    broker: tokio::task::JoinHandle<
        Result<dekopon_brokerd::AuditCheckpoint, dekopon_brokerd::BrokerdError>,
    >,
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

    fn audit(&self) -> PathBuf {
        self.directory.path().join("audit.jsonl")
    }

    async fn wait_for_memory_record(&self) {
        for _ in 0..3_000 {
            let audit = self.audit();
            if fs::read_to_string(audit).is_ok_and(|contents| {
                contents
                    .lines()
                    .filter_map(|line| {
                        serde_json::from_str::<Value>(line)
                            .ok()
                            .map(|record| record["event"].clone())
                    })
                    .any(|event| {
                        event["type"] == "execution"
                            && event["capability"] == "memory.chat.record"
                            && event["outcome"] == "Succeeded"
                    })
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the post-acceptance memory record did not complete within thirty seconds");
    }

    /// Stops both daemons and hands back the directory, which the audit file still lives in.
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
    boot_in(temporary(), responses).await
}

/// Reboots both real processes over the same audit and provider-storage directory.
async fn boot_in(directory: tempfile::TempDir, responses: Vec<Value>) -> Fixture {
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
        &serde_json::to_vec(&gateway_config(directory.path(), uid, &endpoint))
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
    // broker maps the sender's subject to a principal, and the durable audit attributes the effect
    // to that person — not to the process that relayed their message.
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

    let audit = fixture.audit();
    let _directory = fixture.shutdown().await;

    let events = audit_events(&audit);
    let execution = events
        .iter()
        .find(|event| event["type"] == "execution")
        .unwrap_or_else(|| panic!("an execution record exists: {events:#?}"));
    assert_eq!(execution["principal"], MAPPED_PRINCIPAL);
    assert_eq!(execution["via"], GATEWAY_PRINCIPAL);
    // The audit chain's own field naming: `AuditEvent` renames variants, not fields.
    assert_eq!(execution["attested_subject"], MAPPED_SUBJECT);
    assert_eq!(execution["actor"]["agent"], AGENT);
    assert_eq!(execution["capability"], "echo.echo");
    assert_eq!(execution["outcome"], "Succeeded");

    // The decision that authorized it agrees, and the audit carries the subject rather than the
    // message that prompted it.
    let decision = events
        .iter()
        .find(|event| event["type"] == "decision")
        .expect("a decision record exists");
    assert_eq!(decision["allowed"], true);
    assert_eq!(decision["principal"], MAPPED_PRINCIPAL);
    assert_eq!(decision["via"], GATEWAY_PRINCIPAL);
    let serialized = serde_json::to_string(&events).expect("audit serializes");
    assert!(!serialized.contains("say hi"), "{serialized}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_persistent_route_answers_a_follow_up_with_the_exchange_before_it() {
    // The daemon assembled from its own configuration, not a hand-built runner: a second message on
    // the same conversation reaches the model with the first exchange in front of the new question.
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
    fixture.wait_for_memory_record().await;
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

    let audit = fixture.audit();
    let _directory = fixture.shutdown().await;
    // Conversation text may now be in opaque provider storage, but the broker's durable audit
    // chain still never contains a word of it.
    let serialized = serde_json::to_string(&audit_events(&audit)).expect("audit serializes");
    assert!(!serialized.contains("what broke"), "{serialized}");
}

#[tokio::test(flavor = "multi_thread")]
async fn durable_recent_retrieves_the_accepted_turn_after_broker_and_gateway_restart() {
    let fixture = boot(vec![final_answer("The retained answer.")]).await;
    let socket = fixture.socket();
    let first = tokio::task::spawn_blocking(move || ask(&socket, MAPPED_SUBJECT, "remember this"))
        .await
        .expect("first request completes");
    assert_eq!(first, "The retained answer.");
    fixture.wait_for_memory_record().await;
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

    let audit = fixture.audit();
    let _directory = fixture.shutdown().await;
    // A refused capability *listing* is not an invocation, so it produces no decision record; the
    // durable chain stays empty because nothing was ever proposed.
    assert!(audit_events(&audit).is_empty());
}
