use std::{
    collections::BTreeMap, fs, os::unix::fs::PermissionsExt as _, path::Path, sync::Arc,
    time::Duration,
};

use dekopon_broker::{
    AttestorGrant, AuthenticatedContext, Broker, BrokerBuildError, BrokerLimits, CapabilityRoute,
    ConstraintCatalog, ConstraintSet, CredentialStore, IdentityDirectory, InMemoryAuditLog,
    InvocationRequest, PolicyEngine, PolicyWorld,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_broker_protocol::{
    Attestation, BrokerClient, BrokerRequest, BrokerResponse, ClientError, CommandRunOutcome,
    ERROR_BROKER_UNAVAILABLE, ERROR_CAPACITY_EXHAUSTED, ERROR_INVALID_REQUEST,
    ERROR_UNAUTHENTICATED, FrameLimits, ProtocolVersion, RequestEnvelope, ResponseEnvelope,
    read_frame, write_frame,
};
use dekopon_brokerd::{
    BrokerServer, BrokerdError, CONFIG_API_VERSION, MappedPeer, ServerLimits, current_uid, run,
};
use dekopon_capability::{EffectKind, ExecutionConstraints, HttpConstraints, InvocationOutcome};
use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, InvocationId, PrincipalId, ProviderId,
    RiskLevel, SecretUseProposal,
};
use dekopon_test_support::{provider_fixture, shutdown_on};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{UnixListener, UnixStream},
    sync::oneshot,
};

/// One fixture trace context for every request these tests build.
///
/// The trace is mandatory on the wire now; these cases read invocation identifiers and audit
/// fields rather than the trace itself, so one shared value keeps the fixtures about their subject.
const TRACE_PARENT: &str = "00-0000000000000000000000000000f1c7-00000000000000f1-00";

fn context(principal: &str) -> AuthenticatedContext {
    AuthenticatedContext::new(
        principal.parse().expect("valid principal fixture"),
        Actor::Agent {
            agent: "brokerd-test"
                .parse::<AgentId>()
                .expect("valid agent fixture"),
        },
    )
    .expect("trusted context binds")
}

/// The direct grant: `caller`, as the agent its peer identity carries, may `echo.echo`.
const DIRECT_POLICY: &str = r#"
@id("caller-echo")
permit(principal == Dekopon::Principal::"caller",
       action == Dekopon::Action::"echo.echo",
       resource == Dekopon::Provider::"echo")
when { context has agent && context.agent == "brokerd-test" }
unless { context has via };
"#;

/// The attested twin, plus the session gate it now needs.
///
/// `via` names the *peer* principal `context("caller")` builds, because that is the identity the
/// socket authenticates; the policy's own principal is the one the subject maps to.
const ATTESTED_POLICY: &str = r#"
@id("chat-agent-session")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"chat-agent")
when { context has via && context.via == "caller" };

@id("chat-agent-echo")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"echo.echo",
       resource == Dekopon::Provider::"echo")
when { context has via && context.via == "caller"
    && context has agent && context.agent == "chat-agent" };
"#;

fn echo_constraint_set() -> ConstraintSet {
    ConstraintSet {
        route: CapabilityRoute::Generic,
        provider: "echo"
            .parse::<ProviderId>()
            .expect("valid provider fixture"),
        effect: EffectKind::ReadOnly,
        risk: RiskLevel::Low,
        credential: None,
        credential_by_agent: BTreeMap::new(),
        constraints: ExecutionConstraints::default(),
    }
}

fn echo_catalog() -> ConstraintCatalog {
    ConstraintCatalog::new([(
        "echo.echo"
            .parse::<CapabilityId>()
            .expect("valid capability fixture"),
        echo_constraint_set(),
    )])
    .expect("one capability builds a catalog")
}

fn echo_engine<'a>(policies: &str, principals: impl IntoIterator<Item = &'a str>) -> PolicyEngine {
    let world = PolicyWorld::new(
        principals.into_iter().map(|name| {
            name.parse::<PrincipalId>()
                .expect("valid principal fixture")
        }),
        [(
            "echo.echo"
                .parse::<CapabilityId>()
                .expect("valid capability fixture"),
            "echo"
                .parse::<ProviderId>()
                .expect("valid provider fixture"),
        )],
    )
    .expect("distinct fixtures build a world");
    PolicyEngine::new(policies, &world).expect("fixture policy validates")
}

fn request(id: &str) -> InvocationRequest {
    InvocationRequest {
        id: id
            .parse::<InvocationId>()
            .expect("valid invocation fixture"),
        capability: "echo.echo"
            .parse::<CapabilityId>()
            .expect("valid capability fixture"),
        trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
        secret_use: None,
        input: json!({"message": "hello through broker"}),
    }
}

fn write_owner_only(path: &Path, contents: &[u8]) {
    fs::write(path, contents).expect("write fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("secure fixture");
}

/// A fixture directory the broker binds under and its clients connect through.
///
/// `tempfile::tempdir` applies the process umask, which normally leaves the directory
/// world-traversable. That is a parent `socket::bind` refuses, and — now that both sides read one
/// socket rule — a parent `BrokerClient` refuses too: whoever can write the directory can replace
/// the listener under a socket whose own mode still looks private.
fn private_directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("create fixture directory");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("private fixture directory");
    directory
}

fn bind_fixture(path: &Path) -> UnixListener {
    let listener = UnixListener::bind(path).expect("bind server fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("secure server fixture");
    listener
}

async fn broker() -> (Arc<Broker<InMemoryAuditLog>>, Arc<InMemoryAuditLog>) {
    broker_with_audit_bound(8).await
}

async fn broker_with_audit_bound(
    maximum: usize,
) -> (Arc<Broker<InMemoryAuditLog>>, Arc<InMemoryAuditLog>) {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("echo-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("load echo fixture");
    let audit = Arc::new(InMemoryAuditLog::new(maximum).expect("valid audit bound"));
    let broker = Arc::new(
        Broker::new(
            registry,
            "broker-test"
                .parse::<PrincipalId>()
                .expect("valid broker principal"),
            "policy-test".to_owned(),
            echo_engine(DIRECT_POLICY, ["caller"]),
            echo_catalog(),
            CredentialStore::empty(),
            IdentityDirectory::empty(),
            Arc::clone(&audit),
            BrokerLimits::default(),
        )
        .expect("broker starts"),
    );
    (broker, audit)
}

/// The canonical subject the attested fixtures speak for.
const SLACK_SUBJECT: &str = "slack.t0123abc.u9xyz";

fn subject() -> ExternalSubject {
    SLACK_SUBJECT
        .parse::<ExternalSubject>()
        .expect("canonical subject fixture")
}

fn agent(name: &str) -> AgentId {
    name.parse::<AgentId>().expect("valid agent fixture")
}

fn attestor_grant() -> AttestorGrant {
    AttestorGrant {
        namespaces: vec!["slack.t0123abc".to_owned()],
        chat_scopes: Vec::new(),
    }
}

/// A broker carrying both the direct grant and its attested twin, plus the one owner-controlled
/// mapping that turns the subject into a principal.
async fn attested_broker() -> (Arc<Broker<InMemoryAuditLog>>, Arc<InMemoryAuditLog>) {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("echo-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("load echo fixture");
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound"));
    let identities = IdentityDirectory::new([(
        subject(),
        "cpetersen"
            .parse::<PrincipalId>()
            .expect("valid principal fixture"),
    )])
    .expect("one mapping builds a directory");
    let broker = Arc::new(
        Broker::new(
            registry,
            "broker-test"
                .parse::<PrincipalId>()
                .expect("valid broker principal"),
            "policy-test".to_owned(),
            echo_engine(
                &format!("{DIRECT_POLICY}\n{ATTESTED_POLICY}"),
                ["caller", "cpetersen"],
            ),
            echo_catalog(),
            CredentialStore::empty(),
            identities,
            Arc::clone(&audit),
            BrokerLimits::default(),
        )
        .expect("attested broker starts"),
    );
    (broker, audit)
}

fn server_limits() -> ServerLimits {
    ServerLimits {
        frame: FrameLimits {
            max_frame_bytes: 64 * 1024,
            io_timeout: Duration::from_secs(2),
        },
        max_connections: 4,
        shutdown_grace: Duration::from_secs(2),
    }
}

/// The cli-probe twin of the echo grant: `probe` is the word, `cli-probe.upper` the capability.
const CLI_PROBE_POLICY: &str = r#"
@id("caller-upper")
permit(principal == Dekopon::Principal::"caller",
       action == Dekopon::Action::"cli-probe.upper",
       resource == Dekopon::Provider::"cli-probe")
when { context has agent && context.agent == "brokerd-test" }
unless { context has via };
"#;

fn cli_probe_catalog() -> ConstraintCatalog {
    ConstraintCatalog::new([(
        "cli-probe.upper"
            .parse::<CapabilityId>()
            .expect("valid capability fixture"),
        ConstraintSet {
            provider: "cli-probe"
                .parse::<ProviderId>()
                .expect("valid provider fixture"),
            ..echo_constraint_set()
        },
    )])
    .expect("one capability builds a catalog")
}

fn cli_probe_engine() -> PolicyEngine {
    let world = PolicyWorld::new(
        ["caller"
            .parse::<PrincipalId>()
            .expect("valid principal fixture")],
        [(
            "cli-probe.upper"
                .parse::<CapabilityId>()
                .expect("valid capability fixture"),
            "cli-probe"
                .parse::<ProviderId>()
                .expect("valid provider fixture"),
        )],
    )
    .expect("distinct fixtures build a world");
    PolicyEngine::new(CLI_PROBE_POLICY, &world).expect("fixture policy validates")
}

/// A broker over the clap-layer guest, so a socket test can drive a command word end to end.
async fn cli_probe_broker() -> (Arc<Broker<InMemoryAuditLog>>, Arc<InMemoryAuditLog>) {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("cli-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("load cli-probe fixture");
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound"));
    let broker = Arc::new(
        Broker::new(
            registry,
            "broker-test"
                .parse::<PrincipalId>()
                .expect("valid broker principal"),
            "policy-test".to_owned(),
            cli_probe_engine(),
            cli_probe_catalog(),
            CredentialStore::empty(),
            IdentityDirectory::empty(),
            Arc::clone(&audit),
            BrokerLimits::default(),
        )
        .expect("broker starts"),
    );
    (broker, audit)
}

/// A command word answers over the socket as the tool it fronts: the help page at status 0
/// decides nothing, and the proposal built from the piped value is what the next frame submits,
/// so `echo hello | probe upper -` is one run and one authorized invocation.
#[tokio::test(flavor = "multi_thread")]
async fn run_command_over_the_socket_renders_help_then_proposes() {
    let uid = current_uid();
    let directory = private_directory();
    let socket_path = directory.path().join("broker.sock");
    let listener = bind_fixture(&socket_path);
    let (broker, audit) = cli_probe_broker().await;
    let mut identities = BTreeMap::new();
    identities.insert(
        uid,
        MappedPeer {
            context: context("caller"),
            attestor: None,
        },
    );
    let limits = server_limits();
    let server = BrokerServer::new(broker, identities, limits).expect("server limits valid");
    let (shutdown_send, shutdown_receive) = oneshot::channel::<()>();
    let task = tokio::spawn(server.serve(listener, shutdown_on(shutdown_receive)));

    let client = BrokerClient::new(&socket_path, uid, limits.frame).expect("client starts");
    let (_, words, _) = client
        .session_surface(None)
        .await
        .expect("inspect the surface");
    assert_eq!(words, ["probe"]);

    match client
        .run_command(None, "probe".to_owned(), vec!["--help".to_owned()], None)
        .await
        .expect("the help page renders")
    {
        CommandRunOutcome::Rendered {
            stdout,
            stderr,
            status,
        } => {
            assert_eq!(status, 0);
            assert!(stdout.starts_with("Usage: probe <COMMAND>"), "{stdout}");
            assert!(stderr.is_empty(), "{stderr}");
        }
        other => panic!("expected a rendered help page, got {other:?}"),
    }
    assert!(
        audit.records().await.is_empty(),
        "rendering decides nothing"
    );

    let (capability, input) = match client
        .run_command(
            None,
            "probe".to_owned(),
            vec!["upper".to_owned(), "-".to_owned()],
            Some("hello".to_owned()),
        )
        .await
        .expect("the piped value proposes")
    {
        CommandRunOutcome::Proposed { capability, input } => (capability, input),
        other => panic!("expected a proposal, got {other:?}"),
    };
    assert_eq!(capability.as_str(), "cli-probe.upper");
    assert_eq!(input, json!({"text": "hello"}));

    let result = client
        .invoke(
            None,
            InvocationRequest {
                id: "invoke-probe-upper"
                    .parse::<InvocationId>()
                    .expect("valid invocation fixture"),
                capability,
                trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
                secret_use: None,
                input,
            },
        )
        .await
        .expect("invoke the proposal");
    assert_eq!(result.outcome, InvocationOutcome::Succeeded);
    assert_eq!(result.output, Some(json!({"text": "HELLO"})));
    assert_eq!(
        audit.records().await.len(),
        2,
        "one decision and one execution for the one invocation"
    );

    shutdown_send.send(()).expect("signal clean shutdown");
    task.await
        .expect("server task exits")
        .expect("server shuts down");
}

/// An older client's `resolveCommand` is still answered in the shape it reads: a proposal as
/// before, and a rendered help page as the decline that shape can carry, stdout then stderr.
#[tokio::test(flavor = "multi_thread")]
async fn a_legacy_resolve_command_frame_is_still_answered() {
    let uid = current_uid();
    let directory = private_directory();
    let socket_path = directory.path().join("broker.sock");
    let listener = bind_fixture(&socket_path);
    let (broker, audit) = cli_probe_broker().await;
    let mut identities = BTreeMap::new();
    identities.insert(
        uid,
        MappedPeer {
            context: context("caller"),
            attestor: None,
        },
    );
    let limits = server_limits();
    let server = BrokerServer::new(broker, identities, limits).expect("server limits valid");
    let (shutdown_send, shutdown_receive) = oneshot::channel::<()>();
    let task = tokio::spawn(server.serve(listener, shutdown_on(shutdown_receive)));

    async fn legacy(socket_path: &Path, limits: FrameLimits, argv: Vec<String>) -> BrokerResponse {
        let mut stream = UnixStream::connect(socket_path)
            .await
            .expect("connect to the fixture socket");
        let envelope = RequestEnvelope {
            api_version: ProtocolVersion::V1Alpha2,
            request: BrokerRequest::ResolveCommand {
                attestation: None,
                word: "probe".to_owned(),
                argv,
            },
        };
        write_frame(&mut stream, &envelope, limits)
            .await
            .expect("write the legacy frame");
        read_frame::<_, ResponseEnvelope>(&mut stream, limits)
            .await
            .expect("read the legacy answer")
            .response
    }

    match legacy(&socket_path, limits.frame, vec!["--help".to_owned()]).await {
        BrokerResponse::CommandResolution {
            capability: None,
            input: None,
            message: Some(message),
        } => assert!(message.starts_with("Usage: probe <COMMAND>"), "{message}"),
        other => panic!("expected a decline carrying the help page, got {other:?}"),
    }
    match legacy(
        &socket_path,
        limits.frame,
        vec!["upper".to_owned(), "--text".to_owned(), "hi".to_owned()],
    )
    .await
    {
        BrokerResponse::CommandResolution {
            capability: Some(capability),
            input: Some(input),
            message: None,
        } => {
            assert_eq!(capability.as_str(), "cli-probe.upper");
            assert_eq!(input, json!({"text": "hi"}));
        }
        other => panic!("expected a legacy resolution, got {other:?}"),
    }
    assert!(
        audit.records().await.is_empty(),
        "the legacy operation decides nothing either"
    );

    shutdown_send.send(()).expect("signal clean shutdown");
    task.await
        .expect("server task exits")
        .expect("server shuts down");
}

#[tokio::test(flavor = "multi_thread")]
async fn authenticated_unix_peer_can_inspect_and_invoke_under_policy() {
    let uid = current_uid();
    let directory = private_directory();
    let socket_path = directory.path().join("broker.sock");
    let listener = bind_fixture(&socket_path);
    let (broker, audit) = broker().await;
    let mut identities = BTreeMap::new();
    identities.insert(
        uid,
        MappedPeer {
            context: context("caller"),
            attestor: None,
        },
    );
    let limits = server_limits();
    let server = BrokerServer::new(broker, identities, limits).expect("server limits valid");
    let (shutdown_send, shutdown_receive) = oneshot::channel::<()>();
    let task = tokio::spawn(server.serve(listener, shutdown_on(shutdown_receive)));

    let client = BrokerClient::new(&socket_path, uid, limits.frame).expect("client starts");
    let capabilities = client.capabilities().await.expect("inspect capabilities");
    assert_eq!(capabilities.len(), 1);
    assert_eq!(capabilities[0].capability.id.as_str(), "echo.echo");
    let result = client
        .invoke(None, request("invoke-brokerd"))
        .await
        .expect("invoke");
    assert_eq!(result.outcome, InvocationOutcome::Succeeded);
    assert_eq!(
        result.output,
        Some(json!({"message": "hello through broker"}))
    );
    assert_eq!(audit.records().await.len(), 2);

    shutdown_send.send(()).expect("signal clean shutdown");
    task.await
        .expect("server task exits")
        .expect("server shuts down");
}

/// The refusal an operator most often meets: the broker's own readiness probe connects as the
/// broker's UID, so a configuration whose `identities` omit it is answered with the same opaque
/// nothing a stranger gets. What the answer withholds is pinned here; the `broker_peer_unmapped`
/// line that carries the peer UID is pinned in `failure_logging.rs`, whose global subscriber is
/// the only one a spawned connection task reports to.
///
/// The frame is read straight off the socket because this is the one refusal the broker writes
/// before reading a request and closes the socket with: macOS refuses to report a peer's
/// credentials once that close has landed, so a `BrokerClient` here would report losing the
/// server rather than the answer this test is about. The refusal itself survives the close —
/// it is already in this peer's receive buffer.
#[tokio::test(flavor = "multi_thread")]
async fn unmapped_peer_receives_no_capability_information() {
    let directory = private_directory();
    let socket_path = directory.path().join("broker.sock");
    let listener = bind_fixture(&socket_path);
    let (broker, _audit) = broker().await;
    let limits = server_limits();
    let server = BrokerServer::new(broker, BTreeMap::new(), limits).expect("server starts");
    let (shutdown_send, shutdown_receive) = oneshot::channel::<()>();
    let task = tokio::spawn(server.serve(listener, shutdown_on(shutdown_receive)));
    let mut peer = UnixStream::connect(&socket_path)
        .await
        .expect("connect as an unmapped peer");
    let refusal = read_frame::<_, ResponseEnvelope>(&mut peer, limits.frame)
        .await
        .expect("an unmapped peer is answered before it asks for anything");
    let BrokerResponse::Error { code, message } = refusal.response else {
        panic!("an unmapped peer must be refused rather than served");
    };
    assert_eq!(code, ERROR_UNAUTHENTICATED);
    // Not even the provider it would have been allowed to call, had it been mapped.
    assert!(!message.contains("echo"), "{message}");
    shutdown_send.send(()).expect("signal clean shutdown");
    task.await
        .expect("server task exits")
        .expect("server shuts down");
}

#[tokio::test(flavor = "multi_thread")]
async fn full_service_resolves_a_private_map_only_after_dual_drn_authorization() {
    let uid = current_uid();
    let directory = private_directory();
    let config_path = directory.path().join("broker.json");
    let socket_path = directory.path().join("broker.sock");
    let policies_path = directory.path().join("policies.cedar");
    let secret_map_path = directory.path().join("secret-map.yaml");
    let secret_value_path = directory.path().join("api-token");
    write_owner_only(&secret_value_path, b"brokerd-secret-value");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind HTTP fixture");
    let authority = listener
        .local_addr()
        .expect("HTTP fixture address")
        .to_string();
    let (wire_send, wire_receive) = oneshot::channel();
    let upstream = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept HTTP request");
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0_u8; 4096];
            let read = stream.read(&mut chunk).await.expect("read HTTP request");
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        wire_send.send(bytes).expect("record HTTP request");
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            )
            .await
            .expect("write HTTP response");
    });

    let policies = r#"@id("caller-fetch")
permit(principal == Dekopon::Principal::"caller",
       action == Dekopon::Action::"http-probe.fetch",
       resource == Dekopon::Provider::"http-probe")
when { context has agent && context.agent == "brokerd-test" }
unless { context has via };

@id("caller-secret")
permit(principal == Dekopon::Principal::"caller",
       action == Dekopon::Action::"secret.use",
       resource == Dekopon::Secret::"drn:com.xrl:secret:test:api/token")
when { context.capability == "http-probe.fetch"
    && context.provider == "http-probe"
    && context.sink == "httpBearer" };
"#;
    write_owner_only(&policies_path, policies.as_bytes());
    let secret_map = format!(
        "apiVersion: dekopon.dev/secret-map/v1alpha1\nmapRevision: service-test\nsecrets:\n  - drn: drn:com.xrl:secret:test:api/token\n    source: {{ kind: secureFile, path: {} }}\n    bindings:\n      - id: service-token\n        capability: http-probe.fetch\n        sink: httpBearer\n        allowedHosts: [{}]\n        allowedMethods: [GET]\n        allowedPaths: [{{ match: exact, path: /api/v1/thing }}]\n",
        secret_value_path.display(),
        authority
    );
    write_owner_only(&secret_map_path, secret_map.as_bytes());
    let set = ConstraintSet {
        route: CapabilityRoute::Generic,
        provider: "http-probe".parse().expect("provider"),
        effect: EffectKind::ReadOnly,
        risk: RiskLevel::Low,
        credential: None,
        credential_by_agent: BTreeMap::new(),
        constraints: ExecutionConstraints {
            timeout_ms: 5_000,
            max_output_bytes: 64 * 1024,
            http: Some(HttpConstraints {
                allowed_hosts: vec![authority.clone()],
                allowed_methods: vec!["GET".to_owned()],
                max_requests: 1,
                max_request_bytes: 64 * 1024,
                max_response_bytes: 64 * 1024,
                allow_plaintext_loopback: true,
            }),
            storage: None,
            secret_use: None,
        },
    };
    let document = json!({
        "apiVersion": CONFIG_API_VERSION,
        "socketPath": &socket_path,
        "brokerPrincipal": "broker-test",
        "policyRevision": "policy-test",
        "policiesPath": &policies_path,
        "secretMapPath": &secret_map_path,
        "providers": [provider_fixture("http-probe-provider.wasm")],
        "identities": [{
            "uid": uid,
            "principal": "caller",
            "actor": {"type": "agent", "agent": "brokerd-test"}
        }],
        "constraintSets": {
            "http-probe.fetch": serde_json::to_value(set).expect("constraint set")
        }
    });
    write_owner_only(
        &config_path,
        &serde_json::to_vec(&document).expect("config serializes"),
    );

    let (stop, stopped) = oneshot::channel::<()>();
    let started_config = config_path.clone();
    let mut service = tokio::spawn(async move {
        run(started_config, async move {
            #[allow(
                clippy::let_underscore_must_use,
                reason = "a dropped sender is normal test shutdown"
            )]
            let _ = stopped.await;
        })
        .await
    });
    wait_for_socket(&socket_path, &mut service).await;
    let client = BrokerClient::new(&socket_path, uid, FrameLimits::default()).expect("client");
    let mut invocation = InvocationRequest {
        id: "invoke-secret-service".parse().expect("invocation"),
        capability: "http-probe.fetch".parse().expect("capability"),
        trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
        secret_use: Some(SecretUseProposal::HttpBearer {
            secret: "drn:com.xrl:secret:test:api/token".parse().expect("DRN"),
        }),
        input: json!({
            "uri": format!("http://{authority}/api/v1/thing"),
            "method": "GET"
        }),
    };
    let result = client
        .invoke(None, invocation.clone())
        .await
        .expect("secret invocation succeeds");
    assert_eq!(result.outcome, InvocationOutcome::Succeeded);
    let wire = String::from_utf8(wire_receive.await.expect("HTTP wire")).expect("wire text");
    assert!(
        wire.contains("authorization: Bearer brokerd-secret-value"),
        "{wire}"
    );
    upstream.await.expect("HTTP fixture exits");

    // Wrong path is inside capability authority but outside the secret binding. It may not make a
    // second network request, and the host rejection is terminal even if the guest catches it.
    invocation.id = "invoke-secret-wrong-path".parse().expect("invocation");
    invocation.input = json!({
        "uri": format!("http://{authority}/api/v1/other"),
        "method": "GET"
    });
    let denied = client
        .invoke(None, invocation)
        .await
        .expect("host refusal is accounted");
    assert_eq!(denied.outcome, InvocationOutcome::Failed);

    stop.send(()).expect("stop service");
    service
        .await
        .expect("service task exits")
        .expect("service stops");
}

/// Waits until the fixture's socket exists *and* is owner-only.
///
/// Existence alone is not readiness. `socket::bind` binds the listener and then narrows the mode to
/// `0600`, so between those two steps the path exists with the umask's permissions and a client
/// that connects inside that window fails its own `UnsafeSocket` check. That is a test-timing
/// problem rather than an exposure — `validate_private_parent` has already proved the containing
/// directory is owner-only, so no other user can traverse it to reach the socket meanwhile — but
/// polling on `exists()` alone makes the suite flaky under parallel load.
async fn wait_for_socket(
    path: &Path,
    task: &mut tokio::task::JoinHandle<Result<(), BrokerdError>>,
) {
    for _ in 0..3_000 {
        if std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o077 == 0)
        {
            return;
        }
        if task.is_finished() {
            let result = task.await;
            panic!("broker fixture exited before binding its socket: {result:?}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("broker fixture socket did not become owner-only within thirty seconds");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_terminal_audit_is_distinguishable_from_an_invocation_that_never_ran() {
    let uid = current_uid();
    let directory = private_directory();
    let socket_path = directory.path().join("broker.sock");
    let listener = bind_fixture(&socket_path);
    // One audit slot: the first allowed invocation spends it on its Decision, so its terminal
    // Execution append is already doomed when the provider runs.
    let (broker, audit) = broker_with_audit_bound(1).await;
    let mut identities = BTreeMap::new();
    identities.insert(
        uid,
        MappedPeer {
            context: context("caller"),
            attestor: None,
        },
    );
    let limits = server_limits();
    let server = BrokerServer::new(broker, identities, limits).expect("server limits valid");
    let (shutdown_send, shutdown_receive) = oneshot::channel::<()>();
    let task = tokio::spawn(server.serve(listener, shutdown_on(shutdown_receive)));

    let client = BrokerClient::new(&socket_path, uid, limits.frame).expect("client starts");
    let ran = client
        .invoke(None, request("invoke-outcome-unaudited"))
        .await
        .expect_err("a terminal audit failure is not a successful invocation");
    let ClientError::Remote { code, message } = ran else {
        panic!("expected a remote broker failure, got {ran}");
    };
    assert_eq!(code, "outcome-unaudited");
    assert!(
        message.contains("may already have completed"),
        "the client must be told the effect may have happened: {message}"
    );
    // The Decision landed; the provider ran; nothing recorded the outcome.
    assert_eq!(audit.records().await.len(), 1);

    let never_ran = client
        .invoke(None, request("invoke-never-ran"))
        .await
        .expect_err("a full audit cannot authorize");
    let ClientError::Remote {
        code: unran_code,
        message: unran_message,
    } = never_ran
    else {
        panic!("expected a remote broker failure, got {never_ran}");
    };
    // Nothing executed, so this is safe to resubmit — and futile in this bounded in-memory log.
    // Every fresh identifier fails on the same append until the embedding addresses capacity.
    assert_eq!(unran_code, ERROR_CAPACITY_EXHAUSTED);
    assert_ne!(
        unran_code, ERROR_BROKER_UNAVAILABLE,
        "a permanently capped broker must not be reported as briefly unavailable"
    );
    assert!(
        unran_message.contains("operator action"),
        "a permanent exhaustion must not read as a transient outage: {unran_message}"
    );
    assert_ne!(
        unran_code, code,
        "a client must distinguish an effect that may have run from one that never began"
    );

    shutdown_send.send(()).expect("signal clean shutdown");
    task.await
        .expect("server task exits")
        .expect("server shuts down");
}

/// The whole attested path over a real socket: the peer names a canonical subject, the broker
/// maps it, and the invocation runs under the attested context. The peer never names a principal
/// at any point — that mapping is not something the wire can express.
#[tokio::test(flavor = "multi_thread")]
async fn an_attested_invoke_over_the_socket_succeeds_for_an_attestor_peer() {
    let uid = current_uid();
    let directory = private_directory();
    let socket_path = directory.path().join("broker.sock");
    let listener = bind_fixture(&socket_path);
    let (broker, audit) = attested_broker().await;
    let mut identities = BTreeMap::new();
    identities.insert(
        uid,
        MappedPeer {
            context: context("caller"),
            attestor: Some(attestor_grant()),
        },
    );
    let limits = server_limits();
    let server = BrokerServer::new(broker, identities, limits).expect("server limits valid");
    let (shutdown_send, shutdown_receive) = oneshot::channel::<()>();
    let task = tokio::spawn(server.serve(listener, shutdown_on(shutdown_receive)));

    let client = BrokerClient::new(&socket_path, uid, limits.frame).expect("client starts");
    let result = client
        .invoke(
            Some(Attestation::for_subject(subject(), agent("chat-agent"))),
            request("invoke-attested-socket"),
        )
        .await
        .expect("attested invocation completes");
    assert_eq!(result.outcome, InvocationOutcome::Succeeded);
    assert_eq!(
        result.output,
        Some(json!({"message": "hello through broker"}))
    );

    let records = audit.records().await;
    assert_eq!(records.len(), 2);
    let encoded: Value = serde_json::to_value(&records).expect("audit serializes");
    assert_eq!(encoded[0]["principal"], "cpetersen");
    assert_eq!(encoded[0]["via"], "caller");
    assert_eq!(encoded[0]["attested_subject"], SLACK_SUBJECT);

    shutdown_send.send(()).expect("signal clean shutdown");
    task.await
        .expect("server task exits")
        .expect("server shuts down");
}

/// A peer with no attestor grant gets a completed invocation response carrying a denial, not a
/// transport failure. The difference is the audit record: a denial is a decision the broker made
/// and retained, and an error would leave the attempt with nothing accounting for it.
#[tokio::test(flavor = "multi_thread")]
async fn an_attested_invoke_from_a_peer_without_a_grant_is_denied_not_erred() {
    let uid = current_uid();
    let directory = private_directory();
    let socket_path = directory.path().join("broker.sock");
    let listener = bind_fixture(&socket_path);
    let (broker, audit) = attested_broker().await;
    let mut identities = BTreeMap::new();
    identities.insert(
        uid,
        MappedPeer {
            context: context("caller"),
            attestor: None,
        },
    );
    let limits = server_limits();
    let server = BrokerServer::new(broker, identities, limits).expect("server limits valid");
    let (shutdown_send, shutdown_receive) = oneshot::channel::<()>();
    let task = tokio::spawn(server.serve(listener, shutdown_on(shutdown_receive)));

    let client = BrokerClient::new(&socket_path, uid, limits.frame).expect("client starts");
    let result = client
        .invoke(
            Some(Attestation::for_subject(subject(), agent("chat-agent"))),
            request("invoke-ungranted-socket"),
        )
        .await
        .expect("a refused attestation is still a completed invocation response");
    assert_eq!(result.outcome, InvocationOutcome::Denied);
    assert_eq!(result.error.as_deref(), Some("attestation-denied"));

    let records = audit.records().await;
    assert_eq!(records.len(), 1);
    let encoded: Value = serde_json::to_value(&records).expect("audit serializes");
    assert_eq!(
        encoded[0]["principal"], "caller",
        "an unauthorized claim is recorded against the peer that made it"
    );
    assert_eq!(encoded[0]["reason"], "attestation-denied");

    shutdown_send.send(()).expect("signal clean shutdown");
    task.await
        .expect("server task exits")
        .expect("server shuts down");
}

/// `BrokerClient` binds the attestation to its proposal by construction, so reaching the
/// server-side check needs a hand-rolled frame. A claim that names a different invocation is a
/// decode-level protocol error rather than a policy decision — nothing is authorized, and nothing
/// consumes an identifier.
#[tokio::test(flavor = "multi_thread")]
async fn mismatched_attestation_binding_is_a_protocol_error() {
    let uid = current_uid();
    let directory = private_directory();
    let socket_path = directory.path().join("broker.sock");
    let listener = bind_fixture(&socket_path);
    let (broker, audit) = attested_broker().await;
    let mut identities = BTreeMap::new();
    identities.insert(
        uid,
        MappedPeer {
            context: context("caller"),
            attestor: Some(attestor_grant()),
        },
    );
    let limits = server_limits();
    let server = BrokerServer::new(broker, identities, limits).expect("server limits valid");
    let (shutdown_send, shutdown_receive) = oneshot::channel::<()>();
    let task = tokio::spawn(server.serve(listener, shutdown_on(shutdown_receive)));

    let mut stream = UnixStream::connect(&socket_path)
        .await
        .expect("connect to the fixture socket");
    let envelope = RequestEnvelope::invoke(
        Some(Attestation {
            subject: subject(),
            agent: agent("chat-agent"),
            scope: None,
            invocation: Some(
                "invoke-some-other-proposal"
                    .parse::<InvocationId>()
                    .expect("valid invocation fixture"),
            ),
        }),
        request("invoke-bound-identifier"),
    );
    write_frame(&mut stream, &envelope, limits.frame)
        .await
        .expect("write the hand-rolled frame");
    let response = read_frame::<_, ResponseEnvelope>(&mut stream, limits.frame)
        .await
        .expect("read the refusal");
    let BrokerResponse::Error { code, .. } = response.response else {
        panic!("a mismatched binding must not produce an invocation result");
    };
    assert_eq!(code, ERROR_INVALID_REQUEST);
    assert!(
        audit.records().await.is_empty(),
        "a frame refused before dispatch is not a decision about anything"
    );

    shutdown_send.send(()).expect("signal clean shutdown");
    task.await
        .expect("server task exits")
        .expect("server shuts down");
}

/// Inspection follows the same rule as invocation: an attestor peer sees the attested context's
/// capabilities, and a peer without a grant is refused without learning whether the subject is
/// mapped at all.
#[tokio::test(flavor = "multi_thread")]
async fn attested_capabilities_over_the_socket() {
    let uid = current_uid();
    let directory = private_directory();
    let limits = server_limits();

    let granted_path = directory.path().join("granted.sock");
    let granted_listener = bind_fixture(&granted_path);
    let (broker, _audit) = attested_broker().await;
    let mut identities = BTreeMap::new();
    identities.insert(
        uid,
        MappedPeer {
            context: context("caller"),
            attestor: Some(attestor_grant()),
        },
    );
    let granted = BrokerServer::new(broker, identities, limits).expect("server limits valid");
    let (granted_stop, granted_stopped) = oneshot::channel::<()>();
    let granted_task = tokio::spawn(granted.serve(granted_listener, shutdown_on(granted_stopped)));

    let client = BrokerClient::new(&granted_path, uid, limits.frame).expect("client starts");
    let (capabilities, _, _) = client
        .session_surface(Some(Attestation::for_subject(
            subject(),
            agent("chat-agent"),
        )))
        .await
        .expect("an attestor peer may inspect the attested context");
    assert_eq!(capabilities.len(), 1);
    assert_eq!(capabilities[0].capability.id.as_str(), "echo.echo");
    // The peer's own listing is a different answer produced by a different rule, which is what
    // makes the two populations disjoint rather than merely ordered.
    let own = client
        .capabilities()
        .await
        .expect("the peer still sees its own grants");
    assert_eq!(own.len(), 1);

    granted_stop.send(()).expect("signal clean shutdown");
    granted_task
        .await
        .expect("server task exits")
        .expect("server shuts down");

    let ungranted_path = directory.path().join("ungranted.sock");
    let ungranted_listener = bind_fixture(&ungranted_path);
    let (broker, _audit) = attested_broker().await;
    let mut identities = BTreeMap::new();
    identities.insert(
        uid,
        MappedPeer {
            context: context("caller"),
            attestor: None,
        },
    );
    let ungranted = BrokerServer::new(broker, identities, limits).expect("server limits valid");
    let (ungranted_stop, ungranted_stopped) = oneshot::channel::<()>();
    let ungranted_task =
        tokio::spawn(ungranted.serve(ungranted_listener, shutdown_on(ungranted_stopped)));

    let client = BrokerClient::new(&ungranted_path, uid, limits.frame).expect("client starts");
    let refused = client
        .session_surface(Some(Attestation::for_subject(
            subject(),
            agent("chat-agent"),
        )))
        .await
        .expect_err("a peer without attestor authority is refused");
    let ClientError::Remote { code, .. } = refused else {
        panic!("expected a stable remote refusal, got {refused}");
    };
    assert_eq!(code, ERROR_UNAUTHENTICATED);

    ungranted_stop.send(()).expect("signal clean shutdown");
    ungranted_task
        .await
        .expect("server task exits")
        .expect("server shuts down");
}

/// Cedar validates types, not instances, so a policy naming a principal nobody configured is
/// perfectly well typed and would simply never match. The declared world is what turns that into a
/// startup refusal — the same protection the exact engine's reachability check used to provide.
#[tokio::test(flavor = "multi_thread")]
async fn strict_startup_refuses_every_policy_that_names_something_absent() {
    let uid = current_uid();
    let directory = private_directory();
    let config_path = directory.path().join("broker.json");
    let policies_path = directory.path().join("policies.cedar");
    let document = json!({
        "apiVersion": CONFIG_API_VERSION,
        "socketPath": directory.path().join("broker.sock"),
        "brokerPrincipal": "broker-test",
        "policyRevision": "policy-test",
        "policiesPath": &policies_path,
        "strict": true,
        "providers": [provider_fixture("echo-provider.wasm")],
        "identities": [{
            "uid": uid,
            "principal": "caller",
            "actor": {"type": "agent", "agent": "brokerd-test"}
        }],
        "constraintSets": {
            "echo.echo": serde_json::to_value(echo_constraint_set())
                .expect("constraint set serializes")
        }
    });
    write_owner_only(
        &config_path,
        &serde_json::to_vec(&document).expect("config serializes"),
    );

    for (policies, label) in [
        (
            r#"permit(principal == Dekopon::Principal::"nobody",
                      action == Dekopon::Action::"echo.echo",
                      resource == Dekopon::Provider::"echo");"#,
            "an undeclared principal",
        ),
        (
            r#"permit(principal == Dekopon::Principal::"caller",
                      action == Dekopon::Action::"echo.nonexistent",
                      resource == Dekopon::Provider::"echo");"#,
            "an unloaded capability",
        ),
        (
            r#"permit(principal == Dekopon::Principal::"caller",
                      action == Dekopon::Action::"echo.reverse",
                      resource == Dekopon::Provider::"echo");"#,
            "a capability with no constraint set",
        ),
    ] {
        write_owner_only(&policies_path, policies.as_bytes());
        let error = run(&config_path, async {})
            .await
            .err()
            .unwrap_or_else(|| panic!("{label} must refuse startup"));
        assert!(
            matches!(
                error,
                BrokerdError::Policy { .. }
                    | BrokerdError::Broker(BrokerBuildError::UnconstrainedCapability { .. })
            ),
            "{label} produced the wrong refusal: {error:?}"
        );
    }
    assert!(!directory.path().join("broker.sock").exists());
}

/// The default posture is the mirror of `strict_startup_refuses_every_policy_that_names_something_absent`.
///
/// Everything that test proves refuses under `strict: true` must *start* without it, so an operator
/// can ship policy and constraint sets that anticipate a provider they have not dropped in yet. The
/// undeclared principal is the exception and stays fatal: principals come from this very file, not
/// from a loaded component, so naming one that does not exist is always a typo.
#[tokio::test(flavor = "multi_thread")]
async fn default_startup_tolerates_names_no_loaded_provider_declares() {
    let uid = current_uid();
    let directory = private_directory();
    let config_path = directory.path().join("broker.json");
    let policies_path = directory.path().join("policies.cedar");
    let document = json!({
        "apiVersion": CONFIG_API_VERSION,
        "socketPath": directory.path().join("broker.sock"),
        "brokerPrincipal": "broker-test",
        "policyRevision": "policy-test",
        "policiesPath": &policies_path,
        "providers": [provider_fixture("echo-provider.wasm")],
        "identities": [{
            "uid": uid,
            "principal": "caller",
            "actor": {"type": "agent", "agent": "brokerd-test"}
        }],
        "constraintSets": {
            "echo.echo": serde_json::to_value(echo_constraint_set())
                .expect("constraint set serializes")
        }
    });
    write_owner_only(
        &config_path,
        &serde_json::to_vec(&document).expect("config serializes"),
    );

    for (policies, label) in [
        (
            r#"permit(principal == Dekopon::Principal::"caller",
                      action == Dekopon::Action::"echo.nonexistent",
                      resource == Dekopon::Provider::"echo");"#,
            "an unloaded capability",
        ),
        (
            r#"permit(principal == Dekopon::Principal::"caller",
                      action == Dekopon::Action::"echo.reverse",
                      resource == Dekopon::Provider::"echo");"#,
            "a capability with no constraint set",
        ),
        (
            r#"permit(principal == Dekopon::Principal::"caller",
                      action in [Dekopon::Action::"echo.echo",
                                 Dekopon::Action::"echo.nonexistent"],
                      resource == Dekopon::Provider::"echo");"#,
            "a grant mixing a loaded and an unloaded capability",
        ),
    ] {
        write_owner_only(&policies_path, policies.as_bytes());
        run(&config_path, async {})
            .await
            .unwrap_or_else(|error| panic!("{label} must start when tolerating: {error:?}"));
    }

    // Still fatal, in every mode: a principal comes from this configuration, not a component.
    write_owner_only(
        &policies_path,
        r#"permit(principal == Dekopon::Principal::"nobody",
                  action == Dekopon::Action::"echo.echo",
                  resource == Dekopon::Provider::"echo");"#
            .as_bytes(),
    );
    let error = run(&config_path, async {})
        .await
        .expect_err("an undeclared principal refuses startup even when tolerating");
    assert!(matches!(error, BrokerdError::Policy { .. }), "{error:?}");
}
