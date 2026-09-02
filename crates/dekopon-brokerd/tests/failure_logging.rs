//! What an operator can read after a connection fails.
//!
//! Every failure below answers the client with a deliberately generic wire code, so the log line is
//! the only place the cause exists. The named risk this exists for is a full or failing audit
//! filesystem: before, that produced a stream of `broker_connection_failed category=broker` with no
//! io error, no bound, and no invocation to reconcile.
//!
//! This lives in its own test binary because `tracing` resolves per-callsite interest against the
//! global dispatcher, so a sibling test reaching these callsites with no subscriber installed can
//! disable them for the whole process.

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
use dekopon_capability::{EffectKind, ExecutionConstraints, Idempotency};
use dekopon_core::{
    Actor, AgentId, CapabilityId, InvocationId, PrincipalId, ProviderId, RiskLevel, TraceId,
};
use dekopon_test_support::{CaptureLayer, provider_fixture};
use serde_json::json;
use tokio::{
    io::AsyncWriteExt as _,
    net::{UnixListener, UnixStream},
    sync::oneshot,
};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

const POLICY: &str = r#"
@id("caller-echo")
permit(principal == Dekopon::Principal::"caller",
       action == Dekopon::Action::"echo.echo",
       resource == Dekopon::Provider::"echo")
when { context has agent && context.agent == "brokerd-test" }
unless { context has via };
"#;

/// Drains the capture once `marker` has arrived.
///
/// The events under test are emitted by a connection task the client never waits for, so a fixed
/// sleep would only decide how flaky this is on a loaded machine.
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

/// One audit slot: an allowed invocation spends it on its decision, so its terminal append is
/// already doomed when the provider runs.
async fn broker(audit_bound: usize) -> Arc<Broker<InMemoryAuditLog>> {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("echo-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("echo provider fixture loads");
    let world = PolicyWorld::new(
        ["caller".parse::<PrincipalId>().expect("valid principal")],
        [(
            "echo.echo".parse::<CapabilityId>().expect("capability"),
            "echo".parse::<ProviderId>().expect("provider"),
        )],
    )
    .expect("distinct fixtures build a world");
    let catalog = ConstraintCatalog::new([(
        "echo.echo".parse::<CapabilityId>().expect("capability"),
        ConstraintSet {
            route: CapabilityRoute::Generic,
            provider: "echo".parse::<ProviderId>().expect("provider"),
            effect: EffectKind::ReadOnly,
            risk: RiskLevel::Low,
            idempotency: Idempotency::Idempotent,
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

/// Writes one raw length-prefixed frame, bypassing the client that would refuse to build it, and
/// returns the wire code the server answers with.
///
/// `whole` replaces the prefix and body with one buffer written in a single call, for a frame
/// whose refusal must not race its own body.
async fn write_raw(
    socket: &Path,
    prefix: u32,
    body: &[u8],
    limits: FrameLimits,
    whole: Option<&[u8]>,
) -> String {
    let mut stream = UnixStream::connect(socket).await.expect("connect fixture");
    match whole {
        Some(frame) => stream.write_all(frame).await.expect("write whole frame"),
        None => {
            stream
                .write_all(&prefix.to_be_bytes())
                .await
                .expect("write frame prefix");
            stream.write_all(body).await.expect("write frame body");
        }
    }
    stream.flush().await.expect("flush fixture frame");
    let response = read_frame::<_, ResponseEnvelope>(&mut stream, limits)
        .await
        .expect("read the refusal");
    let BrokerResponse::Error { code, .. } = response.response else {
        panic!("an unreadable frame must not produce a result");
    };
    code
}

/// A timeout, an oversized frame, and unreadable JSON all answer `invalid-request`. The kind is
/// what separates "a client is misbehaving" from "the frame ceiling is too small for this input".
#[tokio::test(flavor = "multi_thread")]
async fn framing_and_audit_failures_name_their_cause() {
    let captured = CaptureLayer::workspace();
    tracing_subscriber::registry().with(captured.clone()).init();

    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create server fixture");
    let socket_path = directory.path().join("broker.sock");
    let listener = bind_fixture(&socket_path);
    let server =
        BrokerServer::new(broker(1).await, identities(uid), limits()).expect("server limits valid");
    let (shutdown_send, shutdown_receive) = oneshot::channel::<()>();
    let task = tokio::spawn(server.serve(listener, async move {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "this future's only job is to resolve; a signal and a dropped sender both \
                      mean stop, and serve treats them identically"
        )]
        let _ = shutdown_receive.await;
    }));

    let malformed = b"{ this is not protocol json";
    let code = write_raw(
        &socket_path,
        u32::try_from(malformed.len()).expect("fixture frame fits"),
        malformed,
        limits().frame,
        None,
    )
    .await;
    assert_eq!(code, ERROR_INVALID_REQUEST);
    let unreadable = take_after(&captured, "broker_request_frame_invalid").await;
    assert!(unreadable.contains("deserialize"), "{unreadable}");
    // The frame's own bytes are not diagnostics: an untrusted payload must not become a log field.
    assert!(
        !unreadable.contains("this is not protocol json"),
        "{unreadable}"
    );

    let oversized_code = write_raw(&socket_path, 128 * 1024, b"", limits().frame, None).await;
    assert_eq!(oversized_code, ERROR_INVALID_REQUEST);
    let oversized = take_after(&captured, "broker_request_frame_invalid").await;
    assert!(oversized.contains("frame-too-large"), "{oversized}");
    // The bound and the attempted size are the whole answer to "why did that call fail".
    assert!(oversized.contains("65536"), "{oversized}");

    // A piped value rides a `runCommand` frame under the same ceiling: a real frame carrying a
    // value twice the bound is refused from its length prefix before a byte of it is read. The
    // whole frame goes out in one write so the refusal cannot race the body.
    let oversized_run = serde_json::to_vec(&RequestEnvelope::run_command(
        None,
        "probe".to_owned(),
        vec!["upper".to_owned(), "-".to_owned()],
        Some("x".repeat(128 * 1024)),
    ))
    .expect("the oversized run frame serializes");
    let mut frame = u32::try_from(oversized_run.len())
        .expect("fixture frame fits")
        .to_be_bytes()
        .to_vec();
    frame.extend_from_slice(&oversized_run);
    let stdin_code = write_raw(&socket_path, 0, &[], limits().frame, Some(&frame)).await;
    assert_eq!(stdin_code, ERROR_INVALID_REQUEST);
    let oversized_stdin = take_after(&captured, "broker_request_frame_invalid").await;
    assert!(
        oversized_stdin.contains("frame-too-large"),
        "{oversized_stdin}"
    );
    assert!(oversized_stdin.contains("65536"), "{oversized_stdin}");

    // The consequential one: the decision landed, the provider ran, and nothing recorded the
    // outcome. The invocation identifier and the audit cause both have to survive to the log.
    let client = BrokerClient::new(&socket_path, uid, limits().frame).expect("client starts");
    let request = InvocationRequest {
        id: "invoke-unaudited"
            .parse::<InvocationId>()
            .expect("valid invocation"),
        capability: "echo.echo".parse::<CapabilityId>().expect("capability"),
        trace: "trace-brokerd".parse::<TraceId>().expect("valid trace"),
        trace_parent: None,
        secret_use: None,
        input: json!({"message": "hello through broker"}),
    };
    client
        .invoke(None, request)
        .await
        .expect_err("a terminal audit failure is not a successful invocation");
    // The connection's own verdict is observed the moment its task finishes, with the server still
    // accepting and no second client on the way. It used to wait inside the `JoinSet` for the next
    // accept or for shutdown, so on a quiet broker the one failure an operator must act on was
    // reported whenever the next connection happened to arrive.
    let unaudited = take_after(&captured, "broker_outcome_unaudited").await;
    assert!(
        unaudited.contains("broker_audit_append_failed"),
        "{unaudited}"
    );
    assert!(unaudited.contains("\"full\""), "{unaudited}");
    assert!(unaudited.contains("\"outcome\""), "{unaudited}");
    assert!(unaudited.contains("invoke-unaudited"), "{unaudited}");
    // The chain, not just the category: the bound the log used to omit entirely.
    assert!(
        unaudited.contains("audit log reached its 1-record bound"),
        "{unaudited}"
    );

    shutdown_send.send(()).expect("signal clean shutdown");
    #[allow(
        clippy::let_underscore_must_use,
        reason = "the expect above is the assertion that the task joined; serve's own Result is \
                  the shutdown it was just asked for, and every behavior under test was already \
                  asserted through the client"
    )]
    let _ = task.await.expect("server task exits");
}
