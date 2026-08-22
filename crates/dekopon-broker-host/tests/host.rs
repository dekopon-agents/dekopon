use std::{
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

use dekopon_broker_host::{
    BrokerHostError, BrokerHostLimits, BrokerHostOptions, BrokerProviderRegistry, HTTP_WIT,
    PROVIDER_WIT, STORAGE_WIT,
};
use dekopon_capability::{
    AuthorizedInvocation, EffectKind, ExecutionConstraints, HttpConstraints, Idempotency,
    ProposedInvocation, StorageAccess, StorageConstraints, StorageInterface, StorageNamespace,
    broker::AuthorizationGate,
};
use dekopon_core::{Actor, AgentId, CapabilityId, InvocationId, PrincipalId, RiskLevel, TraceId};
use dekopon_storage_host::{ContinuityPolicy, StorageGrantRequest, StorageHost, StorageLimits};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(format!("examples/providers/{name}"))
}

fn host_fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn authorized(
    capability: CapabilityId,
    input: Value,
    constraints: ExecutionConstraints,
) -> AuthorizedInvocation {
    let provider = capability
        .as_str()
        .split('.')
        .next()
        .expect("fixture capability has a provider prefix")
        .to_owned();
    authorized_for(&provider, capability, input, constraints)
}

fn authorized_for(
    provider: &str,
    capability: CapabilityId,
    input: Value,
    constraints: ExecutionConstraints,
) -> AuthorizedInvocation {
    let proposal = ProposedInvocation::new(
        "invoke-test"
            .parse::<InvocationId>()
            .expect("valid invocation fixture"),
        capability,
        Actor::Agent {
            agent: "provider-test"
                .parse::<AgentId>()
                .expect("valid agent fixture"),
        },
        "trace-test"
            .parse::<TraceId>()
            .expect("valid trace fixture"),
        input,
    );
    AuthorizationGate::new()
        .authorize(
            proposal,
            provider.parse().expect("valid provider fixture"),
            "decision-test".to_owned(),
            "broker-test"
                .parse::<PrincipalId>()
                .expect("valid principal fixture"),
            "policy-test".to_owned(),
            constraints,
        )
        .expect("test broker authorizes bounded fixture")
}

fn http_constraints(authority: String, method: &str) -> ExecutionConstraints {
    ExecutionConstraints {
        timeout_ms: 5_000,
        max_output_bytes: 1024 * 1024,
        http: Some(HttpConstraints {
            allowed_hosts: vec![authority],
            allowed_methods: vec![method.to_owned()],
            max_requests: 1,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 64 * 1024,
            allow_plaintext_loopback: true,
        }),
        storage: None,
    }
}

fn mock_http(response: &[u8]) -> (String, Receiver<Vec<u8>>, thread::JoinHandle<()>) {
    let response = response.to_vec();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback fixture");
    let address = listener.local_addr().expect("fixture address");
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept fixture request");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set fixture timeout");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        let mut expected = None;
        loop {
            if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let body_bytes = content_length(&request[..header_end + 4]);
                let complete = header_end + 4 + body_bytes;
                expected = Some(complete);
                if request.len() >= complete {
                    break;
                }
            }
            let read = stream.read(&mut buffer).expect("read fixture request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        if let Some(expected) = expected {
            request.truncate(expected);
        }
        sender.send(request).expect("record fixture request");
        stream.write_all(&response).expect("write fixture response");
        stream.flush().expect("flush fixture response");
    });
    (format!("127.0.0.1:{}", address.port()), receiver, handle)
}

/// Accepts one request, records it, and never answers, so the call is dispatched but unresolved.
fn mock_http_stalled() -> (String, Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback fixture");
    let address = listener.local_addr().expect("fixture address");
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept fixture request");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set fixture timeout");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        while request.windows(4).all(|window| window != b"\r\n\r\n") {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => request.extend_from_slice(&buffer[..read]),
            }
        }
        #[allow(
            clippy::let_underscore_must_use,
            reason = "this fixture exists to hang rather than to be read; a test that gave up on \
                      the channel and dropped the receiver is a normal way for this send to fail"
        )]
        let _ = sender.send(request);
        // Hold the connection open past the invocation deadline without ever responding.
        thread::sleep(Duration::from_secs(5));
    });
    (format!("127.0.0.1:{}", address.port()), receiver)
}

fn content_length(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_generic_wasi_imports() {
    let error = BrokerProviderRegistry::load(
        [host_fixture("wasi-import.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect_err("the broker linker must expose no generic WASI imports");

    match error {
        BrokerHostError::Instantiate { source, .. } => {
            assert!(
                format!("{source:#}").contains("wasi:io/poll@0.2.0"),
                "link failure must identify the unsupported WASI package"
            );
        }
        other => panic!("expected an import-link failure, got {other}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn loads_http_provider_and_executes_one_authorized_request() {
    let registry = BrokerProviderRegistry::load(
        [fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider loads without host calls during describe");
    let metrics = registry.metrics();
    let (authority, request, server) = mock_http(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Value: one\r\nX-Value: two\r\nSet-Cookie: secret=session\r\nWWW-Authenticate: secret\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
    );
    let capability = "http-probe.fetch"
        .parse()
        .expect("valid capability fixture");
    let output = registry
        .invoke(
            authorized(
                capability,
                json!({
                    "uri": format!("http://{authority}/resource?visible=no"),
                    "method": "PATCH",
                    "headers": [
                        {"name": "x-probe", "value": "one"},
                        {"name": "x-probe", "value": "two"}
                    ],
                    "body": "payload"
                }),
                http_constraints(authority.clone(), "PATCH"),
            ),
            None,
        )
        .await
        .expect("authorized HTTP invocation succeeds");

    assert_eq!(output.provider.as_str(), "http-probe");
    assert_eq!(output.output["status"], 200);
    assert_eq!(output.output["bodyBytes"], 11);
    assert_eq!(output.output["headerCount"], 4);
    // The probe returns the response body itself: base64 always, plus decoded text when it is UTF-8.
    assert_eq!(output.output["body"], "eyJvayI6dHJ1ZX0=");
    assert_eq!(output.output["bodyText"], r#"{"ok":true}"#);
    assert_eq!(output.output["bodyTruncated"], false);
    assert_eq!(output.http_calls.len(), 1);
    assert_eq!(output.http_calls[0].method, "PATCH");
    assert_eq!(output.http_calls[0].authority, authority);
    assert_eq!(output.http_calls[0].status, Some(200));
    let stats = metrics.snapshot();
    assert_eq!(stats.providers_loaded, 1);
    assert_eq!(stats.invocations_started, 1);
    assert_eq!(stats.invocations_succeeded, 1);
    assert_eq!(stats.invocations_failed, 0);
    assert_eq!(stats.http_requests, 1);
    assert!(stats.http_request_bytes > 0);
    assert!(stats.http_response_bytes > 0);
    assert_eq!(stats.active_stores, 0);
    assert!(stats.fuel_consumed > 0);
    let request = request.recv().expect("fixture request recorded");
    assert!(request.starts_with(b"PATCH /resource?visible=no HTTP/1.1\r\n"));
    assert!(request.ends_with(b"\r\n\r\npayload"));
    assert_eq!(
        String::from_utf8_lossy(&request)
            .lines()
            .filter(|line| line.to_ascii_lowercase().starts_with("x-probe:"))
            .count(),
        2
    );
    server.join().expect("fixture server exits");
}

#[tokio::test(flavor = "multi_thread")]
async fn jsonplaceholder_read_and_write_use_separate_broker_grants() {
    let registry = BrokerProviderRegistry::load(
        [fixture("jsonplaceholder-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("JSONPlaceholder provider loads without description-time HTTP");

    let get_body = br#"{"userId":2,"id":7,"title":"mock title","body":"mock body"}"#;
    let get_response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        get_body.len(),
        String::from_utf8_lossy(get_body)
    );
    let (get_authority, get_request, get_server) = mock_http(get_response.as_bytes());
    let get = registry
        .invoke(
            authorized(
                "jsonplaceholder.posts.get"
                    .parse()
                    .expect("valid get capability"),
                json!({
                    "postId": 7,
                    "endpoint": format!("http://{get_authority}")
                }),
                http_constraints(get_authority.clone(), "GET"),
            ),
            None,
        )
        .await
        .expect("authorized JSONPlaceholder read succeeds");
    assert_eq!(get.output["post"]["id"], 7);
    assert_eq!(get.http_calls.len(), 1);
    assert_eq!(get.http_calls[0].method, "GET");
    assert!(
        get_request
            .recv()
            .expect("GET request recorded")
            .starts_with(b"GET /posts/7 HTTP/1.1\r\n")
    );
    get_server.join().expect("GET fixture server exits");

    let create_body = br#"{"userId":3,"id":101,"title":"created title","body":"created body"}"#;
    let create_response = format!(
        "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        create_body.len(),
        String::from_utf8_lossy(create_body)
    );
    let (create_authority, create_request, create_server) = mock_http(create_response.as_bytes());
    let create = registry
        .invoke(
            authorized(
                "jsonplaceholder.posts.create"
                    .parse()
                    .expect("valid create capability"),
                json!({
                    "userId": 3,
                    "title": "created title",
                    "body": "created body",
                    "endpoint": format!("http://{create_authority}")
                }),
                http_constraints(create_authority.clone(), "POST"),
            ),
            None,
        )
        .await
        .expect("authorized JSONPlaceholder write succeeds");
    assert_eq!(create.output["post"]["id"], 101);
    assert_eq!(create.http_calls.len(), 1);
    assert_eq!(create.http_calls[0].method, "POST");
    let request = create_request.recv().expect("POST request recorded");
    assert!(request.starts_with(b"POST /posts HTTP/1.1\r\n"));
    let request_text = String::from_utf8_lossy(&request).to_ascii_lowercase();
    assert!(!request_text.contains("authorization:"));
    assert!(!request_text.contains("cookie:"));
    let body_offset = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("POST headers terminate")
        + 4;
    assert_eq!(
        serde_json::from_slice::<Value>(&request[body_offset..]).expect("POST body is JSON"),
        json!({"userId": 3, "title": "created title", "body": "created body"})
    );
    create_server.join().expect("POST fixture server exits");
}

#[tokio::test(flavor = "multi_thread")]
async fn skylight_private_manifest_is_exact_and_a_nonmatching_grant_denies_before_network() {
    let registry = BrokerProviderRegistry::load(
        [fixture("skylight-private-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("Skylight private provider loads without description-time HTTP");

    let manifest = registry.manifests().next().expect("one manifest is loaded");
    assert_eq!(manifest.id.as_str(), "skylight-private");
    assert!(manifest.command_words.is_empty());
    assert_eq!(manifest.capabilities.len(), 2);
    assert_eq!(
        manifest
            .capabilities
            .iter()
            .map(|capability| capability.id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "skylight.private.account.read",
            "skylight.private.frames.list",
        ]
    );
    let empty_schema = json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    });
    for capability in &manifest.capabilities {
        assert_eq!(capability.effect, EffectKind::ReadOnly);
        assert_eq!(capability.risk, RiskLevel::Medium);
        assert_eq!(capability.idempotency, Idempotency::Idempotent);
        assert_eq!(capability.input_schema, empty_schema);
    }

    // The guest has no endpoint override. A grant for any authority other than the one fixed in the
    // component is rejected during host validation, before DNS or a connection can be attempted.
    let failure = registry
        .invoke(
            authorized_for(
                "skylight-private",
                "skylight.private.account.read"
                    .parse()
                    .expect("valid Skylight capability"),
                json!({}),
                http_constraints("not-skylight.invalid".to_owned(), "GET"),
            ),
            None,
        )
        .await
        .expect_err("a nonmatching HTTP authority grant must fail");
    assert!(matches!(
        failure.error.as_ref(),
        BrokerHostError::HostCallRejected {
            reason: "denied",
            ..
        }
    ));
    assert!(
        failure.http_calls.is_empty(),
        "a pre-dispatch denial must leave no HTTP call evidence"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn denies_http_when_authorization_has_no_http_grant() {
    let registry = BrokerProviderRegistry::load(
        [fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider loads");
    let capability = "http-probe.fetch"
        .parse()
        .expect("valid capability fixture");
    let error = registry
        .invoke(
            authorized(
                capability,
                json!({"uri": "https://example.com/"}),
                ExecutionConstraints::default(),
            ),
            None,
        )
        .await
        .expect_err("missing HTTP authorization must fail")
        .error;

    assert!(matches!(
        error.as_ref(),
        BrokerHostError::HostCallRejected {
            reason: "denied",
            ..
        }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_a_destination_outside_the_exact_authority_grant() {
    let registry = BrokerProviderRegistry::load(
        [fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider loads");
    let capability = "http-probe.fetch"
        .parse()
        .expect("valid capability fixture");
    let error = registry
        .invoke(
            authorized(
                capability,
                json!({"uri": "http://127.0.0.1:9/"}),
                http_constraints("127.0.0.1:10".to_owned(), "GET"),
            ),
            None,
        )
        .await
        .expect_err("different loopback port must be denied before connection")
        .error;

    assert!(matches!(
        error.as_ref(),
        BrokerHostError::HostCallRejected {
            reason: "denied",
            ..
        }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_guest_control_of_authorization_headers() {
    let registry = BrokerProviderRegistry::load(
        [fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider loads");
    let capability = "http-probe.fetch"
        .parse()
        .expect("valid capability fixture");
    let error = registry
        .invoke(
            authorized(
                capability,
                json!({
                    "uri": "http://127.0.0.1:9/",
                    "headers": [{"name": "authorization", "value": "Bearer secret"}]
                }),
                http_constraints("127.0.0.1:9".to_owned(), "GET"),
            ),
            None,
        )
        .await
        .expect_err("guest authorization header must be rejected before connection")
        .error;

    assert!(matches!(
        error.as_ref(),
        BrokerHostError::HostCallRejected {
            reason: "invalid-http-request",
            ..
        }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn guest_code_cannot_mask_a_policy_rejection() {
    let registry = BrokerProviderRegistry::load(
        [fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider loads");
    let capability = "http-probe.fetch"
        .parse()
        .expect("valid capability fixture");
    let error = registry
        .invoke(
            authorized(
                capability,
                json!({
                    "uri": "http://127.0.0.1:9/",
                    "catchError": true
                }),
                http_constraints("127.0.0.1:10".to_owned(), "GET"),
            ),
            None,
        )
        .await
        .expect_err("host rejection remains terminal after the guest catches the WIT error")
        .error;

    assert!(matches!(
        error.as_ref(),
        BrokerHostError::HostCallRejected {
            reason: "denied",
            ..
        }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn enforces_response_bytes_while_streaming() {
    let registry = BrokerProviderRegistry::load(
        [fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider loads");
    let body = "x".repeat(512);
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let (authority, _request, server) = mock_http(response.as_bytes());
    let capability = "http-probe.fetch"
        .parse()
        .expect("valid capability fixture");
    let mut constraints = http_constraints(authority.clone(), "GET");
    constraints
        .http
        .as_mut()
        .expect("HTTP fixture grant")
        .max_response_bytes = 128;
    let error = registry
        .invoke(
            authorized(
                capability,
                json!({"uri": format!("http://{authority}/large")}),
                constraints,
            ),
            None,
        )
        .await
        .expect_err("oversized response must fail the invocation")
        .error;

    assert!(matches!(
        error.as_ref(),
        BrokerHostError::HostCallRejected {
            reason: "byte-limit",
            ..
        }
    ));
    server.join().expect("fixture server exits");
}

#[tokio::test(flavor = "multi_thread")]
async fn returns_redirects_without_following_them() {
    let registry = BrokerProviderRegistry::load(
        [fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider loads");
    let (authority, _request, server) = mock_http(
        b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    let capability = "http-probe.fetch"
        .parse()
        .expect("valid capability fixture");
    let output = registry
        .invoke(
            authorized(
                capability,
                json!({"uri": format!("http://{authority}/redirect")}),
                http_constraints(authority, "GET"),
            ),
            None,
        )
        .await
        .expect("redirect response itself is returned");

    assert_eq!(output.output["status"], 302);
    assert_eq!(output.http_calls.len(), 1);
    server.join().expect("fixture server exits");
}

#[tokio::test(flavor = "multi_thread")]
async fn broker_host_also_runs_import_free_components() {
    let registry =
        BrokerProviderRegistry::load([fixture("echo-provider.wasm")], BrokerHostLimits::default())
            .await
            .expect("import-free provider loads in the broker linker");
    let capability = "echo.echo".parse().expect("valid capability fixture");
    let output = registry
        .invoke(
            authorized(
                capability,
                json!({"message": "hello"}),
                ExecutionConstraints::default(),
            ),
            None,
        )
        .await
        .expect("import-free provider runs without an HTTP grant");

    assert_eq!(output.output, json!({"message": "hello"}));
    assert!(output.http_calls.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_authorization_bound_to_a_different_provider() {
    let registry =
        BrokerProviderRegistry::load([fixture("echo-provider.wasm")], BrokerHostLimits::default())
            .await
            .expect("echo provider loads");
    let capability = "echo.echo".parse().expect("valid capability fixture");
    let error = registry
        .invoke(
            authorized_for(
                "http-probe",
                capability,
                json!({"message": "hello"}),
                ExecutionConstraints::default(),
            ),
            None,
        )
        .await
        .expect_err("authorization cannot be retargeted to the routed provider")
        .error;
    assert!(matches!(
        error.as_ref(),
        BrokerHostError::AuthorizedProviderMismatch { .. }
    ));
}

#[test]
fn broker_bindings_mirror_the_immutable_packages() {
    assert_eq!(
        PROVIDER_WIT,
        include_str!("../../dekopon-provider-sdk/wit/provider.wit")
    );
    assert_eq!(HTTP_WIT, include_str!("../../../wit/http/http.wit"));
    assert_eq!(
        STORAGE_WIT,
        include_str!("../../../wit/storage/storage.wit")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_zero_wasm_resource_ceilings() {
    let limits = BrokerHostLimits {
        max_memories: 0,
        ..BrokerHostLimits::default()
    };
    let error = BrokerProviderRegistry::load([fixture("echo-provider.wasm")], limits)
        .await
        .expect_err("zero store ceiling must fail");
    assert!(matches!(
        error,
        BrokerHostError::InvalidLimit {
            name: "max_memories"
        }
    ));
}

/// The published artifact digest describes the exact buffer Cranelift compiled.
#[tokio::test(flavor = "multi_thread")]
async fn artifact_digest_describes_the_compiled_buffer() {
    let source = fixture("echo-provider.wasm");
    let registry = BrokerProviderRegistry::load([source.clone()], BrokerHostLimits::default())
        .await
        .expect("provider loads");
    let metadata = registry
        .loaded_provider_metadata()
        .next()
        .expect("one loaded provider");

    let bytes = std::fs::read(&source).expect("read artifact");
    let digest = Sha256::digest(&bytes);
    let expected = digest.iter().fold(String::new(), |mut text, byte| {
        use std::fmt::Write as _;
        write!(&mut text, "{byte:02x}").expect("writing to a String cannot fail");
        text
    });
    assert_eq!(metadata.artifact_sha256, expected);
    assert_eq!(metadata.artifact_bytes, bytes.len() as u64);
}

/// Loading a command-word provider proves the export statically instead of instantiating twice.
#[tokio::test(flavor = "multi_thread")]
async fn a_command_word_provider_is_instantiated_once_at_load() {
    let registry = BrokerProviderRegistry::load(
        [fixture("memory-reservation-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("command-word provider loads");
    assert_eq!(registry.command_words(), vec!["recall".to_owned()]);

    let stats = registry.metrics().snapshot();
    assert_eq!(
        stats.component_instantiations, 1,
        "describe is the only instantiation a load needs"
    );
    assert_eq!(stats.stores_created, 1, "{stats:?}");
}

/// An aggregate ceiling below one store could never admit an invocation.
#[tokio::test(flavor = "multi_thread")]
async fn rejects_an_aggregate_ceiling_smaller_than_one_store() {
    let limits = BrokerHostLimits::default();
    let options = BrokerHostOptions {
        max_total_memory_bytes: Some(limits.max_memory_bytes - 1),
        ..BrokerHostOptions::default()
    };
    let error = BrokerProviderRegistry::load_with_options(
        [fixture("echo-provider.wasm")],
        limits,
        None,
        &options,
    )
    .await
    .expect_err("an unusable aggregate ceiling must fail at load");
    assert!(
        matches!(
            error,
            BrokerHostError::InvalidLimit {
                name: "max_total_memory_bytes"
            }
        ),
        "{error:?}"
    );
}

/// A second store is refused rather than OOM-killed once the aggregate ceiling is reserved.
#[tokio::test(flavor = "multi_thread")]
async fn refuses_a_store_beyond_the_aggregate_memory_ceiling() {
    let limits = BrokerHostLimits::default();
    let options = BrokerHostOptions {
        // Exactly one live store fits.
        max_total_memory_bytes: Some(limits.max_memory_bytes),
        ..BrokerHostOptions::default()
    };
    let registry = std::sync::Arc::new(
        BrokerProviderRegistry::load_with_options(
            [fixture("http-probe-provider.wasm")],
            limits,
            None,
            &options,
        )
        .await
        .expect("provider loads under an aggregate ceiling"),
    );

    let (authority, request) = mock_http_stalled();
    let mut constraints = http_constraints(authority.clone(), "GET");
    constraints.timeout_ms = 2_000;
    let holding = tokio::spawn({
        let registry = std::sync::Arc::clone(&registry);
        let authority = authority.clone();
        async move {
            registry
                .invoke(
                    authorized(
                        "http-probe.fetch".parse().expect("capability"),
                        json!({"uri": format!("http://{authority}/stalled")}),
                        constraints,
                    ),
                    None,
                )
                .await
        }
    });
    // The request bytes only arrive once the guest is inside its host call, which proves the first
    // store is alive and holding the whole reservation.
    request
        .recv_timeout(Duration::from_secs(5))
        .expect("stalled fixture receives the first request");

    let error = registry
        .invoke(
            authorized(
                "http-probe.fetch".parse().expect("capability"),
                json!({"uri": format!("http://{authority}/second")}),
                http_constraints(authority.clone(), "GET"),
            ),
            None,
        )
        .await
        .expect_err("a second concurrent store exceeds the aggregate ceiling")
        .error;
    assert!(
        matches!(
            error.as_ref(),
            BrokerHostError::MemoryBudgetExhausted { .. }
        ),
        "{error:?}"
    );

    let held = holding.await.expect("held invocation joins");
    assert!(held.is_err(), "the stalled invocation must time out");
    // The refused store released nothing it never took, and the held one released everything.
    registry
        .invoke(
            authorized(
                "http-probe.fetch".parse().expect("capability"),
                json!({"uri": format!("http://{authority}/third")}),
                http_constraints(authority, "GET"),
            ),
            None,
        )
        .await
        .expect_err("the fixture never answers, but the store is admitted");
}

/// A warm compilation cache serves a later start from the same directory.
#[tokio::test(flavor = "multi_thread")]
async fn a_persistent_compilation_cache_serves_a_second_load() {
    let directory = tempfile::tempdir().expect("cache directory");
    let options = BrokerHostOptions {
        compile_cache_dir: Some(directory.path().canonicalize().expect("canonical cache")),
        ..BrokerHostOptions::default()
    };
    let cold = BrokerProviderRegistry::load_with_options(
        [fixture("echo-provider.wasm")],
        BrokerHostLimits::default(),
        None,
        &options,
    )
    .await
    .expect("cold load populates the cache");
    let cold_digest = cold
        .loaded_provider_metadata()
        .next()
        .expect("one provider")
        .artifact_sha256;

    let warm = BrokerProviderRegistry::load_with_options(
        [fixture("echo-provider.wasm")],
        BrokerHostLimits::default(),
        None,
        &options,
    )
    .await
    .expect("warm load reads the cache");
    let warm_metadata = warm
        .loaded_provider_metadata()
        .next()
        .expect("one provider");
    // The digest is of the artifact bytes, never of a cache entry, so a hit cannot change it.
    assert_eq!(warm_metadata.artifact_sha256, cold_digest);

    let capability = "echo.echo".parse().expect("valid capability fixture");
    let output = warm
        .invoke(
            authorized(capability, json!({"message": "warm"}), constraints_5s()),
            None,
        )
        .await
        .expect("a cached component still invokes");
    assert_eq!(output.output["message"], json!("warm"));
}

fn constraints_5s() -> ExecutionConstraints {
    ExecutionConstraints {
        timeout_ms: 5_000,
        max_output_bytes: 4_096,
        http: None,
        storage: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_authorization_that_exceeds_host_ceilings() {
    let limits = BrokerHostLimits {
        max_timeout: Duration::from_millis(100),
        ..BrokerHostLimits::default()
    };
    let registry = BrokerProviderRegistry::load([fixture("echo-provider.wasm")], limits)
        .await
        .expect("provider loads beneath valid host ceilings");
    let capability = "echo.echo".parse().expect("valid capability fixture");
    let error = registry
        .invoke(
            authorized(
                capability,
                json!({"message": "hello"}),
                ExecutionConstraints {
                    timeout_ms: 101,
                    ..ExecutionConstraints::default()
                },
            ),
            None,
        )
        .await
        .expect_err("authorization cannot widen host timeout")
        .error;
    assert!(matches!(
        error.as_ref(),
        BrokerHostError::AuthorizationExceedsHostLimit {
            field: "timeout_ms"
        }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dispatched_call_survives_a_failed_invocation_as_outcome_unknown() {
    let registry = BrokerProviderRegistry::load(
        [fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider loads");
    let (authority, received) = mock_http_stalled();
    let capability = "http-probe.fetch"
        .parse()
        .expect("valid capability fixture");
    let constraints = ExecutionConstraints {
        timeout_ms: 750,
        ..http_constraints(authority.clone(), "GET")
    };
    let failure = registry
        .invoke(
            authorized(
                capability,
                json!({"uri": format!("http://{authority}/")}),
                constraints,
            ),
            None,
        )
        .await
        .expect_err("an unanswered request cannot succeed");

    let wire = received.recv().expect("fixture request recorded");
    assert!(
        wire.starts_with(b"GET /"),
        "the request must have left the host before the failure"
    );
    assert_eq!(
        failure.http_calls.len(),
        1,
        "a dispatched call must survive the failure it precedes"
    );
    assert_eq!(failure.http_calls[0].method, "GET");
    assert_eq!(failure.http_calls[0].authority, authority);
    assert_eq!(
        failure.http_calls[0].status, None,
        "a call that never received a response records no status"
    );
}

/// Serves a fixed sequence of responses, one connection each, recording every request.
///
/// A conditional write is a two-call capability (a pre-read pins the etag the write then carries),
/// so a single-response mock cannot exercise one. Responses carry `Connection: close`, forcing the
/// client onto a fresh connection per call.
fn mock_http_sequence(
    responses: Vec<Vec<u8>>,
) -> (String, Receiver<Vec<u8>>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback fixture");
    let address = listener.local_addr().expect("fixture address");
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = listener.accept().expect("accept fixture request");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set fixture timeout");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            let mut expected = None;
            loop {
                if let Some(header_end) =
                    request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    let complete = header_end + 4 + content_length(&request[..header_end + 4]);
                    expected = Some(complete);
                    if request.len() >= complete {
                        break;
                    }
                }
                let read = stream.read(&mut buffer).expect("read fixture request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            if let Some(expected) = expected {
                request.truncate(expected);
            }
            sender.send(request).expect("record fixture request");
            stream.write_all(&response).expect("write fixture response");
            stream.flush().expect("flush fixture response");
        }
    });
    (format!("127.0.0.1:{}", address.port()), receiver, handle)
}

fn json_http_response(body: &serde_json::Value) -> Vec<u8> {
    let body = body.to_string();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes()
}

/// A response carrying the etag the conditional write pins itself to.
fn etagged_response(etag: &str) -> Vec<u8> {
    let body = "{}";
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nETag: {etag}\r\nContent-Length: \
         {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn conditional_write_constraints(
    authority: String,
    methods: &[&str],
    max_requests: u32,
) -> ExecutionConstraints {
    ExecutionConstraints {
        timeout_ms: 5_000,
        max_output_bytes: 1024 * 1024,
        http: Some(HttpConstraints {
            allowed_hosts: vec![authority],
            allowed_methods: methods.iter().map(|method| (*method).to_owned()).collect(),
            max_requests,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 256 * 1024,
            allow_plaintext_loopback: true,
        }),
        storage: None,
    }
}

/// Two authorized calls in one invocation, which is the shape worth covering in tree.
///
/// `gh.pull-request.approve` used to be the only in-tree capability that did this, and it left with
/// the GitHub provider. `http-probe.conditional-write` replaces it so host coverage of `maxRequests`,
/// per-call evidence, and the host-call limit does not depend on a provider in another repository.
#[tokio::test(flavor = "multi_thread")]
async fn a_two_request_capability_leaves_two_evidence_entries() {
    let registry = BrokerProviderRegistry::load(
        [fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("the probe loads without host calls during describe");
    let (authority, recorded, server) = mock_http_sequence(vec![
        etagged_response("\"v1\""),
        json_http_response(&json!({"written": true})),
    ]);

    let output = registry
        .invoke(
            authorized(
                "http-probe.conditional-write"
                    .parse()
                    .expect("valid capability"),
                json!({"uri": format!("http://{authority}/resource")}),
                conditional_write_constraints(authority.clone(), &["GET", "POST"], 2),
            ),
            None,
        )
        .await
        .expect("authorized two-call conditional write succeeds");

    assert_eq!(output.provider.as_str(), "http-probe");
    assert_eq!(output.output["observedEtag"], "\"v1\"");

    // The trace of what actually happened: a pre-read, then a write pinned to what it observed.
    assert_eq!(output.http_calls.len(), 2);
    assert_eq!(output.http_calls[0].method, "GET");
    assert_eq!(output.http_calls[1].method, "POST");
    let pre_read = String::from_utf8(recorded.recv().expect("pre-read recorded"))
        .expect("fixture request is UTF-8");
    assert!(pre_read.starts_with("GET /resource "), "{pre_read}");
    let write = String::from_utf8(recorded.recv().expect("write recorded"))
        .expect("fixture request is UTF-8");
    assert!(write.starts_with("POST /resource "), "{write}");
    assert!(write.contains("if-match: \"v1\""), "{write}");
    server.join().expect("fixture server exits");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_without_post_authority_is_a_terminal_policy_rejection() {
    let registry = BrokerProviderRegistry::load(
        [fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("the probe loads");
    let (authority, recorded, server) = mock_http_sequence(vec![etagged_response("\"v1\"")]);

    let failure = registry
        .invoke(
            authorized(
                "http-probe.conditional-write"
                    .parse()
                    .expect("valid capability"),
                json!({"uri": format!("http://{authority}/resource")}),
                conditional_write_constraints(authority.clone(), &["GET"], 2),
            ),
            None,
        )
        .await
        .expect_err("a write without POST authority must fail");

    // The denial is terminal even though the guest catches the HTTP error internally, and the
    // evidence still shows exactly what ran: the pre-read happened, the write never did.
    assert!(matches!(
        failure.error.as_ref(),
        BrokerHostError::HostCallRejected {
            reason: "denied",
            ..
        }
    ));
    assert_eq!(failure.http_calls.len(), 1);
    assert_eq!(failure.http_calls[0].method, "GET");
    drop(recorded);
    server.join().expect("fixture server exits");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_two_request_capability_over_its_call_budget_trips_the_host_call_limit() {
    let registry = BrokerProviderRegistry::load(
        [fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("the probe loads");
    let (authority, recorded, server) = mock_http_sequence(vec![etagged_response("\"v1\"")]);

    let failure = registry
        .invoke(
            authorized(
                "http-probe.conditional-write"
                    .parse()
                    .expect("valid capability"),
                json!({"uri": format!("http://{authority}/resource")}),
                conditional_write_constraints(authority.clone(), &["GET", "POST"], 1),
            ),
            None,
        )
        .await
        .expect_err("a second call over a one-call grant must fail");

    assert!(matches!(
        failure.error.as_ref(),
        BrokerHostError::HostCallRejected {
            reason: "host-call-limit",
            ..
        }
    ));
    assert_eq!(failure.http_calls.len(), 1);
    drop(recorded);
    server.join().expect("fixture server exits");
}

/// A component generated against the immutable `dekopon:provider@0.1.0` two-export world loads,
/// invokes, and contributes no command words.
#[tokio::test(flavor = "multi_thread")]
async fn an_actual_provider_v0_1_component_remains_compatible() {
    let registry = BrokerProviderRegistry::load(
        [fixture("provider-v0-1-compat-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("historical provider component loads");

    assert!(
        registry.command_words().is_empty(),
        "the historical world has no resolve-command export"
    );
    let capability = "provider-v0-1-compat.echo"
        .parse::<CapabilityId>()
        .expect("capability");
    let output = registry
        .invoke(
            authorized_for(
                "provider-v0-1-compat",
                capability,
                json!({"historical": true}),
                ExecutionConstraints {
                    timeout_ms: 5_000,
                    max_output_bytes: 4_096,
                    http: None,
                    storage: None,
                },
            ),
            None,
        )
        .await
        .expect("historical provider invokes");
    assert_eq!(output.output, json!({"historical": true}));

    let error = registry
        .resolve_command("gh", &["gh".to_owned()])
        .await
        .expect_err("no historical provider owns this word");
    assert!(
        matches!(error, BrokerHostError::UnknownCommandWord { ref word } if word == "gh"),
        "{error:?}"
    );
}

/// Two providers declaring one capability are reported together, not one restart apart.
#[tokio::test(flavor = "multi_thread")]
async fn conflicting_providers_are_all_reported_in_one_failure() {
    let error = BrokerProviderRegistry::load(
        [fixture("echo-provider.wasm"), fixture("echo-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect_err("one component loaded twice conflicts with itself");

    let BrokerHostError::ConflictingProviders { report } = error else {
        panic!("expected a conflict report, got {error:?}");
    };
    // The same component twice is both a duplicate provider and five duplicate capabilities. A
    // check that returned on the first would have named one of the six.
    assert_eq!(report.providers.len(), 1, "{report:?}");
    assert_eq!(report.capabilities.len(), 5, "{report:?}");
    let rendered = report.to_string();
    assert!(rendered.contains("6 provider conflict(s)"), "{rendered}");
    assert!(rendered.contains("echo.ransom-case"), "{rendered}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_waiting_namespace_lease_never_stalls_timers_or_a_distinct_namespace() {
    let directory = tempfile::tempdir().expect("storage directory");
    let directory = directory
        .path()
        .canonicalize()
        .expect("canonical directory");
    let root = directory.join("root");
    let key = directory.join("key.yaml");
    std::fs::write(
        &key,
        "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
    )
    .expect("write key");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).expect("key mode");
    let limits = StorageLimits {
        lock_timeout_ms: 500,
        ..StorageLimits::default()
    };
    let storage = StorageHost::open(&root, &key, limits).expect("storage host");
    let held = storage
        .grant(probe_storage_grant("lease-held", "slack.t0123abc.uone"))
        .expect("held grant");

    let competing_host = storage.clone();
    let competing = tokio::task::spawn_blocking(move || {
        competing_host.grant(probe_storage_grant(
            "lease-competing",
            "slack.t0123abc.uone",
        ))
    });
    // This timer runs on the only runtime worker while the blocking lease wait continues elsewhere.
    tokio::time::timeout(
        std::time::Duration::from_millis(100),
        tokio::time::sleep(std::time::Duration::from_millis(20)),
    )
    .await
    .expect("lease wait did not stall the runtime timer");

    let distinct_host = storage.clone();
    let distinct = tokio::task::spawn_blocking(move || {
        distinct_host.grant(probe_storage_grant("lease-distinct", "slack.t0123abc.utwo"))
    });
    let distinct = tokio::time::timeout(std::time::Duration::from_millis(200), distinct)
        .await
        .expect("a distinct namespace was not serialized behind the blocked base")
        .expect("blocking task")
        .expect("distinct grant");
    drop(distinct);

    // Cancelling the waiter does not cancel a native syscall/job. The held grant is released only
    // now; the blocking task then drains and drops whichever grant it obtained.
    competing.abort();
    drop(held);
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
}

fn probe_storage_grant(invocation: &str, subject: &str) -> StorageGrantRequest {
    StorageGrantRequest::new(
        invocation.parse().expect("invocation"),
        "storage-probe.run".parse().expect("capability"),
        "storage-probe".parse().expect("provider"),
        StorageInterface::DurableFiles,
        StorageAccess::ReadWrite,
        StorageNamespace::Chat,
        "provider-test".parse().expect("agent"),
        subject.parse().expect("subject"),
        "slack",
        "probe-transport",
        "c0123abc",
        "c0123abc:1712345678.000100",
        ContinuityPolicy::Stable,
        b"probe-authority".to_vec(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn durable_storage_probe_runs_under_one_exact_consumed_grant() {
    let directory = tempfile::tempdir().expect("storage directory");
    let directory = directory
        .path()
        .canonicalize()
        .expect("canonical storage directory");
    let root = directory.join("root");
    let key = directory.join("key.yaml");
    std::fs::write(
        &key,
        "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
    )
    .expect("write key");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).expect("key mode");
    let storage = StorageHost::open(&root, &key, StorageLimits::default()).expect("storage host");
    let registry = BrokerProviderRegistry::load_with_storage(
        [fixture("storage-probe-provider.wasm")],
        BrokerHostLimits::default(),
        Some(storage.clone()),
    )
    .await
    .expect("probe loads");
    let capability = "storage-probe.run"
        .parse::<CapabilityId>()
        .expect("capability");
    let constraints = ExecutionConstraints {
        timeout_ms: 10_000,
        max_output_bytes: 64 * 1024,
        http: None,
        storage: Some(StorageConstraints {
            interface: StorageInterface::DurableFiles,
            access: StorageAccess::ReadWrite,
            namespace: StorageNamespace::Chat,
        }),
    };
    let grant = storage
        .grant(StorageGrantRequest::new(
            "invoke-test".parse().expect("invocation"),
            capability.clone(),
            "storage-probe".parse().expect("provider"),
            StorageInterface::DurableFiles,
            StorageAccess::ReadWrite,
            StorageNamespace::Chat,
            "provider-test".parse().expect("agent"),
            "slack.t0123abc.u9xyz".parse().expect("subject"),
            "slack",
            "probe-transport",
            "c0123abc",
            "c0123abc:1712345678.000100",
            ContinuityPolicy::Stable,
            b"probe-authority".to_vec(),
        ))
        .expect("grant");
    let output = registry
        .invoke_with_storage(
            authorized_for("storage-probe", capability, json!({}), constraints),
            None,
            Some(grant),
        )
        .await
        .expect("probe succeeds");
    assert_eq!(output.output["clocksCalled"], true);
    assert!(output.storage.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_wasm_storage_denials_are_sticky_and_commit_nothing() {
    for (_index, mode, interface, access, max_calls, reason) in [
        (
            0,
            "read-only-denial",
            StorageInterface::DurableFiles,
            StorageAccess::ReadOnly,
            StorageLimits::default().max_host_calls_per_invocation,
            "denied",
        ),
        (
            1,
            "wrong-interface-denial",
            StorageInterface::Jsonl,
            StorageAccess::ReadWrite,
            StorageLimits::default().max_host_calls_per_invocation,
            "denied",
        ),
        (
            2,
            "quota-denial",
            StorageInterface::DurableFiles,
            StorageAccess::ReadWrite,
            StorageLimits::default().max_host_calls_per_invocation,
            "quota",
        ),
        (
            3,
            "budget-denial",
            StorageInterface::DurableFiles,
            StorageAccess::ReadWrite,
            1,
            "quota",
        ),
        (
            4,
            "drop-after-denial",
            StorageInterface::DurableFiles,
            StorageAccess::ReadWrite,
            StorageLimits::default().max_host_calls_per_invocation,
            "quota",
        ),
    ] {
        let directory = tempfile::tempdir().expect("storage directory");
        let directory = directory
            .path()
            .canonicalize()
            .expect("canonical storage directory");
        let root = directory.join("root");
        let key = directory.join("key.yaml");
        std::fs::write(
            &key,
            "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
        )
        .expect("write key");
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).expect("key mode");
        let storage = StorageHost::open(
            &root,
            &key,
            StorageLimits {
                max_host_calls_per_invocation: max_calls,
                ..StorageLimits::default()
            },
        )
        .expect("storage host");
        let registry = BrokerProviderRegistry::load_with_storage(
            [fixture("storage-probe-provider.wasm")],
            BrokerHostLimits::default(),
            Some(storage.clone()),
        )
        .await
        .expect("probe loads");
        let capability = "storage-probe.run"
            .parse::<CapabilityId>()
            .expect("capability");
        let constraints = ExecutionConstraints {
            timeout_ms: 10_000,
            max_output_bytes: 64 * 1024,
            http: None,
            storage: Some(StorageConstraints {
                interface,
                access,
                namespace: StorageNamespace::Chat,
            }),
        };
        let grant = storage
            .grant(StorageGrantRequest::new(
                "invoke-test".parse().expect("invocation"),
                capability.clone(),
                "storage-probe".parse().expect("provider"),
                interface,
                access,
                StorageNamespace::Chat,
                "provider-test".parse().expect("agent"),
                "slack.t0123abc.u9xyz".parse().expect("subject"),
                "slack",
                "probe-transport",
                "c0123abc",
                "c0123abc:1712345678.000100",
                ContinuityPolicy::Stable,
                b"probe-authority".to_vec(),
            ))
            .expect("grant");
        let before = snapshot_storage_tree(&root);
        let failure = registry
            .invoke_with_storage(
                authorized_for(
                    "storage-probe",
                    capability,
                    json!({"mode": mode}),
                    constraints,
                ),
                None,
                Some(grant),
            )
            .await
            .expect_err("a caught storage denial remains terminal");
        assert!(
            matches!(
                failure.error.as_ref(),
                BrokerHostError::StorageCallRejected {
                    reason: actual,
                    ..
                } if *actual == reason
            ),
            "mode {mode} returned {:?}",
            failure.error
        );
        assert_eq!(
            snapshot_storage_tree(&root),
            before,
            "mode {mode} committed provisional storage after a terminal denial"
        );
    }
}

fn snapshot_storage_tree(root: &Path) -> Vec<(PathBuf, u32, u64, Vec<u8>)> {
    fn visit(root: &Path, path: &Path, output: &mut Vec<(PathBuf, u32, u64, Vec<u8>)>) {
        let mut entries = std::fs::read_dir(path)
            .expect("snapshot directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("snapshot entries");
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).expect("snapshot metadata");
            let relative = path
                .strip_prefix(root)
                .expect("relative path")
                .to_path_buf();
            let contents = if metadata.is_file() {
                std::fs::read(&path).expect("snapshot file")
            } else {
                Vec::new()
            };
            output.push((relative, metadata.mode(), metadata.len(), contents));
            if metadata.is_dir() {
                visit(root, &path, output);
            }
        }
    }
    let mut output = Vec::new();
    visit(root, root, &mut output);
    output
}
