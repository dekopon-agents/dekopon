#![allow(clippy::unwrap_used, clippy::disallowed_methods)]

use std::sync::Arc;

use dekopon_broker::{
    Attestation, AttestorGrant, AuthenticatedContext, Broker, BrokerLimits, CapabilityRoute,
    ChatScopeClaim, ChatTransportKind, ConstraintCatalog, ConstraintSet, Conversation,
    ConversationKind, CredentialStore, IdentityDirectory, InMemoryAuditLog, InvocationRequest,
    PolicyEngine, PolicyWorld,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_capability::{EffectKind, ExecutionConstraints, InvocationOutcome};
use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, InvocationId, PrincipalId, ProviderId,
    RiskLevel, TransportId,
};
use dekopon_test_support::provider_fixture;
use serde_json::json;

const TRACE_PARENT: &str = "00-0000000000000000000000000000f1c7-00000000000000f1-00";

const SLACK_SUBJECT: &str = "slack.t0123abc.u9xyz";

const POLICIES: &str = r#"
@id("attested-reverse")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"cli-probe.reverse",
       resource == Dekopon::Provider::"cli-probe")
when { context.via == "gateway"
    && context.agent == "some-agent" };

@id("prompt-gate")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"some-agent")
when { context.via == "gateway" };
"#;

fn principal(name: &str) -> PrincipalId {
    name.parse().expect("valid principal fixture")
}

fn agent(name: &str) -> AgentId {
    name.parse().expect("valid agent fixture")
}

fn capability(name: &str) -> CapabilityId {
    name.parse().expect("valid capability fixture")
}

fn provider() -> ProviderId {
    "cli-probe".parse().expect("valid provider fixture")
}

fn subject() -> ExternalSubject {
    SLACK_SUBJECT.parse().expect("canonical subject fixture")
}

fn constraint_set(capability_id: &str) -> (CapabilityId, ConstraintSet) {
    (
        capability(capability_id),
        ConstraintSet {
            default_secret_use: None,
            route: CapabilityRoute::Generic,
            provider: provider(),
            effect: EffectKind::ReadOnly,
            risk: RiskLevel::Low,
            credential: None,
            constraints: ExecutionConstraints::default(),
        },
    )
}

fn policy_engine() -> PolicyEngine {
    let world = PolicyWorld::new(
        [principal("cpetersen"), principal("gateway")],
        [(capability("cli-probe.reverse"), provider())],
    )
    .expect("the workflow world builds");
    PolicyEngine::new(POLICIES, &world).expect("the workflow policy set validates")
}

async fn broker() -> Broker<InMemoryAuditLog> {
    configured_broker(
        policy_engine(),
        IdentityDirectory::new([(subject(), principal("cpetersen"))]).expect("directory"),
        Arc::new(InMemoryAuditLog::new(8).expect("audit")),
    )
    .await
}

async fn configured_broker(
    policy: PolicyEngine,
    identities: IdentityDirectory,
    audit: Arc<InMemoryAuditLog>,
) -> Broker<InMemoryAuditLog> {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("cli-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("cli-probe provider fixture loads");
    Broker::new(
        registry,
        principal("broker-test"),
        policy,
        ConstraintCatalog::new([constraint_set("cli-probe.reverse")])
            .expect("one capability builds a catalog"),
        CredentialStore::empty(),
        identities,
        audit,
        BrokerLimits::default(),
    )
    .expect("the broker starts")
}

fn request(index: usize, capability_id: &str) -> InvocationRequest {
    InvocationRequest {
        id: format!("invoke-table-{index}")
            .parse::<InvocationId>()
            .expect("valid invocation fixture"),
        capability: capability(capability_id),
        trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
        input: json!({"text": "decision table"}),
        secret_use: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_prompt_gate_is_a_separate_grant() {
    let broker = broker().await;
    let gateway = AuthenticatedContext::new(
        principal("gateway"),
        Actor::Service {
            principal: principal("gateway"),
        },
    )
    .expect("gateway context binds");
    let grant = AttestorGrant {
        namespaces: Some(vec!["slack.t0123abc".to_owned()]),
    };

    assert!(
        broker
            .capability_surface(
                &gateway,
                Some(&grant),
                Some(&Attestation::for_subject(subject(), agent("some-agent"))),
            )
            .is_some(),
        "the permitted agent may be driven"
    );
    let chat = Attestation::for_chat(
        subject(),
        agent("some-agent"),
        ChatScopeClaim {
            participant: None,
            transport: "scientist-slack".parse::<TransportId>().expect("transport"),
            kind: ChatTransportKind::Slack,
            conversation: Conversation {
                kind: ConversationKind::Thread,
                container: Some("t0123abc".to_owned()),
                id: "c0123abc".to_owned(),
                thread: Some("1712345678.000100".to_owned()),
            },
            delivery: None,
            trigger: dekopon_broker::Trigger::Message,
        },
    );
    let (capabilities, words, help, memory) = broker
        .capability_surface(&gateway, Some(&grant), Some(&chat))
        .expect("legacy subject-only attestor remains compatible with chat operations");
    assert!(!capabilities.is_empty());
    assert_eq!(words, ["probe"]);
    assert!(help.contains_key("probe"));
    assert!(
        memory.is_none(),
        "subject-only attestation grants no storage scope"
    );

    let ordinary = request(98, "cli-probe.reverse");
    let (host, mut stdout) = std::os::unix::net::UnixStream::pair().unwrap();
    let captured = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut bytes).unwrap();
        bytes
    });
    let ordinary_result = broker
        .invoke(
            &gateway,
            Some(&grant),
            Some(&chat.bound_to(ordinary.id.clone())),
            ordinary,
            dekopon_broker_host::asset::AssetInputs {
                streams: Some(dekopon_broker_host::Streams {
                    stdin: None,
                    stdout: host.into(),
                }),
                ..Default::default()
            },
        )
        .await
        .expect("ordinary subject-only chat executes through the upgraded chat operation");
    assert!(!captured.join().unwrap().is_empty());
    assert_eq!(ordinary_result.result.outcome, InvocationOutcome::Succeeded);

    assert!(
        broker
            .capability_surface(
                &gateway,
                Some(&grant),
                Some(&Attestation::for_subject(subject(), agent("other-agent"))),
            )
            .is_none(),
        "an agent no policy names is refused exactly like an unhonored attestation"
    );

    let proposal = request(99, "cli-probe.reverse");
    let refused = broker
        .invoke(
            &gateway,
            Some(&grant),
            Some(
                &Attestation::for_subject(subject(), agent("other-agent"))
                    .bound_to(proposal.id.clone()),
            ),
            proposal,
            Default::default(),
        )
        .await
        .expect("a refused agent is still an accounted decision");
    assert_eq!(refused.result.outcome, InvocationOutcome::Denied);
    assert_eq!(refused.result.error.as_deref(), Some("agent-denied"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_attestor_without_namespaces_speaks_for_exactly_the_mapped_subjects() {
    let broker = broker().await;
    let gateway = AuthenticatedContext::new(
        principal("gateway"),
        Actor::Service {
            principal: principal("gateway"),
        },
    )
    .expect("gateway context binds");
    let grant = AttestorGrant { namespaces: None };
    assert!(
        broker
            .capability_surface(
                &gateway,
                Some(&grant),
                Some(&Attestation::for_subject(subject(), agent("some-agent"))),
            )
            .is_some()
    );
    assert!(
        broker
            .capability_surface(
                &gateway,
                Some(&grant),
                Some(&Attestation::for_subject(
                    "slack.t0123abc.uother".parse().expect("subject"),
                    agent("some-agent"),
                )),
            )
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_the_senders_service_cannot_produce_is_refused() {
    let broker = broker().await;
    let gateway = AuthenticatedContext::new(
        principal("gateway"),
        Actor::Service {
            principal: principal("gateway"),
        },
    )
    .expect("gateway context binds");
    let discord_claim_for_a_slack_sender = Attestation::for_chat(
        subject(),
        agent("some-agent"),
        ChatScopeClaim {
            participant: None,
            transport: "elote-logs".parse::<TransportId>().expect("transport"),
            kind: ChatTransportKind::Discord,
            conversation: Conversation {
                kind: ConversationKind::DirectMessage,
                container: None,
                id: "1338356895504793623".to_owned(),
                thread: None,
            },
            delivery: None,
            trigger: dekopon_broker::Trigger::Message,
        },
    );
    assert!(
        broker
            .capability_surface(
                &gateway,
                Some(&AttestorGrant { namespaces: None }),
                Some(&discord_claim_for_a_slack_sender),
            )
            .is_none()
    );
}

async fn space_broker(
    space_prompt: &str,
    space_capability: &str,
) -> (Broker<InMemoryAuditLog>, Arc<InMemoryAuditLog>) {
    let mappings = [
        (SLACK_SUBJECT, "asker"),
        ("discord.123", "asker"),
        ("slack.t0123abc.c0123abc", "space"),
        ("slack.t0123abc.g0123abc", "space"),
        ("discord.999.456", "space"),
    ]
    .map(|(subject, name)| (subject.parse().expect("subject"), principal(name)));
    let world = PolicyWorld::new(
        [principal("asker"), principal("space"), principal("gateway")],
        [(capability("cli-probe.reverse"), provider())],
    )
    .expect("world")
    .with_subjects(mappings.clone());
    let policy = format!(
        r#"
        @id("asker") permit(principal == Dekopon::Principal::"asker", action, resource);
        {space_prompt}
        {space_capability}
    "#
    );
    let audit = Arc::new(InMemoryAuditLog::new(64).expect("audit"));
    let broker = configured_broker(
        PolicyEngine::new(&policy, &world).expect("policy"),
        IdentityDirectory::new(mappings).expect("directory"),
        Arc::clone(&audit),
    )
    .await;
    (broker, audit)
}

fn channel_claim(
    kind: ChatTransportKind,
    conversation_kind: ConversationKind,
    id: &str,
    trigger: dekopon_broker::Trigger,
) -> Attestation {
    let (subject, container, thread) = match kind {
        ChatTransportKind::Slack => (
            subject(),
            Some("t0123abc".to_owned()),
            Some("1712345678.000100".to_owned()),
        ),
        ChatTransportKind::Discord => (
            "discord.123".parse().expect("subject"),
            (conversation_kind != ConversationKind::DirectMessage).then(|| "999".to_owned()),
            (conversation_kind == ConversationKind::Thread).then(|| "789".to_owned()),
        ),
        ChatTransportKind::Fauxlack
        | ChatTransportKind::Telegram
        | ChatTransportKind::Whatsapp
        | ChatTransportKind::Github
        | ChatTransportKind::Local => panic!("table only covers Slack and Discord"),
    };
    Attestation::for_chat(
        subject,
        agent("some-agent"),
        ChatScopeClaim {
            participant: None,
            transport: "chat".parse().expect("transport"),
            kind,
            conversation: Conversation {
                kind: conversation_kind,
                container,
                id: id.to_owned(),
                thread,
            },
            trigger,
            delivery: None,
        },
    )
}

fn gateway() -> AuthenticatedContext {
    AuthenticatedContext::new(
        principal("gateway"),
        Actor::Service {
            principal: principal("gateway"),
        },
    )
    .expect("gateway")
}

const SPACE_PROMPT: &str = r#"@id("space-prompt") permit(principal == Dekopon::Principal::"space", action == Dekopon::Action::"agent.prompt", resource);"#;

#[tokio::test(flavor = "multi_thread")]
async fn mapped_channels_and_their_threads_require_both_principals_and_list_the_intersection() {
    let (broker, _) = space_broker(SPACE_PROMPT, "").await;
    for (index, (kind, conversation_kind, id, denied)) in [
        (
            ChatTransportKind::Slack,
            ConversationKind::Channel,
            "c0123abc",
            true,
        ),
        (
            ChatTransportKind::Slack,
            ConversationKind::Thread,
            "c0123abc",
            true,
        ),
        (
            ChatTransportKind::Slack,
            ConversationKind::Channel,
            "cother",
            false,
        ),
        (
            ChatTransportKind::Slack,
            ConversationKind::DirectMessage,
            "d0123abc",
            false,
        ),
        (
            ChatTransportKind::Discord,
            ConversationKind::Channel,
            "456",
            true,
        ),
        (
            ChatTransportKind::Discord,
            ConversationKind::Thread,
            "456",
            true,
        ),
        (
            ChatTransportKind::Discord,
            ConversationKind::Channel,
            "457",
            false,
        ),
        (
            ChatTransportKind::Discord,
            ConversationKind::DirectMessage,
            "123",
            false,
        ),
        (
            ChatTransportKind::Slack,
            ConversationKind::Thread,
            "g0123abc",
            true,
        ),
        (
            ChatTransportKind::Slack,
            ConversationKind::Thread,
            "gother",
            false,
        ),
        (
            ChatTransportKind::Slack,
            ConversationKind::GroupDirectMessage,
            "g0123abc",
            false,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let claim = channel_claim(
            kind,
            conversation_kind,
            id,
            dekopon_broker::Trigger::Message,
        );
        let grant = AttestorGrant { namespaces: None };
        let (context, refusal) = broker.resolve_context(&gateway(), Some(&grant), &claim);
        assert!(refusal.is_none());
        let decision =
            broker.authorize_invocation(&context, &request(index, "cli-probe.reverse").capability);
        if denied {
            assert_eq!(
                decision.err().expect("space denial").reason,
                "policy-denied"
            );
        } else {
            assert!(decision.is_ok());
        }
        let surface = broker
            .capability_surface(&gateway(), Some(&grant), Some(&claim))
            .expect("session allowed");
        assert_eq!(surface.0.is_empty(), denied);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_space_must_allow_the_session_gate() {
    let (broker, audit) = space_broker("", "").await;
    let grant = AttestorGrant { namespaces: None };
    let claim = channel_claim(
        ChatTransportKind::Slack,
        ConversationKind::Channel,
        "c0123abc",
        dekopon_broker::Trigger::Message,
    );
    let refusal = match broker.session_surface(&gateway(), Some(&grant), Some(&claim)) {
        Ok(_) => panic!("space must refuse the session"),
        Err(refusal) => refusal,
    };
    assert_eq!(refusal.reason, "agent-denied");
    assert_eq!(
        refusal.session,
        dekopon_broker_protocol::SessionRefusal::Other
    );
    let proposal = request(300, "cli-probe.reverse");
    let result = broker
        .invoke(
            &gateway(),
            Some(&grant),
            Some(&claim.bound_to(proposal.id.clone())),
            proposal,
            Default::default(),
        )
        .await
        .expect("accounted refusal");
    assert_eq!(
        result.result.error.as_deref(),
        Some("chat-attestation-denied")
    );
    assert!(audit.records().iter().any(|event| matches!(event, dekopon_broker::AuditEvent::Decision { reason: Some(reason), .. } if reason == "agent-denied")));
    let dm = channel_claim(
        ChatTransportKind::Slack,
        ConversationKind::DirectMessage,
        "d0123abc",
        dekopon_broker::Trigger::Message,
    );
    assert!(
        broker
            .session_surface(&gateway(), Some(&grant), Some(&dm))
            .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_space_can_distinguish_a_persons_message_from_a_wake() {
    let grant = AttestorGrant { namespaces: None };
    let (broker, _) = space_broker(SPACE_PROMPT, r#"@id("person-turn") permit(principal == Dekopon::Principal::"space", action == Dekopon::Action::"cli-probe.reverse", resource) when { context has startedBy && context.startedBy == "person" };"#).await;
    for (trigger, allowed) in [
        (dekopon_broker::Trigger::Message, true),
        (dekopon_broker::Trigger::Wake, false),
    ] {
        let claim = channel_claim(
            ChatTransportKind::Slack,
            ConversationKind::Channel,
            "c0123abc",
            trigger,
        );
        let (context, refusal) = broker.resolve_context(&gateway(), Some(&grant), &claim);
        assert!(refusal.is_none());
        assert_eq!(
            broker
                .authorize_invocation(&context, &capability("cli-probe.reverse"))
                .is_ok(),
            allowed
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn allowed_space_decisions_commit_to_the_invocation_input() {
    let (broker, audit) = space_broker(SPACE_PROMPT,
        r#"@id("space-capability") permit(principal == Dekopon::Principal::"space", action == Dekopon::Action::"cli-probe.reverse", resource);"#).await;
    let grant = AttestorGrant { namespaces: None };
    let claim = channel_claim(
        ChatTransportKind::Slack,
        ConversationKind::Channel,
        "c0123abc",
        dekopon_broker::Trigger::Message,
    );
    for text in ["first", "second"] {
        let mut proposal = request(400, "cli-probe.reverse");
        proposal.input = json!({"text": text});
        broker
            .invoke(
                &gateway(),
                Some(&grant),
                Some(&claim.bound_to(proposal.id.clone())),
                proposal,
                Default::default(),
            )
            .await
            .expect("accounted invocation");
    }
    let digests = audit
        .records()
        .into_iter()
        .filter_map(|event| match event {
            dekopon_broker::AuditEvent::Decision {
                allowed: true,
                decision_digest,
                ..
            } => Some(decision_digest),
            dekopon_broker::AuditEvent::Decision { allowed: false, .. }
            | dekopon_broker::AuditEvent::Execution { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(digests.len(), 2);
    assert_ne!(digests[0], digests[1]);
}
