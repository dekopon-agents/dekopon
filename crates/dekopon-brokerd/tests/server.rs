use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use dekopon_broker::{
    AttestorGrant, AuthenticatedContext, Broker, BrokerBuildError, BrokerLimits, ConstraintCatalog,
    ConstraintSet, CredentialStore, IdentityDirectory, InMemoryAuditLog, InvocationRequest,
    PolicyEngine, PolicyWorld,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_broker_protocol::{
    AgentInventory, BrokerClient, BrokerResponse, ClientError, ERROR_INVALID_REQUEST,
    ERROR_UNAUTHENTICATED, FrameLimits, ModelUsageReport, ReportedAgent, ReportedAgentCapability,
    RequestEnvelope, ResponseEnvelope, SubjectAttestation, read_frame, write_frame,
};
use dekopon_brokerd::{
    AuditCheckpoint, BrokerServer, BrokerdError, CHECKPOINT_API_VERSION, CONFIG_API_VERSION,
    CheckpointError, MappedPeer, ServerLimits, current_uid, run, run_with_http,
};
use dekopon_capability::{EffectKind, ExecutionConstraints, Idempotency, InvocationOutcome};
use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, InvocationId, PrincipalId, ProviderId,
    RiskLevel, TraceId,
};
use dekopon_webui::ServiceStatus;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpStream, UnixListener, UnixStream},
    sync::oneshot,
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(format!("examples/providers/{name}"))
}

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
        provider: "echo"
            .parse::<ProviderId>()
            .expect("valid provider fixture"),
        effect: EffectKind::ReadOnly,
        risk: RiskLevel::Low,
        idempotency: Idempotency::Idempotent,
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
        trace: "trace-brokerd"
            .parse::<TraceId>()
            .expect("valid trace fixture"),
        trace_parent: None,
        input: json!({"message": "hello through broker"}),
    }
}

fn write_owner_only(path: &Path, contents: &[u8]) {
    fs::write(path, contents).expect("write fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("secure fixture");
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
    let registry =
        BrokerProviderRegistry::load([fixture("echo-provider.wasm")], BrokerHostLimits::default())
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
    let registry =
        BrokerProviderRegistry::load([fixture("echo-provider.wasm")], BrokerHostLimits::default())
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

#[tokio::test(flavor = "multi_thread")]
async fn authenticated_unix_peer_can_inspect_and_invoke_under_policy() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create server fixture");
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
    let task = tokio::spawn(server.serve(listener, async move {
        let _ = shutdown_receive.await;
    }));

    let client = BrokerClient::new(&socket_path, uid, limits.frame).expect("client starts");
    let capabilities = client.capabilities().await.expect("inspect capabilities");
    assert_eq!(capabilities.len(), 1);
    assert_eq!(capabilities[0].capability.id.as_str(), "echo.echo");
    let result = client
        .invoke(request("invoke-brokerd"))
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

#[tokio::test(flavor = "multi_thread")]
async fn mapped_attestor_can_publish_informational_ui_state_without_touching_audit() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create server fixture");
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
    let status = ServiceStatus::default();
    let limits = server_limits();
    let server = BrokerServer::new_with_status(broker, identities, limits, status.clone())
        .expect("server limits valid");
    let (shutdown_send, shutdown_receive) = oneshot::channel::<()>();
    let task = tokio::spawn(server.serve(listener, async move {
        let _ = shutdown_receive.await;
    }));
    let client = BrokerClient::new(&socket_path, uid, limits.frame).expect("client starts");
    client
        .publish_agent_inventory(AgentInventory {
            agents: vec![ReportedAgent {
                id: agent("chat-agent"),
                description: "Answers chat".to_owned(),
                enabled: true,
                model_class: Some("reasoning".to_owned()),
                providers: vec!["echo".parse().expect("valid provider")],
                capabilities: vec![ReportedAgentCapability {
                    id: "echo.echo".parse().expect("valid capability"),
                    provider: "echo".parse().expect("valid provider"),
                    permissions: Vec::new(),
                }],
            }],
            truncated: false,
        })
        .await
        .expect("attestor inventory is accepted");
    client
        .publish_model_usage(ModelUsageReport {
            model_calls: 2,
            input_tokens: 30,
            output_tokens: 7,
            input_unreported_calls: 1,
            ..ModelUsageReport::default()
        })
        .await
        .expect("attestor usage is accepted");

    let (inventory, reports) = status.agents();
    assert_eq!(reports, 1);
    assert_eq!(inventory.agents[0].id.as_str(), "chat-agent");
    assert_eq!(status.tokens().input_tokens, 30);
    assert_eq!(status.tokens().output_tokens, 7);
    assert!(
        audit.records().await.is_empty(),
        "informational UI reports are not authorization audit"
    );

    shutdown_send.send(()).expect("signal clean shutdown");
    task.await
        .expect("server task exits")
        .expect("server shuts down");
}

#[tokio::test(flavor = "multi_thread")]
async fn unmapped_peer_receives_no_capability_information() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create server fixture");
    let socket_path = directory.path().join("broker.sock");
    let listener = bind_fixture(&socket_path);
    let (broker, _audit) = broker().await;
    let limits = server_limits();
    let server = BrokerServer::new(broker, BTreeMap::new(), limits).expect("server starts");
    let (shutdown_send, shutdown_receive) = oneshot::channel::<()>();
    let task = tokio::spawn(server.serve(listener, async move {
        let _ = shutdown_receive.await;
    }));
    let client = BrokerClient::new(&socket_path, uid, limits.frame).expect("client starts");
    assert!(client.capabilities().await.is_err());
    shutdown_send.send(()).expect("signal clean shutdown");
    task.await
        .expect("server task exits")
        .expect("server shuts down");
}

#[tokio::test(flavor = "multi_thread")]
async fn full_service_restores_replay_state_from_verified_audit() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create service fixture");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("secure service directory");
    let config_path = directory.path().join("broker.json");
    let socket_path = directory.path().join("broker.sock");
    let audit_path = directory.path().join("audit.jsonl");
    let checkpoint_path = directory.path().join("checkpoint.json");
    let checkpoint_lock_path = directory.path().join("checkpoint.lock");
    let policies_path = directory.path().join("policies.cedar");
    let document = json!({
        "apiVersion": CONFIG_API_VERSION,
        "socketPath": &socket_path,
        "auditPath": &audit_path,
        "checkpointPath": &checkpoint_path,
        "checkpointLockPath": &checkpoint_lock_path,
        "brokerPrincipal": "broker-test",
        "policyRevision": "policy-test",
        "policiesPath": &policies_path,
        "providers": [fixture("echo-provider.wasm")],
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
    write_owner_only(&policies_path, DIRECT_POLICY.as_bytes());
    write_owner_only(
        &config_path,
        &serde_json::to_vec(&document).expect("config serializes"),
    );

    let (stop, stopped) = oneshot::channel::<()>();
    let first_config = config_path.clone();
    let mut first = tokio::spawn(async move {
        run(first_config, async move {
            let _ = stopped.await;
        })
        .await
    });
    wait_for_socket(&socket_path, &mut first).await;
    let client = BrokerClient::new(&socket_path, uid, FrameLimits::default())
        .expect("create service client");
    let result = client
        .invoke(request("invoke-durable-service"))
        .await
        .expect("first invocation completes");
    assert_eq!(result.outcome, InvocationOutcome::Succeeded);
    stop.send(()).expect("stop first service");
    let checkpoint = first
        .await
        .expect("first service task exits")
        .expect("first service stops cleanly");
    assert_eq!(checkpoint.records, 2);

    let (stop, stopped) = oneshot::channel::<()>();
    let second_config = config_path.clone();
    let mut second = tokio::spawn(async move {
        run(second_config, async move {
            let _ = stopped.await;
        })
        .await
    });
    wait_for_socket(&socket_path, &mut second).await;
    let client = BrokerClient::new(&socket_path, uid, FrameLimits::default())
        .expect("create restarted service client");
    let replay = client
        .invoke(request("invoke-durable-service"))
        .await
        .expect("replay receives an accounted denial");
    assert_eq!(replay.outcome, InvocationOutcome::Denied);
    assert_eq!(replay.error.as_deref(), Some("replayed-invocation"));
    stop.send(()).expect("stop second service");
    let checkpoint = second
        .await
        .expect("second service task exits")
        .expect("second service stops cleanly");
    assert_eq!(checkpoint.records, 3);
    let stored: Value =
        serde_json::from_slice(&fs::read(&checkpoint_path).expect("read durable checkpoint"))
            .expect("checkpoint JSON decodes");
    assert_eq!(stored.as_object().expect("checkpoint object").len(), 3);
    assert_eq!(stored["apiVersion"], CHECKPOINT_API_VERSION);
    assert_eq!(stored["records"], 3);
    assert!(stored["head"].as_str().is_some());

    let audit = fs::read_to_string(&audit_path).expect("read audit before truncation");
    let first = audit.lines().next().expect("audit has a first record");
    fs::write(&audit_path, format!("{first}\n")).expect("write valid-prefix truncation");
    let error = run(&config_path, async {})
        .await
        .expect_err("checkpoint must reject valid-prefix audit rollback");
    assert!(matches!(
        error,
        BrokerdError::Checkpoint(CheckpointError::AuditMismatch)
    ));
    assert!(!socket_path.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn full_service_serves_the_explicit_read_only_http_listener() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create web UI fixture");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("secure fixture directory");
    let config_path = directory.path().join("broker.json");
    let socket_path = directory.path().join("broker.sock");
    let policies_path = directory.path().join("policies.cedar");
    let document = json!({
        "apiVersion": CONFIG_API_VERSION,
        "socketPath": &socket_path,
        "auditPath": directory.path().join("audit.jsonl"),
        "checkpointPath": directory.path().join("checkpoint.json"),
        "checkpointLockPath": directory.path().join("checkpoint.lock"),
        "brokerPrincipal": "broker-test",
        "policyRevision": "policy-test",
        "policiesPath": &policies_path,
        "providers": [fixture("echo-provider.wasm")],
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
    write_owner_only(&policies_path, DIRECT_POLICY.as_bytes());
    write_owner_only(
        &config_path,
        &serde_json::to_vec(&document).expect("config serializes"),
    );
    let reservation =
        std::net::TcpListener::bind("127.0.0.1:0").expect("reserve an ephemeral HTTP port");
    let http_address = reservation.local_addr().expect("reserved address");
    drop(reservation);

    let (stop, stopped) = oneshot::channel::<()>();
    let started_config = config_path.clone();
    let mut service = tokio::spawn(async move {
        run_with_http(started_config, Some(http_address), async move {
            let _ = stopped.await;
        })
        .await
    });
    wait_for_socket(&socket_path, &mut service).await;
    wait_for_http(http_address, &mut service).await;

    let root = http_get(http_address, "/").await;
    assert!(
        root.starts_with("HTTP/1.1 308 Permanent Redirect"),
        "{root}"
    );
    assert!(
        root.to_ascii_lowercase().contains("location: /ui"),
        "{root}"
    );
    let ui = http_get(http_address, "/ui").await;
    assert!(ui.starts_with("HTTP/1.1 200 OK"), "{ui}");
    for expected in ["Dekopon service", "Providers", "Wasmtime", "echo"] {
        assert!(ui.contains(expected), "missing {expected:?} in {ui}");
    }
    let provider = http_get(http_address, "/ui/providers/echo").await;
    assert!(provider.starts_with("HTTP/1.1 200 OK"), "{provider}");
    for expected in [
        "pub capability",
        "echo.echo",
        "Complete manifest",
        "SHA-256",
    ] {
        assert!(
            provider.contains(expected),
            "missing {expected:?} in provider page"
        );
    }

    stop.send(()).expect("stop service");
    service
        .await
        .expect("service task exits")
        .expect("service stops cleanly");
}

async fn wait_for_http<T: std::fmt::Debug>(
    address: std::net::SocketAddr,
    task: &mut tokio::task::JoinHandle<T>,
) {
    for _ in 0..100 {
        assert!(!task.is_finished(), "service exited before HTTP bind");
        if TcpStream::connect(address).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("HTTP listener at {address} did not become ready");
}

async fn http_get(address: std::net::SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(address)
        .await
        .expect("connect to web UI");
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("write HTTP request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read HTTP response");
    String::from_utf8(response).expect("HTTP response is UTF-8")
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
    task: &mut tokio::task::JoinHandle<Result<AuditCheckpoint, BrokerdError>>,
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
    let directory = tempfile::tempdir().expect("create server fixture");
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
    let task = tokio::spawn(server.serve(listener, async move {
        let _ = shutdown_receive.await;
    }));

    let client = BrokerClient::new(&socket_path, uid, limits.frame).expect("client starts");
    let ran = client
        .invoke(request("invoke-outcome-unaudited"))
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
        .invoke(request("invoke-never-ran"))
        .await
        .expect_err("a full audit cannot authorize");
    let ClientError::Remote {
        code: unran_code, ..
    } = never_ran
    else {
        panic!("expected a remote broker failure, got {never_ran}");
    };
    assert_eq!(unran_code, "broker-unavailable");
    assert_ne!(
        unran_code, code,
        "a client must distinguish an effect that may have run from one that never began"
    );

    shutdown_send.send(()).expect("signal clean shutdown");
    let _ = task.await.expect("server task exits");
}

/// The whole attested path over a real socket: the peer names a canonical subject, the broker
/// maps it, and the invocation runs under the attested context. The peer never names a principal
/// at any point — that mapping is not something the wire can express.
#[tokio::test(flavor = "multi_thread")]
async fn invoke_for_over_the_socket_succeeds_for_an_attestor_peer() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create server fixture");
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
    let task = tokio::spawn(server.serve(listener, async move {
        let _ = shutdown_receive.await;
    }));

    let client = BrokerClient::new(&socket_path, uid, limits.frame).expect("client starts");
    let result = client
        .invoke_for(
            request("invoke-attested-socket"),
            subject(),
            agent("chat-agent"),
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
    assert_eq!(encoded[0]["event"]["principal"], "cpetersen");
    assert_eq!(encoded[0]["event"]["via"], "caller");
    assert_eq!(encoded[0]["event"]["attested_subject"], SLACK_SUBJECT);

    shutdown_send.send(()).expect("signal clean shutdown");
    task.await
        .expect("server task exits")
        .expect("server shuts down");
}

/// A peer with no attestor grant gets a completed invocation response carrying a denial, not a
/// transport failure. The difference is the audit record: a denial is a decision the broker made
/// and retained, and an error would leave the attempt with nothing accounting for it.
#[tokio::test(flavor = "multi_thread")]
async fn invoke_for_from_a_peer_without_a_grant_is_denied_not_erred() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create server fixture");
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
    let task = tokio::spawn(server.serve(listener, async move {
        let _ = shutdown_receive.await;
    }));

    let client = BrokerClient::new(&socket_path, uid, limits.frame).expect("client starts");
    let result = client
        .invoke_for(
            request("invoke-ungranted-socket"),
            subject(),
            agent("chat-agent"),
        )
        .await
        .expect("a refused attestation is still a completed invocation response");
    assert_eq!(result.outcome, InvocationOutcome::Denied);
    assert_eq!(result.error.as_deref(), Some("attestation-denied"));

    let records = audit.records().await;
    assert_eq!(records.len(), 1);
    let encoded: Value = serde_json::to_value(&records).expect("audit serializes");
    assert_eq!(
        encoded[0]["event"]["principal"], "caller",
        "an unauthorized claim is recorded against the peer that made it"
    );
    assert_eq!(encoded[0]["event"]["reason"], "attestation-denied");

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
    let directory = tempfile::tempdir().expect("create server fixture");
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
    let task = tokio::spawn(server.serve(listener, async move {
        let _ = shutdown_receive.await;
    }));

    let mut stream = UnixStream::connect(&socket_path)
        .await
        .expect("connect to the fixture socket");
    let envelope = RequestEnvelope::invoke_for(
        request("invoke-bound-identifier"),
        SubjectAttestation {
            subject: subject(),
            agent: agent("chat-agent"),
            invocation: "invoke-some-other-proposal"
                .parse::<InvocationId>()
                .expect("valid invocation fixture"),
        },
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
async fn capabilities_for_over_the_socket() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create server fixture");
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
    let granted_task = tokio::spawn(granted.serve(granted_listener, async move {
        let _ = granted_stopped.await;
    }));

    let client = BrokerClient::new(&granted_path, uid, limits.frame).expect("client starts");
    let capabilities = client
        .capabilities_for(subject(), agent("chat-agent"))
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
    let ungranted_task = tokio::spawn(ungranted.serve(ungranted_listener, async move {
        let _ = ungranted_stopped.await;
    }));

    let client = BrokerClient::new(&ungranted_path, uid, limits.frame).expect("client starts");
    let refused = client
        .capabilities_for(subject(), agent("chat-agent"))
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
    let directory = tempfile::tempdir().expect("create service fixture");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("secure service directory");
    let config_path = directory.path().join("broker.json");
    let policies_path = directory.path().join("policies.cedar");
    let document = json!({
        "apiVersion": CONFIG_API_VERSION,
        "socketPath": directory.path().join("broker.sock"),
        "auditPath": directory.path().join("audit.jsonl"),
        "checkpointPath": directory.path().join("checkpoint.json"),
        "checkpointLockPath": directory.path().join("checkpoint.lock"),
        "brokerPrincipal": "broker-test",
        "policyRevision": "policy-test",
        "policiesPath": &policies_path,
        "strict": true,
        "providers": [fixture("echo-provider.wasm")],
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
    let directory = tempfile::tempdir().expect("create service fixture");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("secure service directory");
    let config_path = directory.path().join("broker.json");
    let policies_path = directory.path().join("policies.cedar");
    let document = json!({
        "apiVersion": CONFIG_API_VERSION,
        "socketPath": directory.path().join("broker.sock"),
        "auditPath": directory.path().join("audit.jsonl"),
        "checkpointPath": directory.path().join("checkpoint.json"),
        "checkpointLockPath": directory.path().join("checkpoint.lock"),
        "brokerPrincipal": "broker-test",
        "policyRevision": "policy-test",
        "policiesPath": &policies_path,
        "providers": [fixture("echo-provider.wasm")],
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
