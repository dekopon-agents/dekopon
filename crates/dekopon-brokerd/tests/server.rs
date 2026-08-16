use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use dekopon_broker::{
    AuthenticatedContext, Broker, BrokerLimits, InMemoryAuditLog, InvocationRequest, PolicyRule,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_broker_protocol::{BrokerClient, ClientError, FrameLimits};
use dekopon_brokerd::{
    AuditCheckpoint, BrokerServer, BrokerdError, CHECKPOINT_API_VERSION, CONFIG_API_VERSION,
    CheckpointError, ServerLimits, current_uid, run,
};
use dekopon_capability::{EffectKind, ExecutionConstraints, Idempotency, InvocationOutcome};
use dekopon_core::{
    Actor, AgentId, CapabilityId, InvocationId, PrincipalId, ProviderId, RiskLevel, TraceId,
};
use serde_json::{Value, json};
use tokio::{net::UnixListener, sync::oneshot};

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

fn rule() -> PolicyRule {
    PolicyRule {
        principal: "caller"
            .parse::<PrincipalId>()
            .expect("valid principal fixture"),
        actor: Actor::Agent {
            agent: "brokerd-test"
                .parse::<AgentId>()
                .expect("valid agent fixture"),
        },
        capability: "echo.echo"
            .parse::<CapabilityId>()
            .expect("valid capability fixture"),
        provider: "echo"
            .parse::<ProviderId>()
            .expect("valid provider fixture"),
        effect: EffectKind::ReadOnly,
        risk: RiskLevel::Low,
        idempotency: Idempotency::Idempotent,
        constraints: ExecutionConstraints::default(),
    }
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
            vec![rule()],
            Arc::clone(&audit),
            BrokerLimits::default(),
        )
        .expect("broker starts"),
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
async fn authenticated_unix_peer_can_inspect_and_invoke_exact_policy() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create server fixture");
    let socket_path = directory.path().join("broker.sock");
    let listener = bind_fixture(&socket_path);
    let (broker, audit) = broker().await;
    let mut identities = BTreeMap::new();
    identities.insert(uid, context("caller"));
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
    let document = json!({
        "apiVersion": CONFIG_API_VERSION,
        "socketPath": &socket_path,
        "auditPath": &audit_path,
        "checkpointPath": &checkpoint_path,
        "checkpointLockPath": &checkpoint_lock_path,
        "brokerPrincipal": "broker-test",
        "policyRevision": "policy-test",
        "providers": [fixture("echo-provider.wasm")],
        "identities": [{
            "uid": uid,
            "principal": "caller",
            "actor": {"type": "agent", "agent": "brokerd-test"}
        }],
        "rules": [serde_json::to_value(rule()).expect("rule serializes")]
    });
    fs::write(
        &config_path,
        serde_json::to_vec(&document).expect("config serializes"),
    )
    .expect("write service config");
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600))
        .expect("secure service config");

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
    identities.insert(uid, context("caller"));
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
