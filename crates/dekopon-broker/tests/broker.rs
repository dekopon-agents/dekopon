use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use dekopon_broker::{
    AttestorGrant, AuditEvent, AuthenticatedContext, Broker, BrokerBuildError, BrokerLimits,
    ConstraintCatalog, ConstraintSet, CredentialStore, FileAuditLog, IdentityDirectory,
    InMemoryAuditLog, InvocationRequest, Leniency, PolicyEngine, PolicyWorld, SecretCatalog,
    SecretMaterial, SecretResolutionError, SecretResolver, SecretUseBinding, StartupWarning,
    SubjectAttestation, verify_audit_chain,
};
use dekopon_broker_host::BoundCredential;
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_capability::{
    EffectKind, ExecutionConstraints, HttpConstraints, HttpPathRule, Idempotency,
};
use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, InvocationId, PrincipalId, ProviderId, Redacted,
    RiskLevel, SecretDrn, SecretSinkKind, SecretUseProposal, TraceId,
};
use dekopon_test_support::{LoopbackServer, provider_fixture};
use serde_json::{Value, json};

/// The canonical subject every attestation fixture stands for.
const SLACK_SUBJECT: &str = "slack.t0123abc.u9xyz";

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

fn context(principal: &str) -> AuthenticatedContext {
    agent_context(principal, "provider-test")
}

/// A directly connected agent context: no attestor peer, no external subject.
fn agent_context(name: &str, agent_name: &str) -> AuthenticatedContext {
    AuthenticatedContext::new(
        principal(name),
        Actor::Agent {
            agent: agent(agent_name),
        },
    )
    .expect("trusted agent context is valid")
}

/// A gateway's own peer context.
///
/// A service actor must carry the principal the transport authenticated, so the two names a
/// caller might expect to vary independently here cannot: `AuthenticatedContext::new` rejects the
/// mismatch.
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
        trace: "trace-test"
            .parse::<TraceId>()
            .expect("valid trace fixture"),
        trace_parent: None,
        input,
        secret_use: None,
    }
}

/// One constraint set with the classification the echo provider actually declares.
fn set(provider: &str, constraints: ExecutionConstraints) -> ConstraintSet {
    set_with_metadata(
        provider,
        EffectKind::ReadOnly,
        RiskLevel::Low,
        Idempotency::Idempotent,
        constraints,
    )
}

fn set_with_metadata(
    provider: &str,
    effect: EffectKind,
    risk: RiskLevel,
    idempotency: Idempotency,
    constraints: ExecutionConstraints,
) -> ConstraintSet {
    ConstraintSet {
        provider: provider
            .parse::<ProviderId>()
            .expect("valid provider fixture"),
        effect,
        risk,
        idempotency,
        credential: None,
        credential_by_agent: BTreeMap::new(),
        constraints,
    }
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

/// A policy engine over exactly the entities a fixture names.
///
/// `principals` is the declared world: a policy naming anything outside it refuses construction,
/// which is what replaced the old engine's reachability check.
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

/// The echo world every echo fixture shares.
fn echo_engine<'a>(policies: &str, principals: impl IntoIterator<Item = &'a str>) -> PolicyEngine {
    engine(
        policies,
        principals,
        [
            ("echo.echo", "echo"),
            ("echo.reverse", "echo"),
            ("echo.upcase", "echo"),
            ("echo.downcase", "echo"),
            ("echo.ransom-case", "echo"),
        ],
    )
}

/// The HTTP probe world.
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

/// The JSONPlaceholder world, which is the only fixture with two differently classified
/// capabilities on one provider.
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

/// [`direct_policy`] for a provider other than `echo`.
fn direct_provider_policy(
    name: &str,
    agent_name: &str,
    provider: &str,
    capability: &str,
) -> String {
    format!(
        r#"permit(principal == Dekopon::Principal::"{name}",
                  action == Dekopon::Action::"{capability}",
                  resource == Dekopon::Provider::"{provider}")
           when {{ context has agent && context.agent == "{agent_name}" }}
           unless {{ context has via }};"#
    )
}

fn direct_http_policy(name: &str, agent_name: &str, capability: &str) -> String {
    direct_provider_policy(name, agent_name, "http-probe", capability)
}

/// The same HTTP grant for a directly connected peer that is no agent at all — the shape
/// `dekopon-run` arrives in, carrying `Actor::Service` and therefore no `context.agent`.
const DIRECT_PEER_HTTP_POLICY: &str = r#"permit(principal == Dekopon::Principal::"direct-peer",
       action == Dekopon::Action::"http-probe.fetch",
       resource == Dekopon::Provider::"http-probe")
unless { context has via };"#;

/// One plaintext loopback GET against a fixture server.
fn loopback_constraints(authority: &str) -> ExecutionConstraints {
    ExecutionConstraints {
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

/// The Cedar spelling of "this exact principal, as this exact agent, connected directly".
fn direct_policy(name: &str, agent_name: &str, capability: &str) -> String {
    format!(
        r#"permit(principal == Dekopon::Principal::"{name}",
                  action == Dekopon::Action::"{capability}",
                  resource == Dekopon::Provider::"echo")
           when {{ context has agent && context.agent == "{agent_name}" }}
           unless {{ context has via }};"#
    )
}

/// The session gate: this principal may drive this agent, but only through this gateway.
fn agent_prompt_policy(name: &str, agent_name: &str, via: &str) -> String {
    format!(
        r#"permit(principal == Dekopon::Principal::"{name}",
                  action == Dekopon::Action::"agent.prompt",
                  resource == Dekopon::Agent::"{agent_name}")
           when {{ context has via && context.via == "{via}" }};"#
    )
}

/// The Cedar spelling of "…reached only through exactly this gateway".
fn attested_policy(name: &str, agent_name: &str, via: &str, capability: &str) -> String {
    format!(
        r#"permit(principal == Dekopon::Principal::"{name}",
                  action == Dekopon::Action::"{capability}",
                  resource == Dekopon::Provider::"echo")
           when {{ context has via && context.via == "{via}"
                && context has agent && context.agent == "{agent_name}" }};"#
    )
}

fn attestor_grant<'a>(namespaces: impl IntoIterator<Item = &'a str>) -> AttestorGrant {
    AttestorGrant {
        namespaces: namespaces.into_iter().map(str::to_owned).collect(),
        chat_scopes: Vec::new(),
    }
}

fn attestation(
    subject: &ExternalSubject,
    agent_name: &str,
    invocation: &str,
) -> SubjectAttestation {
    SubjectAttestation {
        subject: subject.clone(),
        agent: agent(agent_name),
        invocation: invocation
            .parse::<InvocationId>()
            .expect("valid invocation fixture"),
    }
}

fn directory<'a>(entries: impl IntoIterator<Item = (&'a str, &'a str)>) -> IdentityDirectory {
    IdentityDirectory::new(
        entries
            .into_iter()
            .map(|(canonical, name)| (subject(canonical), principal(name))),
    )
    .expect("distinct subject fixtures build a directory")
}

async fn echo_registry(limits: BrokerHostLimits) -> BrokerProviderRegistry {
    BrokerProviderRegistry::load([provider_fixture("echo-provider.wasm")], limits)
        .await
        .expect("echo provider fixture loads")
}

/// A broker whose only grant is attested: `cpetersen` may `echo.echo`, but only through
/// `gateway`.
async fn attested_broker(
    identities: IdentityDirectory,
    audit: Arc<InMemoryAuditLog>,
) -> Broker<InMemoryAuditLog> {
    Broker::new(
        echo_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        "policy-test".to_owned(),
        echo_engine(
            &format!(
                "{}\n{}\n{}",
                attested_policy("cpetersen", "some-agent", "gateway", "echo.echo"),
                agent_prompt_policy("cpetersen", "some-agent", "gateway"),
                // `oncall` may drive the agent and holds no capability, which is what makes
                // "allowed to ask, granted nothing" distinguishable from "may not ask".
                agent_prompt_policy("oncall", "some-agent", "gateway"),
            ),
            ["cpetersen", "oncall", "gateway"],
        ),
        catalog([("echo.echo", set("echo", ExecutionConstraints::default()))]),
        CredentialStore::empty(),
        identities,
        audit,
        BrokerLimits::default(),
    )
    .expect("attested policy is coherent")
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_authorizes_once_and_audits_no_payloads() {
    let registry = echo_registry(BrokerHostLimits::default()).await;
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound"));
    let broker = Broker::new(
        registry,
        "broker-test"
            .parse::<PrincipalId>()
            .expect("valid broker principal"),
        "policy-test".to_owned(),
        echo_engine(
            &direct_policy("caller", "provider-test", "echo.echo"),
            ["caller"],
        ),
        catalog([("echo.echo", set("echo", ExecutionConstraints::default()))]),
        CredentialStore::empty(),
        IdentityDirectory::empty(),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("the policy matches loaded provider metadata");

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
async fn durable_audit_restores_replay_rejection_after_restart() {
    let directory = tempfile::tempdir().expect("create durable broker fixture");
    let path = directory.path().join("audit.jsonl");
    let audit = Arc::new(
        FileAuditLog::open(&path, 8, 64 * 1024)
            .await
            .expect("create durable audit"),
    );
    let policies = direct_policy("caller", "provider-test", "echo.echo");
    let broker = Broker::new(
        echo_registry(BrokerHostLimits::default()).await,
        "broker-test"
            .parse::<PrincipalId>()
            .expect("valid broker principal"),
        "policy-test".to_owned(),
        echo_engine(&policies, ["caller"]),
        catalog([("echo.echo", set("echo", ExecutionConstraints::default()))]),
        CredentialStore::empty(),
        IdentityDirectory::empty(),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("durable broker starts");
    let first = broker
        .invoke(
            &context("caller"),
            request("invoke-durable", "echo.echo", json!({"message": "hello"})),
        )
        .await
        .expect("first invocation succeeds and is durable");
    assert_eq!(
        first.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    drop(broker);
    drop(audit);

    let audit = Arc::new(
        FileAuditLog::open(&path, 8, 64 * 1024)
            .await
            .expect("verified audit reopens"),
    );
    let replay_ids = audit.take_replay_ids().await;
    let broker = Broker::new_with_replay_ids(
        echo_registry(BrokerHostLimits::default()).await,
        "broker-test"
            .parse::<PrincipalId>()
            .expect("valid broker principal"),
        "policy-test".to_owned(),
        echo_engine(&policies, ["caller"]),
        catalog([("echo.echo", set("echo", ExecutionConstraints::default()))]),
        CredentialStore::empty(),
        IdentityDirectory::empty(),
        Arc::clone(&audit),
        BrokerLimits::default(),
        replay_ids,
    )
    .expect("broker restores verified replay state");
    let replay = broker
        .invoke(
            &context("caller"),
            request("invoke-durable", "echo.echo", json!({"message": "again"})),
        )
        .await
        .expect("replay denial is durably audited");
    assert_eq!(
        replay.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(replay.error.as_deref(), Some("replayed-invocation"));
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
        echo_engine(
            &direct_policy("allowed-caller", "provider-test", "echo.echo"),
            ["allowed-caller", "other-caller"],
        ),
        catalog([("echo.echo", set("echo", ExecutionConstraints::default()))]),
        CredentialStore::empty(),
        IdentityDirectory::empty(),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("policy is coherent");

    let allowed = broker.capabilities(&context("allowed-caller"));
    assert_eq!(allowed.len(), 1);
    assert_eq!(allowed[0].provider.as_str(), "echo");
    assert_eq!(allowed[0].capability.id.as_str(), "echo.echo");
    assert!(broker.capabilities(&context("other-caller")).is_empty());

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
        echo_engine("", ["caller"]),
        catalog([(
            "echo.echo",
            set("different-provider", ExecutionConstraints::default()),
        )]),
        CredentialStore::empty(),
        IdentityDirectory::empty(),
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
        echo_engine("", ["caller"]),
        catalog([(
            "echo.echo",
            set(
                "echo",
                ExecutionConstraints {
                    timeout_ms: 101,
                    ..ExecutionConstraints::default()
                },
            ),
        )]),
        CredentialStore::empty(),
        IdentityDirectory::empty(),
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
        IdentityDirectory::empty(),
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
        http_probe_engine(&direct_http_policy(
            "caller",
            "provider-test",
            "http-probe.fetch",
        )),
        catalog([("http-probe.fetch", set("http-probe", constraints))]),
        CredentialStore::empty(),
        IdentityDirectory::empty(),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("the HTTP constraint set matches trusted metadata and host ceilings");

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
    let wire = server.request();
    assert!(wire.ends_with(b"\r\n\r\nbody-secret"));
    server.join();

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
        jsonplaceholder_engine(&direct_provider_policy(
            "caller",
            "provider-test",
            "jsonplaceholder",
            "jsonplaceholder.posts.create",
        )),
        catalog([
            // The read is deployable but ungranted, so its refusal is a policy decision rather
            // than "nothing knows how to run this".
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
                    Idempotency::NonIdempotent,
                    constraints,
                ),
            ),
        ]),
        CredentialStore::empty(),
        IdentityDirectory::empty(),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("the external-write constraint set exactly matches trusted provider metadata");
    let available = broker.capabilities(&context("caller"));
    assert_eq!(available.len(), 1);
    assert_eq!(available[0].capability.effect, EffectKind::ExternalWrite);
    assert_eq!(available[0].capability.risk, RiskLevel::Medium);
    assert_eq!(
        available[0].capability.idempotency,
        Idempotency::NonIdempotent
    );
    let read = broker
        .invoke(
            &context("caller"),
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
    assert_eq!(read.outcome, dekopon_capability::InvocationOutcome::Denied);
    assert_eq!(read.error.as_deref(), Some("policy-denied"));

    let result = broker
        .invoke(
            &context("caller"),
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
        result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    assert_eq!(
        result.output.as_ref().expect("write returns output")["post"]["id"],
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

    let records = audit.records().await;
    assert_eq!(records.len(), 3);
    verify_audit_chain(&records).expect("audit chain verifies");
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
    // The POST is accepted by the server — the non-idempotent effect happens — but the body is
    // not a post, so the guest reports its own failure after the external write has landed.
    let server = LoopbackServer::once(
        b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 8\r\nConnection: close\r\n\r\nnot-json",
    );
    let authority = server.authority().to_owned();
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
        jsonplaceholder_engine(&direct_provider_policy(
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
                Idempotency::NonIdempotent,
                constraints,
            ),
        )]),
        CredentialStore::empty(),
        IdentityDirectory::empty(),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("the external-write constraint set exactly matches trusted provider metadata");

    let result = broker
        .invoke(
            &context("caller"),
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
        result.outcome,
        dekopon_capability::InvocationOutcome::Failed
    );
    assert_eq!(result.error.as_deref(), Some("provider-failure"));
    assert!(
        result
            .evidence
            .iter()
            .any(|evidence| evidence.kind == "http-calls"),
        "a failure that dispatched HTTP must return http-call evidence"
    );

    let records = audit.records().await;
    assert_eq!(records.len(), 2);
    verify_audit_chain(&records).expect("audit chain verifies");
    let AuditEvent::Execution {
        outcome,
        http_calls,
        ..
    } = &records[1].event
    else {
        panic!("the terminal record is an execution event");
    };
    assert_eq!(*outcome, dekopon_capability::InvocationOutcome::Failed);
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
        http_probe_engine(&direct_http_policy(
            "caller",
            "provider-test",
            "http-probe.fetch",
        )),
        catalog([(
            "http-probe.fetch",
            ConstraintSet {
                credential: Some("fetch-token".to_owned()),
                ..set("http-probe", constraints)
            },
        )]),
        credentials,
        IdentityDirectory::empty(),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("the credentialed constraint set matches store and destinations");

    let result = broker
        .invoke(
            &context("caller"),
            request(
                "invoke-credentialed",
                "http-probe.fetch",
                json!({ "uri": format!("http://{authority}/pulls/7"), "method": "GET" }),
            ),
        )
        .await
        .expect("authorized credentialed request succeeds");
    assert_eq!(
        result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );

    // The wire is the only place the secret may appear, exactly once, as the injected header.
    let wire = server.request_text();
    assert!(
        wire.contains(&format!("authorization: Bearer {SECRET}")),
        "{wire}"
    );
    server.join();

    // Presence is recorded; the value is not — not in audit, not in the public result.
    let records = audit.records().await;
    verify_audit_chain(&records).expect("audit chain verifies");
    let serialized = serde_json::to_string(&records).expect("audit serializes");
    assert!(
        serialized.contains("\"credentialInjected\":true"),
        "{serialized}"
    );
    assert!(!serialized.contains(SECRET), "audit leaked the secret");
    assert!(!serialized.contains("Bearer"), "audit leaked the scheme");
    let public = serde_json::to_string(&result).expect("result serializes");
    assert!(!public.contains(SECRET), "result leaked the secret");
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
        direct_http_policy("caller", "provider-test", "http-probe.fetch"),
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
        IdentityDirectory::empty(),
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
    let result = broker
        .invoke(&context("caller"), proposal)
        .await
        .expect("dual-authorized invocation completes");
    assert_eq!(
        result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    let wire = server.request_text();
    assert!(
        wire.contains("authorization: Bearer drn-secret-never-visible"),
        "{wire}"
    );
    server.join();

    let serialized = serde_json::to_string(&audit.records().await).expect("audit serializes");
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
        http_probe_secret_engine(&direct_http_policy(
            "caller",
            "provider-test",
            "http-probe.fetch",
        )),
        catalog([("http-probe.fetch", set("http-probe", constraints))]),
        CredentialStore::empty(),
        IdentityDirectory::empty(),
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
    let result = broker
        .invoke(&context("caller"), proposal)
        .await
        .expect("denial audited");
    assert_eq!(
        result.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(result.error.as_deref(), Some("secret-denied"));
    let encoded = serde_json::to_string(&audit.records().await).expect("audit serializes");
    assert!(encoded.contains(secret_drn().as_str()), "{encoded}");
    assert!(encoded.contains("secret_sink"), "{encoded}");
    assert!(!encoded.contains("never-resolved"), "{encoded}");
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
        direct_http_policy("caller", "provider-test", "http-probe.fetch"),
        r#"permit(principal == Dekopon::Principal::"caller",
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
        IdentityDirectory::empty(),
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
    let result = broker
        .invoke(&context("caller"), proposal)
        .await
        .expect("source failure is a normal audited result");
    assert_eq!(
        result.outcome,
        dekopon_capability::InvocationOutcome::Failed
    );
    assert_eq!(result.error.as_deref(), Some("secret-resolution"));
    let records = audit.records().await;
    assert_eq!(records.len(), 2, "decision plus terminal failed execution");
    let AuditEvent::Execution {
        error, http_calls, ..
    } = &records[1].event
    else {
        panic!("terminal record is execution");
    };
    assert_eq!(error.as_deref(), Some("secret-resolution"));
    assert!(http_calls.is_empty());
}

/// The per-agent axis, end to end: one capability, one constraint set, two organizations' tokens.
///
/// Selection keys on the agent in the trusted context, so all three shapes a caller can arrive in
/// are here — an agent the set names, an agent it does not, and a direct peer that is no agent at
/// all. The wire is the only place a secret may appear; the audit chain gets the symbolic name,
/// which is what keeps the two writes from being indistinguishable after the fact.
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
        3,
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
                "{}\n{}\n{}",
                direct_http_policy("caller", "dekoponville-github", "http-probe.fetch"),
                direct_http_policy("caller", "nestedset-github", "http-probe.fetch"),
                DIRECT_PEER_HTTP_POLICY,
            ),
            ["caller", "direct-peer"],
            [("http-probe.fetch", "http-probe")],
        ),
        catalog([(
            "http-probe.fetch",
            ConstraintSet {
                credential: Some("github-pat".to_owned()),
                credential_by_agent: BTreeMap::from([(
                    agent("nestedset-github"),
                    "github-pat-scientist-hq".to_owned(),
                )]),
                ..set("http-probe", loopback_constraints(&authority))
            },
        )]),
        credentials,
        IdentityDirectory::empty(),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("both the default and the override match the store and its destinations");

    let fetch = |id: &'static str, context: AuthenticatedContext| {
        let authority = authority.clone();
        let broker = &broker;
        async move {
            let result = broker
                .invoke(
                    &context,
                    request(
                        id,
                        "http-probe.fetch",
                        json!({ "uri": format!("http://{authority}/pulls/7"), "method": "GET" }),
                    ),
                )
                .await
                .expect("the authorized request is accounted");
            assert_eq!(
                result.outcome,
                dekopon_capability::InvocationOutcome::Succeeded
            );
        }
    };
    fetch(
        "invoke-nestedset",
        agent_context("caller", "nestedset-github"),
    )
    .await;
    fetch(
        "invoke-dekoponville",
        agent_context("caller", "dekoponville-github"),
    )
    .await;
    // A direct peer such as `dekopon-run` is an `Actor::Service`: no agent, no override, default.
    fetch("invoke-direct", service_context("direct-peer")).await;

    let wire = || server.request_text();
    for (index, (present, absent)) in [
        (OVERRIDE_SECRET, DEFAULT_SECRET),
        (DEFAULT_SECRET, OVERRIDE_SECRET),
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

    // Which authority a write used is exactly what an auditor needs; the value still is not.
    let records = audit.records().await;
    verify_audit_chain(&records).expect("audit chain verifies");
    let encoded = serde_json::to_value(&records).expect("audit serializes");
    let selected = encoded
        .as_array()
        .expect("records serialize as an array")
        .iter()
        .filter_map(|record| record["event"]["credential"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        selected,
        ["github-pat-scientist-hq", "github-pat", "github-pat"],
        "each terminal record names the credential its own invocation selected"
    );
    let serialized = serde_json::to_string(&records).expect("audit serializes");
    for secret in [DEFAULT_SECRET, OVERRIDE_SECRET] {
        assert!(!serialized.contains(secret), "audit leaked a secret");
    }
    assert!(!serialized.contains("Bearer"), "audit leaked the scheme");
}

/// A set may carry overrides and no default, and then "no credential" keeps its original meaning
/// for every agent the overrides do not name: the capability transacts unauthenticated.
#[tokio::test(flavor = "multi_thread")]
async fn an_agent_with_no_override_and_no_default_transacts_unauthenticated() {
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
            direct_http_policy("caller", "dekoponville-github", "http-probe.fetch"),
            direct_http_policy("caller", "nestedset-github", "http-probe.fetch"),
        )),
        catalog([(
            "http-probe.fetch",
            ConstraintSet {
                credential: None,
                credential_by_agent: BTreeMap::from([(
                    agent("nestedset-github"),
                    "github-pat-scientist-hq".to_owned(),
                )]),
                ..set("http-probe", loopback_constraints(&authority))
            },
        )]),
        credentials,
        IdentityDirectory::empty(),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("an override with no default is a coherent set");

    for (id, agent_name) in [
        ("invoke-nestedset", "nestedset-github"),
        ("invoke-dekoponville", "dekoponville-github"),
    ] {
        broker
            .invoke(
                &agent_context("caller", agent_name),
                request(
                    id,
                    "http-probe.fetch",
                    json!({ "uri": format!("http://{authority}/pulls/7"), "method": "GET" }),
                ),
            )
            .await
            .expect("the authorized request is accounted");
    }

    let wire = || server.request_text();
    assert!(wire().contains(&format!("authorization: Bearer {SECRET}")));
    let unauthenticated = wire();
    assert!(
        !unauthenticated
            .to_ascii_lowercase()
            .contains("authorization"),
        "an unmatched agent must send no credential at all: {unauthenticated}"
    );
    server.join();

    let records = audit.records().await;
    verify_audit_chain(&records).expect("audit chain verifies");
    let encoded = serde_json::to_value(&records).expect("audit serializes");
    let selected = encoded
        .as_array()
        .expect("records serialize as an array")
        .iter()
        .filter_map(|record| record["event"]["credential"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        selected,
        ["github-pat-scientist-hq"],
        "an invocation with no credential names none, rather than naming the empty one"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn credentialed_constraint_sets_fail_closed_at_construction() {
    let http = |hosts: Vec<String>| ExecutionConstraints {
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
                    Redacted::new("secret".to_owned()),
                    vec!["api.example.test".to_owned()],
                )
                .expect("valid credential fixture"),
            ),
            (
                "other-token".to_owned(),
                BoundCredential::bearer(
                    "Bearer",
                    Redacted::new("other-secret".to_owned()),
                    vec!["other.example.test".to_owned()],
                )
                .expect("valid credential fixture"),
            ),
        ])
        .expect("credential store builds")
    };
    let build_set = |set: ConstraintSet, store: CredentialStore| {
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
                catalog([("http-probe.fetch", set)]),
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
                credential: Some(credential),
                ..set("http-probe", constraints)
            },
            store,
        )
    };
    let build_override = |credential: String, constraints: ExecutionConstraints| {
        build_set(
            ConstraintSet {
                credential: Some("fetch-token".to_owned()),
                credential_by_agent: BTreeMap::from([(agent("nestedset-github"), credential)]),
                ..set("http-probe", constraints)
            },
            store(),
        )
    };

    // An unnamed credential must fail before any invocation can reference it.
    let error = build(
        "missing-token".to_owned(),
        http(vec!["api.example.test".to_owned()]),
        store(),
    )
    .await
    .expect_err("unknown credential names are refused");
    assert!(matches!(error, BrokerBuildError::UnknownCredential { .. }));

    // The same, for an override: a set is only as proven as the credential nobody validated.
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

    // A credential with no HTTP authority to ride on is a configuration contradiction.
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

    // Coverage is what makes the runtime destination mismatch unreachable: every allowed host
    // must be a destination the credential is explicitly bound to.
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

    // An override is checked against this set's allowed hosts, not against the default's
    // destinations: `other-token` reaches somewhere real, just not where this set may go.
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

/// `via` is the entire reason adding a gateway cannot widen an existing grant, so it has to fail
/// closed in both directions. An attested context must not reach a policy written for directly
/// connected peers, and a direct context must not reach a policy written for an attested one —
/// even when the principal and agent are otherwise identical.
#[tokio::test(flavor = "multi_thread")]
async fn via_isolation_holds_in_both_directions() {
    let audit = Arc::new(InMemoryAuditLog::new(16).expect("valid audit bound"));
    let broker = Broker::new(
        echo_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        "policy-test".to_owned(),
        echo_engine(
            &format!(
                "{}\n{}\n{}",
                direct_policy("caller", "provider-test", "echo.reverse"),
                attested_policy("cpetersen", "some-agent", "gateway", "echo.echo"),
                agent_prompt_policy("cpetersen", "some-agent", "gateway"),
            ),
            ["caller", "cpetersen", "gateway"],
        ),
        catalog([
            ("echo.echo", set("echo", ExecutionConstraints::default())),
            ("echo.reverse", set("echo", ExecutionConstraints::default())),
        ]),
        CredentialStore::empty(),
        directory([(SLACK_SUBJECT, "cpetersen")]),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("direct and attested grants coexist in one policy set");

    let gateway = service_context("gateway");
    let grant = attestor_grant(["slack.t0123abc"]);
    let subject = subject(SLACK_SUBJECT);

    let attested = broker
        .invoke_for(
            &gateway,
            Some(&grant),
            &attestation(&subject, "some-agent", "invoke-attested"),
            request(
                "invoke-attested",
                "echo.echo",
                json!({"message": "on behalf of"}),
            ),
        )
        .await
        .expect("attested invocation is accounted");
    assert_eq!(
        attested.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    assert_eq!(attested.output, Some(json!({"message": "on behalf of"})));

    // The mapped principal arriving as itself, with the same agent actor, is a different context
    // than the attested one and matches nothing.
    let direct = broker
        .invoke(
            &agent_context("cpetersen", "some-agent"),
            request(
                "invoke-direct-as-mapped",
                "echo.echo",
                json!({"message": "direct"}),
            ),
        )
        .await
        .expect("direct proposal is accounted");
    assert_eq!(
        direct.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(direct.error.as_deref(), Some("policy-denied"));
    assert!(
        broker
            .capabilities(&agent_context("cpetersen", "some-agent"))
            .is_empty(),
        "an attested rule must be invisible to the same principal connecting directly"
    );

    // And the attested context cannot borrow authority granted to a direct peer.
    let crossed = broker
        .invoke_for(
            &gateway,
            Some(&grant),
            &attestation(&subject, "some-agent", "invoke-crossed"),
            request(
                "invoke-crossed",
                "echo.reverse",
                json!({"message": "crossed"}),
            ),
        )
        .await
        .expect("attested proposal outside the attested rule is accounted");
    assert_eq!(
        crossed.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(crossed.error.as_deref(), Some("policy-denied"));

    // Capability listings agree with the invocation decisions on both sides of the boundary.
    let visible = broker
        .capabilities_for(&gateway, Some(&grant), &subject, &agent("some-agent"))
        .expect("the attestation is honored")
        .0;
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].capability.id.as_str(), "echo.echo");
    let direct_visible = broker.capabilities(&context("caller"));
    assert_eq!(direct_visible.len(), 1);
    assert_eq!(direct_visible[0].capability.id.as_str(), "echo.reverse");
    assert!(
        broker.capabilities(&gateway).is_empty(),
        "attestor authority is not capability: the gateway holds nothing of its own"
    );

    let records = audit.records().await;
    verify_audit_chain(&records).expect("audit chain verifies");
    assert_eq!(records.len(), 4, "one allow plus execution, two denials");
}

/// A refused attestation is a decision, not an error, and it belongs to the peer that made the
/// claim — no trusted mapping happened, so attributing it to the subject's principal would
/// launder an unauthorized claim into that principal's record. The claimed subject is still
/// recorded, because "which subject did this gateway try to speak for" is the question an
/// operator will ask.
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

    // No attestor authority at all.
    let ungranted = broker
        .invoke_for(
            &gateway,
            None,
            &attestation(&subject, "some-agent", "invoke-no-grant"),
            request("invoke-no-grant", "echo.echo", json!({"message": "claim"})),
        )
        .await
        .expect("a refused attestation is accounted");
    assert_eq!(
        ungranted.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(ungranted.error.as_deref(), Some("attestation-denied"));

    // Authority over a different workspace is not authority over this one.
    let out_of_scope = broker
        .invoke_for(
            &gateway,
            Some(&attestor_grant(["slack.t0999zzz"])),
            &attestation(&subject, "some-agent", "invoke-out-of-scope"),
            request(
                "invoke-out-of-scope",
                "echo.echo",
                json!({"message": "claim"}),
            ),
        )
        .await
        .expect("an out-of-scope attestation is accounted");
    assert_eq!(
        out_of_scope.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(out_of_scope.error.as_deref(), Some("attestation-denied"));

    let records = audit.records().await;
    assert_eq!(records.len(), 2);
    verify_audit_chain(&records).expect("audit chain verifies");
    for record in &records {
        let AuditEvent::Decision {
            principal,
            actor,
            via,
            attested_subject,
            allowed,
            reason,
            ..
        } = &record.event
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

    // A grant that covers the subject but a directory that does not name it is a distinct
    // configuration mistake and gets a distinct reason.
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let broker = attested_broker(IdentityDirectory::empty(), Arc::clone(&audit)).await;
    let unmapped = broker
        .invoke_for(
            &gateway,
            Some(&attestor_grant(["slack.t0123abc"])),
            &attestation(&subject, "some-agent", "invoke-unmapped"),
            request("invoke-unmapped", "echo.echo", json!({"message": "claim"})),
        )
        .await
        .expect("an unmapped subject is accounted");
    assert_eq!(
        unmapped.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(unmapped.error.as_deref(), Some("unmapped-subject"));
    let records = audit.records().await;
    assert_eq!(records.len(), 1);
    let AuditEvent::Decision {
        principal,
        attested_subject,
        reason,
        ..
    } = &records[0].event
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

/// The replay ledger is reserved before the attestation is judged, so a refusal spends the
/// identifier exactly as an allow does. Letting a refused identifier come back with a different
/// claim would make the audit trail ambiguous about which proposal a decision described.
#[tokio::test(flavor = "multi_thread")]
async fn attested_denials_still_consume_the_invocation_identifier() {
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound"));
    let broker = attested_broker(
        directory([(SLACK_SUBJECT, "cpetersen")]),
        Arc::clone(&audit),
    )
    .await;
    let refused = broker
        .invoke_for(
            &service_context("gateway"),
            None,
            &attestation(&subject(SLACK_SUBJECT), "some-agent", "invoke-shared-id"),
            request("invoke-shared-id", "echo.echo", json!({"message": "claim"})),
        )
        .await
        .expect("a refused attestation is accounted");
    assert_eq!(refused.error.as_deref(), Some("attestation-denied"));

    let reused = broker
        .invoke(
            &agent_context("cpetersen", "some-agent"),
            request("invoke-shared-id", "echo.echo", json!({"message": "retry"})),
        )
        .await
        .expect("the reused identifier is accounted");
    assert_eq!(
        reused.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(reused.error.as_deref(), Some("replayed-invocation"));
    assert_eq!(audit.records().await.len(), 2);
}

/// An allowed attested invocation records who it ran as (`principal`), who vouched for it
/// (`via`), and which external identity it stood for (`attestedSubject`) — and nothing about the
/// message that prompted it.
#[tokio::test(flavor = "multi_thread")]
async fn attested_success_audits_via_and_subject() {
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound"));
    let broker = attested_broker(
        directory([(SLACK_SUBJECT, "cpetersen")]),
        Arc::clone(&audit),
    )
    .await;
    let result = broker
        .invoke_for(
            &service_context("gateway"),
            Some(&attestor_grant(["slack.t0123abc"])),
            &attestation(
                &subject(SLACK_SUBJECT),
                "some-agent",
                "invoke-attested-audit",
            ),
            request(
                "invoke-attested-audit",
                "echo.echo",
                json!({"message": "top-secret-payload"}),
            ),
        )
        .await
        .expect("attested invocation is accounted");
    assert_eq!(
        result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );

    let records = audit.records().await;
    assert_eq!(records.len(), 2);
    verify_audit_chain(&records).expect("audit chain verifies");
    let encoded = serde_json::to_value(&records).expect("audit serializes");
    // Event field names are the enum's own snake_case, unlike the camelCase record envelope
    // around them. Asserting the literal wire keys keeps that difference from drifting silently:
    // the record hash is computed over exactly this encoding.
    for (index, kind) in [(0, "decision"), (1, "execution")] {
        let event = &encoded[index]["event"];
        assert_eq!(event["type"], kind);
        assert_eq!(event["principal"], "cpetersen");
        assert_eq!(event["via"], "gateway");
        assert_eq!(event["attested_subject"], SLACK_SUBJECT);
        assert_eq!(
            event["actor"],
            json!({"type": "agent", "agent": "some-agent"})
        );
    }
    assert_eq!(encoded[0]["event"]["allowed"], true);

    let serialized = serde_json::to_string(&records).expect("audit serializes");
    assert!(
        !serialized.contains("top-secret-payload"),
        "a subject is routing metadata; the message it arrived with is not audited"
    );
}

/// `None` and `Some(vec![])` answer different questions, and collapsing them would let a gateway
/// probe the directory: "you may not ask" must not be reported as "this subject has nothing".
#[tokio::test(flavor = "multi_thread")]
async fn capabilities_for_distinguishes_refusal_from_empty() {
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
            .capabilities_for(&gateway, None, &mapped, &agent("some-agent"))
            .is_none(),
        "a peer with no attestor authority learns nothing at all"
    );

    let granted = broker
        .capabilities_for(&gateway, Some(&grant), &mapped, &agent("some-agent"))
        .expect("the attestation is honored")
        .0;
    assert_eq!(granted.len(), 1);
    assert_eq!(granted[0].capability.id.as_str(), "echo.echo");

    // Attested successfully, mapped successfully, and granted nothing: an empty list is the
    // honest answer and is not a refusal.
    let bare = broker
        .capabilities_for(
            &gateway,
            Some(&grant),
            &subject("tel.16034700182"),
            &agent("some-agent"),
        )
        .expect("the attestation is honored for every namespace in the grant")
        .0;
    assert!(bare.is_empty());
}

/// Namespace scoping is prefix matching, and prefix matching without segment boundaries is a
/// tenant-confusion bug: `slack.t0123abc` must not reach into workspace `t0123abcx`.
#[test]
fn attestor_scopes_match_on_segment_boundaries() {
    let grant = attestor_grant(["slack.t0123abc"]);
    grant
        .validate()
        .expect("a service plus one canonical segment is a valid scope");
    assert!(grant.permits(&ExternalSubject::slack("T0123ABC", "U9XYZ").expect("slack subject")));
    assert!(!grant.permits(&ExternalSubject::slack("T0123ABCX", "U9").expect("slack subject")));

    for invalid in [
        // A grant that names nothing cannot be an authority over anything.
        vec![],
        vec!["sms".to_owned()],
        vec!["slack.T0123ABC".to_owned()],
        vec!["slack..u9xyz".to_owned()],
        // Deeper than any canonical subject, so it could only ever match by accident.
        vec!["slack.t0123abc.u9xyz.extra".to_owned()],
    ] {
        let grant = AttestorGrant {
            namespaces: invalid.clone(),
            chat_scopes: Vec::new(),
        };
        assert!(
            grant.validate().is_err(),
            "accepted attestor namespaces {invalid:?}"
        );
    }
}

/// The directory is the only place a subject becomes a principal, so it must be exact: no
/// prefix or fallback resolution, and no subject naming two principals.
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

/// `via` and `attested_subject` are serde defaults that stay absent when empty, so a durable
/// chain written before attestation existed still decodes *and* re-serializes byte for byte. That
/// second half is the load-bearing one: the record hash is a digest of this exact encoding, so a
/// field that appeared as `null` would invalidate every retained record.
#[test]
fn audit_events_written_before_attestation_decode_and_hash_unchanged() {
    let legacy = concat!(
        r#"{"type":"decision","invocation":"invoke-legacy","trace":"trace-legacy","#,
        r#""principal":"caller","actor":{"type":"agent","agent":"provider-test"},"#,
        r#""capability":"echo.echo","provider":"echo","authorized_by":"broker-test","#,
        r#""decision_id":"allow-invoke-legacy","policy_revision":"policy-test","allowed":true,"#,
        r#""decision_digest":"sha256-legacy"}"#,
    );
    let event =
        serde_json::from_str::<AuditEvent>(legacy).expect("pre-attestation records still decode");
    let AuditEvent::Decision {
        via,
        attested_subject,
        ..
    } = &event
    else {
        panic!("the fixture is a decision event");
    };
    assert!(via.is_none());
    assert!(attested_subject.is_none());
    assert_eq!(
        serde_json::to_string(&event).expect("serializes"),
        legacy,
        "an absent attestation must not change the bytes a retained record hashes over"
    );
}

/// The same requirement for the terminal record's `credential`, which per-agent selection added.
///
/// A durable chain written before it existed has to keep hashing to the same value, so the field
/// has to be a skipped-when-absent option rather than a `null`. The name is recorded for the same
/// reason `policy_ids` is: an auditor needs to know which authority a write used, and once one
/// capability can present two, an unnamed one makes two organizations' writes identical.
#[test]
fn execution_records_written_before_per_agent_credentials_decode_and_hash_unchanged() {
    let legacy = concat!(
        r#"{"type":"execution","invocation":"invoke-legacy","trace":"trace-legacy","#,
        r#""principal":"caller","actor":{"type":"agent","agent":"provider-test"},"#,
        r#""capability":"echo.echo","provider":"echo","authorized_by":"broker-test","#,
        r#""decision_id":"allow-invoke-legacy","policy_revision":"policy-test","#,
        r#""effect":"read-only","risk":"Low","idempotency":"idempotent","outcome":"Succeeded","#,
        r#""duration_ms":3,"output_digest":"sha256-legacy"}"#,
    );
    let event =
        serde_json::from_str::<AuditEvent>(legacy).expect("pre-credential records still decode");
    let AuditEvent::Execution { credential, .. } = &event else {
        panic!("the fixture is an execution event");
    };
    assert!(credential.is_none());
    assert_eq!(
        serde_json::to_string(&event).expect("serializes"),
        legacy,
        "an absent credential must not change the bytes a retained record hashes over"
    );
}

/// A capability nothing can execute is refused twice: at startup if any policy could ever permit
/// it, and at decision time with its own reason if it somehow arrives anyway.
#[tokio::test(flavor = "multi_thread")]
async fn a_capability_without_a_constraint_set_fails_closed_at_both_layers() {
    // Startup: the policy names `echo.reverse`, the catalog does not.
    let error = Broker::new(
        echo_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        "policy-test".to_owned(),
        echo_engine(
            &direct_policy("caller", "provider-test", "echo.reverse"),
            ["caller"],
        ),
        catalog([("echo.echo", set("echo", ExecutionConstraints::default()))]),
        CredentialStore::empty(),
        IdentityDirectory::empty(),
        Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound")),
        BrokerLimits::default(),
    )
    .expect_err("a grant nothing knows how to execute must refuse startup");
    assert!(matches!(
        error,
        BrokerBuildError::UnconstrainedCapability { capability } if capability.as_str() == "echo.reverse"
    ));

    // Decision time: a policy that constrains no action can permit anything, so the missing
    // constraint set is the only thing standing between the caller and an unexecutable capability.
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let broker = Broker::new(
        echo_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        "policy-test".to_owned(),
        echo_engine(
            r#"permit(principal == Dekopon::Principal::"caller", action, resource);"#,
            ["caller"],
        ),
        catalog([("echo.echo", set("echo", ExecutionConstraints::default()))]),
        CredentialStore::empty(),
        IdentityDirectory::empty(),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("an unconstrained action scope names no capability, so startup has nothing to check");
    let result = broker
        .invoke(
            &context("caller"),
            request(
                "invoke-unconstrained",
                "echo.reverse",
                json!({"message": "x"}),
            ),
        )
        .await
        .expect("the refusal is accounted");
    assert_eq!(
        result.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(result.error.as_deref(), Some("unconstrained-capability"));
    assert!(
        broker
            .capabilities(&context("caller"))
            .iter()
            .all(|available| available.capability.id.as_str() == "echo.echo"),
        "a capability with no constraint set is never listed"
    );
}

/// A decision is only explainable if the record says which policy made it. The digest is the other
/// half: it says which policy set that identifier belongs to.
#[tokio::test(flavor = "multi_thread")]
async fn audit_records_carry_determining_policy_ids_and_the_policy_digest() {
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound"));
    let broker = Broker::new(
        echo_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        "policy-test".to_owned(),
        echo_engine(
            &format!(
                "@id(\"caller-echo\")\n{}",
                direct_policy("caller", "provider-test", "echo.echo")
            ),
            ["caller", "other-caller"],
        ),
        catalog([("echo.echo", set("echo", ExecutionConstraints::default()))]),
        CredentialStore::empty(),
        IdentityDirectory::empty(),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("the named policy validates");
    let digest = broker.policy_digest().to_owned();
    assert!(digest.starts_with("sha256:"));

    broker
        .invoke(
            &context("caller"),
            request("invoke-explained", "echo.echo", json!({"message": "hi"})),
        )
        .await
        .expect("the allowed invocation is accounted");
    broker
        .invoke(
            &context("other-caller"),
            request("invoke-unexplained", "echo.echo", json!({"message": "hi"})),
        )
        .await
        .expect("the denial is accounted");

    let records = audit.records().await;
    verify_audit_chain(&records).expect("audit chain verifies");
    let encoded = serde_json::to_value(&records).expect("audit serializes");
    // Event fields keep the enum's own snake_case, unlike the camelCase record envelope.
    for index in [0, 1] {
        assert_eq!(
            encoded[index]["event"]["policy_ids"],
            json!(["caller-echo"])
        );
        assert_eq!(encoded[index]["event"]["policy_digest"], json!(digest));
    }
    // A deny-by-default refusal is reached by no policy, so the absent list is the explanation and
    // the field stays off the wire entirely.
    assert_eq!(encoded[2]["event"]["reason"], "policy-denied");
    assert!(encoded[2]["event"].get("policy_ids").is_none());
    assert_eq!(encoded[2]["event"]["policy_digest"], json!(digest));
}

/// Leniency moves *when* the broker complains, never *whether* it enforces.
///
/// The configuration here is byte for byte what
/// [`a_capability_without_a_constraint_set_fails_closed_at_both_layers`] proves refuses startup.
/// Tolerating it must still deny the invocation with the same reason: the startup check is a
/// tripwire, and the decision path is the enforcement.
#[tokio::test(flavor = "multi_thread")]
async fn tolerating_an_unconstrained_capability_warns_but_still_denies_it() {
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let (broker, warnings) = Broker::start(
        echo_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        "policy-test".to_owned(),
        echo_engine(
            &direct_policy("caller", "provider-test", "echo.reverse"),
            ["caller"],
        ),
        catalog([("echo.echo", set("echo", ExecutionConstraints::default()))]),
        CredentialStore::empty(),
        IdentityDirectory::empty(),
        Arc::clone(&audit),
        BrokerLimits::default(),
        Leniency::Tolerant,
        std::iter::empty(),
    )
    .expect("tolerating an unexecutable grant starts");

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(matches!(
        &warnings[0],
        StartupWarning::UnconstrainedCapability { capability }
            if capability.as_str() == "echo.reverse"
    ));

    // The part that matters: enforcement is untouched.
    let result = broker
        .invoke(
            &context("caller"),
            request("invoke-tolerated", "echo.reverse", json!({"message": "x"})),
        )
        .await
        .expect("the refusal is accounted");
    assert_eq!(
        result.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(result.error.as_deref(), Some("unconstrained-capability"));
    assert!(
        broker
            .capabilities(&context("caller"))
            .iter()
            .all(|available| available.capability.id.as_str() == "echo.echo"),
        "a tolerated capability is still never listed"
    );
}

/// A constraint set for a provider that is not loaded is inert either way; tolerating it lets an
/// operator keep configuration for a provider they have not dropped in yet.
#[tokio::test(flavor = "multi_thread")]
async fn tolerating_a_constraint_set_that_routes_nowhere_drops_it() {
    let unrouted = [
        ("echo.echo", set("echo", ExecutionConstraints::default())),
        (
            "gh.pull-request.read",
            set("gh", ExecutionConstraints::default()),
        ),
    ];

    let error = Broker::new(
        echo_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        "policy-test".to_owned(),
        echo_engine(
            &direct_policy("caller", "provider-test", "echo.echo"),
            ["caller"],
        ),
        catalog(unrouted.clone()),
        CredentialStore::empty(),
        IdentityDirectory::empty(),
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
        echo_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        "policy-test".to_owned(),
        echo_engine(
            &direct_policy("caller", "provider-test", "echo.echo"),
            ["caller"],
        ),
        catalog(unrouted),
        CredentialStore::empty(),
        IdentityDirectory::empty(),
        Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound")),
        BrokerLimits::default(),
        Leniency::Tolerant,
        std::iter::empty(),
    )
    .expect("tolerating an unrouted constraint set starts");

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(matches!(
        &warnings[0],
        StartupWarning::UnroutedConstraintSet { capability }
            if capability.as_str() == "gh.pull-request.read"
    ));
    assert_eq!(warnings[0].reason(), "unrouted-constraint-set");

    // The routed half of the same catalog still works.
    let result = broker
        .invoke(
            &context("caller"),
            request("invoke-routed", "echo.echo", json!({"message": "x"})),
        )
        .await
        .expect("the routed capability still executes");
    assert_eq!(
        result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
}

/// Command words are filtered by policy exactly as capabilities are.
///
/// A session is never told a word exists that it could not use, so a principal granted nothing
/// receives an empty vocabulary rather than a map of the deployment's providers.
#[tokio::test(flavor = "multi_thread")]
async fn command_words_are_filtered_by_what_policy_allows() {
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let (broker, _) = Broker::start(
        echo_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        "policy-test".to_owned(),
        echo_engine(
            &direct_policy("caller", "provider-test", "echo.echo"),
            ["caller", "stranger"],
        ),
        catalog([("echo.echo", set("echo", ExecutionConstraints::default()))]),
        CredentialStore::empty(),
        IdentityDirectory::empty(),
        Arc::clone(&audit),
        BrokerLimits::default(),
        Leniency::Strict,
        std::iter::empty(),
    )
    .expect("broker starts");

    // The exact fetched echo fixture declares no command words, so both are empty — but the granted
    // and ungranted contexts must agree on that for the right reason, which the capability lists
    // below establish.
    assert!(broker.command_words(&context("caller")).is_empty());
    assert!(broker.command_words(&context("stranger")).is_empty());
    assert_eq!(broker.capabilities(&context("caller")).len(), 1);
    assert!(
        broker.capabilities(&context("stranger")).is_empty(),
        "the ungranted context reaches nothing, which is what makes its empty vocabulary meaningful"
    );
    for name in ["caller", "stranger"] {
        assert_eq!(
            broker.capability_view(&context(name)),
            (
                broker.capabilities(&context(name)),
                broker.command_words(&context(name))
            ),
            "the combined view must be the same answer as the two listings it replaces"
        );
    }
}

/// A word no loaded provider declares is refused before any component runs.
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_command_word_is_refused_without_running_anything() {
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let (broker, _) = Broker::start(
        echo_registry(BrokerHostLimits::default()).await,
        principal("broker-test"),
        "policy-test".to_owned(),
        echo_engine(
            &direct_policy("caller", "provider-test", "echo.echo"),
            ["caller"],
        ),
        catalog([("echo.echo", set("echo", ExecutionConstraints::default()))]),
        CredentialStore::empty(),
        IdentityDirectory::empty(),
        Arc::clone(&audit),
        BrokerLimits::default(),
        Leniency::Strict,
        std::iter::empty(),
    )
    .expect("broker starts");

    let error = broker
        .resolve_command("gh", &["gh".to_owned(), "pr".to_owned()])
        .await
        .expect_err("no loaded provider declares this word");
    assert!(
        format!("{error}").contains("gh"),
        "the refusal names the word: {error}"
    );
}
