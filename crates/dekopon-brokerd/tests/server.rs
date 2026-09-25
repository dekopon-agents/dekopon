#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "tests spawn, join and drain freely"
)]
#![allow(clippy::unwrap_used)]

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
    Attestation, BrokerClient, BrokerResponse, ClientError, CommandRunOutcome,
    ERROR_BROKER_UNAVAILABLE, ERROR_CAPACITY_EXHAUSTED, ERROR_INVALID_REQUEST,
    ERROR_UNAUTHENTICATED, FrameLimits, RequestEnvelope, ResponseEnvelope, read_frame, write_frame,
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

const POLICY: &str = r#"
@id("chat-agent-session")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"chat-agent")
when { context.via == "caller" };

@id("chat-agent-upper")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"cli-probe.upper",
       resource == Dekopon::Provider::"cli-probe")
when { context.via == "caller" && context.agent == "chat-agent" };
"#;

fn probe_constraint_set() -> ConstraintSet {
    ConstraintSet {
        route: CapabilityRoute::Generic,
        provider: "cli-probe"
            .parse::<ProviderId>()
            .expect("valid provider fixture"),
        effect: EffectKind::ReadOnly,
        risk: RiskLevel::Low,
        credential: None,
        constraints: ExecutionConstraints::default(),
    }
}

fn probe_catalog() -> ConstraintCatalog {
    ConstraintCatalog::new([(
        "cli-probe.upper"
            .parse::<CapabilityId>()
            .expect("valid capability fixture"),
        probe_constraint_set(),
    )])
    .expect("one capability builds a catalog")
}

fn probe_engine<'a>(policies: &str, principals: impl IntoIterator<Item = &'a str>) -> PolicyEngine {
    let world = PolicyWorld::new(
        principals.into_iter().map(|name| {
            name.parse::<PrincipalId>()
                .expect("valid principal fixture")
        }),
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
    PolicyEngine::new(policies, &world).expect("fixture policy validates")
}

fn request(id: &str) -> InvocationRequest {
    InvocationRequest {
        id: id
            .parse::<InvocationId>()
            .expect("valid invocation fixture"),
        capability: "cli-probe.upper"
            .parse::<CapabilityId>()
            .expect("valid capability fixture"),
        trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
        secret_use: None,
        input: json!({"text": "hello through broker"}),
    }
}

fn write_owner_only(path: &Path, contents: &[u8]) {
    fs::write(path, contents).expect("write fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("secure fixture");
}

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
    broker_with(POLICY, 8).await
}

async fn broker_with(
    policies: &str,
    maximum: usize,
) -> (Arc<Broker<InMemoryAuditLog>>, Arc<InMemoryAuditLog>) {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("cli-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("load cli-probe fixture");
    let audit = Arc::new(InMemoryAuditLog::new(maximum).expect("valid audit bound"));
    let broker = Arc::new(
        Broker::new(
            registry,
            "broker-test"
                .parse::<PrincipalId>()
                .expect("valid broker principal"),
            "policy-test".to_owned(),
            probe_engine(policies, ["caller", "cpetersen"]),
            probe_catalog(),
            CredentialStore::empty(),
            identities(),
            Arc::clone(&audit),
            BrokerLimits::default(),
        )
        .expect("broker starts"),
    );
    (broker, audit)
}

const SLACK_SUBJECT: &str = "slack.t0123abc.u9xyz";

fn subject() -> ExternalSubject {
    SLACK_SUBJECT
        .parse::<ExternalSubject>()
        .expect("canonical subject fixture")
}

fn identities() -> IdentityDirectory {
    IdentityDirectory::new([(
        subject(),
        "cpetersen"
            .parse::<PrincipalId>()
            .expect("valid principal fixture"),
    )])
    .expect("one mapping builds a directory")
}

fn agent(name: &str) -> AgentId {
    name.parse::<AgentId>().expect("valid agent fixture")
}

fn session() -> Attestation {
    Attestation::for_subject(subject(), agent("chat-agent"))
}

fn attestor_grant() -> AttestorGrant {
    AttestorGrant {
        namespaces: Some(vec!["slack.t0123abc".to_owned()]),
    }
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

#[tokio::test(flavor = "multi_thread")]
async fn run_command_over_the_socket_renders_help_then_proposes() {
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
            attestor: Some(attestor_grant()),
        },
    );
    let limits = server_limits();
    let server = BrokerServer::new(broker, identities, limits).expect("server limits valid");
    let (shutdown_send, shutdown_receive) = oneshot::channel::<()>();
    let task = tokio::spawn(server.serve(listener, shutdown_on(shutdown_receive)));

    let client = BrokerClient::new(&socket_path, uid, limits.frame).expect("client starts");
    let (_, words, _) = client
        .session_surface(Some(session()))
        .await
        .expect("inspect the surface");
    assert_eq!(words, ["probe"]);

    match client
        .run_command(
            Some(session()),
            "probe".to_owned(),
            vec!["--help".to_owned()],
            None,
            TRACE_PARENT.parse().expect("valid traceparent fixture"),
        )
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
    assert!(audit.records().is_empty(), "rendering decides nothing");

    let (capability, input) = match client
        .run_command(
            Some(session()),
            "probe".to_owned(),
            vec!["upper".to_owned(), "-".to_owned()],
            Some("hello".to_owned()),
            TRACE_PARENT.parse().expect("valid traceparent fixture"),
        )
        .await
        .expect("the piped value proposes")
    {
        CommandRunOutcome::Proposed {
            capability,
            input,
            secret_use: None,
        } => (capability, input),
        other => panic!("expected a proposal, got {other:?}"),
    };
    assert_eq!(capability.as_str(), "cli-probe.upper");
    assert_eq!(input, json!({"text": "hello"}));

    let result = client
        .invoke(
            Some(session()),
            InvocationRequest {
                id: "invoke-probe-upper"
                    .parse::<InvocationId>()
                    .expect("valid invocation fixture"),
                capability,
                trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
                secret_use: None,
                input,
            },
            Default::default(),
        )
        .await
        .expect("invoke the proposal");
    assert_eq!(result.result.outcome, InvocationOutcome::Succeeded);
    assert_eq!(result.result.output, Some(json!({"text": "HELLO"})));
    assert_eq!(
        audit.records().len(),
        2,
        "one decision and one execution for the one invocation"
    );

    shutdown_send.send(()).expect("signal clean shutdown");
    task.await
        .expect("server task exits")
        .expect("server shuts down");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_direct_unix_peer_holds_no_capability_even_when_policy_names_it() {
    let uid = current_uid();
    let directory = private_directory();
    let socket_path = directory.path().join("broker.sock");
    let listener = bind_fixture(&socket_path);
    let (broker, audit) = broker_with(
        &format!(
            "{POLICY}\n{}",
            r#"permit(principal == Dekopon::Principal::"caller", action, resource);"#
        ),
        8,
    )
    .await;
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
    assert!(
        client
            .capabilities()
            .await
            .expect("inspect capabilities")
            .is_empty()
    );
    let (capabilities, words, _) = client
        .session_surface(None)
        .await
        .expect("inspect the surface");
    assert!(capabilities.is_empty() && words.is_empty());
    let result = client
        .invoke(None, request("invoke-direct"), Default::default())
        .await
        .expect("a denial is a completed invocation response");
    assert_eq!(result.result.outcome, InvocationOutcome::Denied);
    assert_eq!(result.result.error.as_deref(), Some("policy-error"));
    assert_eq!(audit.records().len(), 1);

    let (capabilities, _, _) = client
        .session_surface(Some(session()))
        .await
        .expect("the same peer may still attest a session");
    assert_eq!(capabilities.len(), 1);

    shutdown_send.send(()).expect("signal clean shutdown");
    task.await
        .expect("server task exits")
        .expect("server shuts down");
}

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
    assert!(!message.contains("cli-probe"), "{message}");
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

    let policies = r#"@id("chat-agent-session")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"chat-agent");

@id("cpetersen-fetch")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"http-probe.fetch",
       resource == Dekopon::Provider::"http-probe")
when { context.agent == "chat-agent" };

@id("cpetersen-secret")
permit(principal == Dekopon::Principal::"cpetersen",
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
        constraints: ExecutionConstraints {
            asset: None,
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
            "actor": {"type": "agent", "agent": "brokerd-test"},
            "attestor": {}
        }],
        "principals": {"cpetersen": {"subjects": [SLACK_SUBJECT]}},
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
        .invoke(Some(session()), invocation.clone(), Default::default())
        .await
        .expect("secret invocation succeeds");
    assert_eq!(result.result.outcome, InvocationOutcome::Succeeded);
    let wire = String::from_utf8(wire_receive.await.expect("HTTP wire")).expect("wire text");
    assert!(
        wire.contains("authorization: Bearer brokerd-secret-value"),
        "{wire}"
    );
    upstream.await.expect("HTTP fixture exits");

    invocation.id = "invoke-secret-wrong-path".parse().expect("invocation");
    invocation.input = json!({
        "uri": format!("http://{authority}/api/v1/other"),
        "method": "GET"
    });
    let denied = client
        .invoke(Some(session()), invocation, Default::default())
        .await
        .expect("host refusal is accounted");
    assert_eq!(denied.result.outcome, InvocationOutcome::Failed);

    stop.send(()).expect("stop service");
    service
        .await
        .expect("service task exits")
        .expect("service stops");
}

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
    let (broker, audit) = broker_with(POLICY, 1).await;
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
    let ran = client
        .invoke(
            Some(session()),
            request("invoke-outcome-unaudited"),
            Default::default(),
        )
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
    assert_eq!(audit.records().len(), 1);

    let never_ran = client
        .invoke(
            Some(session()),
            request("invoke-never-ran"),
            Default::default(),
        )
        .await
        .expect_err("a full audit cannot authorize");
    let ClientError::Remote {
        code: unran_code,
        message: unran_message,
    } = never_ran
    else {
        panic!("expected a remote broker failure, got {never_ran}");
    };
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

#[tokio::test(flavor = "multi_thread")]
async fn an_attested_invoke_over_the_socket_succeeds_for_an_attestor_peer() {
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
            Some(session()),
            request("invoke-attested-socket"),
            Default::default(),
        )
        .await
        .expect("attested invocation completes");
    assert_eq!(result.result.outcome, InvocationOutcome::Succeeded);
    assert_eq!(
        result.result.output,
        Some(json!({"text": "HELLO THROUGH BROKER"}))
    );

    let records = audit.records();
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

#[tokio::test(flavor = "multi_thread")]
async fn an_attested_invoke_from_a_peer_without_a_grant_is_denied_not_erred() {
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
    let result = client
        .invoke(
            Some(session()),
            request("invoke-ungranted-socket"),
            Default::default(),
        )
        .await
        .expect("a refused attestation is still a completed invocation response");
    assert_eq!(result.result.outcome, InvocationOutcome::Denied);
    assert_eq!(result.result.error.as_deref(), Some("attestation-denied"));

    let records = audit.records();
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

#[tokio::test(flavor = "multi_thread")]
async fn mismatched_attestation_binding_is_a_protocol_error() {
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
        vec![],
        0,
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
        audit.records().is_empty(),
        "a frame refused before dispatch is not a decision about anything"
    );

    shutdown_send.send(()).expect("signal clean shutdown");
    task.await
        .expect("server task exits")
        .expect("server shuts down");
}

#[tokio::test(flavor = "multi_thread")]
async fn attested_capabilities_over_the_socket() {
    let uid = current_uid();
    let directory = private_directory();
    let limits = server_limits();

    let granted_path = directory.path().join("granted.sock");
    let granted_listener = bind_fixture(&granted_path);
    let (granted_broker, _audit) = broker().await;
    let mut identities = BTreeMap::new();
    identities.insert(
        uid,
        MappedPeer {
            context: context("caller"),
            attestor: Some(attestor_grant()),
        },
    );
    let granted =
        BrokerServer::new(granted_broker, identities, limits).expect("server limits valid");
    let (granted_stop, granted_stopped) = oneshot::channel::<()>();
    let granted_task = tokio::spawn(granted.serve(granted_listener, shutdown_on(granted_stopped)));

    let client = BrokerClient::new(&granted_path, uid, limits.frame).expect("client starts");
    let (capabilities, _, _) = client
        .session_surface(Some(session()))
        .await
        .expect("an attestor peer may inspect the attested context");
    assert_eq!(capabilities.len(), 1);
    assert_eq!(capabilities[0].capability.id.as_str(), "cli-probe.upper");

    granted_stop.send(()).expect("signal clean shutdown");
    granted_task
        .await
        .expect("server task exits")
        .expect("server shuts down");

    let ungranted_path = directory.path().join("ungranted.sock");
    let ungranted_listener = bind_fixture(&ungranted_path);
    let (ungranted_broker, _audit) = broker().await;
    let mut identities = BTreeMap::new();
    identities.insert(
        uid,
        MappedPeer {
            context: context("caller"),
            attestor: None,
        },
    );
    let ungranted =
        BrokerServer::new(ungranted_broker, identities, limits).expect("server limits valid");
    let (ungranted_stop, ungranted_stopped) = oneshot::channel::<()>();
    let ungranted_task =
        tokio::spawn(ungranted.serve(ungranted_listener, shutdown_on(ungranted_stopped)));

    let client = BrokerClient::new(&ungranted_path, uid, limits.frame).expect("client starts");
    let refused = client
        .session_surface(Some(session()))
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
        "providers": [provider_fixture("cli-probe-provider.wasm")],
        "identities": [{
            "uid": uid,
            "principal": "caller",
            "actor": {"type": "agent", "agent": "brokerd-test"}
        }],
        "constraintSets": {
            "cli-probe.upper": serde_json::to_value(probe_constraint_set())
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
                      action == Dekopon::Action::"cli-probe.upper",
                      resource == Dekopon::Provider::"cli-probe");"#,
            "an undeclared principal",
        ),
        (
            r#"permit(principal == Dekopon::Principal::"caller",
                      action == Dekopon::Action::"cli-probe.nonexistent",
                      resource == Dekopon::Provider::"cli-probe");"#,
            "an unloaded capability",
        ),
        (
            r#"permit(principal == Dekopon::Principal::"caller",
                      action == Dekopon::Action::"cli-probe.reverse",
                      resource == Dekopon::Provider::"cli-probe");"#,
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
        "providers": [provider_fixture("cli-probe-provider.wasm")],
        "identities": [{
            "uid": uid,
            "principal": "caller",
            "actor": {"type": "agent", "agent": "brokerd-test"}
        }],
        "constraintSets": {
            "cli-probe.upper": serde_json::to_value(probe_constraint_set())
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
                      action == Dekopon::Action::"cli-probe.nonexistent",
                      resource == Dekopon::Provider::"cli-probe");"#,
            "an unloaded capability",
        ),
        (
            r#"permit(principal == Dekopon::Principal::"caller",
                      action == Dekopon::Action::"cli-probe.reverse",
                      resource == Dekopon::Provider::"cli-probe");"#,
            "a capability with no constraint set",
        ),
        (
            r#"permit(principal == Dekopon::Principal::"caller",
                      action in [Dekopon::Action::"cli-probe.upper",
                                 Dekopon::Action::"cli-probe.nonexistent"],
                      resource == Dekopon::Provider::"cli-probe");"#,
            "a grant mixing a loaded and an unloaded capability",
        ),
    ] {
        write_owner_only(&policies_path, policies.as_bytes());
        run(&config_path, async {})
            .await
            .unwrap_or_else(|error| panic!("{label} must start when tolerating: {error:?}"));
    }

    write_owner_only(
        &policies_path,
        r#"permit(principal == Dekopon::Principal::"nobody",
                  action == Dekopon::Action::"cli-probe.upper",
                  resource == Dekopon::Provider::"cli-probe");"#
            .as_bytes(),
    );
    let error = run(&config_path, async {})
        .await
        .expect_err("an undeclared principal refuses startup even when tolerating");
    assert!(matches!(error, BrokerdError::Policy { .. }), "{error:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_non_invoke_frame_with_descriptors_is_refused_without_a_broker_decision() {
    use dekopon_broker_protocol::DescriptorStream;
    use std::os::fd::AsFd as _;
    let uid = current_uid();
    let directory = tempfile::tempdir().unwrap();
    let socket_path = directory.path().join("broker.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    let (broker, audit) = broker().await;
    let identities = BTreeMap::from([(
        uid,
        MappedPeer {
            context: context("caller"),
            attestor: None,
        },
    )]);
    let limits = server_limits();
    let server = BrokerServer::new(broker, identities, limits).unwrap();
    let (shutdown_send, shutdown_receive) = oneshot::channel();
    let task = tokio::spawn(server.serve(listener, shutdown_on(shutdown_receive)));
    let stream = UnixStream::connect(&socket_path).await.unwrap();
    let mut stream = DescriptorStream::new(stream);
    let file = tempfile::tempfile().unwrap();
    stream
        .write_frame(
            &RequestEnvelope::capabilities(None),
            &[file.as_fd()],
            limits.frame,
        )
        .await
        .unwrap();
    let (response, descriptors) = stream
        .read_frame::<ResponseEnvelope>(limits.frame)
        .await
        .unwrap();
    assert!(descriptors.is_empty());
    assert!(
        matches!(response.response, BrokerResponse::Error { code, .. } if code == ERROR_INVALID_REQUEST)
    );
    assert!(audit.records().is_empty());
    shutdown_send.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn successful_asset_descriptors_and_send_effects_cross_the_real_server_with_ordinary_audit() {
    use dekopon_broker_protocol::{AssetEncoding, AssetRow, InvokeAssets};
    use dekopon_capability::AssetConstraints;
    use dekopon_http_host::asset::AssetDirectory;
    use std::os::unix::fs::FileExt as _;
    let uid = current_uid();
    let directory = private_directory();
    let socket_path = directory.path().join("broker.sock");
    let assets_root = directory.path().join("assets");
    fs::create_dir(&assets_root).unwrap();
    let listener = bind_fixture(&socket_path);
    let mut registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .unwrap();
    let asset_directory = AssetDirectory::new(assets_root.clone(), 1024);
    registry.set_assets(asset_directory.clone());
    let capability: CapabilityId = "http-probe.purge".parse().unwrap();
    let provider: ProviderId = "http-probe".parse().unwrap();
    let world = PolicyWorld::new(
        [
            "caller".parse::<PrincipalId>().unwrap(),
            "cpetersen".parse().unwrap(),
        ],
        [(capability.clone(), provider.clone())],
    )
    .unwrap();
    let engine = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"cpetersen", action == Dekopon::Action::"agent.prompt", resource == Dekopon::Agent::"chat-agent");
permit(principal == Dekopon::Principal::"cpetersen", action == Dekopon::Action::"http-probe.purge", resource == Dekopon::Provider::"http-probe");"#,
        &world,
    )
    .unwrap();
    let catalog = ConstraintCatalog::new([(
        capability.clone(),
        ConstraintSet {
            route: CapabilityRoute::Generic,
            provider,
            effect: EffectKind::ExternalWrite,
            risk: RiskLevel::High,
            credential: None,
            constraints: ExecutionConstraints {
                asset: Some(AssetConstraints {
                    attach: true,
                    send: true,
                    remove: false,
                }),
                ..Default::default()
            },
        },
    )])
    .unwrap();
    let audit = Arc::new(InMemoryAuditLog::new(16).unwrap());
    let broker = Arc::new(
        Broker::new(
            registry,
            "broker-test".parse().unwrap(),
            "policy-test".to_owned(),
            engine,
            catalog,
            CredentialStore::empty(),
            identities(),
            Arc::clone(&audit),
            BrokerLimits::default(),
        )
        .unwrap(),
    );
    let limits = server_limits();
    let server = BrokerServer::new(
        broker,
        BTreeMap::from([(
            uid,
            MappedPeer {
                context: context("caller"),
                attestor: Some(attestor_grant()),
            },
        )]),
        limits,
    )
    .unwrap();
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(server.serve(listener, shutdown_on(stopped)));
    let client = BrokerClient::new(&socket_path, uid, limits.frame).unwrap();
    let mut invocation = request("attach-asset");
    invocation.capability = capability.clone();
    invocation.input = json!({"assetMode": "attach"});
    let attached = client
        .invoke(Some(session()), invocation, InvokeAssets::default())
        .await
        .unwrap();
    assert_eq!(attached.result.outcome, InvocationOutcome::Succeeded);
    assert_eq!(attached.attached.len(), 1);
    assert_eq!(attached.descriptors.len(), 1);
    let file = std::fs::File::from(attached.descriptors.into_iter().next().unwrap());
    let mut bytes = [0; 11];
    file.read_exact_at(&mut bytes, 0).unwrap();
    assert_eq!(&bytes, b"asset probe");
    assert!(file.write_at(b"x", 0).is_err());
    assert_eq!(fs::read_dir(&assets_root).unwrap().count(), 0);
    let mut invocation = request("send-asset");
    invocation.capability = capability.clone();
    invocation.input = json!({"assetMode": "send", "reference": "chat-asset:1"});
    let sent = client
        .invoke(
            Some(session()),
            invocation,
            InvokeAssets {
                rows: vec![AssetRow {
                    id: 1,
                    content_type: "text/plain".to_owned(),
                    encoding: AssetEncoding::Identity,
                    bytes: Some(11),
                    origin: "provider:http-probe.purge".to_owned(),
                    sent: false,
                }],
                descriptors: vec![file.into()],
                sends_remaining: 1,
            },
        )
        .await
        .unwrap();
    assert_eq!(sent.result.outcome, InvocationOutcome::Succeeded);
    assert_eq!(sent.sent, vec![1]);
    assert!(sent.descriptors.is_empty());
    assert_eq!(audit.records().len(), 4);

    let mut invocation = request("caught-over-budget");
    invocation.capability = capability.clone();
    invocation.input = json!({"assetMode": "direct-write", "bytes": 1025});
    let refused = client
        .invoke(Some(session()), invocation, InvokeAssets::default())
        .await
        .unwrap();
    assert_eq!(refused.result.outcome, InvocationOutcome::Failed);
    assert_eq!(refused.result.error.as_deref(), Some("over-budget"));
    assert!(
        refused.attached.is_empty()
            && refused.removed.is_empty()
            && refused.sent.is_empty()
            && refused.descriptors.is_empty()
    );

    let stream = UnixStream::connect(&socket_path)
        .await
        .unwrap()
        .into_std()
        .unwrap();
    stream.shutdown(std::net::Shutdown::Read).unwrap();
    let mut stream =
        dekopon_broker_protocol::DescriptorStream::new(UnixStream::from_std(stream).unwrap());
    let mut invocation = request("failed-asset-response-write");
    invocation.capability = capability;
    invocation.input = json!({"assetMode": "attach"});
    let attestation = session().bound_to(invocation.id.clone());
    stream
        .write_frame(
            &RequestEnvelope::invoke(Some(attestation), invocation, vec![], 0),
            &[],
            limits.frame,
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while audit.records().len() < 8 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(stream);
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(fs::read_dir(&assets_root).unwrap().count(), 0);
    asset_directory
        .allocate()
        .await
        .unwrap()
        .write(vec![0; 1024])
        .await
        .unwrap();
}
