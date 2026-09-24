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
    AuthenticatedContext, Broker, BrokerLimits, CapabilityRoute, ConstraintCatalog, ConstraintSet,
    CredentialStore, IdentityDirectory, InMemoryAuditLog, InvocationRequest, PolicyEngine,
    PolicyWorld,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_broker_protocol::{
    BrokerClient, BrokerResponse, ERROR_INVALID_REQUEST, FrameLimits, RequestEnvelope,
    ResponseEnvelope, read_frame,
};
use dekopon_brokerd::{BrokerServer, MappedPeer, ServerLimits, current_uid};
use dekopon_capability::{EffectKind, ExecutionConstraints};
use dekopon_core::{
    Actor, AgentId, CapabilityId, InvocationId, PrincipalId, ProviderId, RiskLevel,
};
use dekopon_test_support::{CaptureLayer, provider_fixture, shutdown_on};
use serde_json::json;
use tokio::{
    io::AsyncWriteExt as _,
    net::{UnixListener, UnixStream},
    sync::oneshot,
};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

const TRACE_PARENT: &str = "00-0000000000000000000000000000f1c7-00000000000000f1-00";

const POLICY: &str = r#"
@id("caller-upper")
permit(principal == Dekopon::Principal::"caller",
       action == Dekopon::Action::"cli-probe.upper",
       resource == Dekopon::Provider::"cli-probe")
when { context has agent && context.agent == "brokerd-test" }
unless { context has via };
"#;

async fn take_after(captured: &CaptureLayer, marker: &str) -> String {
    for _ in 0..200 {
        if captured.saw(marker) {
            return captured.take_events();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no {marker} event arrived: {}", captured.take_events());
}

fn context() -> AuthenticatedContext {
    AuthenticatedContext::new(
        "caller".parse::<PrincipalId>().expect("valid principal"),
        Actor::Agent {
            agent: "brokerd-test".parse::<AgentId>().expect("valid agent"),
        },
    )
    .expect("trusted context binds")
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

fn limits() -> ServerLimits {
    ServerLimits {
        frame: FrameLimits {
            max_frame_bytes: 64 * 1024,
            io_timeout: Duration::from_secs(2),
        },
        max_connections: 4,
        shutdown_grace: Duration::from_secs(2),
    }
}

async fn broker(audit_bound: usize) -> Arc<Broker<InMemoryAuditLog>> {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("cli-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("cli-probe provider fixture loads");
    let world = PolicyWorld::new(
        ["caller".parse::<PrincipalId>().expect("valid principal")],
        [(
            "cli-probe.upper"
                .parse::<CapabilityId>()
                .expect("capability"),
            "cli-probe".parse::<ProviderId>().expect("provider"),
        )],
    )
    .expect("distinct fixtures build a world");
    let catalog = ConstraintCatalog::new([(
        "cli-probe.upper"
            .parse::<CapabilityId>()
            .expect("capability"),
        ConstraintSet {
            route: CapabilityRoute::Generic,
            provider: "cli-probe".parse::<ProviderId>().expect("provider"),
            effect: EffectKind::ReadOnly,
            risk: RiskLevel::Low,
            credential: None,
            credential_by_agent: BTreeMap::new(),
            constraints: ExecutionConstraints::default(),
        },
    )])
    .expect("one capability builds a catalog");
    Arc::new(
        Broker::new(
            registry,
            "broker-test".parse::<PrincipalId>().expect("principal"),
            "policy-test".to_owned(),
            PolicyEngine::new(POLICY, &world).expect("fixture policy validates"),
            catalog,
            CredentialStore::empty(),
            IdentityDirectory::empty(),
            Arc::new(InMemoryAuditLog::new(audit_bound).expect("valid audit bound")),
            BrokerLimits::default(),
        )
        .expect("broker starts"),
    )
}

fn identities(uid: u32) -> BTreeMap<u32, MappedPeer> {
    let mut identities = BTreeMap::new();
    identities.insert(
        uid,
        MappedPeer {
            context: context(),
            attestor: None,
        },
    );
    identities
}

async fn write_raw(socket: &Path, prefix: u32, body: &[u8], limits: FrameLimits) -> String {
    let mut stream = UnixStream::connect(socket).await.expect("connect fixture");
    stream
        .write_all(&prefix.to_be_bytes())
        .await
        .expect("write frame prefix");
    stream.write_all(body).await.expect("write frame body");
    stream.flush().await.expect("flush fixture frame");
    let response = read_frame::<_, ResponseEnvelope>(&mut stream, limits)
        .await
        .expect("read the refusal");
    let BrokerResponse::Error { code, .. } = response.response else {
        panic!("an unreadable frame must not produce a result");
    };
    code
}

#[tokio::test(flavor = "multi_thread")]
async fn framing_audit_and_unmapped_peer_failures_name_their_cause() {
    let captured = CaptureLayer::workspace();
    tracing_subscriber::registry().with(captured.clone()).init();

    let uid = current_uid();
    let directory = private_directory();
    let socket_path = directory.path().join("broker.sock");
    let listener = bind_fixture(&socket_path);
    let shared = broker(1).await;
    let server = BrokerServer::new(Arc::clone(&shared), identities(uid), limits())
        .expect("server limits valid");
    let (shutdown_send, shutdown_receive) = oneshot::channel::<()>();
    let task = tokio::spawn(server.serve(listener, shutdown_on(shutdown_receive)));

    let malformed = b"{ this is not protocol json";
    let code = write_raw(
        &socket_path,
        u32::try_from(malformed.len()).expect("fixture frame fits"),
        malformed,
        limits().frame,
    )
    .await;
    assert_eq!(code, ERROR_INVALID_REQUEST);
    let unreadable = take_after(&captured, "broker_request_frame_invalid").await;
    assert!(unreadable.contains("deserialize"), "{unreadable}");
    assert!(
        !unreadable.contains("this is not protocol json"),
        "{unreadable}"
    );

    let oversized_code = write_raw(&socket_path, 128 * 1024, b"", limits().frame).await;
    assert_eq!(oversized_code, ERROR_INVALID_REQUEST);
    let oversized = take_after(&captured, "broker_request_frame_invalid").await;
    assert!(oversized.contains("frame-too-large"), "{oversized}");
    assert!(oversized.contains("65536"), "{oversized}");

    let oversized_run = serde_json::to_vec(&RequestEnvelope::run_command(
        None,
        "probe".to_owned(),
        vec!["upper".to_owned(), "-".to_owned()],
        Some("x".repeat(128 * 1024)),
        TRACE_PARENT.parse().expect("valid traceparent fixture"),
    ))
    .expect("the oversized run frame serializes");
    assert!(oversized_run.len() > limits().frame.max_frame_bytes);
    let stdin_code = write_raw(
        &socket_path,
        u32::try_from(oversized_run.len()).expect("fixture frame fits"),
        b"",
        limits().frame,
    )
    .await;
    assert_eq!(stdin_code, ERROR_INVALID_REQUEST);
    let oversized_stdin = take_after(&captured, "broker_request_frame_invalid").await;
    assert!(
        oversized_stdin.contains("frame-too-large"),
        "{oversized_stdin}"
    );
    assert!(oversized_stdin.contains("65536"), "{oversized_stdin}");

    let client = BrokerClient::new(&socket_path, uid, limits().frame).expect("client starts");
    let request = InvocationRequest {
        id: "invoke-unaudited"
            .parse::<InvocationId>()
            .expect("valid invocation"),
        capability: "cli-probe.upper"
            .parse::<CapabilityId>()
            .expect("capability"),
        trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
        secret_use: None,
        input: json!({"text": "hello through broker"}),
    };
    client
        .invoke(None, request, Default::default())
        .await
        .expect_err("a terminal audit failure is not a successful invocation");
    let unaudited = take_after(&captured, "broker_outcome_unaudited").await;
    assert!(
        unaudited.contains("broker_audit_append_failed"),
        "{unaudited}"
    );
    assert!(unaudited.contains("\"full\""), "{unaudited}");
    assert!(unaudited.contains("\"outcome\""), "{unaudited}");
    assert!(unaudited.contains("invoke-unaudited"), "{unaudited}");
    assert!(
        unaudited.contains("audit log reached its 1-record bound"),
        "{unaudited}"
    );

    let unmapped_path = directory.path().join("unmapped.sock");
    let unmapped_listener = bind_fixture(&unmapped_path);
    let unmapped_server =
        BrokerServer::new(shared, BTreeMap::new(), limits()).expect("server limits valid");
    let (stop_unmapped, unmapped_stopped) = oneshot::channel::<()>();
    let unmapped_task =
        tokio::spawn(unmapped_server.serve(unmapped_listener, shutdown_on(unmapped_stopped)));
    let _unmapped_peer = UnixStream::connect(&unmapped_path)
        .await
        .expect("connect as an unmapped peer");
    let unmapped = take_after(&captured, "broker_peer_unmapped").await;
    assert!(unmapped.contains(&format!("peer.uid={uid}")), "{unmapped}");
    stop_unmapped.send(()).expect("signal clean shutdown");
    unmapped_task
        .await
        .expect("unmapped server task exits")
        .expect("unmapped server shuts down");

    shutdown_send.send(()).expect("signal clean shutdown");
    #[allow(
        clippy::let_underscore_must_use,
        reason = "the expect above is the assertion that the task joined; serve's own Result is \
                  the shutdown it was just asked for, and every behavior under test was already \
                  asserted through the client"
    )]
    let _ = task.await.expect("server task exits");
}
