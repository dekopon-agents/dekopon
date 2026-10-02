#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "tests spawn, join and drain freely"
)]
#![allow(clippy::unwrap_used)]

mod fixture;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use dekopon_broker_host::{
    BrokerHostError, BrokerHostLimits, BrokerHostOptions, BrokerProviderRegistry,
    CommandRunOutcome, HARD_MAX_PROVIDER_COMPONENT_BYTES, HTTP_WIT, LockedProviderSource,
    PROVIDER_WIT, STORAGE_WIT,
};
use dekopon_capability::{
    AuthorizedInvocation, ExecutionConstraints, HttpConstraints, ProposedInvocation, StorageAccess,
    StorageConstraints, StorageInterface, StorageScope, broker::AuthorizationGate,
};
use dekopon_core::{Actor, AgentId, CapabilityId, InvocationId, PrincipalId, TraceId};
use dekopon_storage_host::{ContinuityPolicy, StorageGrantRequest, StorageHost, StorageLimits};
use dekopon_test_support::{LoopbackServer, provider_fixture, snapshot_tree};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

#[tokio::test]
async fn owner_configured_openobserve_component_links_and_has_no_sql_command() {
    let Ok(component) = std::env::var("OPENOBSERVE_COMPONENT_PATH") else {
        return;
    };
    let options = BrokerHostOptions {
        provider_settings: Arc::new(BTreeMap::from([(
            "openobserve".parse().expect("valid provider ID"),
            json!({"url":"not-a-url-secret","org":"default","stream":"dekopon"}).to_string(),
        )])),
        ..Default::default()
    };
    let registry = BrokerProviderRegistry::load_with_options(
        [component],
        BrokerHostLimits::default(),
        None,
        &options,
    )
    .await
    .expect("broker must link the settings import and describe the provider");
    assert_eq!(registry.manifests().count(), 1);
    let sql = registry
        .run_command(
            "openobserve",
            &["sql".to_owned(), "--help".to_owned()],
            false,
        )
        .await
        .expect("unknown SQL action is rendered as an error");
    assert!(matches!(sql, CommandRunOutcome::Rendered { status: 2, .. }));
    let usage = registry
        .run_command(
            "broker",
            &["usage".to_owned(), "--since".to_owned(), "1h".to_owned()],
            false,
        )
        .await
        .expect("bounded command proposals work without reading settings");
    assert!(matches!(usage, CommandRunOutcome::Proposed { .. }));
    let error = registry
        .invoke(
            authorized(
                "openobserve.trace".parse().expect("valid capability"),
                json!({"sinceSeconds": 3600, "traceId": "0af7651916cd43dd8448eb211c80319c"}),
                http_constraints("example.com".to_owned(), "POST"),
            ),
            None,
            Default::default(),
        )
        .await
        .expect_err("invalid configured endpoint must fail before any request");
    assert!(matches!(
        *error.error,
        BrokerHostError::ProviderFailure { .. }
    ));
    assert!(!error.to_string().contains("not-a-url-secret"));
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
        "0000000000000000000000000000f1c7"
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
        asset: None,
        timeout_ms: 5_000,
        http: Some(HttpConstraints {
            allowed_hosts: vec![authority],
            allowed_methods: vec![method.to_owned()],
            max_requests: 1,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 64 * 1024,
            allow_plaintext_loopback: true,
            propagate_trace: false,
        }),
        storage: None,
        secret_use: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
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
    let server = LoopbackServer::once(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Value: one\r\nX-Value: two\r\nSet-Cookie: secret=session\r\nWWW-Authenticate: secret\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
    );
    let authority = server.authority().to_owned();
    let capability = "http-probe.fetch"
        .parse()
        .expect("valid capability fixture");
    let (output_assets, output_stdout) = fixture::piped_stdout();
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
            output_assets,
        )
        .await
        .expect("authorized HTTP invocation succeeds");
    let output_stdout = output_stdout.json();

    assert_eq!(output.provider.as_str(), "http-probe");
    assert_eq!(output_stdout["status"], 200);
    assert_eq!(output_stdout["bodyBytes"], 11);
    assert_eq!(output_stdout["headerCount"], 4);
    assert_eq!(output_stdout["body"], "eyJvayI6dHJ1ZX0=");
    assert_eq!(output_stdout["bodyText"], r#"{"ok":true}"#);
    assert_eq!(output_stdout["bodyTruncated"], false);
    assert_eq!(output.http_calls.len(), 1);
    assert_eq!(output.http_calls[0].method, "PATCH");
    assert_eq!(output.http_calls[0].authority, authority);
    assert_eq!(output.http_calls[0].status, Some(200));
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
    let (get_assets, get_stdout) = fixture::piped_stdout();
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
            get_assets,
        )
        .await
        .expect("authorized JSONPlaceholder read succeeds");
    let get_stdout = get_stdout.json();
    assert_eq!(get_stdout["post"]["id"], 7);
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
    let (create_assets, create_stdout) = fixture::piped_stdout();
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
            create_assets,
        )
        .await
        .expect("authorized JSONPlaceholder write succeeds");
    let create_stdout = create_stdout.json();
    assert_eq!(create_stdout["post"]["id"], 101);
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
            Default::default(),
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
            Default::default(),
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
            Default::default(),
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
            Default::default(),
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
            Default::default(),
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
    let (output_assets, output_stdout) = fixture::piped_stdout();
    let output = registry
        .invoke(
            authorized(
                capability,
                json!({"uri": format!("http://{authority}/redirect")}),
                http_constraints(authority, "GET"),
            ),
            None,
            output_assets,
        )
        .await
        .expect("redirect response itself is returned");
    let output_stdout = output_stdout.json();

    assert_eq!(output_stdout["status"], 302);
    assert_eq!(output.http_calls.len(), 1);
    server.join();
}

#[tokio::test(flavor = "multi_thread")]
async fn broker_host_also_runs_import_free_components() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("cli-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("import-free provider loads in the broker linker");
    let capability = "cli-probe.upper".parse().expect("valid capability fixture");
    let (output_assets, output_stdout) = fixture::piped_stdout();
    let output = registry
        .invoke(
            authorized(
                capability,
                json!({"text": "hello"}),
                ExecutionConstraints::default(),
            ),
            None,
            output_assets,
        )
        .await
        .expect("import-free provider runs without an HTTP grant");
    let output_stdout = output_stdout.json();

    assert_eq!(output_stdout, json!({"text": "HELLO"}));
    assert!(output.http_calls.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_authorization_bound_to_a_different_provider() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("cli-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("cli-probe provider loads");
    let capability = "cli-probe.upper".parse().expect("valid capability fixture");
    let error = registry
        .invoke(
            authorized_for(
                "http-probe",
                capability,
                json!({"text": "hello"}),
                ExecutionConstraints::default(),
            ),
            None,
            Default::default(),
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
    assert_eq!(
        include_str!("../wit/deps/clock.wit"),
        include_str!("../../../wit/clock/clock.wit")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn injected_guest_clock_does_not_change_host_timeouts() {
    let fixed = std::time::UNIX_EPOCH + Duration::from_millis(951_782_400_123);
    let options = BrokerHostOptions {
        test_clock: Some(fixed),
        ..Default::default()
    };
    let registry = BrokerProviderRegistry::load_with_options(
        [provider_fixture("clock-probe-provider.wasm")],
        BrokerHostLimits::default(),
        None,
        &options,
    )
    .await
    .expect("clock provider loads");
    let (output_assets, output_stdout) = fixture::piped_stdout();
    let _output = registry
        .invoke(
            authorized_for(
                "clock-probe",
                "clock-probe.now".parse().expect("capability"),
                json!({}),
                ExecutionConstraints {
                    http: None,
                    ..http_constraints("example.test".to_owned(), "GET")
                },
            ),
            None,
            output_assets,
        )
        .await
        .expect("injected clock is available during invoke");
    let output_stdout = output_stdout.json();
    assert_eq!(output_stdout["unixMillis"], 951_782_400_123_u64);
}

#[tokio::test(flavor = "multi_thread")]
async fn pinned_https_uses_exact_grant_and_verifies_tls_authority() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let certified = rcgen::generate_simple_self_signed(vec!["fixture.example.test".to_owned()])
        .expect("TLS fixture");
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("TLS versions")
    .with_no_client_auth()
    .with_single_cert(
        vec![certified.cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()).into(),
    )
    .expect("TLS configuration");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback");
    let address = listener.local_addr().expect("port");
    let authority = format!("fixture.example.test:{}", address.port());
    let pin = dekopon_http_host::LoopbackHttpsPin::new(
        &authority,
        address,
        certified.cert.pem().into_bytes(),
    )
    .expect("explicit pin");
    assert!(
        dekopon_http_host::LoopbackHttpsPin::new(
            &authority,
            "1.1.1.1:443".parse().unwrap(),
            vec![1]
        )
        .is_err()
    );
    assert!(dekopon_http_host::LoopbackHttpsPin::new(&authority, address, Vec::new()).is_err());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("connection");
        let mut tls = tokio_rustls::TlsAcceptor::from(Arc::new(config))
            .accept(stream)
            .await
            .expect("TLS handshake");
        let mut request = [0; 2048];
        let count = tls.read(&mut request).await.expect("request");
        assert!(request[..count].starts_with(b"GET /resource HTTP/1.1"));
        tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .expect("response");
        tls.shutdown().await.expect("close");
    });
    let options = BrokerHostOptions {
        loopback_https_pin: Some(pin),
        ..Default::default()
    };
    let registry = BrokerProviderRegistry::load_with_options(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
        None,
        &options,
    )
    .await
    .expect("provider loads");
    let denied = registry
        .invoke(
            authorized(
                "http-probe.fetch".parse().unwrap(),
                json!({"uri": format!("https://{authority}/resource"), "method": "GET"}),
                http_constraints("other.example.test:443".to_owned(), "GET"),
            ),
            None,
            Default::default(),
        )
        .await
        .expect_err("a pin cannot widen the exact grant");
    assert!(matches!(
        denied.error.as_ref(),
        BrokerHostError::HostCallRejected {
            reason: "denied",
            ..
        }
    ));
    let (output_assets, output_stdout) = fixture::piped_stdout();
    let output = registry
        .invoke(
            authorized(
                "http-probe.fetch".parse().unwrap(),
                json!({"uri": format!("https://{authority}/resource"), "method": "GET"}),
                http_constraints(authority.clone(), "GET"),
            ),
            None,
            output_assets,
        )
        .await
        .expect("TLS fixture is reached with exact host grant");
    let output_stdout = output_stdout.json();
    assert_eq!(output_stdout["status"], 200);
    assert_eq!(output.http_calls[0].authority, authority);
    server.await.expect("server finishes");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_loopback_pin_does_not_disable_tls_hostname_verification() {
    let certified = rcgen::generate_simple_self_signed(vec!["wrong.example.test".to_owned()])
        .expect("TLS fixture");
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("TLS versions")
    .with_no_client_auth()
    .with_single_cert(
        vec![certified.cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()).into(),
    )
    .expect("TLS configuration");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback");
    let address = listener.local_addr().expect("port");
    let authority = format!("fixture.example.test:{}", address.port());
    let pin = dekopon_http_host::LoopbackHttpsPin::new(
        &authority,
        address,
        certified.cert.pem().into_bytes(),
    )
    .expect("pin");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("connection");
        let result = tokio_rustls::TlsAcceptor::from(Arc::new(config))
            .accept(stream)
            .await;
        assert!(
            result.is_err(),
            "client must refuse a different TLS identity"
        );
    });
    let options = BrokerHostOptions {
        loopback_https_pin: Some(pin),
        ..Default::default()
    };
    let registry = BrokerProviderRegistry::load_with_options(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
        None,
        &options,
    )
    .await
    .expect("provider loads");
    let failure = registry
        .invoke(
            authorized(
                "http-probe.fetch".parse().unwrap(),
                json!({"uri": format!("https://{authority}/resource"), "method": "GET"}),
                http_constraints(authority, "GET"),
            ),
            None,
            Default::default(),
        )
        .await
        .expect_err("pinned address and CA do not bypass hostname verification");
    assert!(
        matches!(failure.error.as_ref(), BrokerHostError::ProviderFailure { status: 1, stderr, .. } if stderr.starts_with("http-failed: ")),
        "{failure:?}"
    );
    assert_eq!(failure.http_calls[0].status, None);
    server.await.expect("TLS server finishes");
}

#[tokio::test(flavor = "multi_thread")]
async fn run_command_reading_the_clock_traps() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("clock-raw-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("clock provider loads");
    assert_eq!(registry.command_words(), vec!["date".to_owned()]);

    let outcome = registry
        .run_command("date", &[], false)
        .await
        .expect("the bare word proposes");
    assert_eq!(
        outcome,
        CommandRunOutcome::Proposed {
            secret_use: None,
            capability: "clock.now".parse().expect("capability"),
            input: json!({}),
        }
    );

    let error = registry
        .run_command("date", &["--clock-in-run-command".to_owned()], false)
        .await
        .expect_err("a clock read outside invoke traps");
    assert!(
        matches!(
            error,
            BrokerHostError::RunCommandUsedHostImport { ref path }
                if path.ends_with("clock-raw-probe-provider.wasm")
        ),
        "expected the host-import tripwire, got {error:?}"
    );

    let outcome = registry
        .run_command("date", &[], false)
        .await
        .expect("a later run is unaffected");
    assert!(
        matches!(outcome, CommandRunOutcome::Proposed { .. }),
        "{outcome:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_zero_wasm_resource_ceilings() {
    let limits = BrokerHostLimits {
        max_memories: 0,
        ..BrokerHostLimits::default()
    };
    let error = BrokerProviderRegistry::load([provider_fixture("cli-probe-provider.wasm")], limits)
        .await
        .expect_err("zero store ceiling must fail");
    assert!(matches!(
        error,
        BrokerHostError::InvalidLimit {
            name: "max_memories"
        }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn artifact_digest_describes_the_compiled_buffer() {
    let source = provider_fixture("cli-probe-provider.wasm");
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

#[tokio::test(flavor = "multi_thread")]
async fn a_locked_artifact_digest_is_enforced_at_the_compile_boundary() {
    let source = provider_fixture("cli-probe-provider.wasm");
    let bytes = std::fs::read(&source).expect("read artifact");
    let locked = LockedProviderSource::new(
        source,
        bytes.len() as u64,
        "0".repeat(64),
        "cli-probe".parse().expect("provider ID"),
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

#[tokio::test(flavor = "multi_thread")]
async fn a_locked_artifact_length_is_enforced_at_the_compile_boundary() {
    let source = provider_fixture("cli-probe-provider.wasm");
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
        "cli-probe".parse().expect("provider ID"),
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

#[tokio::test(flavor = "multi_thread")]
async fn a_physically_oversized_locked_artifact_is_bounded_before_read() {
    assert!(matches!(
        LockedProviderSource::new(
            "zero.wasm",
            0,
            "0".repeat(64),
            "cli-probe".parse().expect("provider ID")
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
        "cli-probe".parse().expect("provider ID"),
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

#[tokio::test(flavor = "multi_thread")]
async fn a_locked_provider_identity_is_enforced_after_describe() {
    let source = provider_fixture("cli-probe-provider.wasm");
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

#[tokio::test(flavor = "multi_thread")]
async fn a_hand_rolled_run_command_guest_renders_help_and_proposes() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("memory-reservation-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("hand-rolled command-line provider loads");
    let outcome = registry
        .run_command("recall", &["--help".to_owned()], false)
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
        .run_command("recall", &["yesterday".to_owned()], true)
        .await
        .expect("the word proposes");
    assert_eq!(
        outcome,
        CommandRunOutcome::Proposed {
            secret_use: None,
            capability: "ordinary.escape".parse().expect("capability"),
            input: json!({}),
        }
    );

    let outcome = registry
        .run_command("recall", &["--verbose".to_owned()], false)
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

#[tokio::test(flavor = "multi_thread")]
async fn a_run_command_provider_renders_help_reads_stdin_and_declines() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("cli-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("command-line provider loads");
    assert_eq!(registry.command_words(), vec!["probe".to_owned()]);
    let outcome = registry
        .run_command("probe", &["--help".to_owned()], false)
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
        .run_command("probe", &["count".to_owned(), "-".to_owned()], true)
        .await
        .expect("a piped value proposes");
    assert_eq!(
        outcome,
        CommandRunOutcome::Proposed {
            secret_use: None,
            capability: capability.clone(),
            input: json!({"text": "héllo"}),
        }
    );
    let (output_assets, output_stdout) = fixture::piped_stdout();
    let _output = registry
        .invoke(
            authorized_for(
                "cli-probe",
                capability,
                json!({"text": "héllo"}),
                ExecutionConstraints {
                    asset: None,
                    timeout_ms: 5_000,
                    http: None,
                    storage: None,
                    secret_use: None,
                },
            ),
            None,
            output_assets,
        )
        .await
        .expect("the proposed capability runs");
    let output_stdout = output_stdout.json();
    assert_eq!(output_stdout, json!({"characters": 5}));

    let outcome = registry
        .run_command("probe", &["bogus".to_owned()], false)
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
        .run_command("probe", &["count".to_owned(), "-".to_owned()], false)
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
}

#[test]
fn the_default_aggregate_ceiling_is_256_mib_and_admits_a_store() {
    let options = BrokerHostOptions::default();
    assert_eq!(
        options.max_total_memory_bytes,
        Some(dekopon_broker_host::DEFAULT_MAX_TOTAL_MEMORY_BYTES)
    );
    assert_eq!(
        dekopon_broker_host::DEFAULT_MAX_TOTAL_MEMORY_BYTES,
        256 * 1024 * 1024
    );
    assert!(
        dekopon_broker_host::DEFAULT_MAX_TOTAL_MEMORY_BYTES
            >= BrokerHostLimits::default().max_memory_bytes,
        "a default that `Runtime::new` would reject is worse than none"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_an_aggregate_ceiling_smaller_than_one_store() {
    let limits = BrokerHostLimits::default();
    let options = BrokerHostOptions {
        max_total_memory_bytes: Some(limits.max_memory_bytes - 1),
        ..BrokerHostOptions::default()
    };
    let error = BrokerProviderRegistry::load_with_options(
        [provider_fixture("cli-probe-provider.wasm")],
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

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_small_guest_stores_do_not_reserve_their_maximum() {
    let limits = BrokerHostLimits::default();
    let options = BrokerHostOptions {
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
                    Default::default(),
                )
                .await
        }
    });
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
            Default::default(),
        )
        .await
        .expect_err("the fixture never answers, but a second small store is admitted")
        .error;
    assert!(
        matches!(error.as_ref(), BrokerHostError::Timeout { .. })
            || matches!(error.as_ref(), BrokerHostError::ProviderFailure { status: 1, stderr, .. } if stderr.starts_with("http-failed: ")),
        "the second store ran; a budget refusal would be MemoryBudgetExhausted: {error:?}"
    );

    let held = holding.await.expect("held invocation joins");
    assert!(held.is_err(), "the stalled invocation must time out");
    registry
        .invoke(
            authorized(
                "http-probe.fetch".parse().expect("capability"),
                json!({"uri": format!("http://{authority}/third")}),
                http_constraints(authority, "GET"),
            ),
            None,
            Default::default(),
        )
        .await
        .expect_err("the fixture never answers, but the store is admitted");
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_authorization_that_exceeds_host_ceilings() {
    let limits = BrokerHostLimits {
        max_timeout: Duration::from_millis(100),
        ..BrokerHostLimits::default()
    };
    let registry =
        BrokerProviderRegistry::load([provider_fixture("cli-probe-provider.wasm")], limits)
            .await
            .expect("provider loads beneath valid host ceilings");
    let capability = "cli-probe.upper".parse().expect("valid capability fixture");
    let error = registry
        .invoke(
            authorized(
                capability,
                json!({"text": "hello"}),
                ExecutionConstraints {
                    timeout_ms: 101,
                    ..ExecutionConstraints::default()
                },
            ),
            None,
            Default::default(),
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
            Default::default(),
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
        asset: None,
        timeout_ms: 5_000,
        http: Some(HttpConstraints {
            allowed_hosts: vec![authority],
            allowed_methods: methods.iter().map(|method| (*method).to_owned()).collect(),
            max_requests,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 256 * 1024,
            allow_plaintext_loopback: true,
            propagate_trace: false,
        }),
        storage: None,
        secret_use: None,
    }
}

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

    let (output_assets, output_stdout) = fixture::piped_stdout();
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
            output_assets,
        )
        .await
        .expect("authorized two-call conditional write succeeds");
    let output_stdout = output_stdout.json();

    assert_eq!(output.provider.as_str(), "http-probe");
    assert_eq!(output_stdout["observedEtag"], "\"v1\"");

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
            Default::default(),
        )
        .await
        .expect_err("a write without POST authority must fail");

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
            Default::default(),
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

#[tokio::test(flavor = "multi_thread")]
async fn conflicting_providers_are_all_reported_in_one_failure() {
    let error = BrokerProviderRegistry::load(
        [
            provider_fixture("cli-probe-provider.wasm"),
            provider_fixture("cli-probe-provider.wasm"),
        ],
        BrokerHostLimits::default(),
    )
    .await
    .expect_err("one component loaded twice conflicts with itself");

    let BrokerHostError::ConflictingProviders { report } = error else {
        panic!("expected a conflict report, got {error:?}");
    };
    assert_eq!(report.providers.len(), 1, "{report:?}");
    assert_eq!(report.capabilities.len(), 3, "{report:?}");
    assert_eq!(report.command_words.len(), 1, "{report:?}");
    assert!(report.wordless.is_empty(), "{report:?}");
    let rendered = report.to_string();
    assert!(rendered.contains("5 provider conflict(s)"), "{rendered}");
    assert!(rendered.contains("cli-probe.reverse"), "{rendered}");
    assert!(rendered.contains("command word `probe`"), "{rendered}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_waiting_namespace_lease_never_stalls_timers_or_a_distinct_namespace() {
    let directory = tempfile::tempdir().expect("storage directory");
    let directory = directory
        .path()
        .canonicalize()
        .expect("canonical directory");
    let root = directory.join("root");
    let limits = StorageLimits {
        lock_timeout_ms: 500,
        ..StorageLimits::default()
    };
    let storage = StorageHost::open(&root, limits).expect("storage host");
    let held = storage
        .grant(probe_storage_grant("lease-held", "slack.t0123abc.uone"))
        .expect("held grant");

    let competing_host = storage.clone();
    let mut competing = tokio::task::spawn_blocking(move || {
        competing_host.grant(probe_storage_grant(
            "lease-competing",
            "slack.t0123abc.uone",
        ))
    });
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
    // Racing `distinct` against `competing` proves the invariant without an absolute wall-clock
    // cutoff that shrinks under CI scheduling noise; `competing` cannot hang past its own
    // `lock_timeout_ms` deadline, so the race settles even if the invariant is broken. The 5 s
    // outer bound only guards against an unrelated deadlock.
    let distinct = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::select! {
            biased;
            result = distinct => result.expect("blocking task").expect("distinct grant"),
            joined = &mut competing => {
                panic!("a distinct namespace was serialized behind the blocked base: {joined:?}")
            }
        }
    })
    .await
    .expect("distinct grant deadlocked entirely");
    drop(distinct);

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
        StorageScope::PrivateConversation,
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
    let broker = fixture::FixtureHost::builder()
        .component(provider_fixture("storage-probe-provider.wasm"))
        .provider("storage-probe")
        .storage(StorageInterface::DurableFiles, StorageAccess::ReadWrite)
        .build()
        .await
        .expect("probe loads");

    let (output, stdout) = broker
        .invoke_full("storage-probe.run", json!({}))
        .await
        .expect("probe succeeds");

    assert_eq!(stdout["clocksCalled"], true);
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
        let storage = StorageHost::open(
            &root,
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
            asset: None,
            timeout_ms: 10_000,
            http: None,
            storage: Some(StorageConstraints {
                interface,
                access,
                scope: StorageScope::PrivateConversation,
                retention: Default::default(),
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
                StorageScope::PrivateConversation,
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
                Default::default(),
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
            "mode {mode} mutated storage despite denial before mutation"
        );
    }
}

fn snapshot_storage_tree(root: &Path) -> Vec<(PathBuf, u32, u64, Vec<u8>)> {
    snapshot_tree(root)
        .into_iter()
        .map(|entry| (entry.relative, entry.mode, entry.len, entry.contents))
        .collect()
}

fn post_return_component(cleanup: &str) -> tempfile::NamedTempFile {
    use std::io::Write as _;

    let manifest = serde_json::to_string(&json!({
        "apiVersion": dekopon_provider_sdk::ProviderApiVersion::V1Alpha1,
        "id": "cleanup-probe",
        "description": "Post-return failure probe",
        "commandWords": ["cleanup"],
        "capabilities": [{
            "id": "cleanup-probe.noop",
            "description": "No-op probe",
            "effect": dekopon_capability::EffectKind::ReadOnly,
            "risk": dekopon_core::RiskLevel::Low,
            "inputSchema": {"type": "object"}
        }]
    }))
    .expect("serialize manifest");
    let response = r#"{"outcome":"succeeded","output":{}}"#;
    let bytes = |data: &[u8]| {
        data.iter()
            .map(|byte| format!("\\{byte:02x}"))
            .collect::<String>()
    };
    let descriptor = |offset: u32, length: usize| {
        bytes(
            &[
                offset.to_le_bytes(),
                u32::try_from(length).expect("small string").to_le_bytes(),
            ]
            .concat(),
        )
    };
    let command_params = "(param \"argv\" (list string)) (param \"stdin-piped\" bool)";
    let core_params = "i32 i32 i32";
    let wat = format!(
        r#"(component
            (core module $m
                (memory (export "memory") 1)
                (data (i32.const 0) "{manifest_descriptor}")
                (data (i32.const 8) "{response_descriptor}")
                (data (i32.const 16) "\00\00")
                (data (i32.const 64) "{manifest}")
                (data (i32.const 2048) "{response}")
                (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 4096)
                (func (export "describe") (result i32) i32.const 0)
                (func (export "invoke") (param i32 i32 i32 i32) (result i32) i32.const 16)
                (func (export "command") (param {core_params}) (result i32) i32.const 8)
                (func (export "cleanup") (param i32) {cleanup})
            )
            (core instance $i (instantiate $m))
            (func (export "describe") (result string)
                (canon lift (core func $i "describe") (memory (core memory $i "memory"))))
            (func (export "invoke") (param "capability" string) (param "input-json" string) (result (result (error u8)))
                (canon lift (core func $i "invoke") (memory (core memory $i "memory"))
                    (realloc (core func $i "realloc")) (post-return (core func $i "cleanup"))))
            (func (export "run-command") {command_params} (result string)
                (canon lift (core func $i "command") (memory (core memory $i "memory"))
                    (realloc (core func $i "realloc")) (post-return (core func $i "cleanup"))))
        )"#,
        manifest_descriptor = descriptor(64, manifest.len()),
        response_descriptor = descriptor(2048, response.len()),
        manifest = bytes(manifest.as_bytes()),
        response = bytes(response.as_bytes()),
    );
    let mut file = tempfile::NamedTempFile::new().expect("temporary component");
    file.write_all(wat.as_bytes())
        .expect("write component text");
    file
}

#[tokio::test]
async fn automatic_post_return_traps_remain_command_and_invocation_failures() {
    let component = post_return_component("unreachable");
    let registry = BrokerProviderRegistry::load([component.path()], BrokerHostLimits::default())
        .await
        .expect("valid manifest loads without cleanup trap");
    let error = registry
        .run_command("cleanup", &[], false)
        .await
        .expect_err("cleanup trap must not become an output parsing failure");
    let BrokerHostError::RunCommand { source, .. } = error else {
        panic!("expected command failure, got {error:?}");
    };
    assert_eq!(
        source.downcast_ref::<wasmtime::Trap>(),
        Some(&wasmtime::Trap::UnreachableCodeReached)
    );
    let error = registry
        .invoke(
            authorized(
                "cleanup-probe.noop".parse().expect("capability"),
                json!({}),
                ExecutionConstraints {
                    timeout_ms: 5_000,
                    ..ExecutionConstraints::default()
                },
            ),
            None,
            Default::default(),
        )
        .await
        .expect_err("cleanup trap must not return the lifted success response");
    let BrokerHostError::Invoke { source, .. } = *error.error else {
        panic!("expected invocation failure, got {error:?}");
    };
    assert_eq!(
        source.downcast_ref::<wasmtime::Trap>(),
        Some(&wasmtime::Trap::UnreachableCodeReached)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn automatic_post_return_yields_to_the_deadline_and_releases_the_store() {
    let component = post_return_component("(loop $spin br $spin)");
    let limits = BrokerHostLimits {
        fuel: u64::MAX,
        max_timeout: Duration::from_millis(50),
        ..BrokerHostLimits::default()
    };
    let options = BrokerHostOptions {
        max_total_memory_bytes: Some(limits.max_memory_bytes),
        ..BrokerHostOptions::default()
    };
    let registry =
        BrokerProviderRegistry::load_with_options([component.path()], limits, None, &options)
            .await
            .expect("valid manifest loads");
    let error = registry
        .run_command("cleanup", &[], false)
        .await
        .expect_err("looping cleanup times out");
    assert!(
        matches!(error, BrokerHostError::Timeout { timeout_ms: 50, .. }),
        "{error:?}"
    );
    assert!(matches!(
        registry.run_command("cleanup", &[], false).await,
        Err(BrokerHostError::Timeout { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn real_guest_streams_one_and_five_eight_mib_assets_and_attaches_a_read_only_response() {
    use dekopon_broker_host::asset::AssetInputs;
    use dekopon_broker_protocol::{AssetEncoding, AssetRow};
    use dekopon_capability::AssetConstraints;
    use dekopon_http_host::asset::AssetDirectory;
    use std::{fs::File, os::unix::fs::FileExt as _};
    let root = tempfile::tempdir().unwrap();
    let mut registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits {
            max_memory_bytes: 16 * 1024 * 1024,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    registry.set_assets(AssetDirectory::new(
        root.path().to_owned(),
        64 * 1024 * 1024,
    ));
    let input = tempfile::NamedTempFile::new().unwrap();
    input.as_file().set_len(8 * 1024 * 1024).unwrap();
    for (count, response) in [(1, "done"), (5, "")] {
        let server = LoopbackServer::once(format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes());
        let refs = (1..=count)
            .map(|id| format!("chat-asset:{id}"))
            .collect::<Vec<_>>();
        let (piped, stdout) = fixture::piped_stdout();
        let inputs = AssetInputs {
            rows: (1..=count)
                .map(|id| AssetRow {
                    id,
                    content_type: "application/octet-stream".to_owned(),
                    encoding: AssetEncoding::Identity,
                    bytes: Some(8 * 1024 * 1024),
                    origin: "chat".to_owned(),
                    sent: false,
                })
                .collect(),
            descriptors: (1..=count)
                .map(|_| File::open(input.path()).unwrap().into())
                .collect(),
            sends_remaining: 0,
            streams: piped.streams,
            cancel: None,
        };
        let mut constraints = http_constraints(server.authority().to_owned(), "POST");
        constraints.asset = Some(AssetConstraints {
            attach: true,
            ..Default::default()
        });
        let output = registry
            .invoke(
                authorized(
                    "http-probe.fetch".parse().unwrap(),
                    json!({"assetMode": "stream", "references": refs, "uri": server.url()}),
                    constraints,
                ),
                None,
                inputs,
            )
            .await
            .unwrap();
        assert_eq!(stdout.json(), json!({"status": 200}));
        assert_eq!(output.assets.attached.len(), 1);
        assert_eq!(output.assets.attached[0].descriptor, 0);
        assert_eq!(output.assets.attached[0].bytes, response.len() as u64);
        assert_eq!(output.assets.attached[0].content_type, "text/plain");
        let file = output.assets.files[0].file();
        let mut bytes = vec![0; response.len()];
        file.read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(bytes, response.as_bytes());
        assert!(file.write_at(b"x", 0).is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        let wire = server.request();
        let start = wire
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap()
            + 4;
        let length =
            dekopon_core::base64::encoded_len(8 * 1024 * 1024).unwrap() as usize * count as usize;
        assert_eq!(wire.len() - start, length);
        assert_eq!(dekopon_test_support::content_length(&wire[..start]), length);
        assert!(server.recorded().is_empty());
        server.join();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn real_guest_asset_effects_exist_only_on_success_and_caught_denials_stay_terminal() {
    use dekopon_capability::AssetConstraints;
    use dekopon_http_host::asset::AssetDirectory;
    let root = tempfile::tempdir().unwrap();
    let directory = AssetDirectory::new(root.path().to_owned(), 11);
    let mut registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .unwrap();
    registry.set_assets(directory.clone());
    for mode in ["attach", "fail", "trap", "timeout", "catch-denied"] {
        let constraints = ExecutionConstraints {
            timeout_ms: if mode == "timeout" { 10 } else { 5000 },
            asset: Some(AssetConstraints {
                attach: mode != "catch-denied",
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = registry
            .invoke(
                authorized(
                    "http-probe.fetch".parse().unwrap(),
                    json!({"assetMode": mode}),
                    constraints,
                ),
                None,
                Default::default(),
            )
            .await;
        match mode {
            "attach" => assert_eq!(result.unwrap().assets.attached.len(), 1),
            "fail" => assert!(matches!(
                *result.unwrap_err().error,
                BrokerHostError::ProviderFailure { .. }
            )),
            "trap" => assert!(matches!(
                *result.unwrap_err().error,
                BrokerHostError::Invoke { .. }
            )),
            "timeout" => assert!(matches!(
                *result.unwrap_err().error,
                BrokerHostError::Timeout { .. }
            )),
            "catch-denied" => assert!(matches!(
                *result.unwrap_err().error,
                BrokerHostError::HostCallRejected {
                    reason: "asset-call-rejected",
                    ..
                }
            )),
            _ => unreachable!(),
        }
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        directory
            .allocate()
            .await
            .unwrap()
            .write(vec![0; 11])
            .await
            .unwrap();
    }
}
