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
use dekopon_broker_protocol::{BrokerClient, FrameLimits};
use dekopon_brokerd::{BrokerServer, CONFIG_API_VERSION, ServerLimits, current_uid, run};
use dekopon_capability::{EffectKind, ExecutionConstraints, Idempotency, InvocationOutcome};
use dekopon_core::{
    Actor, AgentId, CapabilityId, InvocationId, PrincipalId, ProviderId, RiskLevel, TraceId,
};
use serde_json::json;
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
        input: json!({"message": "hello through broker"}),
    }
}

fn bind_fixture(path: &Path) -> UnixListener {
    let listener = UnixListener::bind(path).expect("bind server fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("secure server fixture");
    listener
}

async fn broker() -> (Arc<Broker<InMemoryAuditLog>>, Arc<InMemoryAuditLog>) {
    let registry =
        BrokerProviderRegistry::load([fixture("echo-provider.wasm")], BrokerHostLimits::default())
            .await
            .expect("load echo fixture");
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound"));
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
    let document = json!({
        "apiVersion": CONFIG_API_VERSION,
        "socketPath": &socket_path,
        "auditPath": &audit_path,
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
    let first = tokio::spawn(async move {
        run(first_config, async move {
            let _ = stopped.await;
        })
        .await
    });
    wait_for_socket(&socket_path).await;
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
    let second = tokio::spawn(async move {
        run(second_config, async move {
            let _ = stopped.await;
        })
        .await
    });
    wait_for_socket(&socket_path).await;
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
}

async fn wait_for_socket(path: &Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("broker fixture socket did not appear");
}
