#![allow(clippy::unwrap_used, clippy::disallowed_methods)]

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use dekopon_broker::{
    AssetInvocationResult, Attestation, AttestorGrant, AuditEvent, AuthenticatedContext, Broker,
    BrokerBuildError, BrokerError, BrokerLimits, CapabilityRoute, ConstraintCatalog, ConstraintSet,
    CredentialRefreshError, CredentialStore, IdentityDirectory, InMemoryAuditLog,
    InvocationRequest, Leniency, PolicyEngine, PolicyWorld, RefreshingCredential, SecretCatalog,
    SecretMaterial, SecretResolutionError, SecretResolver, SecretUseBinding, StartupWarning,
};
use dekopon_broker_host::BoundCredential;
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry, CommandRunOutcome};
use dekopon_capability::{EffectKind, ExecutionConstraints, HttpConstraints, HttpPathRule};
use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, InvocationId, PrincipalId,
    ProviderFailureDetail, ProviderId, Redacted, RiskLevel, SecretDrn, SecretSinkKind,
    SecretUseProposal,
};
use dekopon_test_support::{LoopbackServer, provider_fixture};
use serde_json::json;

const TRACE_PARENT: &str = "00-0000000000000000000000000000f1c7-00000000000000f1-00";

const SLACK_SUBJECT: &str = "slack.t0123abc.u9xyz";

const GATEWAY: &str = "gateway";

fn principal(name: &str) -> PrincipalId {
    name.parse::<PrincipalId>()
        .expect("valid principal fixture")
}

fn agent(name: &str) -> AgentId {
    name.parse::<AgentId>().expect("valid agent fixture")
}

fn subject(canonical: &str) -> ExternalSubject {
    canonical
        .parse::<ExternalSubject>()
        .expect("canonical subject fixture")
}

fn caller_subject(name: &str) -> ExternalSubject {
    subject(&format!("slack.t0123abc.u{}", name.replace('-', "")))
}

fn callers<'a>(names: impl IntoIterator<Item = &'a str>) -> IdentityDirectory {
    IdentityDirectory::new(
        names
            .into_iter()
            .map(|name| (caller_subject(name), principal(name))),
    )
    .expect("distinct caller fixtures build a directory")
}

fn session(name: &str, agent_name: &str) -> AuthenticatedContext {
    AuthenticatedContext::attested(
        principal(name),
        Actor::Agent {
            agent: agent(agent_name),
        },
        principal(GATEWAY),
        caller_subject(name),
    )
    .expect("attested context is valid")
}

fn direct_peer(name: &str, agent_name: &str) -> AuthenticatedContext {
    AuthenticatedContext::new(
        principal(name),
        Actor::Agent {
            agent: agent(agent_name),
        },
    )
    .expect("trusted agent context is valid")
}

async fn invoke_as(
    broker: &Broker<InMemoryAuditLog>,
    name: &str,
    agent_name: &str,
    request: InvocationRequest,
) -> Result<AssetInvocationResult, BrokerError> {
    let attestation = Attestation::for_subject(caller_subject(name), agent(agent_name))
        .bound_to(request.id.clone());
    let (host, mut stdout) = std::os::unix::net::UnixStream::pair().expect("test stdout pipe");
    let capture = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut bytes).expect("stdout capture");
        bytes
    });
    let result = broker
        .invoke(
            &service_context(GATEWAY),
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

fn service_context(name: &str) -> AuthenticatedContext {
    AuthenticatedContext::new(
        principal(name),
        Actor::Service {
            principal: principal(name),
        },
    )
    .expect("trusted service context is valid")
}

fn request(id: &str, capability: &str, input: serde_json::Value) -> InvocationRequest {
    InvocationRequest {
        id: id
            .parse::<InvocationId>()
            .expect("valid invocation fixture"),
        capability: capability
            .parse::<CapabilityId>()
            .expect("valid capability fixture"),
        trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
        input,
        secret_use: None,
    }
}

fn set(provider: &str, constraints: ExecutionConstraints) -> ConstraintSet {
    set_with_metadata(provider, EffectKind::ReadOnly, RiskLevel::Low, constraints)
}

fn set_with_metadata(
    provider: &str,
    effect: EffectKind,
    risk: RiskLevel,
    constraints: ExecutionConstraints,
) -> ConstraintSet {
    ConstraintSet {
        default_secret_use: None,
        route: CapabilityRoute::Generic,
        provider: provider
            .parse::<ProviderId>()
            .expect("valid provider fixture"),
        effect,
        risk,
        credential: None,
        constraints,
    }
}

fn rebind_for_nestedset(name: &str, rebound: &str) -> BTreeMap<AgentId, BTreeMap<String, String>> {
    BTreeMap::from([(
        agent("nestedset-github"),
        BTreeMap::from([(name.to_owned(), rebound.to_owned())]),
    )])
}

fn catalog<'a>(entries: impl IntoIterator<Item = (&'a str, ConstraintSet)>) -> ConstraintCatalog {
    ConstraintCatalog::new(entries.into_iter().map(|(capability, set)| {
        (
            capability
                .parse::<CapabilityId>()
                .expect("valid capability fixture"),
            set,
        )
    }))
    .expect("distinct capability fixtures build a catalog")
}

fn engine<'a>(
    policies: &str,
    principals: impl IntoIterator<Item = &'a str>,
    capabilities: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> PolicyEngine {
    let world = PolicyWorld::new(
        principals.into_iter().map(principal),
        capabilities.into_iter().map(|(capability, provider)| {
            (
                capability
                    .parse::<CapabilityId>()
                    .expect("valid capability fixture"),
                provider
                    .parse::<ProviderId>()
                    .expect("valid provider fixture"),
            )
        }),
    )
    .expect("distinct fixtures build a world");
    PolicyEngine::new(policies, &world).expect("fixture policy validates")
}

fn probe_engine<'a>(policies: &str, principals: impl IntoIterator<Item = &'a str>) -> PolicyEngine {
    engine(
        policies,
        principals,
        [
            ("cli-probe.upper", "cli-probe"),
            ("cli-probe.count", "cli-probe"),
            ("cli-probe.reverse", "cli-probe"),
        ],
    )
}

fn http_probe_engine(policies: &str) -> PolicyEngine {
    engine(policies, ["caller"], [("http-probe.fetch", "http-probe")])
}

fn secret_drn() -> SecretDrn {
    "drn:com.xrl:secret:test:http-probe/token"
        .parse()
        .expect("canonical secret fixture")
}

fn http_probe_secret_engine(policies: &str) -> PolicyEngine {
    let world = PolicyWorld::new(
        [principal("caller")],
        [(
            "http-probe.fetch".parse().expect("capability"),
            "http-probe".parse().expect("provider"),
        )],
    )
    .expect("world")
    .with_secrets([secret_drn()]);
    PolicyEngine::new(policies, &world).expect("secret policy validates")
}

#[derive(Debug)]
struct StaticSecretResolver(&'static [u8]);

#[async_trait]
impl SecretResolver for StaticSecretResolver {
    async fn resolve(&self, _secret: &SecretDrn) -> Result<SecretMaterial, SecretResolutionError> {
        Ok(SecretMaterial::new(self.0.to_vec()))
    }
}

#[derive(Debug)]
struct MissingSecretResolver;

#[async_trait]
impl SecretResolver for MissingSecretResolver {
    async fn resolve(&self, _secret: &SecretDrn) -> Result<SecretMaterial, SecretResolutionError> {
        Err(SecretResolutionError {
            category: "missing",
        })
    }
}

fn provider_policy(name: &str, agent_name: &str, provider: &str, capability: &str) -> String {
    format!(
        r#"@id("{name}-{capability}-via-{agent_name}")
           permit(principal == Dekopon::Principal::"{name}",
                  action == Dekopon::Action::"{capability}",
                  resource == Dekopon::Provider::"{provider}")
           when {{ context.via == "{GATEWAY}" && context.agent == "{agent_name}" }};
{}"#,
        agent_prompt_policy(name, agent_name, GATEWAY)
    )
}

fn http_policy(name: &str, agent_name: &str, capability: &str) -> String {
    provider_policy(name, agent_name, "http-probe", capability)
}

fn loopback_constraints(authority: &str) -> ExecutionConstraints {
    ExecutionConstraints {
        asset: None,
        timeout_ms: 5_000,
        http: Some(HttpConstraints {
            allowed_hosts: vec![authority.to_owned()],
            propagate_trace: false,
            allowed_methods: vec!["GET".to_owned()],
            max_requests: 1,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 64 * 1024,
            allow_plaintext_loopback: true,
            request_templates: Vec::new(),
        }),
        storage: None,
        secret_use: None,
    }
}

fn probe_policy(name: &str, agent_name: &str, capability: &str) -> String {
    provider_policy(name, agent_name, "cli-probe", capability)
}

fn agent_prompt_policy(name: &str, agent_name: &str, via: &str) -> String {
    format!(
        r#"@id("{name}-prompts-{agent_name}")
           permit(principal == Dekopon::Principal::"{name}",
                  action == Dekopon::Action::"agent.prompt",
                  resource == Dekopon::Agent::"{agent_name}")
           when {{ context.via == "{via}" }};"#
    )
}

fn attested_policy(name: &str, agent_name: &str, via: &str, capability: &str) -> String {
    format!(
        r#"@id("{name}-{capability}-attested-via-{agent_name}")
           permit(principal == Dekopon::Principal::"{name}",
                  action == Dekopon::Action::"{capability}",
                  resource == Dekopon::Provider::"cli-probe")
           when {{ context.via == "{via}"
                && context.agent == "{agent_name}" }};"#
    )
}

fn attestor_grant<'a>(namespaces: impl IntoIterator<Item = &'a str>) -> AttestorGrant {
    AttestorGrant {
        namespaces: Some(namespaces.into_iter().map(str::to_owned).collect()),
    }
}

fn attestation(subject: &ExternalSubject, agent_name: &str, invocation: &str) -> Attestation {
    Attestation::for_subject(subject.clone(), agent(agent_name)).bound_to(
        invocation
            .parse::<InvocationId>()
            .expect("valid invocation fixture"),
    )
}

fn directory<'a>(entries: impl IntoIterator<Item = (&'a str, &'a str)>) -> IdentityDirectory {
    IdentityDirectory::new(
        entries
            .into_iter()
            .map(|(canonical, name)| (subject(canonical), principal(name))),
    )
    .expect("distinct subject fixtures build a directory")
}

async fn probe_registry(limits: BrokerHostLimits) -> BrokerProviderRegistry {
    dekopon_broker_host::enable_test_component_memoization();
    BrokerProviderRegistry::load([provider_fixture("cli-probe-provider.wasm")], limits)
        .await
        .expect("cli-probe provider fixture loads")
}

async fn attested_broker(
    identities: IdentityDirectory,
    audit: Arc<InMemoryAuditLog>,
) -> Broker<InMemoryAuditLog> {
    Broker::new(
        probe_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        probe_engine(
            &format!(
                "{}\n{}\n{}\n{}\n{}",
                attested_policy("cpetersen", "some-agent", "gateway", "cli-probe.upper"),
                agent_prompt_policy("cpetersen", "some-agent", "gateway"),
                agent_prompt_policy("oncall", "some-agent", "gateway"),
                attested_policy(
                    "cpetersen",
                    "some-agent",
                    "dekopon-console",
                    "cli-probe.upper"
                )
                .replace("attested-via", "console-attested-via"),
                agent_prompt_policy("cpetersen", "some-agent", "dekopon-console")
                    .replace("-prompts-", "-console-prompts-"),
            ),
            ["cpetersen", "oncall", "gateway", "dekopon-console"],
        ),
        catalog([(
            "cli-probe.upper",
            set("cli-probe", ExecutionConstraints::default()),
        )]),
        CredentialStore::empty(),
        identities,
        audit,
        BrokerLimits::default(),
    )
    .expect("attested policy is coherent")
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_authorizes_and_audits_no_payloads() {
    let registry = probe_registry(BrokerHostLimits::default()).await;
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound"));
    let broker = Broker::new(
        registry,
        "broker-test"
            .parse::<PrincipalId>()
            .expect("valid broker principal"),
        probe_engine(
            &probe_policy("caller", "provider-test", "cli-probe.upper"),
            ["caller"],
        ),
        catalog([(
            "cli-probe.upper",
            set("cli-probe", ExecutionConstraints::default()),
        )]),
        CredentialStore::empty(),
        callers(["caller"]),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("the policy matches loaded provider metadata");

    let result = invoke_as(
        &broker,
        "caller",
        "provider-test",
        request(
            "invoke-once",
            "cli-probe.upper",
            json!({"text": "top-secret-payload"}),
        ),
    )
    .await
    .expect("authorized invocation is accounted");
    assert_eq!(
        result.result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    assert_eq!(result.result.decision.decision_id, "allow-invoke-once");
    assert_eq!(result.result.decision.policy_digest.len(), 64);
    assert_eq!(result.result.evidence.len(), 1);

    let records = audit.records();
    assert_eq!(records.len(), 2);
    assert!(matches!(
        records[0],
        AuditEvent::Decision { allowed: true, .. }
    ));
    assert!(matches!(records[1], AuditEvent::Execution { .. }));
    let serialized = serde_json::to_string(&records).expect("audit serializes");
    assert!(!serialized.contains("top-secret-payload"));
    assert!(!serialized.contains("TOP-SECRET-PAYLOAD"));
}

#[tokio::test(flavor = "multi_thread")]
async fn unmatched_identity_is_denied_before_provider_execution() {
    let registry = probe_registry(BrokerHostLimits::default()).await;
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let broker = Broker::new(
        registry,
        "broker-test"
            .parse::<PrincipalId>()
            .expect("valid broker principal"),
        probe_engine(
            &format!(
                "{}\n{}",
                probe_policy("allowed-caller", "provider-test", "cli-probe.upper"),
                agent_prompt_policy("other-caller", "provider-test", GATEWAY),
            ),
            ["allowed-caller", "other-caller"],
        ),
        catalog([(
            "cli-probe.upper",
            set("cli-probe", ExecutionConstraints::default()),
        )]),
        CredentialStore::empty(),
        callers(["allowed-caller", "other-caller"]),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("policy is coherent");

    let allowed = broker.capabilities(&session("allowed-caller", "provider-test"));
    assert_eq!(allowed.len(), 1);
    assert_eq!(allowed[0].provider.as_str(), "cli-probe");
    assert_eq!(allowed[0].capability.id.as_str(), "cli-probe.upper");
    assert!(
        broker
            .capabilities(&session("other-caller", "provider-test"))
            .is_empty()
    );

    let result = invoke_as(
        &broker,
        "other-caller",
        "provider-test",
        request(
            "invoke-denied",
            "cli-probe.upper",
            json!({"text": "secret"}),
        ),
    )
    .await
    .expect("policy denial is audited");
    assert_eq!(
        result.result.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(result.result.decision.decision_id, "deny-invoke-denied");
    assert_eq!(result.result.error.as_deref(), Some("policy-denied"));
    let records = audit.records();
    assert_eq!(records.len(), 1);
    assert!(matches!(
        records[0],
        AuditEvent::Decision { allowed: false, .. }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_metadata_and_host_ceilings_are_checked_at_startup() {
    dekopon_broker_host::enable_test_component_memoization();
    let registry = probe_registry(BrokerHostLimits::default()).await;
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let error = Broker::new(
        registry,
        "broker-test"
            .parse::<PrincipalId>()
            .expect("valid broker principal"),
        probe_engine("", ["caller"]),
        catalog([(
            "cli-probe.upper",
            set("different-provider", ExecutionConstraints::default()),
        )]),
        CredentialStore::empty(),
        callers(["caller"]),
        audit,
        BrokerLimits::default(),
    )
    .expect_err("trusted provider mismatch must fail broker construction");
    assert!(matches!(error, BrokerBuildError::ProviderMismatch { .. }));

    let registry = probe_registry(BrokerHostLimits {
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
        probe_engine("", ["caller"]),
        catalog([(
            "cli-probe.upper",
            set(
                "cli-probe",
                ExecutionConstraints {
                    timeout_ms: 101,
                    ..ExecutionConstraints::default()
                },
            ),
        )]),
        CredentialStore::empty(),
        callers(["caller"]),
        audit,
        BrokerLimits::default(),
    )
    .expect_err("constraints cannot exceed the independent host timeout");
    assert!(matches!(error, BrokerBuildError::HostConstraint { .. }));

    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("checked HTTP provider loads");
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let error = Broker::new(
        registry,
        "broker-test"
            .parse::<PrincipalId>()
            .expect("valid broker principal"),
        engine(
            "",
            ["caller"],
            [
                ("http-probe.fetch", "http-probe"),
                ("http-probe.conditional-write", "http-probe"),
            ],
        ),
        catalog([(
            "http-probe.conditional-write",
            set("http-probe", ExecutionConstraints::default()),
        )]),
        CredentialStore::empty(),
        callers(["caller"]),
        audit,
        BrokerLimits::default(),
    )
    .expect_err("external-write metadata cannot be downgraded to read-only");
    assert!(matches!(
        error,
        BrokerBuildError::CapabilityMetadataMismatch {
            field: "effect",
            ..
        }
    ));
}

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "one server proves every denied shape sends zero bytes while one admitted shape sends exactly once"
)]
async fn broker_request_policy_narrows_untemplated_post_and_admits_json_reply() {
    dekopon_broker_host::enable_test_component_memoization();
    use dekopon_test_support::CaptureLayer;
    use tracing::instrument::WithSubscriber as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider loads");
    let server = LoopbackServer::once(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}");
    let authority = server.authority().to_owned();
    let mut constraints = loopback_constraints(&authority);
    constraints
        .http
        .as_mut()
        .expect("HTTP grant")
        .allowed_methods = vec!["POST".to_owned(), "GET".to_owned()];
    constraints
        .http
        .as_mut()
        .expect("HTTP grant")
        .allowed_hosts
        .push(format!(
            "localhost:{}",
            authority.split(':').next_back().expect("port")
        ));
    let policies = format!(
        "{}\n{}\n{}\n{}",
        http_policy("caller", "provider-test", "http-probe.fetch"),
        r#"@id("reply-only")
            forbid(principal, action == Dekopon::Action::"http-probe.fetch", resource)
            when { context has request }
            unless { context.request.method == "POST"
                && context.request.url.host == "127.0.0.1"
                && context.request.url.path == "/reply"
                && !(context.request.url has query)
                && context.request has body
                && context.request.body.channel == "C0123"
                && context.request.body has thread_ts };"#,
        r#"@id("no-broadcast")
            forbid(principal, action == Dekopon::Action::"http-probe.fetch", resource)
            when { context has request }
            unless { context.request.method == "POST"
                && context.request.url.host == "127.0.0.1"
                && context.request.url.path == "/reply"
                && !(context.request.url has query)
                && context.request.body.channel == "C0123"
                && context.request.body has thread_ts
                && !(context.request.body has reply_broadcast) };"#,
        r#"@id("thread-required")
            forbid(principal, action == Dekopon::Action::"http-probe.fetch", resource)
            when { context has request }
            unless { context.request.method == "POST"
                && context.request.url.host == "127.0.0.1"
                && context.request.url.path == "/reply"
                && !(context.request.url has query)
                && context.request.body.thread_ts == "1" };"#
    );
    let broker = Broker::new(
        registry,
        principal("broker-test"),
        http_probe_engine(&policies),
        catalog([("http-probe.fetch", set("http-probe", constraints))]),
        CredentialStore::empty(),
        callers(["caller"]),
        Arc::new(InMemoryAuditLog::new(32).expect("audit")),
        BrokerLimits::default(),
    )
    .expect("request forbid loads");
    let capture = CaptureLayer::new();
    for (id, uri, method, content_type, body) in [
        (
            "top-level",
            format!("http://{authority}/reply"),
            "POST",
            "application/json",
            r#"{"channel":"C0123"}"#,
        ),
        (
            "wrong-channel",
            format!("http://{authority}/reply"),
            "POST",
            "application/json",
            r#"{"channel":"OTHER","thread_ts":"1"}"#,
        ),
        (
            "dm",
            format!("http://{authority}/reply"),
            "POST",
            "application/json",
            r#"{"channel":"D0123","thread_ts":"1"}"#,
        ),
        (
            "query",
            format!("http://{authority}/reply?channel=SENTINEL-QUERY"),
            "POST",
            "application/json",
            r#"{"channel":"C0123","thread_ts":"1"}"#,
        ),
        (
            "form",
            format!("http://{authority}/reply"),
            "POST",
            "application/x-www-form-urlencoded",
            "channel=SENTINEL-FORM&thread_ts=1",
        ),
        (
            "multipart",
            format!("http://{authority}/reply"),
            "POST",
            "multipart/form-data; boundary=test",
            "SENTINEL-MULTIPART",
        ),
        (
            "wrong-host",
            format!(
                "http://localhost:{}/reply",
                authority.split(':').next_back().expect("port")
            ),
            "POST",
            "application/json",
            r#"{"channel":"C0123","thread_ts":"1"}"#,
        ),
    ] {
        let record_start = capture.records().len();
        let outcome = invoke_as(&broker, "caller", "provider-test", request(id, "http-probe.fetch", json!({
            "uri": uri, "method": method,
            "headers": [{"name": "content-type", "value": content_type}, {"name": "x-sentinel", "value": "SENTINEL-HEADER"}],
            "body": body
        }))).with_subscriber(tracing_subscriber::registry().with(capture.clone())).await.expect("denial accounted");
        assert_ne!(
            outcome.result.outcome,
            dekopon_capability::InvocationOutcome::Succeeded,
            "{id}"
        );
        let events = capture.events_where(
            "dekopon_broker::audit",
            &[("decision.point", "http.request"), ("invocation.id", id)],
        );
        assert_eq!(events.len(), 1, "{id} must reach Cedar once");
        assert_eq!(events[0].field("decision.allowed"), Some("false"));
        let records = capture.records();
        let request_spans = records[record_start..]
            .iter()
            .filter(|record| {
                matches!(
                    record,
                    dekopon_test_support::Record::Span {
                        name: "http.request",
                        ..
                    }
                )
            })
            .collect::<Vec<_>>();
        assert!(
            request_spans
                .iter()
                .any(|span| span.field("error.code") == Some("Denied")),
            "{id}: policy denial must mark this invocation's HTTP span"
        );
        assert!(
            request_spans
                .iter()
                .any(|span| span.field("error.message") == Some("HTTP request denied by policy")),
            "{id}: this invocation's HTTP span must carry the static error"
        );
        if id == "wrong-channel" || id == "dm" || id == "wrong-host" || id == "query" {
            assert_eq!(
                events[0].field("policy.ids"),
                Some(if id == "wrong-channel" || id == "dm" {
                    "no-broadcast,reply-only"
                } else {
                    "no-broadcast,reply-only,thread-required"
                }),
                "{id}"
            );
        }
        if id == "top-level" {
            assert_eq!(
                events[0].field("policy.ids"),
                Some("no-broadcast,reply-only")
            );
            assert_eq!(events[0].field("policy.error_ids"), Some("thread-required"));
            assert_eq!(events[0].field("policy.errors_present"), Some("true"));
        }
        if id == "form" || id == "multipart" {
            assert_eq!(
                events[0].field("policy.error_ids"),
                Some("no-broadcast,reply-only,thread-required"),
                "{id}"
            );
            let opaque = capture.events_where(
                "dekopon_broker::audit",
                &[
                    ("audit.event", "policy.request.body_opaque"),
                    ("invocation.id", id),
                ],
            );
            assert_eq!(opaque.len(), 1, "{id}");
            assert_eq!(
                opaque[0].field("request.body.reason"),
                Some("media-type"),
                "{id}"
            );
            assert_eq!(
                events[0].field("policy.errors_present"),
                Some("true"),
                "{id}"
            );
        }
        for sentinel in [
            "SENTINEL-QUERY",
            "SENTINEL-HEADER",
            "SENTINEL-FORM",
            "SENTINEL-MULTIPART",
            "OTHER",
            "D0123",
            "attribute",
            "error while evaluating",
        ] {
            assert!(
                !format!("{:?}", events[0]).contains(sentinel),
                "decision leaked {sentinel}"
            );
        }
    }
    let wrong_method = invoke_as(
        &broker,
        "caller",
        "provider-test",
        request(
            "wrong-method",
            "http-probe.fetch",
            json!({
                "uri": format!("http://{authority}/reply"), "method": "GET",
                "headers": [{"name":"content-type", "value":"application/json; charset=utf-8"}],
                "body": "{\"channel\":\"C0123\",\"thread_ts\":\"1\"}"
            }),
        ),
    )
    .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
    .await
    .expect("grant-admitted method reaches Cedar");
    assert_ne!(
        wrong_method.result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    let wrong_method_decisions = capture.events_where(
        "dekopon_broker::audit",
        &[
            ("decision.point", "http.request"),
            ("invocation.id", "wrong-method"),
        ],
    );
    assert_eq!(wrong_method_decisions.len(), 1);
    assert_eq!(
        wrong_method_decisions[0].field("decision.allowed"),
        Some("false")
    );
    assert_eq!(
        wrong_method_decisions[0].field("policy.ids"),
        Some("no-broadcast,reply-only,thread-required")
    );
    assert_eq!(
        wrong_method_decisions[0].field("policy.errors_present"),
        Some("false")
    );
    assert_eq!(wrong_method_decisions[0].field("policy.error_ids"), None);
    let allowed = invoke_as(
        &broker,
        "caller",
        "provider-test",
        request(
            "thread-reply",
            "http-probe.fetch",
            json!({"uri": format!("http://{authority}/reply"), "method": "POST",
               "headers": [{"name":"content-type", "value":"application/json; charset=utf-8"}],
               "body": "{\"channel\":\"C0123\",\"thread_ts\":\"1\"}"}),
        ),
    )
    .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
    .await
    .expect("admitted request accounted");
    assert_eq!(
        allowed.result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded,
        "{allowed:?}"
    );
    let requests = std::iter::once(server.request())
        .chain(server.recorded())
        .collect::<Vec<_>>();
    assert_eq!(
        requests.len(),
        1,
        "the real component sends exactly one HTTP request"
    );
    let wire = std::str::from_utf8(&requests[0]).expect("fixture request is UTF-8");
    assert_eq!(wire.lines().next(), Some("POST /reply HTTP/1.1"));
    let sent_host = wire
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("host"))
                .map(|(_, value)| value.trim())
        })
        .expect("outbound Host header");
    assert_eq!(
        format!("http://{sent_host}/reply"),
        format!("http://{authority}/reply")
    );
    assert!(
        wire.lines()
            .all(|line| !line.to_ascii_lowercase().starts_with("authorization:")),
        "guest sent no credential"
    );
    server.join();
}

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "one server proves the three admitted bodies send exactly once and the four denied ones send nothing"
)]
async fn request_body_fences_read_decide_and_canvas_bodies_and_deny_an_opaque_one_before_egress() {
    dekopon_broker_host::enable_test_component_memoization();
    use dekopon_test_support::CaptureLayer;
    use tracing::instrument::WithSubscriber as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    const DECIDE: &str = r#"{"model":"typesafe/jev-1.13","questions":{"decision":{"type":"choice","instructions":"Choose the best matching label for the supplied evidence.","criteria":{"original":"original","summary":"summary"}}},"state":{"records":[{"ageMs":17,"bytes":512,"exitStatus":null,"outcome":"none","pinned":false,"tool":"none"}]}}"#;
    const CHAT: &str = r#"{"model":"typesafe/jev-1.13","messages":[{"role":"user","content":"SENTINEL-PROMPT"}],"temperature":0.7,"logprobs":true,"top_logprobs":5,"response_format":{"type":"json_object"}}"#;
    const CANVAS: &str = r#"{"canvas_id":"F0C907QUHU7","changes":[{"operation":"replace","section_id":"temp:C:abc","document_content":{"type":"markdown","markdown":"SENTINEL-MARKDOWN"}}]}"#;
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider loads");
    let server = LoopbackServer::serving(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
        3,
    );
    let authority = server.authority().to_owned();
    let mut constraints = loopback_constraints(&authority);
    constraints
        .http
        .as_mut()
        .expect("HTTP grant")
        .allowed_methods = vec!["POST".to_owned()];
    let policies = format!(
        "{}\n{}",
        http_policy("caller", "provider-test", "http-probe.fetch"),
        r#"@id("decide-model-pin")
            forbid(principal, action == Dekopon::Action::"http-probe.fetch", resource)
            when { context has request }
            when { context.request.url.path == "/api/alpha/decisions" }
            unless { context.request has body
                && context.request.body has model
                && context.request.body.model == "typesafe/jev-1.13" };
           @id("canvas-pin")
            forbid(principal, action == Dekopon::Action::"http-probe.fetch", resource)
            when { context has request }
            when { context.request.url.path == "/api/canvases.edit" }
            unless { context.request has body
                && context.request.body has canvas_id
                && context.request.body.canvas_id == "F0C907QUHU7" };
           @id("path-block")
            forbid(principal, action == Dekopon::Action::"http-probe.fetch", resource)
            when { context has request }
            when { context.request.url.path == "/api/blocked" };"#
    );
    let broker = Broker::new(
        registry,
        principal("broker-test"),
        http_probe_engine(&policies),
        catalog([("http-probe.fetch", set("http-probe", constraints))]),
        CredentialStore::empty(),
        callers(["caller"]),
        Arc::new(InMemoryAuditLog::new(64).expect("audit")),
        BrokerLimits::default(),
    )
    .expect("body fences load");
    let capture = CaptureLayer::new();
    for (id, path, body, admitted) in [
        (
            "decide-jev",
            "/api/alpha/decisions",
            DECIDE.to_owned(),
            true,
        ),
        (
            "decide-luna",
            "/api/alpha/decisions",
            DECIDE.replace("typesafe/jev-1.13", "openai/gpt-6-luna-decisions"),
            false,
        ),
        ("chat-jev", "/api/alpha/decisions", CHAT.to_owned(), true),
        (
            "blocked-opaque",
            "/api/blocked",
            CHAT.replace("0.7", "0.12345"),
            false,
        ),
        (
            "chat-opaque",
            "/api/alpha/decisions",
            CHAT.replace("0.7", "0.12345"),
            false,
        ),
        (
            "canvas-pinned",
            "/api/canvases.edit",
            CANVAS.to_owned(),
            true,
        ),
        (
            "canvas-other",
            "/api/canvases.edit",
            CANVAS.replace("F0C907QUHU7", "F0OTHER"),
            false,
        ),
    ] {
        let outcome = invoke_as(
            &broker,
            "caller",
            "provider-test",
            request(
                id,
                "http-probe.fetch",
                json!({
                    "uri": format!("http://{authority}{path}"), "method": "POST",
                    "headers": [{"name": "content-type", "value": "application/json"}],
                    "body": body
                }),
            ),
        )
        .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
        .await
        .expect("invocation accounted");
        assert_eq!(
            outcome.result.outcome == dekopon_capability::InvocationOutcome::Succeeded,
            admitted,
            "{id}: {outcome:?}"
        );
        let decisions = capture.events_where(
            "dekopon_broker::audit",
            &[("decision.point", "http.request"), ("invocation.id", id)],
        );
        assert_eq!(decisions.len(), 1, "{id} reaches Cedar once");
        let decision = &decisions[0];
        let opaque = capture.events_where(
            "dekopon_broker::audit",
            &[
                ("audit.event", "policy.request.body_opaque"),
                ("invocation.id", id),
            ],
        );
        match id {
            "decide-luna" | "canvas-other" => {
                assert_eq!(decision.field("decision.allowed"), Some("false"), "{id}");
                assert_eq!(
                    decision.field("policy.errors_present"),
                    Some("false"),
                    "{id}"
                );
                assert_eq!(
                    decision.field("policy.ids"),
                    Some(if id == "decide-luna" {
                        "decide-model-pin"
                    } else {
                        "canvas-pin"
                    })
                );
                assert!(opaque.is_empty(), "{id}");
            }
            "blocked-opaque" => {
                assert_eq!(decision.field("decision.allowed"), Some("false"));
                assert_eq!(decision.field("policy.errors_present"), Some("false"));
                assert_eq!(decision.field("policy.ids"), Some("path-block"));
                assert!(opaque.is_empty());
            }
            "chat-opaque" => {
                assert_eq!(decision.field("decision.allowed"), Some("false"));
                assert_eq!(decision.field("decision.reason"), Some("policy-error"));
                assert_eq!(decision.field("policy.error_ids"), Some("decide-model-pin"));
                assert_eq!(opaque.len(), 1);
                assert_eq!(
                    opaque[0].field("request.body.reason"),
                    Some("unrepresentable")
                );
                assert_eq!(
                    opaque[0].field("decision.id"),
                    decision.field("decision.id")
                );
            }
            _ => {
                assert_eq!(decision.field("decision.allowed"), Some("true"), "{id}");
                assert!(opaque.is_empty(), "{id}");
            }
        }
    }
    let audit = capture.events_where("dekopon_broker::audit", &[]);
    for sentinel in [
        "SENTINEL-PROMPT",
        "SENTINEL-MARKDOWN",
        "0.12345",
        "typesafe/jev",
        "gpt-6-luna",
        "F0OTHER",
        "F0C907QUHU7",
    ] {
        assert!(
            audit
                .iter()
                .all(|record| !format!("{record:?}").contains(sentinel)),
            "audit leaked {sentinel}"
        );
    }
    let sent = server
        .recorded()
        .into_iter()
        .map(|bytes| String::from_utf8(bytes).expect("fixture request is UTF-8"))
        .collect::<Vec<_>>();
    let lines = sent
        .iter()
        .map(|wire| wire.lines().next().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(
        lines,
        [
            "POST /api/alpha/decisions HTTP/1.1",
            "POST /api/alpha/decisions HTTP/1.1",
            "POST /api/canvases.edit HTTP/1.1",
        ],
        "only the three admitted requests left the broker"
    );
    assert!(sent[0].ends_with(DECIDE) && sent[1].ends_with(CHAT) && sent[2].ends_with(CANVAS));
    server.join();
}

#[tokio::test(flavor = "multi_thread")]
async fn request_audit_sink_failure_is_reported_and_denied_before_dns() {
    dekopon_broker_host::enable_test_component_memoization();
    use dekopon_test_support::CaptureLayer;
    use tracing::instrument::WithSubscriber as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider loads");
    let broker = Broker::new(
        registry,
        principal("broker-test"),
        http_probe_engine(&http_policy("caller", "provider-test", "http-probe.fetch")),
        catalog([(
            "http-probe.fetch",
            set("http-probe", loopback_constraints("no-such-egress.invalid")),
        )]),
        CredentialStore::empty(),
        callers(["caller"]),
        Arc::new(InMemoryAuditLog::new(1).expect("one invocation decision")),
        BrokerLimits::default(),
    )
    .expect("broker loads");
    let capture = CaptureLayer::new();
    let result = invoke_as(
        &broker,
        "caller",
        "provider-test",
        request(
            "audit-full",
            "http-probe.fetch",
            json!({
                "uri": "https://no-such-egress.invalid/reply", "method": "GET"
            }),
        ),
    )
    .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
    .await;
    assert!(
        matches!(result, Err(BrokerError::OutcomeAudit { .. })),
        "the full sink also refuses the terminal outcome"
    );
    let spans = capture.records();
    assert!(
        spans.iter().any(|record| matches!(
            record,
            dekopon_test_support::Record::Span {
                name: "http.request",
                ..
            }
        ) && record.field("error.code") == Some("Denied")),
        "request must not reach DNS"
    );
    assert!(spans.iter().any(|record| matches!(
        record,
        dekopon_test_support::Record::Span {
            name: "http.request",
            ..
        }
    ) && record.field("error.message")
        == Some("HTTP request denied by policy")));
    let decisions = capture.events_where(
        "dekopon_broker::audit",
        &[("decision.point", "http.request")],
    );
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].field("decision.allowed"), Some("true"));
    let failures = capture.events_where(
        "dekopon_broker",
        &[
            ("event", "broker_audit_append_failed"),
            ("audit.stage", "decision"),
        ],
    );
    assert_eq!(failures.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn http_audit_contains_only_sanitized_call_metadata() {
    dekopon_broker_host::enable_test_component_memoization();
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider fixture loads");
    let server = LoopbackServer::once(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
    );
    let authority = server.authority().to_owned();
    let constraints = ExecutionConstraints {
        asset: None,
        timeout_ms: 5_000,
        http: Some(HttpConstraints {
            propagate_trace: false,
            allowed_hosts: vec![authority.clone()],
            allowed_methods: vec!["POST".to_owned()],
            max_requests: 1,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 64 * 1024,
            allow_plaintext_loopback: true,
            request_templates: Vec::new(),
        }),
        storage: None,
        secret_use: None,
    };
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let broker = Broker::new(
        registry,
        "broker-test"
            .parse::<PrincipalId>()
            .expect("valid broker principal"),
        http_probe_engine(&http_policy("caller", "provider-test", "http-probe.fetch")),
        catalog([("http-probe.fetch", set("http-probe", constraints))]),
        CredentialStore::empty(),
        callers(["caller"]),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("the HTTP constraint set matches trusted metadata and host ceilings");

    let result = invoke_as(
        &broker,
        "caller",
        "provider-test",
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
        result.result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    assert_eq!(result.result.evidence.len(), 2);
    assert!(
        result
            .result
            .evidence
            .iter()
            .any(|item| item.kind == "http-calls")
    );
    let wire = server.request();
    assert!(wire.ends_with(b"\r\n\r\nbody-secret"));
    server.join();

    let records = audit.records();
    assert_eq!(records.len(), 3);
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

#[tokio::test(flavor = "multi_thread")]
async fn a_request_template_pins_the_attested_conversation_and_needs_a_chat_scope() {
    dekopon_broker_host::enable_test_component_memoization();
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider fixture loads");
    let server = LoopbackServer::once(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
    );
    let authority = server.authority().to_owned();
    let mut constraints = loopback_constraints(&authority);
    constraints.http.as_mut().expect("HTTP").request_templates = vec![
        serde_json::from_value(json!({
            "method": "GET",
            "path": "/api/conversations.replies",
            "query": {
                "pinned": {"channel": "conversation.id", "ts": "conversation.thread"},
                "allowed": ["limit"]
            }
        }))
        .expect("template fixture"),
    ];
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound"));
    let broker = Broker::new(
        registry,
        principal("broker-test"),
        http_probe_engine(&http_policy("caller", "provider-test", "http-probe.fetch")),
        catalog([("http-probe.fetch", set("http-probe", constraints))]),
        CredentialStore::empty(),
        callers(["caller"]),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("a templated HTTP constraint set is coherent");
    let fetch = |id: &str| {
        request(
            id,
            "http-probe.fetch",
            json!({"uri": format!("http://{authority}/api/conversations.replies?limit=5")}),
        )
    };

    let unscoped = invoke_as(&broker, "caller", "provider-test", fetch("invoke-unscoped"))
        .await
        .expect("a denial is a result");
    assert_eq!(
        unscoped.result.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(
        unscoped.result.error.as_deref(),
        Some("chat-scope-required")
    );

    let request = fetch("invoke-scoped");
    let attestation = Attestation::for_chat(
        caller_subject("caller"),
        agent("provider-test"),
        dekopon_broker::ChatScopeClaim {
            transport: "family-slack".parse().expect("transport"),
            kind: dekopon_broker::ChatTransportKind::Slack,
            conversation: dekopon_broker::Conversation {
                kind: dekopon_broker::ConversationKind::Thread,
                container: Some("t0123abc".to_owned()),
                id: "c0123abc".to_owned(),
                thread: Some("1712345678.000100".to_owned()),
            },
            delivery: None,
            trigger: dekopon_broker::Trigger::Message,
        },
    )
    .bound_to(request.id.clone());
    let (host, mut stdout) = std::os::unix::net::UnixStream::pair().expect("test stdout pipe");
    let capture = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut bytes).expect("stdout capture");
        bytes
    });
    let scoped = broker
        .invoke(
            &service_context(GATEWAY),
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
        .await
        .expect("the scoped invocation runs");
    capture.join().expect("stdout capture thread");
    assert_eq!(
        scoped.result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded,
        "{:?}",
        scoped.result.error
    );
    let wire = String::from_utf8(server.request()).expect("fixture request is UTF-8");
    server.join();
    assert_eq!(
        wire.lines().next(),
        Some(
            "GET /api/conversations.replies?limit=5&channel=C0123ABC&ts=1712345678.000100 HTTP/1.1"
        )
    );
    let records = serde_json::to_string(&audit.records()).expect("audit serializes");
    assert!(!records.contains("C0123ABC"), "{records}");
}

#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one long test scenario")]
async fn external_writes_are_authorized_and_audited_even_when_the_provider_fails() {
    dekopon_broker_host::enable_test_component_memoization();
    use dekopon_capability::InvocationOutcome::{Failed, Succeeded};

    for (response, expected) in [
        (
            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice(),
            Succeeded,
        ),
        (b"not-http".as_slice(), Failed),
    ] {
        let registry = BrokerProviderRegistry::load(
            [provider_fixture("http-probe-provider.wasm")],
            BrokerHostLimits::default(),
        )
        .await
        .expect("checked HTTP provider loads");
        let server = LoopbackServer::once(response);
        let authority = server.authority().to_owned();
        let mut constraints = loopback_constraints(&authority);
        constraints
            .http
            .as_mut()
            .expect("HTTP constraints")
            .allowed_methods = vec!["DELETE".to_owned()];
        let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
        let broker = Broker::new(
            registry,
            principal("broker-test"),
            engine(
                &provider_policy("caller", "provider-test", "http-probe", "http-probe.purge"),
                ["caller"],
                [
                    ("http-probe.fetch", "http-probe"),
                    ("http-probe.purge", "http-probe"),
                ],
            ),
            catalog([
                (
                    "http-probe.fetch",
                    set("http-probe", loopback_constraints(&authority)),
                ),
                (
                    "http-probe.purge",
                    set_with_metadata(
                        "http-probe",
                        EffectKind::ExternalWrite,
                        RiskLevel::High,
                        constraints,
                    ),
                ),
            ]),
            CredentialStore::empty(),
            callers(["caller"]),
            Arc::clone(&audit),
            BrokerLimits::default(),
        )
        .expect("the external-write constraint set exactly matches trusted provider metadata");
        if expected == Succeeded {
            let available = broker.capabilities(&session("caller", "provider-test"));
            assert_eq!(available.len(), 1);
            assert_eq!(available[0].capability.effect, EffectKind::ExternalWrite);
            assert_eq!(available[0].capability.risk, RiskLevel::High);
            let read = invoke_as(
                &broker,
                "caller",
                "provider-test",
                request(
                    "invoke-http-read-with-write-rule",
                    "http-probe.fetch",
                    json!({"uri": format!("http://{authority}/private-path")}),
                ),
            )
            .await
            .expect("ungranted read is denied and audited");
            assert_eq!(
                read.result.outcome,
                dekopon_capability::InvocationOutcome::Denied
            );
            assert_eq!(read.result.error.as_deref(), Some("policy-denied"));
        }
        let result = invoke_as(
            &broker,
            "caller",
            "provider-test",
            request(
                "invoke-http-write",
                "http-probe.purge",
                json!({"uri": format!("http://{authority}/private-path?token=query-secret")}),
            ),
        )
        .await
        .expect("the authorized write is accounted");
        assert_eq!(result.result.outcome, expected);
        let wire = server.request();
        assert!(
            wire.starts_with(b"DELETE /private-path?token=query-secret HTTP/1.1\r\n"),
            "the external write must have left the host before the outcome"
        );
        assert!(wire.ends_with(b"\r\n\r\n"), "DELETE has no body");
        server.join();
        let records = audit.records();
        if expected == Succeeded {
            assert_eq!(records.len(), 4);
        } else {
            assert_eq!(result.result.error.as_deref(), Some("provider-failure"));
            assert_eq!(
                result.result.detail,
                Some(ProviderFailureDetail::new(
                    "provider-exit",
                    "protocol: HTTP transport failed\n"
                )),
                "a typed provider failure carries the provider's own code and message on the wire"
            );
            assert!(
                result
                    .result
                    .evidence
                    .iter()
                    .any(|evidence| evidence.kind == "http-calls"),
                "a failure that dispatched HTTP must return http-call evidence"
            );
            assert_eq!(records.len(), 3);
            let AuditEvent::Execution {
                outcome,
                error,
                error_detail,
                http_calls,
                ..
            } = &records[2]
            else {
                panic!("the terminal record is an execution event");
            };
            assert_eq!(*outcome, Failed);
            assert_eq!(error.as_deref(), Some("provider-failure"));
            assert_eq!(
                error_detail
                    .as_ref()
                    .map(|detail| (detail.code.as_str(), detail.message.as_str())),
                Some(("provider-exit", "protocol: HTTP transport failed\n"))
            );
            assert_eq!(
                http_calls.len(),
                1,
                "the completed call must survive into the failed execution record"
            );
            assert_eq!(http_calls[0].method, "DELETE");
            assert_eq!(http_calls[0].authority, authority);
            assert_eq!(http_calls[0].status, None);
        }
        let serialized = serde_json::to_string(&records).expect("audit serializes");
        assert!(serialized.contains(&authority));
        assert!(serialized.contains("external-write"));
        assert!(serialized.contains("DELETE"));
        assert!(!serialized.contains("private-path"));
        assert!(!serialized.contains("query-secret"));
    }
}

#[derive(Debug)]
struct ScriptedRefreshingCredential {
    destinations: Vec<String>,
    resolutions: Arc<std::sync::atomic::AtomicUsize>,
    script: parking_lot::Mutex<Vec<Result<&'static str, CredentialRefreshError>>>,
}

impl ScriptedRefreshingCredential {
    fn new(
        destinations: Vec<String>,
        script: Vec<Result<&'static str, CredentialRefreshError>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            destinations,
            resolutions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            script: parking_lot::Mutex::new(script),
        })
    }
}

#[async_trait]
impl RefreshingCredential for ScriptedRefreshingCredential {
    fn destinations(&self) -> &[String] {
        &self.destinations
    }

    async fn resolve(&self) -> Result<BoundCredential, CredentialRefreshError> {
        self.resolutions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let next = {
            let mut script = self.script.lock();
            assert!(
                !script.is_empty(),
                "the broker resolved more times than the script allows"
            );
            script.remove(0)
        };
        let access = next?;
        BoundCredential::chatgpt_subscription(
            Redacted::new(access.to_owned()),
            "acct-fixture",
            self.destinations.clone(),
        )
        .map_err(|source| {
            panic!("the fixture access token must be presentable: {source}");
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refreshing_credential_resolves_once_per_invocation_outside_the_guest_budget() {
    dekopon_broker_host::enable_test_component_memoization();
    const ACCESS: &str = "refreshed-access-token-audit-must-never-see";
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider fixture loads");
    let server = LoopbackServer::once(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
    );
    let authority = server.authority().to_owned();
    let constraints = loopback_constraints(&authority);
    assert_eq!(
        constraints
            .http
            .as_ref()
            .expect("the fixture grants HTTP")
            .max_requests,
        1,
        "the whole point is one guest call and no headroom for a renewal"
    );
    let source = ScriptedRefreshingCredential::new(vec![authority.clone()], vec![Ok(ACCESS)]);
    let resolutions = Arc::clone(&source.resolutions);
    let credentials = CredentialStore::new([(
        "chatgpt-fixture".to_owned(),
        source as Arc<dyn RefreshingCredential>,
    )])
    .expect("credential store builds");
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let broker = Broker::new(
        registry,
        principal("broker-test"),
        http_probe_engine(&http_policy("caller", "provider-test", "http-probe.fetch")),
        catalog([(
            "http-probe.fetch",
            ConstraintSet {
                default_secret_use: None,
                route: CapabilityRoute::Generic,
                credential: Some("chatgpt-fixture".to_owned()),
                ..set("http-probe", constraints)
            },
        )]),
        credentials,
        callers(["caller"]),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("a refreshing credential proves its destinations at startup");

    let result = invoke_as(
        &broker,
        "caller",
        "provider-test",
        request(
            "invoke-refreshing",
            "http-probe.fetch",
            json!({ "uri": format!("http://{authority}/images/generations"), "method": "GET" }),
        ),
    )
    .await
    .expect("the renewed credential serves the invocation");

    assert_eq!(
        result.result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    assert_eq!(
        resolutions.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one invocation must resolve the credential exactly once"
    );

    let wire = server.request_text();
    assert!(
        wire.contains(&format!("authorization: Bearer {ACCESS}")),
        "{wire}"
    );
    assert!(wire.contains("chatgpt-account-id: acct-fixture"), "{wire}");
    server.join();

    let records = audit.records();
    let AuditEvent::Execution {
        credential,
        http_calls,
        ..
    } = &records[2]
    else {
        panic!("the terminal record is an execution event");
    };
    assert_eq!(credential.as_deref(), Some("chatgpt-fixture"));
    assert_eq!(
        http_calls.len(),
        1,
        "the renewal must not appear as a second call"
    );
    assert!(http_calls[0].credential_injected);
    let serialized = serde_json::to_string(&records).expect("audit serializes");
    assert!(
        !serialized.contains(ACCESS),
        "audit leaked the access token"
    );
    assert!(
        !serialized.contains("acct-fixture"),
        "audit leaked the account identifier"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unrenewable_credential_fails_its_invocation_and_classifies_why() {
    dekopon_broker_host::enable_test_component_memoization();
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider fixture loads");
    let authority = "127.0.0.1:9".to_owned();
    let source = ScriptedRefreshingCredential::new(
        vec![authority.clone()],
        vec![
            Err(CredentialRefreshError::ReauthorizationRequired),
            Err(CredentialRefreshError::Unavailable {
                category: "transport",
            }),
        ],
    );
    let resolutions = Arc::clone(&source.resolutions);
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound"));
    let broker = Broker::new(
        registry,
        principal("broker-test"),
        http_probe_engine(&http_policy("caller", "provider-test", "http-probe.fetch")),
        catalog([(
            "http-probe.fetch",
            ConstraintSet {
                default_secret_use: None,
                route: CapabilityRoute::Generic,
                credential: Some("chatgpt-fixture".to_owned()),
                ..set("http-probe", loopback_constraints(&authority))
            },
        )]),
        CredentialStore::new([(
            "chatgpt-fixture".to_owned(),
            source as Arc<dyn RefreshingCredential>,
        )])
        .expect("credential store builds"),
        callers(["caller"]),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("the constraint set matches the credential's destinations");

    let invoke = |id: &'static str| {
        invoke_as(
            &broker,
            "caller",
            "provider-test",
            request(
                id,
                "http-probe.fetch",
                json!({ "uri": format!("http://{authority}/"), "method": "GET" }),
            ),
        )
    };

    let permanent = invoke("invoke-reauth")
        .await
        .expect("an unrenewable credential is a failed invocation, not a broker error");
    assert_eq!(
        permanent.result.outcome,
        dekopon_capability::InvocationOutcome::Failed
    );
    assert_eq!(
        permanent.result.error.as_deref(),
        Some("credential-unavailable")
    );
    assert!(
        !permanent
            .result
            .evidence
            .iter()
            .any(|evidence| evidence.kind == "http-calls"),
        "the component must never have run"
    );

    let transient = invoke("invoke-transient")
        .await
        .expect("the broker keeps serving after an unusable credential");
    assert_eq!(
        transient.result.outcome,
        dekopon_capability::InvocationOutcome::Failed
    );
    assert_eq!(
        transient.result.error.as_deref(),
        Some("credential-refresh-failed"),
        "a transient renewal failure must not read as one an operator has to fix"
    );
    assert_eq!(resolutions.load(std::sync::atomic::Ordering::SeqCst), 2);

    let records = audit.records();
    for record in &records {
        if let AuditEvent::Execution { credential, .. } = record {
            assert_eq!(
                credential.as_deref(),
                Some("chatgpt-fixture"),
                "a failed invocation still records which credential it would have presented"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_omitted_secret_proposal_uses_the_configured_default_only_with_binding_and_cedar() {
    dekopon_broker_host::enable_test_component_memoization();
    use dekopon_broker::{DefaultSecretSink, DefaultSecretUse};
    use dekopon_test_support::CaptureLayer;
    use tracing::instrument::WithSubscriber as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    use dekopon_capability::InvocationOutcome::{Denied, Succeeded};
    for (
        default_present,
        binding_present,
        grant_present,
        explicit,
        capability_permitted,
        expected,
    ) in [
        (false, true, true, false, true, Succeeded),
        (true, true, true, false, true, Succeeded),
        (true, true, false, false, true, Denied),
        (true, false, true, false, true, Denied),
        (true, true, true, true, true, Succeeded),
        (true, true, true, false, false, Denied),
    ] {
        let server = LoopbackServer::once(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}");
        let authority = server.authority().to_owned();
        let secret_policy = if grant_present {
            r#"@id("caller-secret-use") permit(principal == Dekopon::Principal::"caller", action == Dekopon::Action::"secret.use", resource == Dekopon::Secret::"drn:com.xrl:secret:test:http-probe/token") when { context.capability == "http-probe.fetch" && context.provider == "http-probe" && context.sink == "httpBearer" };"#
        } else {
            ""
        };
        let registry = BrokerProviderRegistry::load(
            [provider_fixture("http-probe-provider.wasm")],
            BrokerHostLimits::default(),
        )
        .await
        .unwrap();
        let capability_policy = if capability_permitted {
            http_policy("caller", "provider-test", "http-probe.fetch")
        } else {
            agent_prompt_policy("caller", "provider-test", GATEWAY)
        };
        let mut set = set("http-probe", loopback_constraints(&authority));
        if default_present {
            set.default_secret_use = Some(DefaultSecretUse {
                drn: if explicit {
                    "drn:com.xrl:secret:test:unbound/token".parse().unwrap()
                } else {
                    secret_drn()
                },
                sink: DefaultSecretSink::HttpBearer,
            });
        }
        let broker = Broker::new(
            registry,
            principal("broker-test"),
            http_probe_secret_engine(&format!("{capability_policy}\n{secret_policy}")),
            catalog([("http-probe.fetch", set)]),
            CredentialStore::empty(),
            callers(["caller"]),
            Arc::new(InMemoryAuditLog::new(4).unwrap()),
            BrokerLimits::default(),
        )
        .unwrap()
        .with_secret_catalog(
            SecretCatalog::new(
                if binding_present {
                    vec![SecretUseBinding {
                        binding_id: "http-probe-token".to_owned(),
                        secret: secret_drn(),
                        capability: "http-probe.fetch".parse().unwrap(),
                        sink: SecretSinkKind::HttpBearer,
                        basic_username: None,
                        allowed_hosts: vec![authority.clone()],
                        allowed_methods: vec!["GET".to_owned()],
                        allowed_paths: vec![HttpPathRule::Exact {
                            path: "/api/v1/thing".to_owned(),
                        }],
                        allow_query: false,
                        max_injections: 1,
                    }]
                } else {
                    vec![]
                },
                Arc::new(StaticSecretResolver(b"default-private-sentinel")),
            )
            .unwrap(),
        )
        .unwrap();
        let mut proposal = request(
            "invoke-default-secret",
            "http-probe.fetch",
            json!({
                "uri": format!("http://{authority}/api/v1/thing"), "method": "GET"
            }),
        );
        if explicit {
            proposal.secret_use = Some(SecretUseProposal::HttpBearer {
                secret: secret_drn(),
            });
        }
        let capture = CaptureLayer::new();
        let result = invoke_as(&broker, "caller", "provider-test", proposal)
            .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
            .await
            .unwrap();
        assert_eq!(result.result.outcome, expected);
        let decision_events = capture.events_where(
            "dekopon_broker::audit",
            &[
                ("audit.event", "broker.decision"),
                ("decision.point", "invocation"),
            ],
        );
        assert_eq!(decision_events.len(), 1);
        let expected_drn = secret_drn();
        assert_eq!(
            decision_events[0].field("secret"),
            default_present.then_some(expected_drn.as_str())
        );
        assert_eq!(
            decision_events[0].field("secret.sink"),
            default_present.then_some("httpBearer")
        );
        assert_eq!(
            decision_events[0].field("decision.allowed"),
            Some(
                if expected == dekopon_capability::InvocationOutcome::Succeeded {
                    "true"
                } else {
                    "false"
                }
            )
        );
        if expected == dekopon_capability::InvocationOutcome::Succeeded {
            let request = server.request_text();
            assert_eq!(
                request.contains("authorization: Bearer default-private-sentinel"),
                default_present
            );
            server.join();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn model_selected_drn_requires_dual_policy_and_exact_private_binding() {
    dekopon_broker_host::enable_test_component_memoization();
    const SECRET: &[u8] = b"drn-secret-never-visible";
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider fixture loads");
    let server = LoopbackServer::once(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
    );
    let authority = server.authority().to_owned();
    let policy = format!(
        "{}\n{}",
        http_policy("caller", "provider-test", "http-probe.fetch"),
        r#"@id("caller-secret-use")
           permit(principal == Dekopon::Principal::"caller",
                  action == Dekopon::Action::"secret.use",
                  resource == Dekopon::Secret::"drn:com.xrl:secret:test:http-probe/token")
           when { context.capability == "http-probe.fetch"
               && context.provider == "http-probe"
               && context.sink == "httpBearer" };"#,
    );
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("audit"));
    let broker = Broker::new(
        registry,
        principal("broker-test"),
        http_probe_secret_engine(&policy),
        catalog([(
            "http-probe.fetch",
            set("http-probe", loopback_constraints(&authority)),
        )]),
        CredentialStore::empty(),
        callers(["caller"]),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("broker")
    .with_secret_catalog(
        SecretCatalog::new(
            vec![SecretUseBinding {
                binding_id: "http-probe-token".to_owned(),
                secret: secret_drn(),
                capability: "http-probe.fetch".parse().expect("capability"),
                sink: SecretSinkKind::HttpBearer,
                basic_username: None,
                allowed_hosts: vec![authority.clone()],
                allowed_methods: vec!["GET".to_owned()],
                allowed_paths: vec![HttpPathRule::Exact {
                    path: "/api/v1/thing".to_owned(),
                }],
                allow_query: false,
                max_injections: 1,
            }],
            Arc::new(StaticSecretResolver(SECRET)),
        )
        .expect("secret catalog"),
    )
    .expect("binding fits capability");

    let mut proposal = request(
        "invoke-drn-secret",
        "http-probe.fetch",
        json!({
            "uri": format!("http://{authority}/api/v1/thing"),
            "method": "GET"
        }),
    );
    proposal.secret_use = Some(SecretUseProposal::HttpBearer {
        secret: secret_drn(),
    });
    let result = invoke_as(&broker, "caller", "provider-test", proposal)
        .await
        .expect("dual-authorized invocation completes");
    assert_eq!(
        result.result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    let wire = server.request_text();
    assert!(
        wire.contains("authorization: Bearer drn-secret-never-visible"),
        "{wire}"
    );
    server.join();

    let serialized = serde_json::to_string(&audit.records()).expect("audit serializes");
    assert!(
        serialized.contains(secret_drn().as_str()),
        "DRN is attributable"
    );
    assert!(
        !serialized.contains("drn-secret-never-visible"),
        "secret leaked"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn secret_policy_denial_and_source_failure_have_distinct_audited_outcomes() {
    dekopon_broker_host::enable_test_component_memoization();
    use dekopon_capability::InvocationOutcome::{Denied, Failed};

    for (policy, resolver, outcome, error) in [
        (
            "",
            Arc::new(StaticSecretResolver(b"never-resolved")) as Arc<dyn SecretResolver>,
            Denied,
            "secret-denied",
        ),
        (
            r#"@id("caller-secret-use")
            permit(principal == Dekopon::Principal::"caller",
                   action == Dekopon::Action::"secret.use",
                   resource == Dekopon::Secret::"drn:com.xrl:secret:test:http-probe/token");"#,
            Arc::new(MissingSecretResolver),
            Failed,
            "secret-resolution",
        ),
    ] {
        let registry = BrokerProviderRegistry::load(
            [provider_fixture("http-probe-provider.wasm")],
            BrokerHostLimits::default(),
        )
        .await
        .expect("HTTP provider fixture loads");
        let audit = Arc::new(InMemoryAuditLog::new(4).expect("audit"));
        let broker = Broker::new(
            registry,
            principal("broker-test"),
            http_probe_secret_engine(&format!(
                "{}\n{policy}",
                http_policy("caller", "provider-test", "http-probe.fetch")
            )),
            catalog([(
                "http-probe.fetch",
                set("http-probe", loopback_constraints("127.0.0.1:9")),
            )]),
            CredentialStore::empty(),
            callers(["caller"]),
            Arc::clone(&audit),
            BrokerLimits::default(),
        )
        .expect("broker")
        .with_secret_catalog(
            SecretCatalog::new(
                vec![SecretUseBinding {
                    binding_id: "http-probe-token".to_owned(),
                    secret: secret_drn(),
                    capability: "http-probe.fetch".parse().expect("capability"),
                    sink: SecretSinkKind::HttpBearer,
                    basic_username: None,
                    allowed_hosts: vec!["127.0.0.1:9".to_owned()],
                    allowed_methods: vec!["GET".to_owned()],
                    allowed_paths: vec![HttpPathRule::Exact {
                        path: "/".to_owned(),
                    }],
                    allow_query: false,
                    max_injections: 1,
                }],
                resolver,
            )
            .expect("catalog"),
        )
        .expect("binding");
        let mut proposal = request(
            "invoke-secret-refused",
            "http-probe.fetch",
            json!({
                "uri": "http://127.0.0.1:9/", "method": "GET"
            }),
        );
        proposal.secret_use = Some(SecretUseProposal::HttpBearer {
            secret: secret_drn(),
        });
        let result = invoke_as(&broker, "caller", "provider-test", proposal)
            .await
            .expect("refusal audited");
        assert_eq!(result.result.outcome, outcome);
        assert_eq!(result.result.error.as_deref(), Some(error));
        let records = audit.records();
        if outcome == Denied {
            let encoded = serde_json::to_string(&records).expect("audit serializes");
            assert!(encoded.contains(secret_drn().as_str()), "{encoded}");
            assert!(encoded.contains("secret_sink"), "{encoded}");
            assert!(!encoded.contains("never-resolved"), "{encoded}");
        } else {
            assert_eq!(records.len(), 2, "decision plus terminal failed execution");
            let AuditEvent::Execution {
                error, http_calls, ..
            } = &records[1]
            else {
                panic!("terminal record is execution");
            };
            assert_eq!(error.as_deref(), Some("secret-resolution"));
            assert!(http_calls.is_empty());
        }
    }
}

/// The secret [`basic_secret_broker`] resolves; at least the 16 bytes the native sink requires.
const BASIC_SECRET: &[u8] = b"drn-secret-never-visible";

fn basic_fetch_argv(uri: &str) -> Vec<String> {
    let drn = secret_drn();
    ["fetch", "--uri", uri, "--basic", "user-a", drn.as_str()]
        .map(str::to_owned)
        .into()
}

fn basic_secret_broker(
    registry: BrokerProviderRegistry,
    authority: &str,
    username: &str,
    audit: &Arc<InMemoryAuditLog>,
) -> Broker<InMemoryAuditLog> {
    let policy = format!(
        "{}\n{}",
        http_policy("caller", "provider-test", "http-probe.fetch"),
        r#"@id("caller-basic-secret-use")
           permit(principal == Dekopon::Principal::"caller",
                  action == Dekopon::Action::"secret.use",
                  resource == Dekopon::Secret::"drn:com.xrl:secret:test:http-probe/token")
           when { context.capability == "http-probe.fetch"
               && context.provider == "http-probe"
               && context.sink == "httpBasic" };"#,
    );
    Broker::new(
        registry,
        principal("broker-test"),
        http_probe_secret_engine(&policy),
        catalog([(
            "http-probe.fetch",
            set("http-probe", loopback_constraints(authority)),
        )]),
        CredentialStore::empty(),
        callers(["caller"]),
        Arc::clone(audit),
        BrokerLimits::default(),
    )
    .expect("broker")
    .with_secret_catalog(
        SecretCatalog::new(
            vec![SecretUseBinding {
                binding_id: "http-probe-basic".to_owned(),
                secret: secret_drn(),
                capability: "http-probe.fetch".parse().expect("capability"),
                sink: SecretSinkKind::HttpBasic,
                basic_username: Some(username.to_owned()),
                allowed_hosts: vec![authority.to_owned()],
                allowed_methods: vec!["GET".to_owned()],
                allowed_paths: vec![HttpPathRule::Exact {
                    path: "/api/v1/thing".to_owned(),
                }],
                allow_query: false,
                max_injections: 1,
            }],
            Arc::new(StaticSecretResolver(BASIC_SECRET)),
        )
        .expect("secret catalog"),
    )
    .expect("binding fits capability")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_word_s_basic_proposal_needs_a_binding_for_its_exact_username() {
    dekopon_broker_host::enable_test_component_memoization();
    let server = LoopbackServer::once(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
    );
    let authority = server.authority().to_owned();
    let uri = format!("http://{authority}/api/v1/thing");
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("audit"));
    let other_user = basic_secret_broker(
        BrokerProviderRegistry::load(
            [provider_fixture("http-probe-provider.wasm")],
            BrokerHostLimits::default(),
        )
        .await
        .expect("HTTP provider fixture loads"),
        &authority,
        "user-b",
        &audit,
    );
    let bound_user = basic_secret_broker(
        BrokerProviderRegistry::load(
            [provider_fixture("http-probe-provider.wasm")],
            BrokerHostLimits::default(),
        )
        .await
        .expect("HTTP provider fixture loads"),
        &authority,
        "user-a",
        &audit,
    );

    let proposed = bound_user
        .run_command(
            &session("caller", "provider-test"),
            None,
            None,
            "httpprobe",
            &basic_fetch_argv(&uri),
            false,
        )
        .await
        .expect("the word proposes");
    assert_eq!(
        proposed,
        CommandRunOutcome::Proposed {
            capability: "http-probe.fetch".parse().expect("capability"),
            input: json!({"uri": uri}),
            secret_use: Some(SecretUseProposal::HttpBasic {
                secret: secret_drn(),
                username: "user-a".to_owned(),
            }),
        }
    );
    assert!(audit.records().is_empty(), "running a word decides nothing");
    let CommandRunOutcome::Proposed {
        capability,
        input,
        secret_use,
    } = proposed
    else {
        panic!("expected a proposal, got {proposed:?}");
    };
    assert!(secret_use.is_some(), "the proposal names its secret use");
    let submit = |id: &str| InvocationRequest {
        id: id
            .parse::<InvocationId>()
            .expect("valid invocation fixture"),
        capability: capability.clone(),
        trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
        input: input.clone(),
        secret_use: secret_use.clone(),
    };

    let refused = invoke_as(
        &other_user,
        "caller",
        "provider-test",
        submit("invoke-basic-other-user"),
    )
    .await
    .expect("denial audited");
    assert_eq!(
        refused.result.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(refused.result.error.as_deref(), Some("secret-denied"));

    let allowed = invoke_as(
        &bound_user,
        "caller",
        "provider-test",
        submit("invoke-basic-bound-user"),
    )
    .await
    .expect("dual-authorized invocation completes");
    assert_eq!(
        allowed.result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    let wire = server.request_text();
    assert!(
        wire.contains("authorization: Basic dXNlci1hOmRybi1zZWNyZXQtbmV2ZXItdmlzaWJsZQ=="),
        "{wire}"
    );
    server.join();

    let serialized = serde_json::to_string(&audit.records()).expect("audit serializes");
    assert!(
        serialized.contains(secret_drn().as_str()),
        "DRN is attributable"
    );
    for leaked in [
        "drn-secret-never-visible",
        "dXNlci1hOmRybi1zZWNyZXQtbmV2ZXItdmlzaWJsZQ",
    ] {
        assert!(!serialized.contains(leaked), "secret leaked: {leaked}");
    }
}

#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one long test scenario")]
async fn per_agent_credentials_select_only_a_named_credential_and_never_expose_it() {
    dekopon_broker_host::enable_test_component_memoization();
    const DEFAULT_SECRET: &str = "dekopon-agents-token";
    const OVERRIDE_SECRET: &str = "scientist-hq-token";
    for credential in [Some("github-pat"), None] {
        let registry = BrokerProviderRegistry::load(
            [provider_fixture("http-probe-provider.wasm")],
            BrokerHostLimits::default(),
        )
        .await
        .expect("HTTP provider fixture loads");
        let server = LoopbackServer::serving(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
        2,
    );
        let authority = server.authority().to_owned();
        let credentials = CredentialStore::new(
            [
                (
                    "github-pat".to_owned(),
                    BoundCredential::bearer(
                        "Bearer",
                        Redacted::new(DEFAULT_SECRET.to_owned()),
                        vec![authority.clone()],
                    )
                    .expect("valid credential fixture"),
                ),
                (
                    "github-pat-scientist-hq".to_owned(),
                    BoundCredential::bearer(
                        "Bearer",
                        Redacted::new(OVERRIDE_SECRET.to_owned()),
                        vec![authority.clone()],
                    )
                    .expect("valid credential fixture"),
                ),
            ]
            .into_iter()
            .filter(|(name, _)| credential.is_some() || name != "github-pat"),
        )
        .expect("credential store builds");
        let audit = Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound"));
        let broker = Broker::new(
            registry,
            principal("broker-test"),
            engine(
                &format!(
                    "{}\n{}",
                    http_policy("caller", "dekoponville-github", "http-probe.fetch"),
                    http_policy("caller", "nestedset-github", "http-probe.fetch"),
                ),
                ["caller"],
                [("http-probe.fetch", "http-probe")],
            ),
            catalog([(
                "http-probe.fetch",
                ConstraintSet {
                    default_secret_use: None,
                    route: CapabilityRoute::Generic,
                    credential: credential.map(str::to_owned),
                    ..set("http-probe", loopback_constraints(&authority))
                },
            )])
            .with_agent_credentials(rebind_for_nestedset(
                "github-pat",
                "github-pat-scientist-hq",
            )),
            credentials,
            callers(["caller"]),
            Arc::clone(&audit),
            BrokerLimits::default(),
        )
        .expect("both the default and the override match the store and its destinations");

        for (id, agent_name) in [
            ("invoke-nestedset", "nestedset-github"),
            ("invoke-dekoponville", "dekoponville-github"),
        ] {
            let result = invoke_as(
                &broker,
                "caller",
                agent_name,
                request(
                    id,
                    "http-probe.fetch",
                    json!({ "uri": format!("http://{authority}/pulls/7"), "method": "GET" }),
                ),
            )
            .await
            .expect("the authorized request is accounted");
            assert_eq!(
                result.result.outcome,
                dekopon_capability::InvocationOutcome::Succeeded
            );
            let public = serde_json::to_string(&result.result).expect("result serializes");
            for secret in [DEFAULT_SECRET, OVERRIDE_SECRET] {
                assert!(!public.contains(secret), "result leaked the secret");
            }
        }

        let wire = || server.request_text();
        for (index, (present, absent)) in [
            (OVERRIDE_SECRET, DEFAULT_SECRET),
            (DEFAULT_SECRET, OVERRIDE_SECRET),
        ]
        .into_iter()
        .enumerate()
        {
            let request = wire();
            if credential.is_some() {
                assert!(
                    request.contains(&format!("authorization: Bearer {present}")),
                    "request {index} presented the wrong credential: {request}"
                );
                assert!(
                    !request.contains(absent),
                    "request {index} leaked the other organization's token: {request}"
                );
            } else {
                assert!(
                    !request.to_ascii_lowercase().contains("authorization"),
                    "request {index} carried a credential its set never named: {request}"
                );
            }
        }
        server.join();

        let records = audit.records();
        let encoded = serde_json::to_value(&records).expect("audit serializes");
        let selected = encoded
            .as_array()
            .expect("records serialize as an array")
            .iter()
            .filter_map(|record| record["credential"].as_str())
            .collect::<Vec<_>>();
        let serialized = serde_json::to_string(&records).expect("audit serializes");
        if credential.is_some() {
            assert_eq!(
                selected,
                ["github-pat-scientist-hq", "github-pat"],
                "each terminal record names the credential its own invocation selected"
            );
            assert!(
                serialized.contains("\"credentialInjected\":true"),
                "{serialized}"
            );
        } else {
            assert!(
                !serialized.contains("\"credential\""),
                "an invocation with no credential names none: {serialized}"
            );
        }
        for secret in [DEFAULT_SECRET, OVERRIDE_SECRET] {
            assert!(!serialized.contains(secret), "audit leaked a secret");
        }
        assert!(!serialized.contains("Bearer"), "audit leaked the scheme");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn credentialed_constraint_sets_fail_closed_at_construction() {
    dekopon_broker_host::enable_test_component_memoization();
    let http = |hosts: Vec<String>| ExecutionConstraints {
        asset: None,
        timeout_ms: 5_000,
        http: Some(HttpConstraints {
            allowed_hosts: hosts,
            propagate_trace: false,
            allowed_methods: vec!["GET".to_owned()],
            max_requests: 1,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 64 * 1024,
            allow_plaintext_loopback: false,
            request_templates: Vec::new(),
        }),
        storage: None,
        secret_use: None,
    };
    let store = || {
        CredentialStore::new([
            (
                "fetch-token".to_owned(),
                BoundCredential::bearer(
                    "Bearer",
                    Redacted::new("fetch-token-secret-value".to_owned()),
                    vec!["api.example.test".to_owned()],
                )
                .expect("valid credential fixture"),
            ),
            (
                "other-token".to_owned(),
                BoundCredential::bearer(
                    "Bearer",
                    Redacted::new("other-token-secret-value".to_owned()),
                    vec!["other.example.test".to_owned()],
                )
                .expect("valid credential fixture"),
            ),
        ])
        .expect("credential store builds")
    };
    let build_set = |set: ConstraintSet, store: CredentialStore, rebound: Option<String>| {
        let registry_limits = BrokerHostLimits::default();
        let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
        async move {
            let registry = BrokerProviderRegistry::load(
                [provider_fixture("http-probe-provider.wasm")],
                registry_limits,
            )
            .await
            .expect("HTTP provider fixture loads");
            Broker::new(
                registry,
                "broker-test"
                    .parse::<PrincipalId>()
                    .expect("valid broker principal"),
                http_probe_engine(""),
                catalog([("http-probe.fetch", set)]).with_agent_credentials(
                    rebound.map_or_else(BTreeMap::new, |credential| {
                        rebind_for_nestedset("fetch-token", &credential)
                    }),
                ),
                store,
                IdentityDirectory::empty(),
                audit,
                BrokerLimits::default(),
            )
        }
    };
    let build = |credential: String, constraints: ExecutionConstraints, store: CredentialStore| {
        build_set(
            ConstraintSet {
                default_secret_use: None,
                route: CapabilityRoute::Generic,
                credential: Some(credential),
                ..set("http-probe", constraints)
            },
            store,
            None,
        )
    };
    let build_override = |credential: String, constraints: ExecutionConstraints| {
        build_set(
            ConstraintSet {
                default_secret_use: None,
                route: CapabilityRoute::Generic,
                credential: Some("fetch-token".to_owned()),
                ..set("http-probe", constraints)
            },
            store(),
            Some(credential),
        )
    };

    let error = build(
        "missing-token".to_owned(),
        http(vec!["api.example.test".to_owned()]),
        store(),
    )
    .await
    .expect_err("unknown credential names are refused");
    assert!(matches!(error, BrokerBuildError::UnknownCredential { .. }));

    let error = build_override(
        "missing-token".to_owned(),
        http(vec!["api.example.test".to_owned()]),
    )
    .await
    .expect_err("an override naming an unknown credential is refused");
    assert!(matches!(
        error,
        BrokerBuildError::UnknownCredential { name, .. } if name == "missing-token"
    ));

    let error = build(
        "fetch-token".to_owned(),
        ExecutionConstraints::default(),
        store(),
    )
    .await
    .expect_err("credentialed constraint sets require HTTP authority");
    assert!(matches!(
        error,
        BrokerBuildError::CredentialWithoutHttp { .. }
    ));

    let error = build(
        "fetch-token".to_owned(),
        http(vec![
            "api.example.test".to_owned(),
            "other.example.test".to_owned(),
        ]),
        store(),
    )
    .await
    .expect_err("allowed hosts outside the binding are refused");
    assert!(matches!(
        error,
        BrokerBuildError::CredentialDestinationMismatch { host, .. } if host == "other.example.test"
    ));

    let error = build_override(
        "other-token".to_owned(),
        http(vec!["api.example.test".to_owned()]),
    )
    .await
    .expect_err("an override that does not cover the set's hosts is refused");
    assert!(matches!(
        error,
        BrokerBuildError::CredentialDestinationMismatch { name, host, .. }
            if name == "other-token" && host == "api.example.test"
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_direct_peer_is_denied_every_capability_and_attested_sessions_follow_policy() {
    use dekopon_test_support::CaptureLayer;
    use tracing::instrument::WithSubscriber as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let audit = Arc::new(InMemoryAuditLog::new(16).expect("valid audit bound"));
    let broker = Broker::new(
        probe_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        probe_engine(
            &format!(
                "{}\n{}\n{}",
                r#"@id("caller-unconstrained") permit(principal == Dekopon::Principal::"caller", action, resource);"#,
                attested_policy("cpetersen", "some-agent", GATEWAY, "cli-probe.upper"),
                agent_prompt_policy("cpetersen", "some-agent", GATEWAY),
            ),
            ["caller", "cpetersen", GATEWAY],
        ),
        catalog([
            (
                "cli-probe.upper",
                set("cli-probe", ExecutionConstraints::default()),
            ),
            (
                "cli-probe.reverse",
                set("cli-probe", ExecutionConstraints::default()),
            ),
        ]),
        CredentialStore::empty(),
        directory([(SLACK_SUBJECT, "cpetersen")]),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("an unconditional grant and an attested grant coexist in one policy set");

    let gateway = service_context(GATEWAY);
    let grant = attestor_grant(["slack.t0123abc"]);
    let subject = subject(SLACK_SUBJECT);

    let (attested_stdout, mut attested_reader) = std::os::unix::net::UnixStream::pair().unwrap();
    let attested_capture = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut attested_reader, &mut bytes).unwrap();
        bytes
    });
    let attested = broker
        .invoke(
            &gateway,
            Some(&grant),
            Some(&attestation(&subject, "some-agent", "invoke-attested")),
            request(
                "invoke-attested",
                "cli-probe.upper",
                json!({"text": "on behalf of"}),
            ),
            dekopon_broker_host::asset::AssetInputs {
                streams: Some(dekopon_broker_host::Streams {
                    stdin: None,
                    stdout: attested_stdout.into(),
                }),
                ..Default::default()
            },
        )
        .await
        .expect("attested invocation is accounted");
    assert_eq!(
        attested_capture.join().unwrap(),
        b"{\"text\":\"ON BEHALF OF\"}\n"
    );
    assert_eq!(
        attested.result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );

    let crossed = broker
        .invoke(
            &gateway,
            Some(&grant),
            Some(&attestation(&subject, "some-agent", "invoke-crossed")),
            request(
                "invoke-crossed",
                "cli-probe.reverse",
                json!({"text": "crossed"}),
            ),
            Default::default(),
        )
        .await
        .expect("attested proposal outside the attested rule is accounted");
    assert_eq!(
        crossed.result.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(crossed.result.error.as_deref(), Some("policy-denied"));

    let visible = broker
        .capability_surface(
            &gateway,
            Some(&grant),
            Some(&Attestation::for_subject(
                subject.clone(),
                agent("some-agent"),
            )),
        )
        .expect("the attestation is honored")
        .0;
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].capability.id.as_str(), "cli-probe.upper");

    for (id, peer) in [
        (
            "invoke-direct-granted",
            direct_peer("caller", "provider-test"),
        ),
        ("invoke-direct-service", service_context("caller")),
        (
            "invoke-direct-as-mapped",
            direct_peer("cpetersen", "some-agent"),
        ),
    ] {
        let capture = CaptureLayer::new();
        let direct = broker
            .invoke(
                &peer,
                None,
                None,
                request(id, "cli-probe.upper", json!({"text": "direct"})),
                Default::default(),
            )
            .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
            .await
            .expect("direct proposal is accounted");
        let decisions = capture.events_where(
            "dekopon_broker::audit",
            &[("audit.event", "broker.decision"), ("invocation.id", id)],
        );
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].field("decision.reason"), Some("policy-error"));
        assert_eq!(decisions[0].field("policy.errors_present"), None);
        assert_eq!(
            direct.result.outcome,
            dekopon_capability::InvocationOutcome::Denied,
            "{id}"
        );
        assert_eq!(direct.result.error.as_deref(), Some("policy-error"), "{id}");
        assert_eq!(
            broker.capability_view(&peer),
            (Vec::new(), Vec::new(), BTreeMap::new()),
            "{id}"
        );
    }
    assert!(
        broker.capabilities(&gateway).is_empty(),
        "attestor authority is not capability: the gateway holds nothing of its own"
    );

    let records = audit.records();
    assert_eq!(records.len(), 6, "one allow plus execution, four denials");
}

#[tokio::test(flavor = "multi_thread")]
async fn attestation_refusals_are_audited_denials_under_the_peer() {
    let gateway = service_context("gateway");
    let subject = subject(SLACK_SUBJECT);

    let audit = Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound"));
    let broker = attested_broker(
        directory([(SLACK_SUBJECT, "cpetersen")]),
        Arc::clone(&audit),
    )
    .await;

    for (id, grant) in [
        ("invoke-no-grant", None),
        (
            "invoke-out-of-scope",
            Some(attestor_grant(["slack.t0999zzz"])),
        ),
    ] {
        let result = broker
            .invoke(
                &gateway,
                grant.as_ref(),
                Some(&attestation(&subject, "some-agent", id)),
                request(id, "cli-probe.upper", json!({"text": "claim"})),
                Default::default(),
            )
            .await
            .expect("a refused attestation is accounted");
        assert_eq!(
            result.result.outcome,
            dekopon_capability::InvocationOutcome::Denied
        );
        assert_eq!(result.result.error.as_deref(), Some("attestation-denied"));
    }

    let records = audit.records();
    assert_eq!(records.len(), 2);
    for record in &records {
        let AuditEvent::Decision {
            principal,
            actor,
            via,
            attested_subject,
            allowed,
            reason,
            ..
        } = record
        else {
            panic!("a refusal records a decision event");
        };
        assert_eq!(principal.as_ref().map(PrincipalId::as_str), Some("gateway"));
        assert_eq!(
            actor,
            &Some(Actor::Service {
                principal: crate::principal("gateway")
            })
        );
        assert!(
            via.is_none(),
            "a refusal derived no attested context, so there is no `via` to record"
        );
        assert_eq!(
            attested_subject.as_ref().map(ExternalSubject::canonical),
            Some(SLACK_SUBJECT.to_owned())
        );
        assert!(!allowed);
        assert_eq!(reason.as_deref(), Some("attestation-denied"));
    }

    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let broker = attested_broker(IdentityDirectory::empty(), Arc::clone(&audit)).await;
    let unmapped = broker
        .invoke(
            &gateway,
            Some(&attestor_grant(["slack.t0123abc"])),
            Some(&attestation(&subject, "some-agent", "invoke-unmapped")),
            request(
                "invoke-unmapped",
                "cli-probe.upper",
                json!({"text": "claim"}),
            ),
            Default::default(),
        )
        .await
        .expect("an unmapped subject is accounted");
    assert_eq!(
        unmapped.result.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(unmapped.result.error.as_deref(), Some("unmapped-subject"));
    let records = audit.records();
    assert_eq!(records.len(), 1);
    let AuditEvent::Decision {
        principal,
        attested_subject,
        reason,
        ..
    } = &records[0]
    else {
        panic!("a refusal records a decision event");
    };
    assert_eq!(principal.as_ref().map(PrincipalId::as_str), Some("gateway"));
    assert_eq!(
        attested_subject.as_ref().map(ExternalSubject::canonical),
        Some(SLACK_SUBJECT.to_owned())
    );
    assert_eq!(reason.as_deref(), Some("unmapped-subject"));
}

#[tokio::test(flavor = "multi_thread")]
async fn attested_success_audits_via_and_subject() {
    let (host, mut stdout) = std::os::unix::net::UnixStream::pair().unwrap();
    let capture = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut bytes).unwrap();
        bytes
    });
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound"));
    let broker = attested_broker(
        directory([(SLACK_SUBJECT, "cpetersen")]),
        Arc::clone(&audit),
    )
    .await;
    let result = broker
        .invoke(
            &service_context("gateway"),
            Some(&attestor_grant(["slack.t0123abc"])),
            Some(&attestation(
                &subject(SLACK_SUBJECT),
                "some-agent",
                "invoke-attested-audit",
            )),
            request(
                "invoke-attested-audit",
                "cli-probe.upper",
                json!({"text": "top-secret-payload"}),
            ),
            dekopon_broker_host::asset::AssetInputs {
                streams: Some(dekopon_broker_host::Streams {
                    stdin: None,
                    stdout: host.into(),
                }),
                ..Default::default()
            },
        )
        .await
        .expect("attested invocation is accounted");
    assert_eq!(
        capture.join().unwrap(),
        b"{\"text\":\"TOP-SECRET-PAYLOAD\"}\n"
    );
    assert_eq!(
        result.result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );

    let records = audit.records();
    assert_eq!(records.len(), 2);
    let encoded = serde_json::to_value(&records).expect("audit serializes");
    for (index, kind) in [(0, "decision"), (1, "execution")] {
        let event = &encoded[index];
        assert_eq!(event["type"], kind);
        assert_eq!(event["principal"], "cpetersen");
        assert_eq!(event["via"], "gateway");
        assert_eq!(event["attested_subject"], SLACK_SUBJECT);
        assert_eq!(
            event["actor"],
            json!({"type": "agent", "agent": "some-agent"})
        );
    }
    assert_eq!(encoded[0]["allowed"], true);

    let serialized = serde_json::to_string(&records).expect("audit serializes");
    assert!(
        !serialized.contains("top-secret-payload"),
        "a subject is routing metadata; the message it arrived with is not audited"
    );
    assert!(!serialized.contains("TOP-SECRET-PAYLOAD"));
}

fn smoke_claim() -> Attestation {
    Attestation::for_chat(
        subject(SLACK_SUBJECT),
        agent("some-agent"),
        dekopon_broker::ChatScopeClaim {
            transport: "console-smoke".parse().expect("transport"),
            kind: dekopon_broker::ChatTransportKind::Local,
            conversation: dekopon_broker::Conversation {
                kind: dekopon_broker::ConversationKind::DirectMessage,
                container: None,
                id: "console-smoke".to_owned(),
                thread: None,
            },
            delivery: None,
            trigger: dekopon_broker::Trigger::Message,
        },
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_literal_smoke_claim_passes_every_ordinary_attestation_entry() {
    let audit = Arc::new(InMemoryAuditLog::new(64).expect("audit"));
    let broker = attested_broker(
        directory([(SLACK_SUBJECT, "cpetersen")]),
        Arc::clone(&audit),
    )
    .await;
    let peer = service_context("gateway");
    let grant = attestor_grant(["slack.t0123abc"]);
    let literal = smoke_claim();
    assert_eq!(
        broker
            .capability_surface(&peer, Some(&grant), Some(&literal))
            .expect("granted peer may claim smoke")
            .0
            .len(),
        1
    );
    assert!(
        broker
            .capability_surface(&peer, None, Some(&literal))
            .is_none()
    );
    let help = broker
        .run_command(
            &peer,
            Some(&grant),
            Some(&literal),
            "probe",
            &["--help".to_owned()],
            false,
        )
        .await
        .expect("the literal may run the fixture's real help command");
    assert!(matches!(
        help,
        CommandRunOutcome::Rendered { status: 0, stdout, stderr }
            if stdout.starts_with("Usage: probe <COMMAND>") && stderr.is_empty()
    ));

    let mut deviations = Vec::new();
    let mut wrong = literal.clone();
    wrong.scope.as_mut().expect("scope").kind = dekopon_broker::ChatTransportKind::Slack;
    deviations.push(wrong);
    let mut wrong = literal.clone();
    wrong.scope.as_mut().expect("scope").conversation.kind =
        dekopon_broker::ConversationKind::Thread;
    deviations.push(wrong);
    let mut wrong = literal.clone();
    wrong.scope.as_mut().expect("scope").conversation.container = Some("other".to_owned());
    deviations.push(wrong);
    let mut wrong = literal.clone();
    wrong.scope.as_mut().expect("scope").conversation.id = "other".to_owned();
    deviations.push(wrong);
    let mut wrong = literal.clone();
    wrong.scope.as_mut().expect("scope").conversation.thread = Some("other".to_owned());
    deviations.push(wrong);
    let mut wrong = literal.clone();
    wrong.scope.as_mut().expect("scope").trigger = dekopon_broker::Trigger::Probe;
    deviations.push(wrong);

    for (index, claim) in deviations.iter().enumerate() {
        assert!(
            broker
                .capability_surface(&peer, Some(&grant), Some(claim))
                .is_none()
        );
        assert!(matches!(
            broker
                .run_command(
                    &peer,
                    Some(&grant),
                    Some(claim),
                    "probe",
                    &["--help".to_owned()],
                    false,
                )
                .await,
            Err(dekopon_broker_host::BrokerHostError::UnknownCommandWord { word })
                if word == "probe"
        ));
        let id = format!("smoke-refused-{index}")
            .parse::<InvocationId>()
            .expect("id");
        let result = broker
            .invoke(
                &peer,
                Some(&grant),
                Some(&claim.bound_to(id.clone())),
                request(
                    &format!("smoke-refused-{index}"),
                    "cli-probe.upper",
                    json!({"text": "untrusted"}),
                ),
                Default::default(),
            )
            .await
            .expect("denial accounted");
        assert_eq!(
            result.result.outcome,
            dekopon_capability::InvocationOutcome::Denied
        );
        assert_eq!(
            result.result.error.as_deref(),
            Some("chat-attestation-denied")
        );
    }
    assert_eq!(
        audit.records().iter().filter(|event| matches!(event,
            AuditEvent::Decision { reason: Some(reason), .. } if reason == "console-smoke-claim-denied"
        )).count(),
        deviations.len()
    );
}

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "one peer exercises four refusal routes and mapped/unmapped subjects"
)]
async fn authenticated_console_cannot_attest_a_real_scope() {
    let audit = Arc::new(InMemoryAuditLog::new(64).expect("audit"));
    let broker = attested_broker(
        directory([(SLACK_SUBJECT, "cpetersen")]),
        Arc::clone(&audit),
    )
    .await;
    let console = service_context("dekopon-console");
    let gateway = service_context("gateway");
    let grant = attestor_grant(["slack.t0123abc"]);
    let mut real = smoke_claim();
    let scope = real.scope.as_mut().expect("chat scope");
    scope.transport = "scientist-slack".parse().expect("transport");
    scope.kind = dekopon_broker::ChatTransportKind::Slack;
    scope.conversation.kind = dekopon_broker::ConversationKind::Thread;
    scope.conversation.container = Some("t0123abc".to_owned());
    scope.conversation.id = "c0123abc".to_owned();
    scope.conversation.thread = Some("1712345678.000100".to_owned());
    let refused_capabilities = broker
        .capability_surface(&console, Some(&grant), Some(&real))
        .is_none();
    assert!(
        broker
            .capability_surface(&gateway, Some(&grant), Some(&real))
            .is_some()
    );
    assert!(
        broker
            .capability_surface(&console, Some(&grant), Some(&smoke_claim()))
            .is_some()
    );
    assert!(
        broker
            .capability_surface(
                &console,
                Some(&grant),
                Some(&Attestation::for_subject(
                    subject(SLACK_SUBJECT),
                    agent("some-agent")
                ))
            )
            .is_some()
    );
    assert!(
        audit.records().is_empty(),
        "inspection must not evaluate a capability"
    );

    let CommandRunOutcome::Rendered { stderr, status, .. } = broker
        .run_command(
            &console,
            Some(&grant),
            Some(&real),
            "probe",
            &["--help".to_owned()],
            false,
        )
        .await
        .expect("known console denial is rendered")
    else {
        panic!("rendered denial");
    };
    let refused_run_command = status == 2 && stderr.starts_with("dekopon sandbox:");
    let unknown = Attestation::for_chat(
        subject("slack.t0123abc.unmapped"),
        agent("some-agent"),
        real.scope.clone().expect("scope"),
    );
    assert!(
        broker
            .capability_surface(&console, Some(&grant), Some(&unknown))
            .is_none()
    );
    let CommandRunOutcome::Rendered {
        stderr: unmapped_stderr,
        status: unmapped_status,
        ..
    } = broker
        .run_command(&console, Some(&grant), Some(&unknown), "probe", &[], false)
        .await
        .expect("the unmapped subject receives the same console rule")
    else {
        panic!("console denial is rendered");
    };
    assert_eq!(unmapped_status, status);
    assert_eq!(unmapped_stderr, stderr);

    let id = "console-real-invoke".parse::<InvocationId>().expect("id");
    let result = broker
        .invoke(
            &console,
            Some(&grant),
            Some(&real.clone().bound_to(id)),
            request(
                "console-real-invoke",
                "cli-probe.upper",
                json!({"text": "no execution"}),
            ),
            Default::default(),
        )
        .await
        .expect("denial is accounted");
    let refused_invoke = result.result.outcome == dekopon_capability::InvocationOutcome::Denied
        && result.result.error.as_deref() == Some(dekopon_broker::CONSOLE_REAL_SCOPE_REFUSAL);
    let unmapped_invoke = broker
        .invoke(
            &console,
            Some(&grant),
            Some(
                &unknown
                    .clone()
                    .bound_to("console-unmapped-invoke".parse().expect("id")),
            ),
            request(
                "console-unmapped-invoke",
                "cli-probe.upper",
                json!({"text": "no execution"}),
            ),
            Default::default(),
        )
        .await
        .expect("unmapped denial is accounted");
    assert_eq!(unmapped_invoke.result.error, result.result.error);

    let turn = dekopon_broker_protocol::DeliveredTurnRequest::new(
        "console-real-record".parse().expect("id"),
        TRACE_PARENT.parse().expect("trace"),
        dekopon_broker_protocol::DeliveryIdentity::Local {
            transport: "local-dev".parse().expect("transport"),
            conversation: "placeholder".to_owned(),
            boot_nonce: "0123456789abcdef0123456789abcdef".to_owned(),
            connection: 1,
            sequence: 1,
        },
        "synthetic".to_owned(),
        dekopon_broker_protocol::DeliveredAnswer::accepted_by_transport("answer".to_owned()),
    );
    let unmapped_turn = turn.clone();
    let result = broker
        .record_delivered_turn(
            &console,
            Some(&grant),
            &real.bound_to("console-real-record".parse().expect("id")),
            turn,
        )
        .await
        .expect("record refusal accounted");
    let refused_record = result.outcome == dekopon_capability::InvocationOutcome::Denied
        && result.error.as_deref() == Some(dekopon_broker::CONSOLE_REAL_SCOPE_REFUSAL);
    let unmapped_record = broker
        .record_delivered_turn(
            &console,
            Some(&grant),
            &unknown.bound_to("console-real-record".parse().expect("id")),
            unmapped_turn,
        )
        .await
        .expect("unmapped record denial is accounted");
    assert_eq!(unmapped_record.error, result.error);
    assert!(
        refused_capabilities && refused_run_command && refused_invoke && refused_record,
        "console boundary (capabilities, run-command, invoke, record): \
         {refused_capabilities}, {refused_run_command}, {refused_invoke}, {refused_record}"
    );
    assert_eq!(
        audit.records().len(),
        4,
        "denials audit decisions but never execute"
    );
    for event in audit.records() {
        let AuditEvent::Decision { reason, .. } = event else {
            panic!("denial only audits a decision");
        };
        assert_eq!(reason.as_deref(), Some("console-real-scope-denied"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn attested_capabilities_distinguishes_refusal_from_empty() {
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let broker = attested_broker(
        directory([(SLACK_SUBJECT, "cpetersen"), ("tel.16034700182", "oncall")]),
        audit,
    )
    .await;
    let gateway = service_context("gateway");
    let grant = attestor_grant(["slack.t0123abc", "tel"]);
    let mapped = subject(SLACK_SUBJECT);

    assert!(
        broker
            .capability_surface(
                &gateway,
                None,
                Some(&Attestation::for_subject(
                    mapped.clone(),
                    agent("some-agent"),
                )),
            )
            .is_none(),
        "a peer with no attestor authority learns nothing at all"
    );

    let granted = broker
        .capability_surface(
            &gateway,
            Some(&grant),
            Some(&Attestation::for_subject(
                mapped.clone(),
                agent("some-agent"),
            )),
        )
        .expect("the attestation is honored")
        .0;
    assert_eq!(granted.len(), 1);
    assert_eq!(granted[0].capability.id.as_str(), "cli-probe.upper");

    let bare = broker
        .capability_surface(
            &gateway,
            Some(&grant),
            Some(&Attestation::for_subject(
                subject("tel.16034700182"),
                agent("some-agent"),
            )),
        )
        .expect("the attestation is honored for every namespace in the grant")
        .0;
    assert!(bare.is_empty());
}

#[test]
fn attestor_scopes_match_on_segment_boundaries() {
    let grant = attestor_grant(["slack.t0123abc"]);
    grant
        .validate()
        .expect("a service plus one canonical segment is a valid scope");
    assert!(grant.permits(&ExternalSubject::slack("T0123ABC", "U9XYZ").expect("slack subject")));
    assert!(!grant.permits(&ExternalSubject::slack("T0123ABCX", "U9").expect("slack subject")));

    for invalid in [
        vec![],
        vec!["sms".to_owned()],
        vec!["slack.T0123ABC".to_owned()],
        vec!["slack..u9xyz".to_owned()],
        vec!["slack.t0123abc.u9xyz.extra".to_owned()],
    ] {
        let grant = AttestorGrant {
            namespaces: Some(invalid.clone()),
        };
        assert!(
            grant.validate().is_err(),
            "accepted attestor namespaces {invalid:?}"
        );
    }
}

#[test]
fn github_attestor_scopes_accept_canonical_owner_and_repo_namespaces() {
    let owner = attestor_grant(["github.dekopon-agents"]);
    owner.validate().expect("canonical GitHub owner scope");
    let repo = attestor_grant(["github.dekopon-agents.my_repo"]);
    repo.validate().expect("canonical GitHub repo scope");
    let matched = ExternalSubject::github("dekopon-agents", "my_repo").unwrap();
    assert!(owner.permits(&matched));
    assert!(repo.permits(&matched));
    assert!(!repo.permits(&ExternalSubject::github("dekopon-agents", "my_repo-extra").unwrap()));
    for scope in [
        "github.Owner".to_owned(),
        format!("github.{}", "o".repeat(40)),
    ] {
        let error = attestor_grant([scope.as_str()])
            .validate()
            .expect_err("noncanonical owner scope");
        assert!(
            matches!(error, BrokerBuildError::InvalidAttestorScope { scope: rejected } if rejected == scope)
        );
    }
}

#[test]
fn identity_directory_rejects_duplicates_and_resolves_exactly() {
    let slack = subject(SLACK_SUBJECT);
    let telephone = subject("tel.16034700182");
    let resolved = IdentityDirectory::new([
        (slack.clone(), principal("cpetersen")),
        (telephone.clone(), principal("oncall")),
    ])
    .expect("distinct subjects build a directory");
    assert_eq!(resolved.resolve(&slack), Some(&principal("cpetersen")));
    assert_eq!(resolved.resolve(&telephone), Some(&principal("oncall")));
    assert!(
        resolved.resolve(&subject("slack.t0123abc.u0000")).is_none(),
        "an unmapped subject in a mapped workspace resolves to nothing"
    );
    assert!(IdentityDirectory::empty().resolve(&slack).is_none());

    let error = IdentityDirectory::new([
        (slack.clone(), principal("cpetersen")),
        (slack, principal("someone-else")),
    ])
    .expect_err("one subject must not name two principals");
    assert!(matches!(
        error,
        BrokerBuildError::DuplicateSubjectMapping { subject } if subject == SLACK_SUBJECT
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_capability_without_a_constraint_set_fails_closed_at_both_layers() {
    let error = Broker::new(
        probe_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        probe_engine(
            &probe_policy("caller", "provider-test", "cli-probe.reverse"),
            ["caller"],
        ),
        catalog([(
            "cli-probe.upper",
            set("cli-probe", ExecutionConstraints::default()),
        )]),
        CredentialStore::empty(),
        callers(["caller"]),
        Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound")),
        BrokerLimits::default(),
    )
    .expect_err("a grant nothing knows how to execute must refuse startup");
    assert!(matches!(
        error,
        BrokerBuildError::UnconstrainedCapability { capability } if capability.as_str() == "cli-probe.reverse"
    ));

    let unscoped = r#"@id("caller-unconstrained")
        permit(principal == Dekopon::Principal::"caller", action, resource);"#;
    for (leniency, policy) in [
        (Leniency::Strict, unscoped.to_owned()),
        (
            Leniency::Tolerant,
            probe_policy("caller", "provider-test", "cli-probe.reverse"),
        ),
    ] {
        let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
        let (broker, warnings) = Broker::start(
            probe_registry(BrokerHostLimits::default()).await,
            principal("broker-test"),
            probe_engine(&policy, ["caller"]),
            catalog([(
                "cli-probe.upper",
                set("cli-probe", ExecutionConstraints::default()),
            )]),
            CredentialStore::empty(),
            callers(["caller"]),
            Arc::clone(&audit),
            BrokerLimits::default(),
            leniency,
        )
        .expect("an unnamed or tolerated unexecutable grant starts");
        if leniency == Leniency::Tolerant {
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            assert!(matches!(
                &warnings[0],
                StartupWarning::UnconstrainedCapability { capability }
                    if capability.as_str() == "cli-probe.reverse"
            ));
        }
        let result = invoke_as(
            &broker,
            "caller",
            "provider-test",
            request(
                "invoke-unconstrained",
                "cli-probe.reverse",
                json!({"text": "x"}),
            ),
        )
        .await
        .expect("the refusal is accounted");
        assert_eq!(
            result.result.outcome,
            dekopon_capability::InvocationOutcome::Denied
        );
        assert_eq!(
            result.result.error.as_deref(),
            Some("unconstrained-capability")
        );
        assert!(
            broker
                .capabilities(&session("caller", "provider-test"))
                .iter()
                .all(|available| available.capability.id.as_str() == "cli-probe.upper"),
            "a capability with no constraint set is never listed"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn audit_records_carry_determining_policy_ids_and_the_policy_digest() {
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound"));
    let broker = Broker::new(
        probe_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        probe_engine(
            &format!(
                "{}\n{}",
                probe_policy("caller", "provider-test", "cli-probe.upper"),
                agent_prompt_policy("other-caller", "provider-test", GATEWAY),
            ),
            ["caller", "other-caller"],
        ),
        catalog([(
            "cli-probe.upper",
            set("cli-probe", ExecutionConstraints::default()),
        )]),
        CredentialStore::empty(),
        callers(["caller", "other-caller"]),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("the named policy validates");
    let digest = broker.policy_digest().to_owned();
    assert!(digest.starts_with("sha256:"));

    let result = invoke_as(
        &broker,
        "caller",
        "provider-test",
        request("invoke-explained", "cli-probe.upper", json!({"text": "hi"})),
    )
    .await
    .expect("the allowed invocation is accounted");
    assert_eq!(result.result.decision.decision_id, "allow-invoke-explained");
    assert_eq!(
        result.result.decision.policy_digest,
        digest["sha256:".len()..]
    );
    let result = invoke_as(
        &broker,
        "other-caller",
        "provider-test",
        request(
            "invoke-unexplained",
            "cli-probe.upper",
            json!({"text": "hi"}),
        ),
    )
    .await
    .expect("the denial is accounted");
    assert_eq!(
        result.result.decision.decision_id,
        "deny-invoke-unexplained"
    );
    assert_eq!(
        result.result.decision.policy_digest,
        digest["sha256:".len()..]
    );

    let records = audit.records();
    let encoded = serde_json::to_value(&records).expect("audit serializes");
    for index in [0, 1] {
        assert_eq!(
            encoded[index]["policy_ids"],
            json!(["caller-cli-probe.upper-via-provider-test"])
        );
        assert_eq!(encoded[index]["policy_digest"], json!(digest));
    }
    assert_eq!(encoded[2]["reason"], "policy-denied");
    assert!(encoded[2].get("policy_ids").is_none());
    assert_eq!(encoded[2]["policy_digest"], json!(digest));
}

#[tokio::test(flavor = "multi_thread")]
async fn tolerating_a_constraint_set_that_routes_nowhere_drops_it() {
    let unrouted = [
        (
            "cli-probe.upper",
            set("cli-probe", ExecutionConstraints::default()),
        ),
        (
            "gh.pull-request.read",
            set("gh", ExecutionConstraints::default()),
        ),
    ];

    let error = Broker::new(
        probe_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        probe_engine(
            &probe_policy("caller", "provider-test", "cli-probe.upper"),
            ["caller"],
        ),
        catalog(unrouted.clone()),
        CredentialStore::empty(),
        callers(["caller"]),
        Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound")),
        BrokerLimits::default(),
    )
    .expect_err("strict startup refuses a set naming no loaded route");
    assert!(matches!(
        error,
        BrokerBuildError::UnknownCapability { capability }
            if capability.as_str() == "gh.pull-request.read"
    ));

    let (broker, warnings) = Broker::start(
        probe_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        probe_engine(
            &probe_policy("caller", "provider-test", "cli-probe.upper"),
            ["caller"],
        ),
        catalog(unrouted),
        CredentialStore::empty(),
        callers(["caller"]),
        Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound")),
        BrokerLimits::default(),
        Leniency::Tolerant,
    )
    .expect("tolerating an unrouted constraint set starts");

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(matches!(
        &warnings[0],
        StartupWarning::UnroutedConstraintSet { capability }
            if capability.as_str() == "gh.pull-request.read"
    ));
    assert_eq!(warnings[0].reason(), "unrouted-constraint-set");

    let result = invoke_as(
        &broker,
        "caller",
        "provider-test",
        request("invoke-routed", "cli-probe.upper", json!({"text": "x"})),
    )
    .await
    .expect("the routed capability still executes");
    assert_eq!(
        result.result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn command_words_are_filtered_by_policy_and_unknown_words_are_refused() {
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let (broker, _) = Broker::start(
        probe_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        probe_engine(
            &probe_policy("caller", "provider-test", "cli-probe.upper"),
            ["caller", "stranger"],
        ),
        catalog([(
            "cli-probe.upper",
            set("cli-probe", ExecutionConstraints::default()),
        )]),
        CredentialStore::empty(),
        callers(["caller"]),
        Arc::clone(&audit),
        BrokerLimits::default(),
        Leniency::Strict,
    )
    .expect("broker starts");

    assert_eq!(
        broker.command_words(&session("caller", "provider-test")),
        ["probe"]
    );
    assert!(
        broker
            .command_words(&session("stranger", "provider-test"))
            .is_empty()
    );
    assert_eq!(
        broker
            .capabilities(&session("caller", "provider-test"))
            .len(),
        1
    );
    assert!(
        broker
            .capabilities(&session("stranger", "provider-test"))
            .is_empty(),
        "the ungranted context reaches nothing, which is what makes its empty vocabulary meaningful"
    );

    let caller_help = broker.command_word_help(&session("caller", "provider-test"));
    let probe_page = caller_help.get("probe").expect("caller reaches probe");
    assert!(
        probe_page.starts_with("Usage: probe <COMMAND>"),
        "{probe_page}"
    );
    assert_eq!(
        broker.command_word_help(&session("stranger", "provider-test")),
        BTreeMap::new(),
        "an ungranted context reaches no word, so it reaches no word's help either"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_command_word_is_refused_without_running_anything() {
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let (broker, _) = Broker::start(
        probe_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        probe_engine(
            &probe_policy("caller", "provider-test", "cli-probe.upper"),
            ["caller"],
        ),
        catalog([(
            "cli-probe.upper",
            set("cli-probe", ExecutionConstraints::default()),
        )]),
        CredentialStore::empty(),
        callers(["caller"]),
        Arc::clone(&audit),
        BrokerLimits::default(),
        Leniency::Strict,
    )
    .expect("broker starts");

    let error = broker
        .run_command(
            &session("caller", "provider-test"),
            None,
            None,
            "gh",
            &["gh".to_owned(), "pr".to_owned()],
            false,
        )
        .await
        .expect_err("no loaded provider declares this word");
    assert!(
        format!("{error}").contains("gh"),
        "the refusal names the word: {error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
async fn a_command_word_renders_help_and_reads_the_piped_value_through_the_broker() {
    dekopon_broker_host::enable_test_component_memoization();
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("cli-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("cli-probe fixture loads");
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let (broker, _) = Broker::start(
        registry,
        principal("broker-test"),
        engine("", ["caller"], [("cli-probe.upper", "cli-probe")]),
        catalog([(
            "cli-probe.upper",
            set("cli-probe", ExecutionConstraints::default()),
        )]),
        CredentialStore::empty(),
        callers(["caller"]),
        Arc::clone(&audit),
        BrokerLimits::default(),
        Leniency::Strict,
    )
    .expect("broker starts");
    let caller = session("caller", "provider-test");

    match broker
        .run_command(&caller, None, None, "probe", &["--help".to_owned()], false)
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

    let proposed = broker
        .run_command(
            &caller,
            None,
            None,
            "probe",
            &["upper".to_owned(), "-".to_owned()],
            true,
        )
        .await
        .expect("the piped value proposes");
    assert_eq!(
        proposed,
        CommandRunOutcome::Proposed {
            secret_use: None,
            capability: "cli-probe.upper".parse().expect("valid capability fixture"),
            input: json!({"text": "", "piped": true}),
        }
    );

    match broker
        .run_command(&caller, None, None, "probe", &["bogus".to_owned()], false)
        .await
        .expect("a usage error renders")
    {
        CommandRunOutcome::Rendered {
            stdout,
            stderr,
            status,
        } => {
            assert_eq!(status, 2);
            assert!(stdout.is_empty(), "{stdout}");
            assert!(
                stderr.starts_with("error: unrecognized subcommand 'bogus'"),
                "{stderr}"
            );
        }
        other => panic!("expected a rendered usage error, got {other:?}"),
    }
    assert!(audit.records().is_empty(), "running a word decides nothing");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spaces_request_forbid_stops_http_before_the_network() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider");
    let server =
        LoopbackServer::once(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let mappings = [
        (subject(SLACK_SUBJECT), principal("caller")),
        (subject("slack.t0123abc.c0123abc"), principal("space")),
    ];
    let world = PolicyWorld::new(
        [principal("caller"), principal("space")],
        [(
            "http-probe.fetch".parse().expect("capability"),
            "http-probe".parse().expect("provider"),
        )],
    )
    .expect("world")
    .with_subjects(mappings.clone());
    let policy = r#"@id("both") permit(principal, action, resource);
        @id("space-request") forbid(principal == Dekopon::Principal::"space", action == Dekopon::Action::"http-probe.fetch", resource) when { context has request };"#;
    let audit = Arc::new(InMemoryAuditLog::new(32).expect("audit"));
    let broker = Broker::new(
        registry,
        principal("broker-test"),
        PolicyEngine::new(policy, &world).expect("policy"),
        catalog([(
            "http-probe.fetch",
            set("http-probe", loopback_constraints(server.authority())),
        )]),
        CredentialStore::empty(),
        IdentityDirectory::new(mappings).expect("directory"),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("broker");
    let claim = Attestation::for_chat(
        subject(SLACK_SUBJECT),
        agent("provider-test"),
        dekopon_broker::ChatScopeClaim {
            transport: "slack".parse().expect("transport"),
            kind: dekopon_broker::ChatTransportKind::Slack,
            conversation: dekopon_broker::Conversation {
                kind: dekopon_broker::ConversationKind::Channel,
                container: Some("t0123abc".into()),
                id: "c0123abc".into(),
                thread: Some("1712345678.000100".into()),
            },
            trigger: dekopon_broker::Trigger::Message,
            delivery: None,
        },
    );
    let request = request(
        "space-request",
        "http-probe.fetch",
        json!({"uri": format!("http://{}/reply", server.authority()), "method": "GET"}),
    );
    let (host, _stdout) = std::os::unix::net::UnixStream::pair().expect("stdout");
    let result = broker
        .invoke(
            &service_context(GATEWAY),
            Some(&AttestorGrant { namespaces: None }),
            Some(&claim.bound_to(request.id.clone())),
            request,
            dekopon_broker_host::asset::AssetInputs {
                streams: Some(dekopon_broker_host::Streams {
                    stdin: None,
                    stdout: host.into(),
                }),
                ..Default::default()
            },
        )
        .await
        .expect("accounted invocation");
    assert_ne!(
        result.result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    assert!(audit.records().iter().any(|event| matches!(event, AuditEvent::Decision { decision_point, allowed: false, policy_ids, .. } if decision_point == "http.request" && policy_ids == &["both".to_owned()])));
    assert!(server.recorded().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_space_only_policy_error_records_its_id() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider");
    let server =
        LoopbackServer::once(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let mappings = [
        (subject(SLACK_SUBJECT), principal("caller")),
        (subject("slack.t0123abc.c0123abc"), principal("space")),
    ];
    let world = PolicyWorld::new(
        [principal("caller"), principal("space")],
        [(
            "http-probe.fetch".parse().expect("capability"),
            "http-probe".parse().expect("provider"),
        )],
    )
    .expect("world")
    .with_subjects(mappings.clone());
    let policy = r#"@id("both") permit(principal, action, resource);
        @id("space-error") forbid(principal == Dekopon::Principal::"space", action == Dekopon::Action::"http-probe.fetch", resource) when { context has request && 9223372036854775807 + 1 > 0 };"#;
    let audit = Arc::new(InMemoryAuditLog::new(32).expect("audit"));
    let broker = Broker::new(
        registry,
        principal("broker-test"),
        PolicyEngine::new(policy, &world).expect("policy"),
        catalog([(
            "http-probe.fetch",
            set("http-probe", loopback_constraints(server.authority())),
        )]),
        CredentialStore::empty(),
        IdentityDirectory::new(mappings).expect("directory"),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("broker");
    let claim = Attestation::for_chat(
        subject(SLACK_SUBJECT),
        agent("provider-test"),
        dekopon_broker::ChatScopeClaim {
            transport: "slack".parse().expect("transport"),
            kind: dekopon_broker::ChatTransportKind::Slack,
            conversation: dekopon_broker::Conversation {
                kind: dekopon_broker::ConversationKind::Channel,
                container: Some("t0123abc".into()),
                id: "c0123abc".into(),
                thread: Some("1712345678.000100".into()),
            },
            trigger: dekopon_broker::Trigger::Message,
            delivery: None,
        },
    );
    let request = request(
        "space-request",
        "http-probe.fetch",
        json!({"uri": format!("http://{}/reply", server.authority()), "method": "GET"}),
    );
    let (host, _stdout) = std::os::unix::net::UnixStream::pair().expect("stdout");
    broker
        .invoke(
            &service_context(GATEWAY),
            Some(&AttestorGrant { namespaces: None }),
            Some(&claim.bound_to(request.id.clone())),
            request,
            dekopon_broker_host::asset::AssetInputs {
                streams: Some(dekopon_broker_host::Streams {
                    stdin: None,
                    stdout: host.into(),
                }),
                ..Default::default()
            },
        )
        .await
        .expect("accounted invocation");
    let error_ids = audit
        .records()
        .into_iter()
        .filter_map(|event| match event {
            AuditEvent::Decision {
                decision_point,
                policy_error_ids,
                ..
            } if decision_point == "http.request" => Some(policy_error_ids),
            AuditEvent::Decision { .. } | AuditEvent::Execution { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(error_ids, [vec!["space-error".to_owned()]]);
}
