use std::{
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    time::Duration,
};

use dekopon_broker_host::{
    BrokerHostError, BrokerHostLimits, BrokerHostOptions, BrokerProviderRegistry,
    CommandRunOutcome, HARD_MAX_PROVIDER_COMPONENT_BYTES, HTTP_WIT, LockedProviderSource,
    PROVIDER_WIT, STORAGE_WIT,
};
use dekopon_capability::{
    AuthorizedInvocation, ExecutionConstraints, HttpConstraints, ProposedInvocation, StorageAccess,
    StorageConstraints, StorageInterface, StorageNamespace, broker::AuthorizationGate,
};
use dekopon_core::{Actor, AgentId, CapabilityId, InvocationId, PrincipalId, TraceId};
use dekopon_storage_host::{ContinuityPolicy, StorageGrantRequest, StorageHost, StorageLimits};
use dekopon_test_support::{LoopbackServer, provider_fixture, snapshot_tree};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

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
        secret_use: None,
    }
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
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider loads without host calls during describe");
    let metrics = registry.metrics();
    let server = LoopbackServer::once(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Value: one\r\nX-Value: two\r\nSet-Cookie: secret=session\r\nWWW-Authenticate: secret\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
    );
    let authority = server.authority().to_owned();
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
    let request = server.request();
    assert!(request.starts_with(b"PATCH /resource?visible=no HTTP/1.1\r\n"));
    assert!(request.ends_with(b"\r\n\r\npayload"));
    assert_eq!(
        String::from_utf8_lossy(&request)
            .lines()
            .filter(|line| line.to_ascii_lowercase().starts_with("x-probe:"))
            .count(),
        2
    );
    server.join();
}

#[tokio::test(flavor = "multi_thread")]
async fn jsonplaceholder_read_and_write_use_separate_broker_grants() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("jsonplaceholder-provider.wasm")],
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
    let get_server = LoopbackServer::once(get_response.as_bytes());
    let get_authority = get_server.authority().to_owned();
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
        get_server
            .request()
            .starts_with(b"GET /posts/7 HTTP/1.1\r\n")
    );
    get_server.join();

    let create_body = br#"{"userId":3,"id":101,"title":"created title","body":"created body"}"#;
    let create_response = format!(
        "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        create_body.len(),
        String::from_utf8_lossy(create_body)
    );
    let create_server = LoopbackServer::once(create_response.as_bytes());
    let create_authority = create_server.authority().to_owned();
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
    let request = create_server.request();
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
    create_server.join();
}

#[tokio::test(flavor = "multi_thread")]
async fn denies_http_when_authorization_has_no_http_grant() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
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
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider loads");
    let capability = "http-probe.fetch"
        .parse()
        .expect("valid capability fixture");
    let failure = registry
        .invoke(
            authorized(
                capability,
                json!({"uri": "http://127.0.0.1:9/"}),
                http_constraints("127.0.0.1:10".to_owned(), "GET"),
            ),
            None,
        )
        .await
        .expect_err("different loopback port must be denied before connection");

    assert!(matches!(
        failure.error.as_ref(),
        BrokerHostError::HostCallRejected {
            reason: "denied",
            ..
        }
    ));
    assert!(
        failure.http_calls.is_empty(),
        "an authority denial before dispatch must leave no HTTP call evidence"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_guest_control_of_authorization_headers() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
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
        [provider_fixture("http-probe-provider.wasm")],
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
        [provider_fixture("http-probe-provider.wasm")],
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
    let server = LoopbackServer::once(response.as_bytes());
    let authority = server.authority().to_owned();
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
    server.join();
}

#[tokio::test(flavor = "multi_thread")]
async fn returns_redirects_without_following_them() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider loads");
    let server = LoopbackServer::once(
        b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    let authority = server.authority().to_owned();
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
    server.join();
}

#[tokio::test(flavor = "multi_thread")]
async fn broker_host_also_runs_import_free_components() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("echo-provider.wasm")],
        BrokerHostLimits::default(),
    )
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
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("echo-provider.wasm")],
        BrokerHostLimits::default(),
    )
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
    let error = BrokerProviderRegistry::load([provider_fixture("echo-provider.wasm")], limits)
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
    let source = provider_fixture("echo-provider.wasm");
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

/// A provider lock is compared with the same buffer Wasmtime would compile.
#[tokio::test(flavor = "multi_thread")]
async fn a_locked_artifact_digest_is_enforced_at_the_compile_boundary() {
    let source = provider_fixture("echo-provider.wasm");
    let bytes = std::fs::read(&source).expect("read artifact");
    let locked = LockedProviderSource::new(
        source,
        bytes.len() as u64,
        "0".repeat(64),
        "echo".parse().expect("provider ID"),
    )
    .expect("well-formed locked source");

    let error = BrokerProviderRegistry::load_locked_with_options(
        [locked],
        BrokerHostLimits::default(),
        None,
        &BrokerHostOptions::default(),
    )
    .await
    .expect_err("a different locked digest must refuse the component");
    assert!(
        matches!(error, BrokerHostError::ArtifactDigestMismatch { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("provider lock expects"));
}

/// The locked descriptor length is enforced against that same compile buffer.
#[tokio::test(flavor = "multi_thread")]
async fn a_locked_artifact_length_is_enforced_at_the_compile_boundary() {
    let source = provider_fixture("echo-provider.wasm");
    let bytes = std::fs::read(&source).expect("read artifact");
    let digest = Sha256::digest(&bytes)
        .iter()
        .fold(String::new(), |mut text, byte| {
            use std::fmt::Write as _;
            write!(&mut text, "{byte:02x}").expect("writing to a String cannot fail");
            text
        });
    let locked = LockedProviderSource::new(
        source,
        bytes.len() as u64 + 1,
        digest,
        "echo".parse().expect("provider ID"),
    )
    .expect("well-formed locked source");

    let error = BrokerProviderRegistry::load_locked_with_options(
        [locked],
        BrokerHostLimits::default(),
        None,
        &BrokerHostOptions::default(),
    )
    .await
    .expect_err("a different locked length must refuse the component");
    assert!(
        matches!(error, BrokerHostError::ArtifactSizeMismatch { .. }),
        "{error:?}"
    );
}

/// A replaced locked file is refused from descriptor metadata before its oversized body is read.
#[tokio::test(flavor = "multi_thread")]
async fn a_physically_oversized_locked_artifact_is_bounded_before_read() {
    assert!(matches!(
        LockedProviderSource::new(
            "zero.wasm",
            0,
            "0".repeat(64),
            "echo".parse().expect("provider ID")
        ),
        Err(BrokerHostError::InvalidArtifactSize { .. })
    ));

    let directory = tempfile::tempdir().expect("oversized artifact directory");
    let source = directory.path().join("oversized.wasm");
    let file = std::fs::File::create(&source).expect("create sparse artifact");
    file.set_len(HARD_MAX_PROVIDER_COMPONENT_BYTES + 1)
        .expect("size sparse artifact");
    let locked = LockedProviderSource::new(
        source.clone(),
        1,
        "0".repeat(64),
        "echo".parse().expect("provider ID"),
    )
    .expect("well-formed locked source");

    let error = BrokerProviderRegistry::load_locked_with_options(
        [locked],
        BrokerHostLimits::default(),
        None,
        &BrokerHostOptions::default(),
    )
    .await
    .expect_err("descriptor mismatch refuses before reading the sparse body");
    assert!(
        matches!(error, BrokerHostError::ArtifactSizeMismatch { .. }),
        "{error:?}"
    );

    let error = BrokerProviderRegistry::load([source], BrokerHostLimits::default())
        .await
        .expect_err("legacy paths share the hard source ceiling");
    assert!(
        matches!(error, BrokerHostError::ArtifactTooLarge { .. }),
        "{error:?}"
    );
}

/// The provider identity is lock input too, not metadata the component may replace.
#[tokio::test(flavor = "multi_thread")]
async fn a_locked_provider_identity_is_enforced_after_describe() {
    let source = provider_fixture("echo-provider.wasm");
    let bytes = std::fs::read(&source).expect("read artifact");
    let digest = Sha256::digest(&bytes);
    let digest = digest.iter().fold(String::new(), |mut text, byte| {
        use std::fmt::Write as _;
        write!(&mut text, "{byte:02x}").expect("writing to a String cannot fail");
        text
    });
    let locked = LockedProviderSource::new(
        source,
        bytes.len() as u64,
        digest,
        "other".parse().expect("provider ID"),
    )
    .expect("well-formed locked source");

    let error = BrokerProviderRegistry::load_locked_with_options(
        [locked],
        BrokerHostLimits::default(),
        None,
        &BrokerHostOptions::default(),
    )
    .await
    .expect_err("a different locked provider ID must refuse the component");
    assert!(
        matches!(error, BrokerHostError::ProviderIdentityMismatch { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("provider lock expects other"));
}

/// Loading a command-word provider proves the export statically instead of instantiating twice.
#[tokio::test(flavor = "multi_thread")]
async fn a_command_word_provider_is_instantiated_once_at_load() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("memory-reservation-probe-provider.wasm")],
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
    assert_eq!(stats.command_resolutions, 0, "{stats:?}");

    // The first run is the second instantiation, in its own fresh store; this hand-rolled
    // `run-command` guest ignores the piped value rather than refusing it.
    let outcome = registry
        .run_command("recall", &["recall".to_owned()], Some("piped"))
        .await
        .expect("the probe rewrites its word");
    assert!(
        matches!(outcome, CommandRunOutcome::Proposed { .. }),
        "{outcome:?}"
    );
    let stats = registry.metrics().snapshot();
    assert_eq!(stats.component_instantiations, 2, "{stats:?}");
    assert_eq!(stats.stores_created, 2, "{stats:?}");
    assert_eq!(stats.command_resolutions, 1, "{stats:?}");
}

/// The checked-in `memory-reservation-probe` component is the hand-rolled `run-command` guest:
/// no argument parser, values shifted out of argv by hand. Its help page, its proposal, and its
/// decline prove the clap-free baseline at the current package against a real component.
#[tokio::test(flavor = "multi_thread")]
async fn a_hand_rolled_run_command_guest_renders_help_and_proposes() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("memory-reservation-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("hand-rolled command-line provider loads");
    let mut exports = registry
        .loaded_provider_metadata()
        .flat_map(|metadata| metadata.exports.into_iter().map(|item| item.name))
        .collect::<Vec<_>>();
    exports.sort();
    assert_eq!(exports, ["describe", "invoke", "run-command"]);

    let outcome = registry
        .run_command("recall", &["--help".to_owned()], None)
        .await
        .expect("help renders");
    let CommandRunOutcome::Rendered {
        stdout,
        stderr,
        status,
    } = outcome
    else {
        panic!("expected rendered help, got {outcome:?}");
    };
    assert_eq!(status, 0);
    assert!(stdout.starts_with("Usage: recall"), "{stdout:?}");
    assert!(stderr.is_empty(), "{stderr:?}");

    let outcome = registry
        .run_command("recall", &["yesterday".to_owned()], Some("piped"))
        .await
        .expect("the word proposes");
    assert_eq!(
        outcome,
        CommandRunOutcome::Proposed {
            capability: "ordinary.escape".parse().expect("capability"),
            input: json!({}),
        }
    );

    let outcome = registry
        .run_command("recall", &["--verbose".to_owned()], None)
        .await
        .expect("a decline is an outcome, not a host error");
    assert!(
        matches!(
            outcome,
            CommandRunOutcome::Failed { ref error }
                if error.code == "usage" && error.message.contains("--verbose")
        ),
        "{outcome:?}"
    );
}

/// The checked-in `cli-probe` component exports `run-command` and renders through the SDK's
/// `clap` layer: the load reads that export from the component type, the typed
/// `(list<string>, option<string>)` call delivers the piped value, and clap's help page, clap's
/// usage error, a proposal, and a decline each parse as the shared outcome.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_command_provider_renders_help_reads_stdin_and_declines() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("cli-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("command-line provider loads");
    assert_eq!(registry.command_words(), vec!["probe".to_owned()]);
    let mut exports = registry
        .loaded_provider_metadata()
        .flat_map(|metadata| metadata.exports.into_iter().map(|item| item.name))
        .collect::<Vec<_>>();
    exports.sort();
    assert_eq!(exports, ["describe", "invoke", "run-command"]);

    let outcome = registry
        .run_command("probe", &["--help".to_owned()], None)
        .await
        .expect("help renders");
    let CommandRunOutcome::Rendered {
        stdout,
        stderr,
        status,
    } = outcome
    else {
        panic!("expected rendered help, got {outcome:?}");
    };
    assert_eq!(status, 0);
    assert!(stdout.starts_with("Usage: probe <COMMAND>"), "{stdout:?}");
    for subcommand in ["upper", "count", "reverse"] {
        assert!(stdout.contains(&format!("\n  {subcommand} ")), "{stdout:?}");
    }
    assert!(stderr.is_empty(), "{stderr:?}");

    let capability = "cli-probe.count"
        .parse::<CapabilityId>()
        .expect("capability");
    let outcome = registry
        .run_command(
            "probe",
            &["count".to_owned(), "-".to_owned()],
            Some("héllo"),
        )
        .await
        .expect("a piped value proposes");
    assert_eq!(
        outcome,
        CommandRunOutcome::Proposed {
            capability: capability.clone(),
            input: json!({"text": "héllo"}),
        }
    );
    // The proposal is authorized and executed as any other: the loop from word to output closes.
    let output = registry
        .invoke(
            authorized_for(
                "cli-probe",
                capability,
                json!({"text": "héllo"}),
                ExecutionConstraints {
                    timeout_ms: 5_000,
                    max_output_bytes: 4_096,
                    http: None,
                    storage: None,
                    secret_use: None,
                },
            ),
            None,
        )
        .await
        .expect("the proposed capability runs");
    assert_eq!(output.output, json!({"characters": 5}));

    let outcome = registry
        .run_command("probe", &["bogus".to_owned()], None)
        .await
        .expect("a usage error is rendered, not a host error");
    let CommandRunOutcome::Rendered {
        stdout,
        stderr,
        status,
    } = outcome
    else {
        panic!("expected a rendered usage error, got {outcome:?}");
    };
    assert_eq!(status, 2);
    assert!(stdout.is_empty(), "{stdout:?}");
    assert!(
        stderr.starts_with("error: unrecognized subcommand 'bogus'"),
        "{stderr:?}"
    );
    assert!(stderr.contains("\nUsage: probe <COMMAND>\n"), "{stderr:?}");

    let outcome = registry
        .run_command("probe", &["count".to_owned(), "-".to_owned()], None)
        .await
        .expect("a decline is an outcome, not a host error");
    assert!(
        matches!(
            outcome,
            CommandRunOutcome::Failed { ref error }
                if error.code == "usage" && error.message.contains("nothing was piped in")
        ),
        "{outcome:?}"
    );

    let stats = registry.metrics().snapshot();
    assert_eq!(stats.command_resolutions, 4, "{stats:?}");
    assert_eq!(
        stats.component_instantiations, 6,
        "describe, four runs, and one invocation each instantiate once: {stats:?}"
    );
}

/// A word plus its piped value beyond the input bound is refused before a store exists.
#[tokio::test(flavor = "multi_thread")]
async fn command_input_beyond_the_bound_is_refused_before_a_store_exists() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("memory-reservation-probe-provider.wasm")],
        BrokerHostLimits {
            max_input_bytes: 8,
            ..BrokerHostLimits::default()
        },
    )
    .await
    .expect("command-word provider loads");

    let error = registry
        .run_command("recall", &["recall".to_owned()], Some("more than eight"))
        .await
        .expect_err("argv plus stdin exceed the bound");

    assert!(
        matches!(
            error,
            BrokerHostError::CommandInputTooLarge {
                length: 21,
                maximum: 8,
                ..
            }
        ),
        "{error:?}"
    );
    let stats = registry.metrics().snapshot();
    assert_eq!(
        stats.stores_created, 1,
        "only describe built a store: {stats:?}"
    );
    assert_eq!(stats.command_resolutions, 0, "{stats:?}");
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
        [provider_fixture("echo-provider.wasm")],
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
            [provider_fixture("http-probe-provider.wasm")],
            limits,
            None,
            &options,
        )
        .await
        .expect("provider loads under an aggregate ceiling"),
    );

    let stalled = LoopbackServer::stalled();
    let authority = stalled.authority().to_owned();
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
    let first = stalled.request();
    assert!(
        first.starts_with(b"GET /stalled "),
        "the stalled fixture receives the first request"
    );

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
        [provider_fixture("echo-provider.wasm")],
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
        [provider_fixture("echo-provider.wasm")],
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
        secret_use: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_authorization_that_exceeds_host_ceilings() {
    let limits = BrokerHostLimits {
        max_timeout: Duration::from_millis(100),
        ..BrokerHostLimits::default()
    };
    let registry = BrokerProviderRegistry::load([provider_fixture("echo-provider.wasm")], limits)
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
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider loads");
    let stalled = LoopbackServer::stalled();
    let authority = stalled.authority().to_owned();
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

    let wire = stalled.request();
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
        secret_use: None,
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
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("the probe loads without host calls during describe");
    let server = LoopbackServer::sequence(vec![
        etagged_response("\"v1\""),
        json_http_response(&json!({"written": true})),
    ]);
    let authority = server.authority().to_owned();

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
    let pre_read = server.request_text();
    assert!(pre_read.starts_with("GET /resource "), "{pre_read}");
    let write = server.request_text();
    assert!(write.starts_with("POST /resource "), "{write}");
    assert!(write.contains("if-match: \"v1\""), "{write}");
    server.join();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_without_post_authority_is_a_terminal_policy_rejection() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("the probe loads");
    let server = LoopbackServer::sequence(vec![etagged_response("\"v1\"")]);
    let authority = server.authority().to_owned();

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
    server.join();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_two_request_capability_over_its_call_budget_trips_the_host_call_limit() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("the probe loads");
    let server = LoopbackServer::sequence(vec![etagged_response("\"v1\"")]);
    let authority = server.authority().to_owned();

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
    server.join();
}

/// A component generated against the immutable `dekopon:provider@0.1.0` two-export world loads,
/// invokes, and contributes no command words.
#[tokio::test(flavor = "multi_thread")]
async fn an_actual_provider_v0_1_component_remains_compatible() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("provider-v0-1-compat-provider.wasm")],
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
                    secret_use: None,
                },
            ),
            None,
        )
        .await
        .expect("historical provider invokes");
    assert_eq!(output.output, json!({"historical": true}));

    let error = registry
        .run_command("gh", &["gh".to_owned()], None)
        .await
        .expect_err("no historical provider owns this word");
    assert!(
        matches!(error, BrokerHostError::UnknownCommandWord { ref word } if word == "gh"),
        "{error:?}"
    );
}

/// A component generated against the immutable `dekopon:provider@0.2.0` `provider-commands`
/// world loads, invokes, and runs its word through the legacy `resolve-command` export on the
/// same path a `run-command` guest takes.
#[tokio::test(flavor = "multi_thread")]
async fn an_actual_provider_v0_2_component_remains_compatible() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("provider-v0-2-compat-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("historical command-word provider component loads");

    assert_eq!(registry.command_words(), vec!["compat".to_owned()]);
    let capability = "provider-v0-2-compat.echo"
        .parse::<CapabilityId>()
        .expect("capability");
    let output = registry
        .invoke(
            authorized_for(
                "provider-v0-2-compat",
                capability.clone(),
                json!({"historical": true}),
                ExecutionConstraints {
                    timeout_ms: 5_000,
                    max_output_bytes: 4_096,
                    http: None,
                    storage: None,
                    secret_use: None,
                },
            ),
            None,
        )
        .await
        .expect("historical provider invokes");
    assert_eq!(output.output, json!({"historical": true}));

    // The legacy export has no stdin parameter: the piped value is dropped by contract, and the
    // legacy `resolved` answer arrives as a proposal.
    let outcome = registry
        .run_command("compat", &["echo".to_owned()], Some("piped"))
        .await
        .expect("the historical rewrite still runs");
    assert_eq!(
        outcome,
        CommandRunOutcome::Proposed {
            capability,
            input: json!({}),
        }
    );
    let outcome = registry
        .run_command("compat", &["bogus".to_owned()], None)
        .await
        .expect("a decline is an outcome, not a host error");
    assert!(
        matches!(outcome, CommandRunOutcome::Failed { ref error } if error.code == "usage"),
        "{outcome:?}"
    );
    let stats = registry.metrics().snapshot();
    assert_eq!(stats.command_resolutions, 2, "{stats:?}");
}

/// Two providers declaring one capability are reported together, not one restart apart.
#[tokio::test(flavor = "multi_thread")]
async fn conflicting_providers_are_all_reported_in_one_failure() {
    let error = BrokerProviderRegistry::load(
        [
            provider_fixture("echo-provider.wasm"),
            provider_fixture("echo-provider.wasm"),
        ],
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

/// The same storage-backed invocation as the raw-API tests above, driven through the testkit.
///
/// It is the one place in this suite that exercises the composition a provider author actually
/// uses, and it keeps `dekopon-provider-sdk-testkit` honest against the host it wraps. Every other
/// storage test here — the sticky-denial matrix below in particular — still drives
/// `invoke_with_storage` directly, so the host's own behaviour is never observed only through a
/// wrapper around it.
#[tokio::test(flavor = "multi_thread")]
async fn durable_storage_probe_runs_under_one_exact_consumed_grant() {
    let broker = dekopon_provider_sdk_testkit::FakeBroker::builder()
        .component(provider_fixture("storage-probe-provider.wasm"))
        .provider("storage-probe")
        .storage(StorageInterface::DurableFiles, StorageAccess::ReadWrite)
        .build()
        .await
        .expect("probe loads");

    let output = broker
        .invoke_full("storage-probe.run", json!({}))
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
            [provider_fixture("storage-probe-provider.wasm")],
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
            secret_use: None,
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

/// Every entry under `root` with the mode, length, and contents a provisional write would change.
fn snapshot_storage_tree(root: &Path) -> Vec<(PathBuf, u32, u64, Vec<u8>)> {
    snapshot_tree(root)
        .into_iter()
        .map(|entry| (entry.relative, entry.mode, entry.len, entry.contents))
        .collect()
}
