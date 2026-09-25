#![allow(clippy::unwrap_used)]

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
use serde_json::{Value, json};

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
    subject(&format!("slack.t0123abc.{}", name.replace('-', "")))
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
    broker
        .invoke(
            &service_context(GATEWAY),
            Some(&AttestorGrant { namespaces: None }),
            Some(&attestation),
            request,
            Default::default(),
        )
        .await
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

fn jsonplaceholder_engine(policies: &str) -> PolicyEngine {
    engine(
        policies,
        ["caller"],
        [
            ("jsonplaceholder.posts.get", "jsonplaceholder"),
            ("jsonplaceholder.posts.create", "jsonplaceholder"),
        ],
    )
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
        max_output_bytes: 1024 * 1024,
        http: Some(HttpConstraints {
            allowed_hosts: vec![authority.to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            max_requests: 1,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 64 * 1024,
            allow_plaintext_loopback: true,
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
        "policy-test".to_owned(),
        probe_engine(
            &format!(
                "{}\n{}\n{}",
                attested_policy("cpetersen", "some-agent", "gateway", "cli-probe.upper"),
                agent_prompt_policy("cpetersen", "some-agent", "gateway"),
                agent_prompt_policy("oncall", "some-agent", "gateway"),
            ),
            ["cpetersen", "oncall", "gateway"],
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
        "policy-test".to_owned(),
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
    assert_eq!(result.result.decision.policy_revision, "policy-test");
    assert_eq!(
        result.result.output,
        Some(json!({"text": "TOP-SECRET-PAYLOAD"}))
    );
    assert_eq!(result.result.evidence.len(), 2);

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
        "policy-test".to_owned(),
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
    assert!(result.result.output.is_none());
    let records = audit.records();
    assert_eq!(records.len(), 1);
    assert!(matches!(
        records[0],
        AuditEvent::Decision { allowed: false, .. }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_metadata_and_host_ceilings_are_checked_at_startup() {
    let registry = probe_registry(BrokerHostLimits::default()).await;
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let error = Broker::new(
        registry,
        "broker-test"
            .parse::<PrincipalId>()
            .expect("valid broker principal"),
        "policy-test".to_owned(),
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
        "policy-test".to_owned(),
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
        [provider_fixture("jsonplaceholder-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("JSONPlaceholder provider fixture loads");
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let error = Broker::new(
        registry,
        "broker-test"
            .parse::<PrincipalId>()
            .expect("valid broker principal"),
        "policy-test".to_owned(),
        engine(
            "",
            ["caller"],
            [
                ("jsonplaceholder.posts.get", "jsonplaceholder"),
                ("jsonplaceholder.posts.create", "jsonplaceholder"),
            ],
        ),
        catalog([(
            "jsonplaceholder.posts.create",
            set("jsonplaceholder", ExecutionConstraints::default()),
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
async fn http_audit_contains_only_sanitized_call_metadata() {
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
        max_output_bytes: 1024 * 1024,
        http: Some(HttpConstraints {
            allowed_hosts: vec![authority.clone()],
            allowed_methods: vec!["POST".to_owned()],
            max_requests: 1,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 64 * 1024,
            allow_plaintext_loopback: true,
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
        "policy-test".to_owned(),
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
    assert_eq!(result.result.evidence.len(), 3);
    let wire = server.request();
    assert!(wire.ends_with(b"\r\n\r\nbody-secret"));
    server.join();

    let records = audit.records();
    assert_eq!(records.len(), 2);
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
async fn jsonplaceholder_write_requires_external_write_policy_and_redacts_content() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("jsonplaceholder-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("JSONPlaceholder provider fixture loads");
    let response_body = br#"{"userId":3,"id":101,"title":"private title","body":"private body"}"#;
    let response = format!(
        "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response_body.len(),
        String::from_utf8_lossy(response_body)
    );
    let server = LoopbackServer::once(response.as_bytes());
    let authority = server.authority().to_owned();
    let constraints = ExecutionConstraints {
        asset: None,
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
        storage: None,
        secret_use: None,
    };
    let read_constraints = ExecutionConstraints {
        http: Some(HttpConstraints {
            allowed_methods: vec!["GET".to_owned()],
            ..constraints
                .http
                .clone()
                .expect("the write constraints grant HTTP authority")
        }),
        ..constraints.clone()
    };
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let broker = Broker::new(
        registry,
        "broker-test"
            .parse::<PrincipalId>()
            .expect("valid broker principal"),
        "policy-jsonplaceholder".to_owned(),
        jsonplaceholder_engine(&provider_policy(
            "caller",
            "provider-test",
            "jsonplaceholder",
            "jsonplaceholder.posts.create",
        )),
        catalog([
            (
                "jsonplaceholder.posts.get",
                set("jsonplaceholder", read_constraints),
            ),
            (
                "jsonplaceholder.posts.create",
                set_with_metadata(
                    "jsonplaceholder",
                    EffectKind::ExternalWrite,
                    RiskLevel::Medium,
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
    let available = broker.capabilities(&session("caller", "provider-test"));
    assert_eq!(available.len(), 1);
    assert_eq!(available[0].capability.effect, EffectKind::ExternalWrite);
    assert_eq!(available[0].capability.risk, RiskLevel::Medium);
    let read = invoke_as(
        &broker,
        "caller",
        "provider-test",
        request(
            "invoke-json-read-with-write-rule",
            "jsonplaceholder.posts.get",
            json!({
                "postId": 7,
                "endpoint": format!("http://{authority}")
            }),
        ),
    )
    .await
    .expect("ungranted read is denied and audited");
    assert_eq!(
        read.result.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(read.result.error.as_deref(), Some("policy-denied"));

    let result = invoke_as(
        &broker,
        "caller",
        "provider-test",
        request(
            "invoke-json-write",
            "jsonplaceholder.posts.create",
            json!({
                "userId": 3,
                "title": "private title",
                "body": "private body",
                "endpoint": format!("http://{authority}")
            }),
        ),
    )
    .await
    .expect("authorized JSONPlaceholder write succeeds");
    assert_eq!(
        result.result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    assert_eq!(
        result.result.output.as_ref().expect("write returns output")["post"]["id"],
        101
    );
    let wire = server.request();
    assert!(wire.starts_with(b"POST /posts HTTP/1.1\r\n"));
    let body_offset = wire
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("POST headers terminate")
        + 4;
    assert_eq!(
        serde_json::from_slice::<Value>(&wire[body_offset..]).expect("POST body is JSON"),
        json!({"userId": 3, "title": "private title", "body": "private body"})
    );
    server.join();

    let records = audit.records();
    assert_eq!(records.len(), 3);
    let serialized = serde_json::to_string(&records).expect("audit serializes");
    assert!(serialized.contains(&authority));
    assert!(serialized.contains("external-write"));
    assert!(serialized.contains("POST"));
    assert!(!serialized.contains("private title"));
    assert!(!serialized.contains("private body"));
    assert!(!serialized.contains("/posts"));
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_execution_audits_the_external_write_that_already_landed() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("jsonplaceholder-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("JSONPlaceholder provider fixture loads");
    let server = LoopbackServer::once(
        b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 8\r\nConnection: close\r\n\r\nnot-json",
    );
    let authority = server.authority().to_owned();
    let constraints = ExecutionConstraints {
        asset: None,
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
        storage: None,
        secret_use: None,
    };
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let broker = Broker::new(
        registry,
        "broker-test"
            .parse::<PrincipalId>()
            .expect("valid broker principal"),
        "policy-jsonplaceholder".to_owned(),
        jsonplaceholder_engine(&provider_policy(
            "caller",
            "provider-test",
            "jsonplaceholder",
            "jsonplaceholder.posts.create",
        )),
        catalog([(
            "jsonplaceholder.posts.create",
            set_with_metadata(
                "jsonplaceholder",
                EffectKind::ExternalWrite,
                RiskLevel::Medium,
                constraints,
            ),
        )]),
        CredentialStore::empty(),
        callers(["caller"]),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("the external-write constraint set exactly matches trusted provider metadata");

    let result = invoke_as(
        &broker,
        "caller",
        "provider-test",
        request(
            "invoke-json-write-failure",
            "jsonplaceholder.posts.create",
            json!({
                "userId": 3,
                "title": "private title",
                "body": "private body",
                "endpoint": format!("http://{authority}")
            }),
        ),
    )
    .await
    .expect("a failing provider is still durably accounted");

    let wire = server.request();
    assert!(
        wire.starts_with(b"POST /posts HTTP/1.1\r\n"),
        "the external write must have left the host before the failure"
    );
    server.join();

    assert_eq!(
        result.result.outcome,
        dekopon_capability::InvocationOutcome::Failed
    );
    assert_eq!(result.result.error.as_deref(), Some("provider-failure"));
    assert_eq!(
        result.result.detail,
        Some(ProviderFailureDetail::new(
            "invalid-response",
            "endpoint returned an invalid post"
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

    let records = audit.records();
    assert_eq!(records.len(), 2);
    let AuditEvent::Execution {
        outcome,
        error,
        error_detail,
        http_calls,
        ..
    } = &records[1]
    else {
        panic!("the terminal record is an execution event");
    };
    assert_eq!(*outcome, dekopon_capability::InvocationOutcome::Failed);
    assert_eq!(error.as_deref(), Some("provider-failure"));
    assert_eq!(
        error_detail
            .as_ref()
            .map(|detail| (detail.code.as_str(), detail.message.as_str())),
        Some(("invalid-response", "endpoint returned an invalid post"))
    );
    assert_eq!(
        http_calls.len(),
        1,
        "the completed call must survive into the failed execution record"
    );
    assert_eq!(http_calls[0].method, "POST");
    assert_eq!(http_calls[0].authority, authority);
    assert_eq!(http_calls[0].status, Some(201));

    let serialized = serde_json::to_string(&records).expect("audit serializes");
    assert!(!serialized.contains("private title"));
    assert!(!serialized.contains("private body"));
    assert!(!serialized.contains("/posts"));
}

#[tokio::test(flavor = "multi_thread")]
async fn credentialed_constraint_sets_inject_bound_secrets_and_never_audit_them() {
    const SECRET: &str = "audit-must-never-see-this";
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
    let credentials = CredentialStore::new([(
        "fetch-token".to_owned(),
        BoundCredential::bearer(
            "Bearer",
            Redacted::new(SECRET.to_owned()),
            vec![authority.clone()],
        )
        .expect("valid credential fixture"),
    )])
    .expect("credential store builds");
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let broker = Broker::new(
        registry,
        "broker-test"
            .parse::<PrincipalId>()
            .expect("valid broker principal"),
        "policy-test".to_owned(),
        http_probe_engine(&http_policy("caller", "provider-test", "http-probe.fetch")),
        catalog([(
            "http-probe.fetch",
            ConstraintSet {
                route: CapabilityRoute::Generic,
                credential: Some("fetch-token".to_owned()),
                ..set("http-probe", constraints)
            },
        )]),
        credentials,
        callers(["caller"]),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("the credentialed constraint set matches store and destinations");

    let result = invoke_as(
        &broker,
        "caller",
        "provider-test",
        request(
            "invoke-credentialed",
            "http-probe.fetch",
            json!({ "uri": format!("http://{authority}/pulls/7"), "method": "GET" }),
        ),
    )
    .await
    .expect("authorized credentialed request succeeds");
    assert_eq!(
        result.result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );

    let wire = server.request_text();
    assert!(
        wire.contains(&format!("authorization: Bearer {SECRET}")),
        "{wire}"
    );
    server.join();

    let records = audit.records();
    let serialized = serde_json::to_string(&records).expect("audit serializes");
    assert!(
        serialized.contains("\"credentialInjected\":true"),
        "{serialized}"
    );
    assert!(!serialized.contains(SECRET), "audit leaked the secret");
    assert!(!serialized.contains("Bearer"), "audit leaked the scheme");
    let public = serde_json::to_string(&result.result).expect("result serializes");
    assert!(!public.contains(SECRET), "result leaked the secret");
}

#[derive(Debug)]
struct ScriptedRefreshingCredential {
    destinations: Vec<String>,
    resolutions: Arc<std::sync::atomic::AtomicUsize>,
    script: std::sync::Mutex<Vec<Result<&'static str, CredentialRefreshError>>>,
}

impl ScriptedRefreshingCredential {
    fn new(
        destinations: Vec<String>,
        script: Vec<Result<&'static str, CredentialRefreshError>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            destinations,
            resolutions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            script: std::sync::Mutex::new(script),
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
            let mut script = self.script.lock().expect("script lock");
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
        "policy-test".to_owned(),
        http_probe_engine(&http_policy("caller", "provider-test", "http-probe.fetch")),
        catalog([(
            "http-probe.fetch",
            ConstraintSet {
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
    } = &records[1]
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
        "policy-test".to_owned(),
        http_probe_engine(&http_policy("caller", "provider-test", "http-probe.fetch")),
        catalog([(
            "http-probe.fetch",
            ConstraintSet {
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
async fn model_selected_drn_requires_dual_policy_and_exact_private_binding() {
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
        "policy-test".to_owned(),
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
async fn capability_policy_alone_cannot_authorize_a_drn() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider fixture loads");
    let constraints = loopback_constraints("127.0.0.1:9");
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("audit"));
    let broker = Broker::new(
        registry,
        principal("broker-test"),
        "policy-test".to_owned(),
        http_probe_secret_engine(&http_policy("caller", "provider-test", "http-probe.fetch")),
        catalog([("http-probe.fetch", set("http-probe", constraints))]),
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
            Arc::new(StaticSecretResolver(b"never-resolved")),
        )
        .expect("catalog"),
    )
    .expect("binding");
    let mut proposal = request(
        "invoke-secret-denied",
        "http-probe.fetch",
        json!({"uri": "http://127.0.0.1:9/", "method": "GET"}),
    );
    proposal.secret_use = Some(SecretUseProposal::HttpBearer {
        secret: secret_drn(),
    });
    let result = invoke_as(&broker, "caller", "provider-test", proposal)
        .await
        .expect("denial audited");
    assert_eq!(
        result.result.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(result.result.error.as_deref(), Some("secret-denied"));
    let encoded = serde_json::to_string(&audit.records()).expect("audit serializes");
    assert!(encoded.contains(secret_drn().as_str()), "{encoded}");
    assert!(encoded.contains("secret_sink"), "{encoded}");
    assert!(!encoded.contains("never-resolved"), "{encoded}");
}

/// The secret [`basic_secret_broker`] resolves; at least the 16 bytes the native sink requires.
const BASIC_SECRET: &[u8] = b"drn-secret-never-visible";

fn basic_fetch_argv(uri: &str) -> Vec<String> {
    let drn = secret_drn();
    ["fetch", "--uri", uri, "--basic", "user-a", drn.as_str()]
        .map(str::to_owned)
        .into()
}

/// Cedar's policy permits secret.use regardless of username; the binding, not the policy, is what
/// restricts which name may present the secret.
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
        "policy-test".to_owned(),
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
async fn a_command_word_proposes_basic_secret_use_from_its_argv() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider fixture loads");
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("audit"));
    let broker = basic_secret_broker(registry, "127.0.0.1:9", "user-a", &audit);
    let uri = "http://127.0.0.1:9/api/v1/thing";

    let proposed = broker
        .run_command(
            &session("caller", "provider-test"),
            None,
            None,
            "httpprobe",
            &basic_fetch_argv(uri),
            None,
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
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_word_s_basic_proposal_needs_a_binding_for_its_exact_username() {
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
            None,
        )
        .await
        .expect("the word proposes");
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
    // base64("user-a:drn-secret-never-visible"): the bound username, then the resolved secret.
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
async fn authorized_source_failure_is_a_terminal_audited_failure_not_an_ambiguous_gap() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider fixture loads");
    let authority = "127.0.0.1:9";
    let policy = format!(
        "{}\n{}",
        http_policy("caller", "provider-test", "http-probe.fetch"),
        r#"@id("caller-secret-use")
           permit(principal == Dekopon::Principal::"caller",
                  action == Dekopon::Action::"secret.use",
                  resource == Dekopon::Secret::"drn:com.xrl:secret:test:http-probe/token");"#,
    );
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("audit"));
    let broker = Broker::new(
        registry,
        principal("broker-test"),
        "policy-test".to_owned(),
        http_probe_secret_engine(&policy),
        catalog([(
            "http-probe.fetch",
            set("http-probe", loopback_constraints(authority)),
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
                binding_id: "missing-token".to_owned(),
                secret: secret_drn(),
                capability: "http-probe.fetch".parse().expect("capability"),
                sink: SecretSinkKind::HttpBearer,
                basic_username: None,
                allowed_hosts: vec![authority.to_owned()],
                allowed_methods: vec!["GET".to_owned()],
                allowed_paths: vec![HttpPathRule::Exact {
                    path: "/".to_owned(),
                }],
                allow_query: false,
                max_injections: 1,
            }],
            Arc::new(MissingSecretResolver),
        )
        .expect("catalog"),
    )
    .expect("binding");
    let mut proposal = request(
        "invoke-secret-missing",
        "http-probe.fetch",
        json!({"uri": "http://127.0.0.1:9/", "method": "GET"}),
    );
    proposal.secret_use = Some(SecretUseProposal::HttpBearer {
        secret: secret_drn(),
    });
    let result = invoke_as(&broker, "caller", "provider-test", proposal)
        .await
        .expect("source failure is a normal audited result");
    assert_eq!(
        result.result.outcome,
        dekopon_capability::InvocationOutcome::Failed
    );
    assert_eq!(result.result.error.as_deref(), Some("secret-resolution"));
    let records = audit.records();
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

#[tokio::test(flavor = "multi_thread")]
async fn per_agent_credentials_select_by_agent_and_fall_back_to_the_default() {
    const DEFAULT_SECRET: &str = "dekopon-agents-token";
    const OVERRIDE_SECRET: &str = "scientist-hq-token";
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
    let credentials = CredentialStore::new([
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
    ])
    .expect("credential store builds");
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound"));
    let broker = Broker::new(
        registry,
        principal("broker-test"),
        "policy-test".to_owned(),
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
                route: CapabilityRoute::Generic,
                credential: Some("github-pat".to_owned()),
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
        assert!(
            request.contains(&format!("authorization: Bearer {present}")),
            "request {index} presented the wrong credential: {request}"
        );
        assert!(
            !request.contains(absent),
            "request {index} leaked the other organization's token: {request}"
        );
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
    assert_eq!(
        selected,
        ["github-pat-scientist-hq", "github-pat"],
        "each terminal record names the credential its own invocation selected"
    );
    let serialized = serde_json::to_string(&records).expect("audit serializes");
    for secret in [DEFAULT_SECRET, OVERRIDE_SECRET] {
        assert!(!serialized.contains(secret), "audit leaked a secret");
    }
    assert!(!serialized.contains("Bearer"), "audit leaked the scheme");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rebinding_never_adds_a_credential_to_a_set_that_names_none() {
    const SECRET: &str = "scientist-hq-token";
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
    let credentials = CredentialStore::new([(
        "github-pat-scientist-hq".to_owned(),
        BoundCredential::bearer(
            "Bearer",
            Redacted::new(SECRET.to_owned()),
            vec![authority.clone()],
        )
        .expect("valid credential fixture"),
    )])
    .expect("credential store builds");
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound"));
    let broker = Broker::new(
        registry,
        principal("broker-test"),
        "policy-test".to_owned(),
        http_probe_engine(&format!(
            "{}\n{}",
            http_policy("caller", "dekoponville-github", "http-probe.fetch"),
            http_policy("caller", "nestedset-github", "http-probe.fetch"),
        )),
        catalog([(
            "http-probe.fetch",
            set("http-probe", loopback_constraints(&authority)),
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
    .expect("a rebinding of a name no set uses is inert");

    for (id, agent_name) in [
        ("invoke-nestedset", "nestedset-github"),
        ("invoke-dekoponville", "dekoponville-github"),
    ] {
        invoke_as(
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
    }

    for index in 0..2 {
        let request = server.request_text();
        assert!(
            !request.to_ascii_lowercase().contains("authorization"),
            "request {index} carried a credential its set never named: {request}"
        );
    }
    server.join();

    let serialized = serde_json::to_string(&audit.records()).expect("audit serializes");
    assert!(!serialized.contains(SECRET), "audit leaked a secret");
    assert!(
        !serialized.contains("\"credential\""),
        "an invocation with no credential names none: {serialized}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn credentialed_constraint_sets_fail_closed_at_construction() {
    let http = |hosts: Vec<String>| ExecutionConstraints {
        asset: None,
        timeout_ms: 5_000,
        max_output_bytes: 1024 * 1024,
        http: Some(HttpConstraints {
            allowed_hosts: hosts,
            allowed_methods: vec!["GET".to_owned()],
            max_requests: 1,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 64 * 1024,
            allow_plaintext_loopback: false,
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
                "policy-test".to_owned(),
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
    let audit = Arc::new(InMemoryAuditLog::new(16).expect("valid audit bound"));
    let broker = Broker::new(
        probe_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        "policy-test".to_owned(),
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
            Default::default(),
        )
        .await
        .expect("attested invocation is accounted");
    assert_eq!(
        attested.result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    assert_eq!(
        attested.result.output,
        Some(json!({"text": "ON BEHALF OF"}))
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
        let direct = broker
            .invoke(
                &peer,
                None,
                None,
                request(id, "cli-probe.upper", json!({"text": "direct"})),
                Default::default(),
            )
            .await
            .expect("direct proposal is accounted");
        assert_eq!(
            direct.result.outcome,
            dekopon_capability::InvocationOutcome::Denied,
            "{id}"
        );
        assert_eq!(direct.result.error.as_deref(), Some("policy-error"), "{id}");
        assert_eq!(
            broker.capability_view(&peer),
            (Vec::new(), Vec::new()),
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

    let ungranted = broker
        .invoke(
            &gateway,
            None,
            Some(&attestation(&subject, "some-agent", "invoke-no-grant")),
            request(
                "invoke-no-grant",
                "cli-probe.upper",
                json!({"text": "claim"}),
            ),
            Default::default(),
        )
        .await
        .expect("a refused attestation is accounted");
    assert_eq!(
        ungranted.result.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(
        ungranted.result.error.as_deref(),
        Some("attestation-denied")
    );

    let out_of_scope = broker
        .invoke(
            &gateway,
            Some(&attestor_grant(["slack.t0999zzz"])),
            Some(&attestation(&subject, "some-agent", "invoke-out-of-scope")),
            request(
                "invoke-out-of-scope",
                "cli-probe.upper",
                json!({"text": "claim"}),
            ),
            Default::default(),
        )
        .await
        .expect("an out-of-scope attestation is accounted");
    assert_eq!(
        out_of_scope.result.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(
        out_of_scope.result.error.as_deref(),
        Some("attestation-denied")
    );

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
            Default::default(),
        )
        .await
        .expect("attested invocation is accounted");
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
        "policy-test".to_owned(),
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

    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let broker = Broker::new(
        probe_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        "policy-test".to_owned(),
        probe_engine(
            r#"@id("caller-unconstrained") permit(principal == Dekopon::Principal::"caller", action, resource);"#,
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
    .expect("an unconstrained action scope names no capability, so startup has nothing to check");
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

#[tokio::test(flavor = "multi_thread")]
async fn audit_records_carry_determining_policy_ids_and_the_policy_digest() {
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound"));
    let broker = Broker::new(
        probe_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        "policy-test".to_owned(),
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

    invoke_as(
        &broker,
        "caller",
        "provider-test",
        request("invoke-explained", "cli-probe.upper", json!({"text": "hi"})),
    )
    .await
    .expect("the allowed invocation is accounted");
    invoke_as(
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
async fn tolerating_an_unconstrained_capability_warns_but_still_denies_it() {
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let (broker, warnings) = Broker::start(
        probe_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        "policy-test".to_owned(),
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
        Arc::clone(&audit),
        BrokerLimits::default(),
        Leniency::Tolerant,
    )
    .expect("tolerating an unexecutable grant starts");

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(matches!(
        &warnings[0],
        StartupWarning::UnconstrainedCapability { capability }
            if capability.as_str() == "cli-probe.reverse"
    ));

    let result = invoke_as(
        &broker,
        "caller",
        "provider-test",
        request(
            "invoke-tolerated",
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
        "a tolerated capability is still never listed"
    );
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
        "policy-test".to_owned(),
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
        "policy-test".to_owned(),
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
async fn command_words_are_filtered_by_what_policy_allows() {
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let (broker, _) = Broker::start(
        probe_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        "policy-test".to_owned(),
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
    for name in ["caller", "stranger"] {
        assert_eq!(
            broker.capability_view(&session(name, "provider-test")),
            (
                broker.capabilities(&session(name, "provider-test")),
                broker.command_words(&session(name, "provider-test"))
            ),
            "the combined view must be the same answer as the two listings it replaces"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_command_word_is_refused_without_running_anything() {
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let (broker, _) = Broker::start(
        probe_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        "policy-test".to_owned(),
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
            None,
        )
        .await
        .expect_err("no loaded provider declares this word");
    assert!(
        format!("{error}").contains("gh"),
        "the refusal names the word: {error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_word_renders_help_and_reads_the_piped_value_through_the_broker() {
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
        "policy-test".to_owned(),
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
        .run_command(&caller, None, None, "probe", &["--help".to_owned()], None)
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
            Some("hello"),
        )
        .await
        .expect("the piped value proposes");
    assert_eq!(
        proposed,
        CommandRunOutcome::Proposed {
            secret_use: None,
            capability: "cli-probe.upper".parse().expect("valid capability fixture"),
            input: json!({"text": "hello"}),
        }
    );

    match broker
        .run_command(&caller, None, None, "probe", &["bogus".to_owned()], None)
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
