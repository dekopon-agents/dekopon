#![allow(clippy::unwrap_used, clippy::disallowed_methods)]

use std::sync::Arc;

use dekopon_broker::{
    AssetInvocationResult, Attestation, AttestorGrant, AuthenticatedContext, Broker, BrokerError,
    BrokerLimits, CapabilityRoute, ConstraintCatalog, ConstraintSet, CredentialStore,
    IdentityDirectory, InvocationRequest, PolicyEngine, PolicyWorld, TraceOnlyAuditLog,
};
use dekopon_broker_host::{BoundCredential, BrokerHostLimits, BrokerProviderRegistry};
use dekopon_capability::{EffectKind, ExecutionConstraints, HttpConstraints, InvocationOutcome};
use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, InvocationId, PrincipalId, ProviderId, Redacted,
    RiskLevel,
};
use dekopon_test_support::{CaptureLayer, LoopbackServer, Record, provider_fixture};
use sha2::{Digest, Sha256};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

const SECRET: &str = "audit-log-record-must-never-see-this";

const SUBJECT: &str = "slack.t0123abc.u9xyz";

const POLICY: &str = r#"
@id("http-fetch")
permit(principal == Dekopon::Principal::"caller",
       action == Dekopon::Action::"http-probe.fetch",
       resource == Dekopon::Provider::"http-probe")
when { context.agent == "provider-test" };

@id("prompt")
permit(principal == Dekopon::Principal::"caller",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"provider-test");
"#;

fn principal(name: &str) -> PrincipalId {
    name.parse().expect("valid principal fixture")
}

async fn invoke_as_caller(
    broker: &Broker<TraceOnlyAuditLog>,
    request: InvocationRequest,
) -> Result<AssetInvocationResult, BrokerError> {
    let gateway = AuthenticatedContext::new(
        principal("gateway"),
        Actor::Service {
            principal: principal("gateway"),
        },
    )
    .expect("gateway context binds");
    let attestation = Attestation::for_subject(
        SUBJECT
            .parse::<ExternalSubject>()
            .expect("canonical subject"),
        "provider-test".parse::<AgentId>().expect("valid agent"),
    )
    .bound_to(request.id.clone());
    let (host, mut stdout) = std::os::unix::net::UnixStream::pair().expect("test stdout pipe");
    let capture = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut bytes).expect("capture stdout");
        bytes
    });
    let result = broker
        .invoke(
            &gateway,
            Some(&AttestorGrant { namespaces: None }),
            Some(&attestation),
            request,
            dekopon_broker_host::asset::AssetInputs {
                streams: Some(dekopon_broker_host::Streams {
                    stdin: None,
                    stdout: host.into(),
                }),
                ..Default::default()
            },
        )
        .await;
    let _stdout = capture.join().expect("stdout capture thread");
    result
}

fn request(id: &str, capability: &str, input: serde_json::Value) -> InvocationRequest {
    InvocationRequest {
        id: id
            .parse::<InvocationId>()
            .expect("valid invocation fixture"),
        capability: capability
            .parse::<CapabilityId>()
            .expect("valid capability fixture"),
        trace_parent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
            .parse()
            .expect("valid traceparent fixture"),
        input,
        secret_use: None,
    }
}

fn loopback_constraints(authority: &str) -> ExecutionConstraints {
    ExecutionConstraints {
        asset: None,
        timeout_ms: 5_000,
        http: Some(HttpConstraints {
            allowed_hosts: vec![authority.to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            max_requests: 1,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 64 * 1024,
            allow_plaintext_loopback: true,
            propagate_trace: false,
            request_templates: Vec::new(),
        }),
        storage: None,
        secret_use: None,
    }
}

fn only(captured: &CaptureLayer, fields: &[(&str, &str)]) -> Record {
    let matching = captured.events_where("dekopon_broker::audit", fields);
    let [record] = <[Record; 1]>::try_from(matching).unwrap_or_else(|matching| {
        panic!(
            "{} audit records matched {fields:?}: {matching:?}",
            matching.len()
        )
    });
    record
}

#[tokio::test(flavor = "multi_thread")]
async fn each_decision_emits_one_audit_record_inside_its_own_span() {
    let captured = CaptureLayer::workspace();
    tracing_subscriber::registry().with(captured.clone()).init();

    let fixture = provider_fixture("http-probe-provider.wasm");
    let expected_digest = format!(
        "{:x}",
        Sha256::digest(std::fs::read(&fixture).expect("fixture bytes"))
    );
    let registry = BrokerProviderRegistry::load([fixture], BrokerHostLimits::default())
        .await
        .expect("HTTP provider fixture loads");
    let artifact_digest = registry
        .loaded_provider_metadata()
        .next()
        .expect("loaded fixture")
        .artifact_sha256;
    let server = LoopbackServer::once(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
    );
    let authority = server.authority().to_owned();
    let world = PolicyWorld::new(
        [principal("caller")],
        [(
            "http-probe.fetch"
                .parse::<CapabilityId>()
                .expect("capability"),
            "http-probe".parse::<ProviderId>().expect("provider"),
        )],
    )
    .expect("distinct fixtures build a world");
    let broker = Broker::new(
        registry,
        principal("broker-test"),
        "policy-test".to_owned(),
        PolicyEngine::new(POLICY, &world).expect("fixture policy validates"),
        ConstraintCatalog::new([(
            "http-probe.fetch"
                .parse::<CapabilityId>()
                .expect("capability"),
            ConstraintSet {
                default_secret_use: None,
                route: CapabilityRoute::Generic,
                provider: "http-probe".parse::<ProviderId>().expect("provider"),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                credential: Some("fetch-token".to_owned()),
                constraints: loopback_constraints(&authority),
            },
        )])
        .expect("one capability builds a catalog"),
        CredentialStore::new([(
            "fetch-token".to_owned(),
            BoundCredential::bearer(
                "Bearer",
                Redacted::new(SECRET.to_owned()),
                vec![authority.clone()],
            )
            .expect("valid credential fixture"),
        )])
        .expect("credential store builds"),
        IdentityDirectory::new([(
            SUBJECT
                .parse::<ExternalSubject>()
                .expect("canonical subject"),
            principal("caller"),
        )])
        .expect("one mapping builds a directory"),
        Arc::new(TraceOnlyAuditLog),
        BrokerLimits::default(),
    )
    .expect("the credentialed constraint set matches store and destinations");

    let allowed = invoke_as_caller(
        &broker,
        request(
            "invoke-audited",
            "http-probe.fetch",
            serde_json::json!({ "uri": format!("http://{authority}/pulls/7"), "method": "GET" }),
        ),
    )
    .await
    .expect("authorized credentialed request succeeds");
    assert_eq!(allowed.result.outcome, InvocationOutcome::Succeeded);
    server.join();

    let denied = invoke_as_caller(
        &broker,
        request("invoke-refused", "http-probe.absent", serde_json::json!({})),
    )
    .await
    .expect("an unconstrained capability is still an accounted decision");
    assert_eq!(denied.result.outcome, InvocationOutcome::Denied);

    let records = captured.events_where("dekopon_broker::audit", &[]);
    assert_eq!(
        records.len(),
        4,
        "invocation and HTTP decisions, execution and refusal: {records:?}"
    );

    let decision = only(
        &captured,
        &[
            ("audit.event", "broker.decision"),
            ("invocation.id", "invoke-audited"),
            ("decision.point", "invocation"),
        ],
    );
    assert_eq!(decision.parent(), Some("broker.execute"), "{decision:?}");
    for (name, value) in [
        ("capability.id", "http-probe.fetch"),
        ("decision.allowed", "true"),
        ("principal", "caller"),
        ("actor.kind", "agent"),
        ("actor.id", "provider-test"),
        ("provider", "http-probe"),
        ("policy.ids", "http-fetch"),
    ] {
        assert_eq!(decision.field(name), Some(value), "{name}: {decision:?}");
    }
    assert!(decision.field("policy.digest").is_some(), "{decision:?}");
    assert_eq!(decision.field("policy.revision"), None);
    let request_decision = only(
        &captured,
        &[
            ("audit.event", "broker.decision"),
            ("invocation.id", "invoke-audited"),
            ("decision.point", "http.request"),
        ],
    );
    assert_eq!(request_decision.parent(), Some("http.request"));
    assert_eq!(request_decision.field("decision.allowed"), Some("true"));

    let execution = only(&captured, &[("audit.event", "broker.execution")]);
    assert_eq!(execution.parent(), Some("broker.execute"), "{execution:?}");
    assert_eq!(execution.field("output.digest"), None, "{execution:?}");
    assert_eq!(artifact_digest, expected_digest);
    assert_eq!(expected_digest.len(), 64);
    assert_eq!(
        execution.field("provider.artifact_sha256"),
        Some(expected_digest.as_str())
    );
    assert_eq!(execution.field("policy.revision"), None);
    for (name, value) in [
        ("invocation.id", "invoke-audited"),
        ("outcome", "Succeeded"),
        ("effect", "ReadOnly"),
        ("risk", "Low"),
        ("credential", "fetch-token"),
    ] {
        assert_eq!(execution.field(name), Some(value), "{name}: {execution:?}");
    }
    let calls: serde_json::Value =
        serde_json::from_str(execution.field("http.calls").unwrap()).unwrap();
    assert_eq!(calls[0]["credentialInjected"], true, "{calls}");

    let refusal = only(&captured, &[("invocation.id", "invoke-refused")]);
    assert_eq!(refusal.parent(), Some("broker.authorize"), "{refusal:?}");
    assert_eq!(refusal.field("decision.allowed"), Some("false"));
    assert_eq!(
        refusal.field("decision.reason"),
        Some("unconstrained-capability")
    );
    assert_eq!(refusal.field("policy.ids"), None, "{refusal:?}");

    let everything = captured.events_text() + &captured.spans_text();
    assert!(!everything.contains(SECRET), "a record leaked the secret");
    assert!(
        !everything.contains("Bearer"),
        "a record leaked the credential scheme"
    );
}
