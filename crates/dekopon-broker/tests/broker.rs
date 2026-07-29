use std::{
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

use dekopon_broker::{
    AuditEvent, AuthenticatedContext, Broker, BrokerBuildError, BrokerLimits, InMemoryAuditLog,
    InvocationRequest, PolicyRule, verify_audit_chain,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_capability::{EffectKind, ExecutionConstraints, HttpConstraints, Idempotency};
use dekopon_core::{
    Actor, AgentId, CapabilityId, InvocationId, PrincipalId, ProviderId, RiskLevel, TraceId,
};
use serde_json::json;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(format!("examples/providers/{name}"))
}

fn context(principal: &str) -> AuthenticatedContext {
    AuthenticatedContext::new(
        principal
            .parse::<PrincipalId>()
            .expect("valid principal fixture"),
        Actor::Agent {
            agent: "provider-test"
                .parse::<AgentId>()
                .expect("valid agent fixture"),
        },
    )
    .expect("trusted agent context is valid")
}

fn request(id: &str, capability: &str, input: serde_json::Value) -> InvocationRequest {
    InvocationRequest {
        id: id
            .parse::<InvocationId>()
            .expect("valid invocation fixture"),
        capability: capability
            .parse::<CapabilityId>()
            .expect("valid capability fixture"),
        trace: "trace-test"
            .parse::<TraceId>()
            .expect("valid trace fixture"),
        input,
    }
}

fn rule(
    principal: &str,
    capability: &str,
    provider: &str,
    constraints: ExecutionConstraints,
) -> PolicyRule {
    PolicyRule {
        principal: principal
            .parse::<PrincipalId>()
            .expect("valid principal fixture"),
        actor: Actor::Agent {
            agent: "provider-test"
                .parse::<AgentId>()
                .expect("valid agent fixture"),
        },
        capability: capability
            .parse::<CapabilityId>()
            .expect("valid capability fixture"),
        provider: provider
            .parse::<ProviderId>()
            .expect("valid provider fixture"),
        effect: EffectKind::ReadOnly,
        risk: RiskLevel::Low,
        idempotency: Idempotency::Idempotent,
        constraints,
    }
}

async fn echo_registry(limits: BrokerHostLimits) -> BrokerProviderRegistry {
    BrokerProviderRegistry::load([fixture("echo-provider.wasm")], limits)
        .await
        .expect("echo provider fixture loads")
}

fn mock_http(response: &[u8]) -> (String, mpsc::Receiver<Vec<u8>>, thread::JoinHandle<()>) {
    let response = response.to_vec();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback fixture");
    let address = listener.local_addr().expect("fixture address");
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept fixture request");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set fixture timeout");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let body_length = String::from_utf8_lossy(&request[..header_end + 4])
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + body_length {
                    break;
                }
            }
            let read = stream.read(&mut buffer).expect("read fixture request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        sender.send(request).expect("record fixture request");
        stream.write_all(&response).expect("write fixture response");
        stream.flush().expect("flush fixture response");
    });
    (format!("127.0.0.1:{}", address.port()), receiver, handle)
}

#[tokio::test(flavor = "multi_thread")]
async fn exact_policy_authorizes_once_and_audits_no_payloads() {
    let registry = echo_registry(BrokerHostLimits::default()).await;
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound"));
    let broker = Broker::new(
        registry,
        "broker-test"
            .parse::<PrincipalId>()
            .expect("valid broker principal"),
        "policy-test".to_owned(),
        vec![rule(
            "caller",
            "echo.echo",
            "echo",
            ExecutionConstraints::default(),
        )],
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("exact policy matches loaded provider metadata");

    let result = broker
        .invoke(
            &context("caller"),
            request(
                "invoke-once",
                "echo.echo",
                json!({"message": "top-secret-payload"}),
            ),
        )
        .await
        .expect("authorized invocation is accounted");
    assert_eq!(
        result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    assert_eq!(result.decision.decision_id, "allow-invoke-once");
    assert_eq!(result.decision.policy_revision, "policy-test");
    assert_eq!(
        result.output,
        Some(json!({"message": "top-secret-payload"}))
    );
    assert_eq!(result.evidence.len(), 2);

    let replay = broker
        .invoke(
            &context("caller"),
            request(
                "invoke-once",
                "echo.echo",
                json!({"message": "top-secret-payload"}),
            ),
        )
        .await
        .expect("replay denial is audited");
    assert_eq!(
        replay.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(replay.error.as_deref(), Some("replayed-invocation"));

    let records = audit.records().await;
    assert_eq!(records.len(), 3);
    verify_audit_chain(&records).expect("audit chain verifies");
    assert!(matches!(
        records[0].event,
        AuditEvent::Decision { allowed: true, .. }
    ));
    assert!(matches!(records[1].event, AuditEvent::Execution { .. }));
    let serialized = serde_json::to_string(&records).expect("audit serializes");
    assert!(!serialized.contains("top-secret-payload"));
}

#[tokio::test(flavor = "multi_thread")]
async fn unmatched_identity_is_denied_before_provider_execution() {
    let registry = echo_registry(BrokerHostLimits::default()).await;
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let broker = Broker::new(
        registry,
        "broker-test"
            .parse::<PrincipalId>()
            .expect("valid broker principal"),
        "policy-test".to_owned(),
        vec![rule(
            "allowed-caller",
            "echo.echo",
            "echo",
            ExecutionConstraints::default(),
        )],
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("policy is coherent");

    let result = broker
        .invoke(
            &context("other-caller"),
            request("invoke-denied", "echo.echo", json!({"message": "secret"})),
        )
        .await
        .expect("policy denial is audited");
    assert_eq!(
        result.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(result.decision.decision_id, "deny-invoke-denied");
    assert_eq!(result.error.as_deref(), Some("policy-denied"));
    assert!(result.output.is_none());
    let records = audit.records().await;
    assert_eq!(records.len(), 1);
    assert!(matches!(
        records[0].event,
        AuditEvent::Decision { allowed: false, .. }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_metadata_and_host_ceilings_are_checked_at_startup() {
    let registry = echo_registry(BrokerHostLimits::default()).await;
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let error = Broker::new(
        registry,
        "broker-test"
            .parse::<PrincipalId>()
            .expect("valid broker principal"),
        "policy-test".to_owned(),
        vec![rule(
            "caller",
            "echo.echo",
            "different-provider",
            ExecutionConstraints::default(),
        )],
        audit,
        BrokerLimits::default(),
    )
    .expect_err("trusted provider mismatch must fail broker construction");
    assert!(matches!(error, BrokerBuildError::ProviderMismatch { .. }));

    let registry = echo_registry(BrokerHostLimits {
        max_timeout: Duration::from_millis(100),
        ..BrokerHostLimits::default()
    })
    .await;
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let error = Broker::new(
        registry,
        "broker-test"
            .parse::<PrincipalId>()
            .expect("valid broker principal"),
        "policy-test".to_owned(),
        vec![rule(
            "caller",
            "echo.echo",
            "echo",
            ExecutionConstraints {
                timeout_ms: 101,
                ..ExecutionConstraints::default()
            },
        )],
        audit,
        BrokerLimits::default(),
    )
    .expect_err("policy cannot exceed independent host timeout");
    assert!(matches!(error, BrokerBuildError::HostConstraint { .. }));
}

#[tokio::test(flavor = "multi_thread")]
async fn http_audit_contains_only_sanitized_call_metadata() {
    let registry = BrokerProviderRegistry::load(
        [fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider fixture loads");
    let (authority, received, server) = mock_http(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
    );
    let constraints = ExecutionConstraints {
        timeout_ms: 5_000,
        max_output_bytes: 1024 * 1024,
        http: Some(HttpConstraints {
            allowed_hosts: vec![authority.clone()],
            allowed_methods: vec!["POST".to_owned()],
            max_requests: 1,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 64 * 1024,
            allow_plaintext_loopback: true,
        }),
    };
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let broker = Broker::new(
        registry,
        "broker-test"
            .parse::<PrincipalId>()
            .expect("valid broker principal"),
        "policy-test".to_owned(),
        vec![rule(
            "caller",
            "http-probe.fetch",
            "http-probe",
            constraints,
        )],
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("HTTP rule matches trusted metadata and host ceilings");

    let result = broker
        .invoke(
            &context("caller"),
            request(
                "invoke-http",
                "http-probe.fetch",
                json!({
                    "uri": format!("http://{authority}/private-path?token=query-secret"),
                    "method": "POST",
                    "headers": [{"name": "x-private-input", "value": "header-secret"}],
                    "body": "body-secret"
                }),
            ),
        )
        .await
        .expect("authorized HTTP request succeeds");
    assert_eq!(
        result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    assert_eq!(result.evidence.len(), 3);
    let wire = received.recv().expect("fixture request recorded");
    assert!(wire.ends_with(b"\r\n\r\nbody-secret"));
    server.join().expect("fixture server exits");

    let records = audit.records().await;
    assert_eq!(records.len(), 2);
    verify_audit_chain(&records).expect("audit chain verifies");
    let serialized = serde_json::to_string(&records).expect("audit serializes");
    assert!(serialized.contains(&authority));
    assert!(serialized.contains("POST"));
    for secret in [
        "private-path",
        "query-secret",
        "x-private-input",
        "header-secret",
        "body-secret",
    ] {
        assert!(!serialized.contains(secret), "audit leaked {secret}");
    }
}
