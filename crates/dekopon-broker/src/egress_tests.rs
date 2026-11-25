use std::{
    collections::BTreeMap,
    num::NonZeroU64,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_broker_protocol::{Grant, GrantId, UsageClasses};
use dekopon_capability::{
    EffectKind, ExecutionConstraints, HttpConstraints, HttpPathRule, InvocationOutcome,
};
use dekopon_core::{
    Actor, AgentId, CapabilityId, InvocationId, PrincipalId, Redacted, RiskLevel, SecretDrn,
    SecretSinkKind, Subject,
    base64::{Engine as _, URL_SAFE_NO_PAD},
};
use dekopon_test_support::{LoopbackServer, provider_fixture};
use serde_json::{Value, json};

use crate::{
    AssetInvocationResult, Attestation, AttestorGrant, AuditEvent, AuditSink, AuthenticatedContext,
    Broker, BrokerLimits, CapabilityRoute, ConstraintCatalog, ConstraintSet, CookieSet,
    DefaultSecretSink, DefaultSecretUse, EgressError, EgressProfile, EgressProfileMaterial,
    EgressPush, EgressReturn, IdentityDirectory, InMemoryAuditLog, InvocationRequest, PolicyEngine,
    PolicyWorld, PushedToken, SecretCatalog, SecretMaterial, SecretResolutionError, SecretResolver,
    SecretUseBinding, TokenTarget, TraceOnlyAuditLog,
};

const SUBJECT: &str = "drn:slack:user:t0123abc:u9xyz";
const SPACE_SUBJECT: &str = "drn:slack:channel:t0123abc:c0123abc";
const AGENT: &str = "provider-test";
const CAPABILITY: &str = "http-probe.fetch";
const RUNNER: &str = "drn:secret:vm/runner";
const PROFILE: &str = "drn:secret:vm/egress-default";
const AGENT_PROFILE: &str = "drn:secret:vm/egress-agent";
const RETURN_HEADER: &str = "x-dekopon-egress-return";
const PUSH_HEADER: &str = "x-dekopon-egress-push";

#[derive(Debug)]
struct ProfileResolver {
    runner: String,
    tokens: BTreeMap<SecretDrn, String>,
    pushes: AtomicUsize,
}

impl ProfileResolver {
    fn new(tokens: impl IntoIterator<Item = (&'static str, String)>) -> Arc<Self> {
        Arc::new(Self {
            runner: generated_token("runner-bearer"),
            tokens: tokens
                .into_iter()
                .map(|(drn, token)| (drn.parse().expect("profile DRN"), token))
                .collect(),
            pushes: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl SecretResolver for ProfileResolver {
    async fn resolve(&self, secret: &SecretDrn) -> Result<SecretMaterial, SecretResolutionError> {
        if secret.as_str() == RUNNER {
            Ok(SecretMaterial::new(self.runner.as_bytes().to_vec()))
        } else {
            Err(SecretResolutionError {
                category: "missing",
            })
        }
    }

    async fn egress_profile(
        &self,
        profile: &SecretDrn,
    ) -> Result<EgressProfileMaterial, SecretResolutionError> {
        self.pushes.fetch_add(1, Ordering::SeqCst);
        let token = self.tokens.get(profile).ok_or(SecretResolutionError {
            category: "missing",
        })?;
        Ok(material(token))
    }
}

fn material(token: &str) -> EgressProfileMaterial {
    EgressProfileMaterial {
        cookies: CookieSet::default(),
        tokens: vec![PushedToken {
            token: Redacted::new(token.to_owned()),
            on: vec![TokenTarget::Bearer {
                host: "chatgpt.com".to_owned(),
                paths: None,
            }],
        }],
    }
}

fn generated_token(label: &str) -> String {
    let mut bytes = [0_u8; 24];
    getrandom::fill(&mut bytes).expect("random token bytes");
    format!("{label}-{}", URL_SAFE_NO_PAD.encode(bytes))
}

fn principal(name: &str) -> PrincipalId {
    name.parse().expect("principal")
}

fn capability() -> CapabilityId {
    CAPABILITY.parse().expect("capability")
}

fn policy(forbidden_profiles: &[&str]) -> String {
    let mut policy = format!(
        r#"@id("fetch") permit(principal,
            action == Dekopon::Action::"{CAPABILITY}", resource == Dekopon::Provider::"http-probe");
        @id("prompt") permit(principal,
            action == Dekopon::Action::"agent.prompt", resource == Dekopon::Agent::"{AGENT}");
        @id("runner") permit(principal,
            action == Dekopon::Action::"secret.use", resource == Dekopon::Secret::"{RUNNER}");
        "#
    );
    for (index, profile) in forbidden_profiles.iter().enumerate() {
        policy.push_str(&format!(
            r#"@id("profile-{index}") forbid(principal,
                action == Dekopon::Action::"secret.use", resource == Dekopon::Secret::"{profile}")
                when {{ context.sink == "vmEgressProfile" }};
            "#
        ));
    }
    policy
}

fn vm_set(authority: &str) -> ConstraintSet {
    ConstraintSet {
        default_secret_use: Some(DefaultSecretUse {
            drn: RUNNER.parse().expect("runner DRN"),
            sink: DefaultSecretSink::HttpBearer,
        }),
        egress_profile: Some(EgressProfile {
            drn: PROFILE.parse().expect("profile DRN"),
        }),
        route: CapabilityRoute::Generic,
        provider: "http-probe".parse().expect("provider"),
        effect: EffectKind::ReadOnly,
        risk: RiskLevel::Low,
        constraints: ExecutionConstraints {
            asset: None,
            timeout_ms: 5_000,
            http: Some(HttpConstraints {
                allowed_hosts: vec![authority.to_owned()],
                allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
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
        },
    }
}

fn runner_binding(authority: &str) -> SecretUseBinding {
    SecretUseBinding {
        binding_id: "vm-runner".to_owned(),
        secret: RUNNER.parse().expect("runner DRN"),
        capabilities: [capability()].into(),
        sink: SecretSinkKind::HttpBearer,
        basic_username: None,
        allowed_hosts: vec![authority.to_owned()],
        allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
        allowed_paths: vec![HttpPathRule::SegmentPrefix {
            path: "/v1/sessions".to_owned(),
        }],
        allow_query: false,
        max_injections: 1,
    }
}

async fn vm_broker(
    authority: &str,
    resolver: Arc<ProfileResolver>,
    forbidden_profiles: &[&str],
    swaps: BTreeMap<AgentId, BTreeMap<SecretDrn, SecretDrn>>,
) -> Broker {
    let audit = AuditSink::TraceOnly(TraceOnlyAuditLog);
    audited_vm_broker(
        authority,
        resolver,
        &policy(forbidden_profiles),
        swaps,
        audit,
    )
    .await
}

async fn audited_vm_broker(
    authority: &str,
    resolver: Arc<ProfileResolver>,
    policy: &str,
    swaps: BTreeMap<AgentId, BTreeMap<SecretDrn, SecretDrn>>,
    audit: AuditSink,
) -> Broker {
    let _memoization = dekopon_broker_host::enable_test_component_memoization();
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("HTTP provider fixture loads");
    let world = PolicyWorld::new(
        [principal("caller"), principal("space")],
        [(capability(), "http-probe".parse().expect("provider"))],
    )
    .expect("world")
    .with_secrets([RUNNER, PROFILE, AGENT_PROFILE].map(|drn| drn.parse().expect("secret DRN")));
    Broker::new(
        registry,
        principal("broker-test"),
        PolicyEngine::new(policy, &world).expect("policy"),
        ConstraintCatalog::new([(capability(), vm_set(authority))])
            .expect("catalog")
            .with_agent_secrets(swaps),
        IdentityDirectory::new([
            (SUBJECT.parse().expect("subject"), principal("caller")),
            (
                SPACE_SUBJECT.parse().expect("space subject"),
                principal("space"),
            ),
        ])
        .expect("directory"),
        Arc::new(audit),
        BrokerLimits::default(),
    )
    .expect("broker")
    .with_secret_catalog(
        SecretCatalog::new(vec![runner_binding(authority)], resolver).expect("secret catalog"),
    )
    .expect("the runner default has one binding")
}

async fn invoke(broker: &Broker, id: &str, input: Value) -> (AssetInvocationResult, String) {
    invoke_scoped(broker, id, input, None).await
}

async fn invoke_scoped(
    broker: &Broker,
    id: &str,
    input: Value,
    scope: Option<dekopon_broker_protocol::ChatScopeClaim>,
) -> (AssetInvocationResult, String) {
    let gateway = AuthenticatedContext::new(
        principal("gateway"),
        Actor::Service {
            principal: principal("gateway"),
        },
    )
    .expect("gateway context");
    let invocation = id.parse::<InvocationId>().expect("invocation id");
    let subjects = dekopon_core::AttestedSubject::one(SUBJECT.parse::<Subject>().expect("subject"))
        .expect("one-form subject");
    let agent = AGENT.parse::<AgentId>().expect("agent");
    let attestation = match scope {
        Some(scope) => Attestation::for_chat(subjects, agent, scope),
        None => Attestation::for_subject(subjects, agent),
    }
    .bound_to(invocation.clone());
    let (host, mut stdout) = std::os::unix::net::UnixStream::pair().expect("stdout pipe");
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
            InvocationRequest {
                id: invocation,
                capability: capability(),
                trace_parent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                    .parse()
                    .expect("traceparent"),
                input,
                secret_use: None,
            },
            dekopon_broker_host::asset::AssetInputs {
                streams: Some(dekopon_broker_host::Streams {
                    stdin: None,
                    stdout: host.into(),
                }),
                ..Default::default()
            },
        )
        .await
        .expect("the broker answers");
    let stdout = capture.join().expect("stdout capture thread");
    (
        result,
        String::from_utf8(stdout).expect("provider stdout is UTF-8"),
    )
}

fn pushed(request: &str) -> Value {
    let lines = request
        .lines()
        .filter_map(|line| line.strip_prefix(&format!("{PUSH_HEADER}: ")))
        .collect::<Vec<_>>();
    let [value] = lines[..] else {
        panic!("exactly one push header: {request}");
    };
    let encoded = value.strip_prefix("v1.").expect("versioned push");
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).expect("base64url push"))
        .expect("JSON push")
}

fn response(extra: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{extra}Content-Length: 2\r\nConnection: close\r\n\r\n{{}}"
    )
    .into_bytes()
}

fn encoded_return(applied: bool) -> String {
    format!(
        "v1.{}",
        URL_SAFE_NO_PAD.encode(json!({ "applied": applied }).to_string())
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_vm_exec_push_leaves_in_the_host_owned_header_and_never_in_provider_memory() {
    let token = generated_token("vm-access");
    let returned = encoded_return(true);
    let runner = LoopbackServer::once(&response(&format!("{RETURN_HEADER}: {returned}\r\n")));
    let authority = runner.authority().to_owned();
    let resolver = ProfileResolver::new([(PROFILE, token.clone())]);
    let runner_token = resolver.runner.clone();
    let broker = vm_broker(&authority, resolver, &[], BTreeMap::new()).await;

    let (result, stdout) = invoke(
        &broker,
        "vm-exec",
        json!({
            "uri": format!("http://{authority}/v1/sessions/s-1/exec"),
            "method": "POST",
            "body": "{\"argv\":[\"true\"],\"deadlineMs\":1000}",
        }),
    )
    .await;

    assert_eq!(result.result.outcome, InvocationOutcome::Succeeded);
    let request = runner.request_text();
    assert!(
        request.starts_with("POST /v1/sessions/s-1/exec HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(request.contains(&format!("authorization: Bearer {runner_token}")));
    assert_eq!(
        pushed(&request),
        json!({
            "profile": PROFILE,
            "cookies": [],
            "tokens": [{"token": token, "on": [{"form": "bearer", "host": "chatgpt.com"}]}],
        })
    );
    assert!(runner.recorded().is_empty(), "exactly one request");
    runner.join();
    let summary: Value = serde_json::from_str(stdout.trim()).expect("probe summary");
    assert_eq!(summary["status"], 200);
    assert_eq!(summary["headerCount"], 2);
    assert!(!stdout.contains(&token));
    assert!(!stdout.contains(&returned));
    assert!(!stdout.contains("v1."));
}

#[tokio::test(flavor = "multi_thread")]
async fn every_vm_call_under_an_egress_profile_carries_the_push() {
    let token = generated_token("vm-access");
    let calls = [
        ("POST", "/v1/sessions"),
        ("POST", "/v1/sessions/s-1/exec"),
        ("GET", "/v1/sessions/s-1/jobs/j-1"),
        ("GET", "/v1/sessions/s-1/artifacts/a-1"),
    ];
    let runner = LoopbackServer::serving(&response(""), calls.len());
    let authority = runner.authority().to_owned();
    let resolver = ProfileResolver::new([(PROFILE, token.clone())]);
    let broker = vm_broker(&authority, Arc::clone(&resolver), &[], BTreeMap::new()).await;

    for (index, (method, path)) in calls.into_iter().enumerate() {
        let (result, _) = invoke(
            &broker,
            &format!("vm-call-{index}"),
            json!({ "uri": format!("http://{authority}{path}"), "method": method }),
        )
        .await;
        assert_eq!(result.result.outcome, InvocationOutcome::Succeeded);
        let request = runner.request_text();
        assert!(
            request.starts_with(&format!("{method} {path} HTTP/1.1\r\n")),
            "{request}"
        );
        assert_eq!(pushed(&request)["profile"], PROFILE);
        assert_eq!(pushed(&request)["tokens"][0]["token"], token.as_str());
        assert!(pushed(&request).get("grant").is_none());
    }
    assert!(runner.recorded().is_empty());
    runner.join();
    assert_eq!(resolver.pushes.load(Ordering::SeqCst), calls.len());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_push_is_refused_naming_the_profile() {
    let profile: SecretDrn = PROFILE.parse().expect("profile DRN");
    let oversized = EgressPush {
        profile: profile.clone(),
        grant: None,
        material: material(&generated_token("oversized").repeat(600)),
    };
    assert!(matches!(
        oversized.encode(),
        Err(EgressError::PushTooLarge { profile: named }) if named == profile
    ));

    let runner = LoopbackServer::stalled();
    let authority = runner.authority().to_owned();
    let resolver = ProfileResolver::new([(PROFILE, generated_token("oversized").repeat(600))]);
    let broker = vm_broker(&authority, resolver, &[], BTreeMap::new()).await;
    let (result, _) = invoke(
        &broker,
        "vm-oversized",
        json!({ "uri": format!("http://{authority}/v1/sessions"), "method": "POST" }),
    )
    .await;
    assert_eq!(result.result.outcome, InvocationOutcome::Failed);
    assert_eq!(
        result.result.error.as_deref(),
        Some("egress-push-too-large")
    );
    let detail = result
        .result
        .detail
        .as_ref()
        .expect("the failure names its profile");
    assert_eq!(detail.code, "egress-push-too-large");
    assert!(detail.message.contains(PROFILE), "{detail}");
    assert!(
        runner.recorded().is_empty(),
        "no request reached the runner"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cedar_secret_use_chooses_the_pushed_set() {
    let default_token = generated_token("default-access");
    let agent_token = generated_token("agent-access");
    let swaps = BTreeMap::from([(
        AGENT.parse::<AgentId>().expect("agent"),
        BTreeMap::from([(
            PROFILE.parse().expect("profile DRN"),
            AGENT_PROFILE.parse().expect("agent profile DRN"),
        )]),
    )]);

    let runner = LoopbackServer::once(&response(""));
    let authority = runner.authority().to_owned();
    let resolver = ProfileResolver::new([
        (PROFILE, default_token.clone()),
        (AGENT_PROFILE, agent_token.clone()),
    ]);
    let broker = vm_broker(&authority, resolver, &[PROFILE], swaps.clone()).await;
    let (result, _) = invoke(
        &broker,
        "vm-agent",
        json!({ "uri": format!("http://{authority}/v1/sessions"), "method": "POST" }),
    )
    .await;
    assert_eq!(result.result.outcome, InvocationOutcome::Succeeded);
    let request = runner.request_text();
    assert_eq!(pushed(&request)["profile"], AGENT_PROFILE);
    assert_eq!(pushed(&request)["tokens"][0]["token"], agent_token.as_str());
    assert!(!request.contains(&default_token));
    runner.join();

    let runner = LoopbackServer::stalled();
    let authority = runner.authority().to_owned();
    let resolver = ProfileResolver::new([(PROFILE, default_token), (AGENT_PROFILE, agent_token)]);
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("audit bound"));
    let broker = audited_vm_broker(
        &authority,
        Arc::clone(&resolver),
        &policy(&[AGENT_PROFILE]),
        swaps,
        AuditSink::InMemory(Arc::clone(&audit)),
    )
    .await;
    let (result, _) = invoke(
        &broker,
        "vm-denied",
        json!({ "uri": format!("http://{authority}/v1/sessions"), "method": "POST" }),
    )
    .await;
    assert_eq!(result.result.outcome, InvocationOutcome::Failed);
    assert_eq!(result.result.error.as_deref(), Some("egress-denied"));
    let detail = result
        .result
        .detail
        .as_ref()
        .expect("the denial names its profile");
    assert_eq!(detail.code, "egress-denied");
    assert!(detail.message.contains(AGENT_PROFILE), "{detail}");
    assert_eq!(resolver.pushes.load(Ordering::SeqCst), 0);
    assert!(
        runner.recorded().is_empty(),
        "no request reached the runner"
    );
    let records = audit.records();
    let [
        AuditEvent::Decision {
            decision_point: invocation_point,
            allowed: true,
            ..
        },
        AuditEvent::Decision {
            decision_point,
            secret,
            secret_sink,
            policy_ids,
            allowed: false,
            ..
        },
        AuditEvent::Execution {
            policy_ids: failure_policy_ids,
            ..
        },
    ] = &records[..]
    else {
        panic!("an invocation decision, a denied egress decision and a failure: {records:?}");
    };
    assert_eq!(invocation_point, "invocation");
    assert_eq!(decision_point, "egress-profile");
    assert_eq!(secret.as_ref().map(SecretDrn::as_str), Some(AGENT_PROFILE));
    assert_eq!(*secret_sink, Some(SecretSinkKind::VmEgressProfile));
    assert_eq!(policy_ids, &["profile-0"]);
    assert_eq!(failure_policy_ids, &["profile-0"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_space_denied_egress_push_keeps_the_space_decision_in_its_record() {
    let runner = LoopbackServer::stalled();
    let authority = runner.authority().to_owned();
    let resolver = ProfileResolver::new([(PROFILE, generated_token("vm-access"))]);
    let audit = Arc::new(InMemoryAuditLog::new(8).expect("audit bound"));
    let space_forbid = format!(
        r#"@id("space-forbid") forbid(principal == Dekopon::Principal::"space",
            action == Dekopon::Action::"secret.use", resource == Dekopon::Secret::"{PROFILE}")
            when {{ context.sink == "vmEgressProfile" }};"#
    );
    let broker = audited_vm_broker(
        &authority,
        Arc::clone(&resolver),
        &(policy(&[]) + &space_forbid),
        BTreeMap::new(),
        AuditSink::InMemory(Arc::clone(&audit)),
    )
    .await;
    let scope = dekopon_broker_protocol::ChatScopeClaim {
        participant: None,
        github: None,
        transport: "chat".parse().expect("transport"),
        kind: dekopon_broker_protocol::ChatTransportKind::Slack,
        conversation: dekopon_broker_protocol::Conversation {
            kind: dekopon_broker_protocol::ConversationKind::Thread,
            container: Some("t0123abc".into()),
            id: "c0123abc".into(),
            thread: Some("1712345678.000100".into()),
        },
        trigger: dekopon_broker_protocol::Trigger::Message,
        delivery: Some(dekopon_broker_protocol::DeliveryIdentity::Slack {
            channel: "c0123abc".into(),
            timestamp: "1712345678.000101".into(),
        }),
    };

    let (result, _) = invoke_scoped(
        &broker,
        "vm-space-denied",
        json!({ "uri": format!("http://{authority}/v1/sessions"), "method": "POST" }),
        Some(scope),
    )
    .await;

    assert_eq!(result.result.error.as_deref(), Some("egress-denied"));
    assert_eq!(resolver.pushes.load(Ordering::SeqCst), 0);
    assert!(
        runner.recorded().is_empty(),
        "no request reached the runner"
    );
    let records = audit.records();
    let Some(AuditEvent::Decision {
        authority,
        provider,
        allowed: false,
        reason,
        ..
    }) = records.iter().find(|record| {
        matches!(record, AuditEvent::Decision { decision_point, .. } if decision_point == "egress-profile")
    })
    else {
        panic!("a denied egress decision: {records:?}");
    };
    let space = authority
        .space
        .as_ref()
        .expect("the space decision is kept");
    assert_eq!(space.id.as_str(), "space");
    assert_eq!(space.asker_allowed, Some(true));
    assert_eq!(space.allowed, Some(false));
    assert_eq!(space.policy_ids, ["space-forbid"]);
    assert_eq!(
        provider.as_ref().map(dekopon_core::ProviderId::as_str),
        Some("http-probe")
    );
    assert_eq!(reason.as_deref(), Some("egress-denied"));
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_exec_push_carries_the_grant() {
    let runner = LoopbackServer::stalled();
    let authority = runner.authority().to_owned();
    let resolver = ProfileResolver::new([(PROFILE, generated_token("vm-access"))]);
    let broker = vm_broker(&authority, resolver, &[], BTreeMap::new()).await;
    let grant = Grant {
        id: GrantId::new(NonZeroU64::MIN),
        amount: NonZeroU64::new(4_096).expect("nonzero amount"),
        weights: UsageClasses::default(),
    };
    let (headers, _) = broker
        .egress_push(&PROFILE.parse().expect("profile DRN"), Some(grant))
        .await
        .expect("the push encodes");
    let decode = |value: &Redacted<String>| -> Value {
        let encoded = value.expose().strip_prefix("v1.").expect("versioned");
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).expect("base64url")).expect("JSON")
    };
    assert!(decode(&headers.every).get("grant").is_none());
    assert_eq!(
        decode(&headers.exec)["grant"],
        serde_json::to_value(grant).expect("grant JSON")
    );
}

#[test]
fn a_return_decodes_only_from_its_versioned_shape() {
    assert_eq!(
        EgressReturn::decode(encoded_return(false).as_bytes()),
        Some(EgressReturn::NotApplied)
    );
    assert_eq!(
        EgressReturn::decode(encoded_return(true).as_bytes()),
        Some(EgressReturn::Applied {
            rotations: Vec::new(),
            tallies: Vec::new(),
        })
    );
    let unknown = format!(
        "v1.{}",
        URL_SAFE_NO_PAD.encode(json!({"applied": true, "extra": 1}).to_string())
    );
    assert_eq!(EgressReturn::decode(unknown.as_bytes()), None);
    assert_eq!(EgressReturn::decode(b"applied"), None);
}

#[test]
fn the_return_bound_counts_the_version_prefix() {
    let mut padded = json!({ "applied": true }).to_string();
    padded.push_str(&" ".repeat(12_286 - padded.len()));
    let encoded = URL_SAFE_NO_PAD.encode(padded);
    assert_eq!(encoded.len(), 16_382);
    assert_eq!(
        EgressReturn::decode(format!("v1.{encoded}").as_bytes()),
        None
    );
}
