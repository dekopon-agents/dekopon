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
        "providers": [echo_provider(), provider("memory-chat")],
        "identities": [{
            "uid": uid,
            "principal": GATEWAY_PRINCIPAL,
            "actor": {"type": "service", "principal": GATEWAY_PRINCIPAL},
            "attestor": {
                "namespaces": ["tel"],
                "chatScopes": [{
                    "kind": "local",
                    "transport": "dev",
                    "conversation": {"kind": "any", "ids": ["dev"]},
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
                // The reference driver implements every surface, so the local transport is where
                // a whole run is readable as lines rather than inferred from a service's UI.
                //
                // `stream` is the one setting a test chooses, because it decides which surface the
                // session has: with the stream on there is one message and it is the stream, so no
                // progress line is ever posted, and with it off the progress message is the
                // surface that grows and then becomes the answer. One session never has both.
                "liveness": {
                    "mode": "native",
                    "progress": "message",
                    "stream": stream,
                    "cancelButton": true,
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
            // Persistent so one fixture covers both properties: an unauthorized subject is still
            // refused before a model call, and a follow-up still reaches the model with the
            // exchange before it in front of the question.
            "memory": {"mode": "persistent"}
        }],
        "sessions": {"maxConcurrent": 2},
        "shutdownGraceMs": 30_000
    });
    if let Some(scope) = conversation_scope {
        // The scope lives under `memory:`; `conversation:` is the match. A shared window is
        // refused on a route whose kind list is exactly `[directMessage]` — the direct message
        // already is the subject — so a shared fixture widens the route to every kind, which is
        // what an operator choosing a shared audience has to write.
        config["routes"][0]["conversation"] = json!({"kind": "any"});
        config["routes"][0]["memory"]["scope"] = json!(scope);
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
///
/// `first_answer_delay` holds the first answer back after the request has been read and counted —
/// a turn a person is still waiting on — and `first_answer_hold` holds it until the test says
/// otherwise. Both wait here, on the mock's own thread, because the one thing a test about the
/// gateway's clock must not do is change the clock it is measuring.
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

/// Writes one scripted turn as the chat-completions event stream a real endpoint sends.
///
/// The gateway's model client streams by default, so a mock that answered one JSON document would
/// be testing a path production no longer takes. Visible text is deliberately split across two
/// chunks: one delta proves nothing about cumulative rendering, and the second is what a progress
/// surface has to grow into rather than replace.
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

/// The chunk sequence one scripted `choices[0].message` is delivered as.
fn chunks(body: &Value) -> Vec<Value> {
    let message = &body["choices"][0]["message"];
    // The role chunk carries `content: null`, which a strict accumulator must tolerate rather than
    // treat as the empty answer; `usage: null` rides every chunk for the same reason.
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
///
/// The answer is the line carrying `reply`, which is no longer the first line back: a session
/// publishes its reaction, its typing lease, its native status and every stream render on the same
/// connection first. Tests that only want the answer read through them; [`ask_lines`] is for the
/// ones whose subject is the sequence itself.
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
            // The whole workspace rather than the broker's audit alone: the gateway's progress
            // records are the other half of what a run leaves behind, and they have to be read
            // under the same exclusive hold or another test's session lands in the middle of them.
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

    /// Every audit record so far, in order, each rendered as ` field=value…`.
    fn records(&self) -> Vec<String> {
        self.capture
            .events()
            .into_iter()
            .map(|(fields, _)| fields)
            .collect()
    }

    /// Only the records the broker crates wrote, by callsite target.
    ///
    /// The capture is the whole workspace, because the gateway's progress records are the other
    /// half of what a run leaves behind. That makes "no record carries the message text" a claim
    /// that has to name whose records it is about: the gateway's own receipt record carries the
    /// inbound text by design, and the broker's records are the ones that must never see a word of
    /// it.
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

    /// Every `gateway.progress` record, in order, each with the span names enclosing it.
    ///
    /// The scope travels because "one record per event" is only half the claim: a record written
    /// outside the session's own span would be an orphan no operator could tie back to the
    /// conversation, and the immediate parent alone cannot say — the loop writes these from two
    /// or three spans down, and the policy task writes its terminal from the session span itself.
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

    /// Waits for the gateway's own record of `event`, and reports whether it arrived.
    ///
    /// A test that has to act while a session runs needs the daemon's word for what it did rather
    /// than its own for what it sent: the socket write carrying a stop word returns long before
    /// the matcher has found the session that word stops. Reported rather than asserted here
    /// because the caller is holding a run still while it waits, and a caller that panicked inside
    /// this loop would hang on the turn nobody is going to answer instead of failing.
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

/// One field's value as it rendered, with the quotes a string field carries stripped.
///
/// Beside [`has`] rather than through it: an assertion on the *sequence* of records needs the
/// value itself, and "does this record contain that pair" cannot answer what order they came in.
fn field<'a>(record: &'a str, name: &str) -> Option<&'a str> {
    let rendered = format!(" {name}=");
    let start = record.find(&rendered)? + rendered.len();
    let rest = &record[start..];
    let end = rest.find(' ').unwrap_or(rest.len());
    Some(rest[..end].trim_matches('"'))
}

/// How many text deltas one scripted answer actually puts on the wire.
///
/// Counted from the chunks the mock endpoint serves rather than written down as a number, so the
/// trace's count is compared against the script itself: a fixture that later splits its text
/// differently moves both sides together, and a client that drops or invents a fragment moves only
/// one.
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
///
/// Streaming on, because that is what an agent route with a model that streams looks like in
/// production: the answer grows on screen and the surface it grew in becomes it.
async fn boot(responses: Vec<Value>) -> Fixture {
    boot_in_with_scope(temporary(), responses, None, "plain", true).await
}

/// The same pair whose surface is the progress message rather than the stream.
///
/// Both are configurations of one policy, and a test has to say which it is asserting on: the
/// progress line, its keep-alive ticks, and the detail levels only exist on this side of that
/// choice.
async fn boot_progress(responses: Vec<Value>) -> Fixture {
    boot_in_with_scope(temporary(), responses, None, "plain", false).await
}

/// The same progress-message pair on the operator's own keep-alive schedule, against a model that
/// holds its answer long enough for that schedule to run.
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

/// The same streamed pair whose first answer the test lets go itself, so that whatever the test
/// does next happens while the run is still inside its first model turn. See [`ModelHold`].
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

/// The same progress-message pair with the route's detail level chosen, which is the only thing
/// that differs between the three renderings of one run.
async fn boot_rendering(responses: Vec<Value>, progress_detail: &str) -> Fixture {
    boot_in_with_scope(temporary(), responses, None, progress_detail, false).await
}

/// Boots the same real pair with the route's shared scope explicitly enabled.
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

/// Reboots both real processes over the same provider-storage directory.
async fn boot_in(directory: tempfile::TempDir, responses: Vec<Value>) -> Fixture {
    boot_in_with_scope(directory, responses, None, "plain", true).await
}

/// A hold on the mock model's first answer that the test lets go itself.
///
/// Timing a race loses it. `model_requests` moves the moment the model connection is accepted, and
/// an answer that arrives in the next microsecond has finished the tool call, the capability and
/// the second turn before a second caller's line is even written. A test whose subject is what
/// reaches a session *while* it runs holds the answer instead and releases it once the gateway has
/// recorded what the test was waiting for, so mid-run is a fact about the run rather than a bet on
/// how a runner schedules threads.
#[derive(Clone)]
struct ModelHold(Arc<(Mutex<bool>, Condvar)>);

impl ModelHold {
    fn new() -> Self {
        Self(Arc::new((Mutex::new(false), Condvar::new())))
    }

    /// Parks the mock's own thread until the test releases it.
    fn wait(&self) {
        let (released, ready) = &*self.0;
        let mut released = released.lock().expect("the model hold locks");
        while !*released {
            released = ready.wait(released).expect("the model hold locks");
        }
    }

    /// Lets the held answer go.
    fn release(&self) {
        let (released, ready) = &*self.0;
        *released.lock().expect("the model hold locks") = true;
        ready.notify_all();
    }
}

/// What a fixture may do to a run's timing: the operator's keep-alive schedule, and when the mock
/// model's first answer arrives — after a delay it measures itself, or when the test says so.
///
/// All three default to what production ships — the configured 15/45/every-60 schedule, an answer
/// that arrives at once, and nothing holding it — because only the test about ticks and the test
/// about a stop landing mid-run have any business changing them.
#[derive(Default)]
struct Timing {
    /// Replaces the transport's `keepAlive` block when the schedule itself is the subject.
    keep_alive: Option<Value>,
    /// How long the mock model holds its first answer, so a schedule has a run to run against.
    model_delay: Duration,
    /// A hold on that same first answer the test releases by hand, for a run that has to still be
    /// running when something else reaches the gateway.
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
    let records = audit.broker_records().concat();
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
    // Conversation text may now be in opaque provider storage, but the broker's own records still
    // never contain a word of it.
    let records = audit.broker_records().concat();
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
    // A refused capability *listing* is not an invocation, so it produces neither a decision nor an
    // execution record: nothing was ever proposed. The capture is the whole workspace, so the
    // gateway's own lifecycle records are in it too and the assertion names what must be absent
    // rather than counting what is present.
    let proposed = audit
        .records()
        .into_iter()
        .filter(|record| {
            has(record, "audit.event", "\"broker.decision\"")
                || has(record, "audit.event", "\"broker.execution\"")
        })
        .collect::<Vec<_>>();
    assert!(proposed.is_empty(), "{proposed:#?}");
}

// ---------------------------------------------------------------------------
// The local driver, end to end
// ---------------------------------------------------------------------------

/// Every line one request produced, in order, up to and including the answer.
///
/// [`ask`] reads exactly one line because, before this, exactly one line existed. The reference
/// driver writes the whole run — the typing lease, the native status, the reaction, the progress
/// message and each of its edits, every stream render, and the answer that replaces it — so the
/// assertion a person cares about is the sequence, not the last element of it.
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

/// The same, on a connection a second caller can send a stop word into while it is open.
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

/// The kind of every line, as one readable sequence.
///
/// A progress message and a stream render both carry `{id, text, cancel}`, so naming the key is
/// what distinguishes them; an answer that replaced a message in place carries the identifier it
/// landed in, and one posted on its own does not.
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

/// Every `text` written under one key, in order.
fn texts(lines: &[Value], key: &str) -> Vec<String> {
    lines
        .iter()
        .filter_map(|line| line.get(key)?.get("text")?.as_str().map(ToOwned::to_owned))
        .collect()
}

/// The number a keep-alive line carries, which is the only number a `plain` one has.
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
        bash_tool_call("call-1", "echo.echo --message hi | jq -r .message"),
        final_answer("The capability echoed hi."),
    ]
}

/// The ladder every session opens with, in the order the policy climbs it.
///
/// The reaction is first because it is the cheapest thing a service will take and the only one that
/// lands on the message the person just sent; the lease and the native status follow. What the
/// order is worth asserting for is that all three are spent before the model has written anything.
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
        "The capability echoed hi."
    );
    fixture.shutdown().await;
}

/// The other half of the same policy: the surface is a progress message rather than the stream.
///
/// Posted on the tool call rather than at t=0, edited as the run changes, and finalized into the
/// answer under the identifier it was posted with — a fast one-turn answer must not leave a
/// "Working on it…" line behind the reply that arrived a second later, which is why the post waits
/// for something worth saying.
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

/// A slow first turn, seen from the chair: ticks on the operator's schedule, in one message, and
/// an end to them.
///
/// Real time and a short configured schedule rather than `tokio::time::pause()`. This fixture is
/// two daemons, a wasm provider, and a blocking model client on one runtime, and a paused clock
/// auto-advances only on a park nothing woke the driver from — with that much cross-thread I/O in
/// flight, never. The 15 s, 45 s, then every 60 s arithmetic belongs to the policy's own tests,
/// which hold a clock still around nothing else; what only an end-to-end run can show is that a
/// tick reaches a real surface, that it posts the message a slow first turn otherwise never gets,
/// that later ticks edit that same message rather than posting beside it, and that `max` stops
/// them while the model is still thinking.
///
/// The progress message rather than the stream, because a tick is an edit of that message: a
/// streamed session's surface shows the answer growing and has no line to write a tick into.
#[tokio::test(flavor = "multi_thread")]
async fn keep_alive_ticks_edit_one_message_until_the_budget_stops_them() {
    let _audit = Audit::exclusive().await;
    // Ticks fall at 1 s, 2 s and 3 s against a model that holds its answer for five. A fourth
    // would have landed at 4 s, still a second inside the run: `max` is the only reason it does
    // not.
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

/// A stop word said by a second caller while the run it stops is still inside its first turn.
///
/// The first answer is held rather than timed. `model_requests` moves when the model connection is
/// accepted, so a mock that answers at once has run the tool call, the capability and the second
/// turn before the stop line is written — a test of what happens *after* a run, which passes or
/// fails on how a runner schedules threads. The held turn goes only once the gateway has recorded
/// that it cancelled this session, which is the moment the word is known to have landed mid-run.
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
    // A second caller, in the same conversation, saying the word rather than pressing anything:
    // this is the path every transport has, including the four with no components at all.
    // The session has to exist before the word can stop it: a stop word said to an idle agent
    // falls through to ordinary routing, which is the behavior the matcher is required to keep.
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
    // Written bytes are not a delivered stop: this record is written after the matcher found the
    // session and cancelled it, and that is when the held turn may go.
    let cancelled = audit
        .wait_for_gateway_event("gateway_session_stop_requested")
        .await;
    // Released either way, so a stop that never landed fails here rather than leaving the run
    // parked on an answer nobody will send.
    hold.release();
    assert!(
        cancelled,
        "the word reached the gateway but stopped no session: {:?}",
        audit.records()
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

/// Everything one run put on the trace, and everything it must not have.
///
/// Held under the same exclusive audit lock as the records it reads: the assertion is this run's
/// own sequence of progress records, and a session from another test landing in the middle of them
/// would make it a sequence of two runs.
#[tokio::test(flavor = "multi_thread")]
async fn the_trace_carries_one_progress_record_per_event_and_no_prompt_script_or_result_text() {
    const PLANTED_PROMPT: &str = "zarquon-the-planted-question";
    // Echoed back by the provider, so this string is genuinely in the tool result the model read.
    const PLANTED_RESULT: &str = "zarquon-the-planted-result";

    let audit = Audit::exclusive().await;
    let script = format!("echo.echo --message {PLANTED_RESULT} | jq -r .message");
    let responses = vec![bash_tool_call("call-1", &script), final_answer("Echoed.")];
    let fixture = boot(responses.clone()).await;
    let socket = fixture.socket();
    // Every line the driver wrote, not only the answer: `ask` keeps the reply and drops the
    // renders, and the renders are half of what a redaction claim is about — a progress line or a
    // stream fragment carrying the question, the script, or the provider's output would be a leak
    // the trace assertions below could not see.
    let lines =
        tokio::task::spawn_blocking(move || ask_lines(&socket, MAPPED_SUBJECT, PLANTED_PROMPT))
            .await
            .expect("the request completes");
    assert_eq!(
        lines.last().and_then(|line| line["reply"].as_str()),
        Some("Echoed."),
        "{lines:?}"
    );
    fixture.shutdown().await;

    // One record per event the run emitted, in the order it emitted them. Written as the whole
    // sequence rather than as a count and a set of kinds: a duplicate `started`, a missing
    // `tool_finished`, or a `finished` that arrived before the tool it counted are all the same
    // size and the same kinds, and only the order tells them apart.
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
    for planted in [PLANTED_PROMPT, script.as_str(), PLANTED_RESULT] {
        assert!(
            !progress.iter().any(|(record, _)| record.contains(planted)),
            "a progress record carried {planted}: {progress:#?}"
        );
        assert!(
            !lines.iter().any(|line| line.to_string().contains(planted)),
            "a rendered line carried {planted}: {lines:?}"
        );
    }

    // The deltas the stream produced are counted where the turn is accounted for, and what was on
    // screen is counted where it was rendered — neither of them repeats the text.
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
    // Turn 1 answers with a tool call and no text; the final turn carries the answer.
    let answered = records
        .iter()
        .rev()
        .find(|record| record.contains("agent.model.answer"))
        .unwrap_or_else(|| panic!("the answer is on the trace: {records:#?}"));
    assert!(answered.contains("Echoed."), "{answered}");
}
