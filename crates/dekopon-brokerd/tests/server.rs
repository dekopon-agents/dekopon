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

use dekopon_agent::BrokerLeg;
use dekopon_broker::{
    AttestorGrant, AuthenticatedContext, Broker, BrokerBuildError, BrokerLimits, CapabilityRoute,
    ConstraintCatalog, ConstraintSet, CredentialStore, IdentityDirectory, InMemoryAuditLog,
    InvocationRequest, PolicyEngine, PolicyWorld,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_broker_protocol::{
    Attestation, BrokerClient, BrokerRequest, BrokerResponse, ChatScopeClaim, ChatTransportKind,
    ClientError, CommandRunOutcome, Conversation, ConversationKind, DescriptorStream,
    ERROR_BROKER_UNAVAILABLE, ERROR_CAPACITY_EXHAUSTED, ERROR_INVALID_REQUEST,
    ERROR_UNAUTHENTICATED, FrameLimits, ProtocolVersion, RequestEnvelope, ResponseEnvelope,
    Trigger, UpcallStdin, UpcallStreams, read_frame, write_frame,
};
use dekopon_brokerd::{
    BrokerServer, BrokerdError, CONFIG_API_VERSION, MappedPeer, ServerLimits, capabilities,
    current_uid, run,
};
use dekopon_capability::{EffectKind, ExecutionConstraints, InvocationOutcome};
use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, InvocationId, PrincipalId, ProviderId,
    RiskLevel, SecretUseProposal,
};
use dekopon_shell::{
    CallBudget, CapabilityCallResult, CapabilityInvoker, CommandProposal, ExitCode, Interpreter,
    JobControl, JobId, JobRefusal, JobSeed, JobSummary, JobWait, Limits, Streams, TreeContext,
};
use dekopon_test_support::{provider_fixture, shutdown_on};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{UnixListener, UnixStream},
    sync::oneshot,
};

#[path = "fixture/parked_component.rs"]
mod parked_component;
#[path = "fixture/spawn_component.rs"]
mod spawn_component;

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

fn probe_capabilities() -> serde_json::Value {
    json!({
        "cli-probe": {
            "constraints": {"timeoutMs": 30_000},
            "capabilities": {"cli-probe.upper": {}}
        }
    })
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

fn stdio(
    stdin: Option<&[u8]>,
) -> (
    dekopon_broker_protocol::InvokeAssets,
    std::thread::JoinHandle<Vec<u8>>,
) {
    use std::io::Write;
    let (host, mut stdout) = std::os::unix::net::UnixStream::pair().unwrap();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut bytes).unwrap();
        bytes
    });
    let stdin = stdin.map(|bytes| {
        let (host, mut feeder) = std::os::unix::net::UnixStream::pair().unwrap();
        feeder.write_all(bytes).unwrap();
        host.into()
    });
    (
        dekopon_broker_protocol::InvokeAssets {
            streams: Some(dekopon_broker_protocol::Streams {
                stdin,
                stdout: host.into(),
            }),
            ..Default::default()
        },
        reader,
    )
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
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
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
    let (_, words, _, _) = client
        .session_surface(Some(session()))
        .await
        .expect("inspect the surface");
    assert_eq!(words, ["probe"]);

    match client
        .run_command(
            Some(session()),
            "probe".to_owned(),
            vec!["--help".to_owned()],
            false,
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
            true,
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
    assert_eq!(input, json!({"text": "", "piped": true}));

    let (streams, stdout) = stdio(Some(b"hello"));
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
            streams,
            async |_upcall| unreachable!(),
        )
        .await
        .expect("invoke the proposal");
    assert_eq!(stdout.join().unwrap(), b"{\"text\":\"HELLO\"}\n");
    assert_eq!(result.result.outcome, InvocationOutcome::Succeeded);
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
            r#"@id("caller-unconstrained") permit(principal == Dekopon::Principal::"caller", action, resource);"#
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
    let (capabilities, words, _, _) = client
        .session_surface(None)
        .await
        .expect("inspect the surface");
    assert!(capabilities.is_empty() && words.is_empty());
    let result = client
        .invoke(
            None,
            request("invoke-direct"),
            Default::default(),
            async |_upcall| unreachable!(),
        )
        .await
        .expect("a denial is a completed invocation response");
    assert_eq!(result.result.outcome, InvocationOutcome::Denied);
    assert_eq!(result.result.error.as_deref(), Some("policy-error"));
    assert_eq!(audit.records().len(), 1);

    let (capabilities, _, _, _) = client
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
#[expect(
    clippy::too_many_lines,
    reason = "one private credential resolution and authorization scenario"
)]
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
    let document = json!({
        "apiVersion": CONFIG_API_VERSION,
        "socketPath": &socket_path,
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
        "capabilities": {
            "http-probe": {
                "constraints": {
                    "timeoutMs": 5_000,
                    "http": {
                        "allowedHosts": [&authority],
                        "allowedMethods": ["GET"],
                        "maxRequests": 1,
                        "maxRequestBytes": 64 * 1024,
                        "maxResponseBytes": 64 * 1024,
                        "allowPlaintextLoopback": true
                    }
                },
                "capabilities": {"http-probe.fetch": {}}
            }
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
    let (streams, stdout) = stdio(None);
    let result = client
        .invoke(
            Some(session()),
            invocation.clone(),
            streams,
            async |_upcall| unreachable!(),
        )
        .await
        .expect("secret invocation succeeds");
    assert_eq!(
        serde_json::from_slice::<Value>(&stdout.join().unwrap()).unwrap()["status"],
        200
    );
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
        .invoke(
            Some(session()),
            invocation,
            Default::default(),
            async |_upcall| unreachable!(),
        )
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
            async |_upcall| unreachable!(),
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
            async |_upcall| unreachable!(),
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
    let (streams, stdout) = stdio(None);
    let result = client
        .invoke(
            Some(session()),
            request("invoke-attested-socket"),
            streams,
            async |_upcall| unreachable!(),
        )
        .await
        .expect("attested invocation completes");
    assert_eq!(
        stdout.join().unwrap(),
        b"{\"text\":\"HELLO THROUGH BROKER\"}\n"
    );
    assert_eq!(result.result.outcome, InvocationOutcome::Succeeded);

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
async fn a_peer_hangup_during_parked_invoke_still_audits_and_closes_streams() {
    let component = parked_component::component();
    let registry = BrokerProviderRegistry::load([component.path()], BrokerHostLimits::default())
        .await
        .expect("typed parked provider loads");
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("audit bound"));
    let broker = Arc::new(
        Broker::new(
            registry,
            "broker-test".parse().unwrap(),
            "policy-test".to_owned(),
            probe_engine(POLICY, ["caller", "cpetersen"]),
            probe_catalog(),
            CredentialStore::empty(),
            identities(),
            Arc::clone(&audit),
            BrokerLimits::default(),
        )
        .expect("broker starts"),
    );
    let uid = current_uid();
    let directory = private_directory();
    let socket_path = directory.path().join("broker.sock");
    let listener = bind_fixture(&socket_path);
    let identities = BTreeMap::from([(
        uid,
        MappedPeer {
            context: context("caller"),
            attestor: Some(attestor_grant()),
        },
    )]);
    let limits = server_limits();
    let server = BrokerServer::new(broker, identities, limits).expect("server starts");
    let (shutdown, stop) = oneshot::channel::<()>();
    let server_task = tokio::spawn(server.serve(listener, shutdown_on(stop)));
    let client = BrokerClient::new(&socket_path, uid, limits.frame).expect("client starts");
    let (stdin, _feeder) = std::os::unix::net::UnixStream::pair().unwrap();
    let (stdout, mut reader) = std::os::unix::net::UnixStream::pair().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let pending = tokio::spawn(async move {
        client
            .invoke(
                Some(session()),
                request("invoke-parked-peer-hangup"),
                dekopon_broker_protocol::InvokeAssets {
                    streams: Some(dekopon_broker_protocol::Streams {
                        stdin: Some(stdin.into()),
                        stdout: stdout.into(),
                    }),
                    ..Default::default()
                },
                async |_upcall| unreachable!(),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while audit.records().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("decision audited before hangup");
    pending.abort();
    assert!(
        pending
            .await
            .expect_err("client was aborted")
            .is_cancelled()
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while audit.records().len() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("terminal execution audit survives peer hangup");
    let encoded = serde_json::to_value(audit.records()).unwrap();
    assert_eq!(encoded[1]["error"], "peer-disconnected");
    let read = tokio::task::spawn_blocking(move || {
        use std::io::Read as _;
        let mut byte = [0_u8; 1];
        reader.read(&mut byte)
    })
    .await
    .unwrap()
    .expect("stdout closes after cleanup");
    assert_eq!(read, 0, "host must release stdout on peer hangup");
    shutdown.send(()).expect("stop broker");
    server_task
        .await
        .expect("server task")
        .expect("clean shutdown");
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
            async |_upcall| unreachable!(),
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
        None,
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
async fn unsolicited_upcall_result_is_an_invalid_request() {
    use dekopon_broker_protocol::BrokerRequest;

    let uid = current_uid();
    let directory = private_directory();
    let socket_path = directory.path().join("unsolicited.sock");
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
    let server = BrokerServer::new(broker, identities, limits).expect("server limits");
    let (shutdown_send, shutdown_receive) = oneshot::channel::<()>();
    let task = tokio::spawn(server.serve(listener, shutdown_on(shutdown_receive)));

    let mut stream = UnixStream::connect(&socket_path).await.expect("connect");
    write_frame(
        &mut stream,
        &RequestEnvelope {
            api_version: dekopon_broker_protocol::ProtocolVersion::V1Alpha2,
            request: BrokerRequest::UpcallResult {
                status: 0,
                stderr: String::new(),
            },
        },
        limits.frame,
    )
    .await
    .expect("send stray result");
    let response = read_frame::<_, ResponseEnvelope>(&mut stream, limits.frame)
        .await
        .expect("refusal");
    assert!(
        matches!(response.response, BrokerResponse::Error { code, .. } if code == ERROR_INVALID_REQUEST)
    );
    assert!(audit.records().is_empty());
    assert_eq!(stream.read(&mut [0]).await.expect("connection closes"), 0);
    shutdown_send.send(()).expect("shutdown");
    task.await.expect("join").expect("clean shutdown");
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
    let (capabilities, _, _, _) = client
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
        "policiesPath": &policies_path,
        "strict": true,
        "providers": [provider_fixture("cli-probe-provider.wasm")],
        "identities": [{
            "uid": uid,
            "principal": "caller",
            "actor": {"type": "agent", "agent": "brokerd-test"}
        }],
        "capabilities": probe_capabilities()
    });
    write_owner_only(
        &config_path,
        &serde_json::to_vec(&document).expect("config serializes"),
    );

    for (policies, label) in [
        (
            r#"@id("names-nobody")
               permit(principal == Dekopon::Principal::"nobody",
                      action == Dekopon::Action::"cli-probe.upper",
                      resource == Dekopon::Provider::"cli-probe");"#,
            "an undeclared principal",
        ),
        (
            r#"@id("names-an-unloaded-capability")
               permit(principal == Dekopon::Principal::"caller",
                      action == Dekopon::Action::"cli-probe.nonexistent",
                      resource == Dekopon::Provider::"cli-probe");"#,
            "an unloaded capability",
        ),
        (
            r#"@id("names-an-unconstrained-capability")
               permit(principal == Dekopon::Principal::"caller",
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
        "policiesPath": &policies_path,
        "providers": [provider_fixture("cli-probe-provider.wasm")],
        "identities": [{
            "uid": uid,
            "principal": "caller",
            "actor": {"type": "agent", "agent": "brokerd-test"}
        }],
        "capabilities": probe_capabilities()
    });
    write_owner_only(
        &config_path,
        &serde_json::to_vec(&document).expect("config serializes"),
    );

    for (policies, label) in [
        (
            r#"@id("names-an-unloaded-capability")
               permit(principal == Dekopon::Principal::"caller",
                      action == Dekopon::Action::"cli-probe.nonexistent",
                      resource == Dekopon::Provider::"cli-probe");"#,
            "an unloaded capability",
        ),
        (
            r#"@id("names-an-unconstrained-capability")
               permit(principal == Dekopon::Principal::"caller",
                      action == Dekopon::Action::"cli-probe.reverse",
                      resource == Dekopon::Provider::"cli-probe");"#,
            "a capability with no constraint set",
        ),
        (
            r#"@id("mixes-loaded-and-unloaded")
               permit(principal == Dekopon::Principal::"caller",
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
        r#"@id("names-nobody")
           permit(principal == Dekopon::Principal::"nobody",
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
#[expect(clippy::too_many_lines, reason = "one long test scenario")]
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
        r#"@id("cpetersen-may-prompt-chat-agent")
permit(principal == Dekopon::Principal::"cpetersen", action == Dekopon::Action::"agent.prompt", resource == Dekopon::Agent::"chat-agent");
@id("cpetersen-http-probe-purge")
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
    let (streams, attached_stdout) = stdio(None);
    let attached = client
        .invoke(
            Some(session()),
            invocation,
            streams,
            async |_upcall| unreachable!(),
        )
        .await
        .unwrap();
    assert_eq!(attached.result.outcome, InvocationOutcome::Succeeded);
    assert_eq!(attached_stdout.join().unwrap(), b"{\"ok\":true}\n");
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
    let (streams, sent_stdout) = stdio(None);
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
                streams: streams.streams,
            },
            async |_upcall| unreachable!(),
        )
        .await
        .unwrap();
    assert_eq!(sent.result.outcome, InvocationOutcome::Succeeded);
    assert!(!sent_stdout.join().unwrap().is_empty());
    assert_eq!(sent.sent, vec![1]);
    assert!(sent.descriptors.is_empty());
    assert_eq!(audit.records().len(), 4);

    let mut invocation = request("caught-over-budget");
    invocation.capability = capability.clone();
    invocation.input = json!({"assetMode": "budget-write", "bytes": 1025});
    let refused = client
        .invoke(
            Some(session()),
            invocation,
            InvokeAssets::default(),
            async |_upcall| unreachable!(),
        )
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
            &RequestEnvelope::invoke(Some(attestation), invocation, vec![], 0, None),
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

#[tokio::test(flavor = "multi_thread")]
async fn a_capability_the_owner_did_not_list_gets_no_constraint_set() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("cli-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("load cli-probe fixture");
    let unlisted = "cli-probe.reverse"
        .parse::<CapabilityId>()
        .expect("valid capability fixture");
    assert!(
        registry
            .capabilities()
            .any(|(_, capability)| capability.id == unlisted),
        "the fixture declares a capability the block leaves out"
    );
    let providers = serde_json::from_value(probe_capabilities()).expect("capabilities decode");

    let (sets, problems) = capabilities::constraint_sets(&providers, |provider| {
        let declared = registry
            .capabilities()
            .filter(|(owner, _)| *owner == provider)
            .map(|(_, capability)| capabilities::ManifestCapability {
                id: &capability.id,
                effect: capability.effect,
                risk: capability.risk,
            })
            .collect::<Vec<_>>();
        (!declared.is_empty()).then_some(declared)
    });

    assert_eq!(problems, []);
    assert_eq!(
        sets.keys().map(CapabilityId::as_str).collect::<Vec<_>>(),
        ["cli-probe.upper"]
    );
}

fn spawn_catalog() -> ConstraintCatalog {
    let mut write = probe_constraint_set();
    write.effect = EffectKind::ExternalWrite;
    write.risk = RiskLevel::High;
    ConstraintCatalog::new([
        ("cli-probe.upper".parse().unwrap(), probe_constraint_set()),
        ("cli-probe.write".parse().unwrap(), write),
    ])
    .expect("both fixture capabilities have constraints")
}

fn spawn_policy() -> PolicyEngine {
    let world = PolicyWorld::new(
        ["caller", "cpetersen"]
            .into_iter()
            .map(|name| name.parse().unwrap()),
        ["cli-probe.upper", "cli-probe.write"]
            .into_iter()
            .map(|name| (name.parse().unwrap(), "cli-probe".parse().unwrap())),
    )
    .expect("spawn policy world");
    PolicyEngine::new(
        &format!(
            "{POLICY}\n@id(\"chat-agent-write\")\npermit(principal == Dekopon::Principal::\"cpetersen\", action == Dekopon::Action::\"cli-probe.write\", resource == Dekopon::Provider::\"cli-probe\") when {{ context.via == \"caller\" && context.agent == \"chat-agent\" && context has trigger && context.trigger == \"message\" }};"
        ),
        &world,
    )
    .expect("write allowed by policy but not by probe")
}

struct SpawnBroker {
    _directory: tempfile::TempDir,
    _component: tempfile::NamedTempFile,
    socket: std::path::PathBuf,
    audit: Arc<InMemoryAuditLog>,
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<Result<(), dekopon_brokerd::ServerError>>,
}

impl SpawnBroker {
    async fn start(max_connections: usize) -> Self {
        Self::start_for(max_connections, spawn_component::SCRIPT).await
    }

    async fn start_for(max_connections: usize, script: &str) -> Self {
        let component = if script == spawn_component::SCRIPT {
            spawn_component::component()
        } else {
            spawn_component::component_for(script)
        };
        let registry =
            BrokerProviderRegistry::load([component.path()], BrokerHostLimits::default())
                .await
                .expect("spawn fixture loads");
        let audit = Arc::new(InMemoryAuditLog::new(8).expect("audit bound"));
        let broker = Arc::new(
            Broker::new(
                registry,
                "broker-test".parse().unwrap(),
                "policy-test".to_owned(),
                spawn_policy(),
                spawn_catalog(),
                CredentialStore::empty(),
                identities(),
                Arc::clone(&audit),
                BrokerLimits::default(),
            )
            .expect("broker starts"),
        );
        let directory = private_directory();
        let socket = directory.path().join("broker.sock");
        let listener = bind_fixture(&socket);
        let identities = BTreeMap::from([(
            current_uid(),
            MappedPeer {
                context: AuthenticatedContext::new(
                    "caller".parse().unwrap(),
                    Actor::Service {
                        principal: "caller".parse().unwrap(),
                    },
                )
                .unwrap(),
                attestor: Some(attestor_grant()),
            },
        )]);
        let limits = ServerLimits {
            max_connections,
            ..server_limits()
        };
        let server = BrokerServer::new(broker, identities, limits).expect("server starts");
        let (shutdown, stop) = oneshot::channel::<()>();
        let server = tokio::spawn(server.serve(listener, shutdown_on(stop)));
        Self {
            _directory: directory,
            _component: component,
            socket,
            audit,
            shutdown,
            server,
        }
    }

    async fn invoke(&self, id: &str, input: Value) -> DescriptorStream {
        let mut invocation = request(id);
        invocation.input = input;
        let attestation = session().bound_to(invocation.id.clone());
        let mut gateway = DescriptorStream::new(UnixStream::connect(&self.socket).await.unwrap());
        gateway
            .write_frame(
                &RequestEnvelope::invoke(Some(attestation), invocation, vec![], 0, None),
                &[],
                server_limits().frame,
            )
            .await
            .unwrap();
        gateway
    }

    async fn stop(self) {
        self.shutdown.send(()).expect("stop broker");
        self.server
            .await
            .expect("server task")
            .expect("clean shutdown");
    }
}

struct ProbeJobs {
    leg: parking_lot::Mutex<Option<std::sync::Weak<BrokerLeg>>>,
    exit: parking_lot::Mutex<Option<ExitCode>>,
}

impl JobControl for ProbeJobs {
    fn start(&self, seed: JobSeed) -> Result<JobId, JobRefusal> {
        let leg = self
            .leg
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .expect("probe job uses the same live broker leg");
        let tree = seed.tree().clone();
        let outcome = Interpreter::new(Limits::default()).run_seed(seed, leg.as_ref(), &tree);
        *self.exit.lock() = Some(outcome.exit_code);
        Ok(JobId::new(1))
    }

    fn list(&self) -> Vec<JobSummary> {
        Vec::new()
    }

    fn wait(
        &self,
        ids: &[JobId],
        _keep_waiting: &dyn Fn() -> bool,
    ) -> Vec<Result<JobWait, JobRefusal>> {
        ids.iter()
            .map(|id| {
                assert_eq!(*id, JobId::new(1));
                Ok(JobWait::Exited(
                    self.exit.lock().expect("probe job already ran"),
                ))
            })
            .collect()
    }

    fn kill(&self, _id: JobId) -> Result<(), JobRefusal> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn real_broker_probe_child_and_probe_started_job_refuse_write_on_same_leg() {
    for script in ["probe", "probe & wait $!"] {
        let broker = SpawnBroker::start_for(4, script).await;
        let claim = Attestation::for_chat(
            subject(),
            agent("chat-agent"),
            ChatScopeClaim {
                transport: "scientist-slack".parse().unwrap(),
                kind: ChatTransportKind::Slack,
                conversation: Conversation {
                    kind: ConversationKind::Thread,
                    container: Some("t0123abc".to_owned()),
                    id: "c0123abc".to_owned(),
                    thread: Some("1712345678.000100".to_owned()),
                },
                trigger: Trigger::Probe,
            },
        );
        let client = BrokerClient::new(&broker.socket, current_uid(), server_limits().frame)
            .expect("real broker client");
        let control = Arc::new(ProbeJobs {
            leg: parking_lot::Mutex::new(None),
            exit: parking_lot::Mutex::new(None),
        });
        let leg = Arc::new(
            BrokerLeg::connect(client, Some(claim))
                .await
                .expect("broker-owned probe surface")
                .with_job_control(Arc::clone(&control) as Arc<dyn JobControl>),
        );
        *control.leg.lock() = Some(Arc::downgrade(&leg));
        assert!(
            !leg.is_granted("cli-probe.write"),
            "broker withheld write from probe"
        );
        let (stdout, _reader) = std::os::unix::net::UnixStream::pair().unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::task::spawn_blocking(move || {
                let tree = TreeContext::new(Limits::default(), CallBudget::new(64));
                leg.invoke(
                    CommandProposal::new("cli-probe.upper", json!({}), None),
                    Streams {
                        stdin: None,
                        stdout: stdout.into(),
                    },
                    &tree,
                )
            }),
        )
        .await
        .expect("gateway child terminates")
        .expect("blocking worker joins");
        assert!(
            matches!(result, CapabilityCallResult::Exited { status, .. } if status.get() == 127),
            "child's write is unavailable at the leg surface: {result:?}"
        );
        assert_eq!(
            control.exit.lock().map(ExitCode::get),
            (script.contains('&')).then_some(ExitCode::NOT_FOUND.get()),
            "a probe-started job ran on the same leg"
        );
        let audit = serde_json::to_value(broker.audit.records()).unwrap();
        let records = audit.as_array().expect("audit records");
        let invocations = records
            .iter()
            .filter_map(|row| row["invocation"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            invocations.len(),
            2,
            "parent decision and outcome audited: {audit}"
        );
        assert_eq!(
            invocations[0], invocations[1],
            "no child Invoke reached the real broker: {audit}"
        );
        broker.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn real_broker_refuses_probe_and_job_child_writes_during_spawn_upcalls() {
    for trigger in [Trigger::Probe, Trigger::Job] {
        let broker = SpawnBroker::start(4).await;
        let claim = Attestation::for_chat(
            subject(),
            agent("chat-agent"),
            ChatScopeClaim {
                transport: "scientist-slack".parse().unwrap(),
                kind: ChatTransportKind::Slack,
                conversation: Conversation {
                    kind: ConversationKind::Thread,
                    container: Some("t0123abc".to_owned()),
                    id: "c0123abc".to_owned(),
                    thread: Some("1712345678.000100".to_owned()),
                },
                trigger,
            },
        );
        let mut parent = DescriptorStream::new(UnixStream::connect(&broker.socket).await.unwrap());
        let parent_id = "parent-child-write";
        parent
            .write_frame(
                &RequestEnvelope::invoke(
                    Some(claim.clone().bound_to(parent_id.parse().unwrap())),
                    request(parent_id),
                    Vec::new(),
                    0,
                    None,
                ),
                &[],
                server_limits().frame,
            )
            .await
            .unwrap();
        let stdout = receive_upcall(&mut parent, parent_id).await;
        let client = BrokerClient::new(&broker.socket, current_uid(), server_limits().frame)
            .expect("authenticated child client");
        let mut child = request("nested-write");
        child.capability = "cli-probe.write".parse().unwrap();
        let result = client
            .invoke(
                Some(claim),
                child,
                Default::default(),
                async |_upcall| unreachable!(),
            )
            .await
            .expect("real broker answers child proposal");
        assert_eq!(result.result.outcome, InvocationOutcome::Denied);
        assert_eq!(
            result.result.error.as_deref(),
            Some(match trigger {
                Trigger::Probe => "probe-write",
                Trigger::Job => "policy-denied",
                Trigger::Message | Trigger::Wake => unreachable!(),
            })
        );
        drop(stdout);
        answer_upcall(&mut parent, 126, "").await;
        assert_eq!(
            terminal(&mut parent).await.outcome,
            InvocationOutcome::Failed
        );
        broker.stop().await;
    }
}

async fn read_response(
    gateway: &mut DescriptorStream,
) -> (BrokerResponse, Vec<std::os::fd::OwnedFd>) {
    let (envelope, descriptors) = tokio::time::timeout(
        Duration::from_secs(10),
        gateway.read_frame::<ResponseEnvelope>(server_limits().frame),
    )
    .await
    .expect("broker answers without hanging")
    .expect("well-formed frame");
    (envelope.response, descriptors)
}

async fn receive_upcall(gateway: &mut DescriptorStream, id: &str) -> UpcallStreams {
    let (response, descriptors) = read_response(gateway).await;
    let BrokerResponse::Upcall {
        parent,
        script,
        stdin,
        ..
    } = response
    else {
        panic!("expected an upcall, got {response:?}");
    };
    assert_eq!(parent.as_str(), id);
    assert_eq!(script, spawn_component::SCRIPT);
    assert_eq!(stdin, UpcallStdin::None);
    UpcallStreams::receive(stdin, descriptors).expect("upcall descriptors")
}

async fn answer_upcall(gateway: &mut DescriptorStream, status: u8, stderr: &str) {
    gateway
        .write_frame(
            &RequestEnvelope {
                api_version: ProtocolVersion::V1Alpha2,
                request: BrokerRequest::UpcallResult {
                    status,
                    stderr: stderr.to_owned(),
                },
            },
            &[],
            server_limits().frame,
        )
        .await
        .unwrap();
}

async fn terminal(gateway: &mut DescriptorStream) -> dekopon_capability::InvocationResult {
    let response = read_response(gateway).await.0;
    let BrokerResponse::Invocation { result, .. } = response else {
        panic!("expected the invocation result, got {response:?}");
    };
    result
}

async fn wait_for_peer_close(stdout: std::os::fd::OwnedFd) -> std::io::ErrorKind {
    tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let mut stdout = std::os::unix::net::UnixStream::from(stdout);
        for _ in 0..300 {
            if let Err(error) = stdout.write_all(b"x") {
                return error.kind();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the host kept the child's stdout open");
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn guest_wait_before_300k_stdout() {
    const CHILD_OUTPUT_BYTES: usize = 300 * 1024;
    let broker = SpawnBroker::start(4).await;
    let mut gateway = broker.invoke("invoke-wait-first", json!({})).await;
    let streams = receive_upcall(&mut gateway, "invoke-wait-first").await;
    let written = tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let mut stdout = std::os::unix::net::UnixStream::from(streams.stdout);
        stdout.write_all(&vec![b'x'; CHILD_OUTPUT_BYTES])
    });
    tokio::time::timeout(Duration::from_secs(10), written)
        .await
        .expect("the host drains a waiting guest's stdout")
        .unwrap()
        .expect("the whole child output is accepted");
    answer_upcall(&mut gateway, 0, "").await;
    let result = terminal(&mut gateway).await;
    assert_eq!(result.outcome, InvocationOutcome::Succeeded);
    broker.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_upcall_result_reaches_the_guest_byte_for_byte() {
    let broker = SpawnBroker::start(4).await;
    let mut gateway = broker.invoke("invoke-intact-result", json!({})).await;
    let streams = receive_upcall(&mut gateway, "invoke-intact-result").await;
    drop(streams);
    answer_upcall(&mut gateway, 42, "child stderr").await;
    let result = terminal(&mut gateway).await;
    assert_eq!(result.outcome, InvocationOutcome::Failed);
    assert_eq!(result.exit_status.map(std::num::NonZeroU8::get), Some(42));
    broker.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn gateway_hangup_mid_upcall() {
    let broker = SpawnBroker::start(4).await;
    let mut gateway = broker.invoke("invoke-gateway-hangup", json!({})).await;
    let streams = receive_upcall(&mut gateway, "invoke-gateway-hangup").await;
    drop(gateway);
    assert_eq!(
        wait_for_peer_close(streams.stdout).await,
        std::io::ErrorKind::BrokenPipe
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while broker.audit.records().len() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("terminal audit after the gateway hangs up");
    let encoded = serde_json::to_value(broker.audit.records()).unwrap();
    assert_eq!(encoded[1]["error"], "peer-disconnected");
    broker.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn capacity_exhausted_frame() {
    let broker = SpawnBroker::start(1).await;
    let mut parent = broker.invoke("invoke-holds-the-permit", json!({})).await;
    let streams = receive_upcall(&mut parent, "invoke-holds-the-permit").await;
    let mut child = DescriptorStream::new(UnixStream::connect(&broker.socket).await.unwrap());
    let refusal = read_response(&mut child).await.0;
    let BrokerResponse::Error { code, .. } = refusal else {
        panic!("expected a capacity refusal, got {refusal:?}");
    };
    assert_eq!(code, ERROR_CAPACITY_EXHAUSTED);
    let client = BrokerClient::new(&broker.socket, current_uid(), server_limits().frame)
        .expect("capacity client");
    assert!(matches!(
        client.session_surface(None).await,
        Err(ClientError::Remote { code, .. }) if code == ERROR_CAPACITY_EXHAUSTED
    ));
    drop(streams);
    answer_upcall(&mut parent, 0, "").await;
    assert_eq!(
        terminal(&mut parent).await.outcome,
        InvocationOutcome::Succeeded
    );
    broker.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_upcall_result_cancels_parent_and_releases_child() {
    use std::os::fd::AsFd as _;

    let broker = SpawnBroker::start(4).await;
    let mut gateway = broker.invoke("invoke-malformed-reply", json!({})).await;
    let streams = receive_upcall(&mut gateway, "invoke-malformed-reply").await;
    let (extra, _) = std::os::unix::net::UnixStream::pair().unwrap();
    gateway
        .write_frame(
            &RequestEnvelope {
                api_version: ProtocolVersion::V1Alpha2,
                request: BrokerRequest::UpcallResult {
                    status: 0,
                    stderr: String::new(),
                },
            },
            &[extra.as_fd()],
            server_limits().frame,
        )
        .await
        .unwrap();
    assert_eq!(
        wait_for_peer_close(streams.stdout).await,
        std::io::ErrorKind::BrokenPipe
    );
    assert_eq!(
        terminal(&mut gateway).await.outcome,
        InvocationOutcome::Failed
    );
    broker.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn child_panic_terminal() {
    let broker = SpawnBroker::start(4).await;
    let mut gateway = broker.invoke("invoke-child-panic", json!({})).await;
    let streams = receive_upcall(&mut gateway, "invoke-child-panic").await;
    {
        use std::io::Write as _;
        std::os::unix::net::UnixStream::from(streams.stdout)
            .write_all(b"partial")
            .unwrap();
    }
    answer_upcall(&mut gateway, 70, "child script panicked").await;
    let result = terminal(&mut gateway).await;
    assert_eq!(result.outcome, InvocationOutcome::Failed);
    assert_eq!(result.exit_status.map(std::num::NonZeroU8::get), Some(70));
    broker.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_parent_trap_mid_upcall_still_ends_the_conversation() {
    let broker = SpawnBroker::start(4).await;
    let mut gateway = broker
        .invoke("invoke-parent-trap", json!({"trap": true}))
        .await;
    let streams = receive_upcall(&mut gateway, "invoke-parent-trap").await;
    {
        use std::io::Write as _;
        (&std::os::unix::net::UnixStream::from(streams.stdout.try_clone().unwrap()))
            .write_all(b"x")
            .unwrap();
    }
    assert_eq!(
        wait_for_peer_close(streams.stdout).await,
        std::io::ErrorKind::BrokenPipe
    );
    answer_upcall(&mut gateway, 0, "").await;
    assert_eq!(
        terminal(&mut gateway).await.outcome,
        InvocationOutcome::Failed
    );
    broker.stop().await;
}
