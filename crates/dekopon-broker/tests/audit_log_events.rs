//! Every broker decision is one structured log record inside the span that made it.
//!
//! The audit record used to exist only in the JSONL file: `policy_ids`, `policy_digest`,
//! `policy_revision`, `secret_sink`, and the sanitized HTTP evidence reached no telemetry at all,
//! so an operator holding a complete trace still could not say which policy authorized a write.
//! These tests hold the emission and its containment together — the record carries the symbolic
//! credential name and never the bytes behind it — and they run against a sink that writes
//! nothing, because the log record is the audit record whether or not a file was configured.
//!
//! This lives in its own test binary because `tracing` resolves per-callsite interest against the
//! global dispatcher: a sibling test that reached these callsites with no subscriber installed
//! would disable them for the whole process.

use std::{collections::BTreeMap, sync::Arc};

use dekopon_broker::{
    AuthenticatedContext, Broker, BrokerLimits, CapabilityRoute, ConstraintCatalog, ConstraintSet,
    CredentialStore, IdentityDirectory, InvocationRequest, PolicyEngine, PolicyWorld,
    TraceOnlyAuditLog,
};
use dekopon_broker_host::{BoundCredential, BrokerHostLimits, BrokerProviderRegistry};
use dekopon_capability::{
    EffectKind, ExecutionConstraints, HttpConstraints, Idempotency, InvocationOutcome,
};
use dekopon_core::{
    Actor, AgentId, CapabilityId, InvocationId, PrincipalId, ProviderId, Redacted, RiskLevel,
};
use dekopon_test_support::{CaptureLayer, LoopbackServer, Record, provider_fixture};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

/// The credential value the audit record must never carry, in any field, at any level.
const SECRET: &str = "audit-log-record-must-never-see-this";

const POLICY: &str = r#"
@id("http-fetch")
permit(principal == Dekopon::Principal::"caller",
       action == Dekopon::Action::"http-probe.fetch",
       resource == Dekopon::Provider::"http-probe")
when { context has agent && context.agent == "provider-test" };
"#;

fn principal(name: &str) -> PrincipalId {
    name.parse().expect("valid principal fixture")
}

fn caller() -> AuthenticatedContext {
    AuthenticatedContext::new(
        principal("caller"),
        Actor::Agent {
            agent: "provider-test".parse::<AgentId>().expect("valid agent"),
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
        trace_parent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
            .parse()
            .expect("valid traceparent fixture"),
        input,
        secret_use: None,
    }
}

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

/// Every captured audit record: its fields and the span the subscriber attributed it to.
fn audit_records(captured: &CaptureLayer) -> Vec<(String, Option<String>)> {
    captured
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::Event {
                target,
                fields,
                parent,
                ..
            } if target == "dekopon_broker::audit" => Some((fields, parent)),
            _ => None,
        })
        .collect()
}

/// The one captured record whose fields contain every needle, with the span it was attributed to.
fn only<'a>(
    records: &'a [(String, Option<String>)],
    needles: &[&str],
) -> (&'a str, Option<&'a str>) {
    let mut matching = records
        .iter()
        .filter(|(fields, _)| needles.iter().all(|needle| fields.contains(needle)));
    let (fields, parent) = matching.next().unwrap_or_else(|| {
        panic!("no audit record matched {needles:?} among {records:?}");
    });
    assert!(
        matching.next().is_none(),
        "more than one audit record matched {needles:?} among {records:?}"
    );
    (fields.as_str(), parent.as_deref())
}

/// One allowed credentialed execution and one refusal, with no durable sink behind either.
#[tokio::test(flavor = "multi_thread")]
async fn each_decision_emits_one_audit_record_inside_its_own_span() {
    let captured = CaptureLayer::workspace();
    tracing_subscriber::registry().with(captured.clone()).init();

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
                route: CapabilityRoute::Generic,
                provider: "http-probe".parse::<ProviderId>().expect("provider"),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                idempotency: Idempotency::Idempotent,
                credential: Some("fetch-token".to_owned()),
                credential_by_agent: BTreeMap::new(),
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
        IdentityDirectory::empty(),
        // No file: the record is the log event, and nothing else is configured to keep it.
        Arc::new(TraceOnlyAuditLog),
        BrokerLimits::default(),
    )
    .expect("the credentialed constraint set matches store and destinations");

    let allowed = broker
        .invoke(
            &caller(),
            None,
            None,
            request(
                "invoke-audited",
                "http-probe.fetch",
                serde_json::json!({ "uri": format!("http://{authority}/pulls/7"), "method": "GET" }),
            ),
        )
        .await
        .expect("authorized credentialed request succeeds");
    assert_eq!(allowed.outcome, InvocationOutcome::Succeeded);
    server.join();

    let denied = broker
        .invoke(
            &caller(),
            None,
            None,
            request("invoke-refused", "http-probe.absent", serde_json::json!({})),
        )
        .await
        .expect("an unconstrained capability is still an accounted decision");
    assert_eq!(denied.outcome, InvocationOutcome::Denied);

    let records = audit_records(&captured);
    assert_eq!(
        records.len(),
        3,
        "one decision each and one execution, no more and no fewer: {records:?}"
    );

    // The allowed decision, inside the span that made it. An allow is appended after the
    // authorization block has handed off, so it belongs to `broker.execute`; only a refusal is
    // recorded from inside `broker.authorize`. Both descend from the caller's `broker.invocation`.
    let (decision, parent) = only(
        &records,
        &["broker.decision", "invocation.id=invoke-audited"],
    );
    assert_eq!(parent, Some("broker.execute"), "{decision}");
    for expected in [
        "capability.id=http-probe.fetch",
        "decision.allowed=true",
        "principal=\"caller\"",
        "actor.kind=\"agent\"",
        "actor.id=\"provider-test\"",
        "provider=\"http-probe\"",
        "policy.revision=\"policy-test\"",
        // Every one of these reached the JSONL file and nothing else before this change.
        "policy.ids=\"http-fetch\"",
        "policy.digest=",
    ] {
        assert!(
            decision.contains(expected),
            "{expected} missing: {decision}"
        );
    }

    // The terminal execution record, inside `broker.execute`.
    let (execution, parent) = only(&records, &["broker.execution"]);
    assert_eq!(parent, Some("broker.execute"), "{execution}");
    for expected in [
        "invocation.id=invoke-audited",
        "outcome=Succeeded",
        "effect=ReadOnly",
        "risk=Low",
        // The symbolic name owner configuration holds, never the bytes behind it.
        "credential=\"fetch-token\"",
        "\\\"credentialInjected\\\":true",
        "output.digest=",
    ] {
        assert!(
            execution.contains(expected),
            "{expected} missing: {execution}"
        );
    }
    // The refusal, with no policy to name: deny-by-default is its own explanation.
    let (refusal, parent) = only(&records, &["invocation.id=invoke-refused"]);
    assert_eq!(parent, Some("broker.authorize"), "{refusal}");
    assert!(refusal.contains("decision.allowed=false"), "{refusal}");
    assert!(
        refusal.contains("decision.reason=\"unconstrained-capability\""),
        "{refusal}"
    );
    assert!(!refusal.contains("policy.ids="), "{refusal}");

    // Containment: no captured record — audit or otherwise — may carry the credential.
    let everything = captured.events_text() + &captured.spans_text();
    assert!(!everything.contains(SECRET), "a record leaked the secret");
    assert!(
        !everything.contains("Bearer"),
        "a record leaked the credential scheme"
    );
}
