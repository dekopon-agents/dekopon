use std::sync::Arc;

use dekopon_broker::{
    AssetInvocationResult, Attestation, AttestorGrant, AuthenticatedContext, Broker, BrokerError,
    BrokerLimits, CapabilityRoute, ConstraintCatalog, ConstraintSet, CredentialStore,
    IdentityDirectory, InvocationRequest, PolicyEngine, PolicyWorld, TraceOnlyAuditLog,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_capability::{EffectKind, ExecutionConstraints, HttpConstraints};
use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, InvocationId, PrincipalId, RiskLevel,
};
use dekopon_test_support::provider_fixture;

pub(crate) const SUBJECT: &str = "slack.t0123abc.u9xyz";

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

pub(crate) fn principal(name: &str) -> PrincipalId {
    name.parse().expect("valid principal fixture")
}

pub(crate) async fn invoke_as_caller(
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

pub(crate) fn request(id: &str, capability: &str, input: serde_json::Value) -> InvocationRequest {
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
            signed_download: None,
        }),
        storage: None,
        secret_use: None,
    }
}

pub(crate) async fn http_probe_broker(authority: &str) -> Broker<TraceOnlyAuditLog> {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("provider");
    let world = PolicyWorld::new(
        [principal("caller")],
        [(
            "http-probe.fetch".parse().expect("capability"),
            "http-probe".parse().expect("provider"),
        )],
    )
    .expect("world");
    Broker::new(
        registry,
        principal("broker-test"),
        PolicyEngine::new(POLICY, &world).expect("policy"),
        ConstraintCatalog::new([(
            "http-probe.fetch".parse().expect("capability"),
            ConstraintSet {
                default_secret_use: None,
                route: CapabilityRoute::Generic,
                provider: "http-probe".parse().expect("provider"),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                credential: None,
                constraints: loopback_constraints(authority),
            },
        )])
        .expect("catalog"),
        CredentialStore::empty(),
        IdentityDirectory::new([(SUBJECT.parse().expect("subject"), principal("caller"))])
            .expect("directory"),
        Arc::new(TraceOnlyAuditLog),
        BrokerLimits::default(),
    )
    .expect("broker")
}
