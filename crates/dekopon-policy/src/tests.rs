use dekopon_capability::EffectKind;
use dekopon_core::{RiskLevel, SecretSinkKind};
use serde_json::json;

use super::{
    AGENT_PROMPT_ACTION, MAX_POLICY_BYTES, PolicyBuildError, PolicyContext, PolicyDecision,
    PolicyEngine, PolicyRequest, PolicyTarget, PolicyWorld, SECRET_USE_ACTION, UnresolvedKind,
};

fn world() -> PolicyWorld {
    PolicyWorld::new(
        [
            "cpetersen".parse().expect("valid principal fixture"),
            "direct-caller".parse().expect("valid principal fixture"),
        ],
        [
            (
                "cli-probe.upper".parse().expect("valid capability fixture"),
                "cli-probe".parse().expect("valid provider fixture"),
            ),
            (
                "cli-probe.reverse"
                    .parse()
                    .expect("valid capability fixture"),
                "cli-probe".parse().expect("valid provider fixture"),
            ),
        ],
    )
    .expect("distinct fixtures build a world")
}

fn world_with_secret() -> PolicyWorld {
    world().with_secrets(["drn:com.xrl:secret:prod:api/token"
        .parse()
        .expect("canonical secret DRN")])
}

fn capability_request(principal: &str, capability: &str, context: PolicyContext) -> PolicyRequest {
    PolicyRequest {
        principal: principal.parse().expect("valid principal fixture"),
        target: PolicyTarget::Capability {
            capability: capability.parse().expect("valid capability fixture"),
            provider: "cli-probe".parse().expect("valid provider fixture"),
            effect: EffectKind::ReadOnly,
            risk: RiskLevel::Low,
        },
        context,
    }
}

fn prompt_request(principal: &str, agent: &str, context: PolicyContext) -> PolicyRequest {
    PolicyRequest {
        principal: principal.parse().expect("valid principal fixture"),
        target: PolicyTarget::AgentPrompt {
            agent: agent.parse().expect("valid agent fixture"),
        },
        context,
    }
}

fn secret_request(principal: &str, context: PolicyContext) -> PolicyRequest {
    PolicyRequest {
        principal: principal.parse().expect("valid principal fixture"),
        target: PolicyTarget::SecretUse {
            secret: "drn:com.xrl:secret:prod:api/token"
                .parse()
                .expect("canonical secret DRN"),
            capability: "cli-probe.upper".parse().expect("capability"),
            provider: "cli-probe".parse().expect("provider"),
            sink: SecretSinkKind::HttpBearer,
        },
        context,
    }
}

fn via(name: &str) -> PolicyContext {
    PolicyContext {
        via: Some(name.to_owned()),
        ..PolicyContext::default()
    }
}

#[test]
fn empty_policy_text_is_valid_and_permits_nothing() {
    for source in ["", "   \n\t  "] {
        let engine = PolicyEngine::new(source, &world()).expect("empty policy text is valid");
        assert_eq!(engine.policy_count(), 0);
        assert_eq!(engine.referenced_capabilities().count(), 0);
        let decision = engine.authorize(capability_request(
            "cpetersen",
            "cli-probe.upper",
            PolicyContext::default(),
        ));
        assert_eq!(decision, PolicyDecision::default());
        assert!(!decision.allowed);
        assert!(!decision.errors_present);
    }
}

#[test]
fn undeclared_names_refuse_construction() {
    let unknown_principal = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"nobody",
                  action == Dekopon::Action::"cli-probe.upper",
                  resource == Dekopon::Provider::"cli-probe");"#,
        &world(),
    )
    .expect_err("an undeclared principal must refuse startup");
    assert!(matches!(
        unknown_principal,
        PolicyBuildError::UnknownPrincipal { ref principal, .. } if principal == "nobody"
    ));

    let unknown_provider = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action == Dekopon::Action::"cli-probe.upper",
                  resource == Dekopon::Provider::"github");"#,
        &world(),
    )
    .expect_err("an undeclared provider must refuse startup");
    assert!(matches!(
        unknown_provider,
        PolicyBuildError::Validation { .. } | PolicyBuildError::UnknownProvider { .. }
    ));

    let unknown_action = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action == Dekopon::Action::"gh.pull-request.approve",
                  resource == Dekopon::Provider::"cli-probe");"#,
        &world(),
    )
    .expect_err("an undeclared action must refuse startup");
    assert!(matches!(
        unknown_action,
        PolicyBuildError::Validation { .. } | PolicyBuildError::UnknownAction { .. }
    ));

    let unknown_type = PolicyEngine::new(
        r#"permit(principal == Dekopon::Robot::"hal",
                  action == Dekopon::Action::"cli-probe.upper",
                  resource == Dekopon::Provider::"cli-probe");"#,
        &world(),
    )
    .expect_err("an undeclared entity type must refuse startup");
    assert!(matches!(
        unknown_type,
        PolicyBuildError::Validation { .. } | PolicyBuildError::UnknownEntityType { .. }
    ));
}

#[test]
fn strict_validation_rejects_attributes_an_action_never_carries() {
    let error = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action == Dekopon::Action::"agent.prompt",
                  resource == Dekopon::Agent::"reviewer")
           when { context.effect == "read-only" };"#,
        &world(),
    )
    .expect_err("agent.prompt carries no effect attribute");
    assert!(matches!(error, PolicyBuildError::Validation { .. }));

    PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action == Dekopon::Action::"cli-probe.upper",
                  resource == Dekopon::Provider::"cli-probe")
           when { context.effect == "read-only" };"#,
        &world(),
    )
    .expect("a capability action always carries its classification");
}

#[test]
fn context_conditions_isolate_attested_and_direct_authority() {
    let engine = PolicyEngine::new(
        r#"
        @id("attested-upper")
        permit(principal == Dekopon::Principal::"cpetersen",
               action == Dekopon::Action::"cli-probe.upper",
               resource == Dekopon::Provider::"cli-probe")
        when { context has via && context.via == "dekopond-gateway" };

        @id("direct-reverse")
        permit(principal == Dekopon::Principal::"direct-caller",
               action == Dekopon::Action::"cli-probe.reverse",
               resource == Dekopon::Provider::"cli-probe")
        unless { context has via };
        "#,
        &world(),
    )
    .expect("the workflow policy set validates");

    let attested = engine.authorize(capability_request(
        "cpetersen",
        "cli-probe.upper",
        via("dekopond-gateway"),
    ));
    assert!(attested.allowed);
    assert_eq!(attested.determining_policy_ids, ["attested-upper"]);

    let direct = engine.authorize(capability_request(
        "cpetersen",
        "cli-probe.upper",
        PolicyContext::default(),
    ));
    assert!(!direct.allowed);
    assert!(direct.determining_policy_ids.is_empty());

    let other_gateway = engine.authorize(capability_request(
        "cpetersen",
        "cli-probe.upper",
        via("someone-elses-gateway"),
    ));
    assert!(!other_gateway.allowed);

    let direct_grant = engine.authorize(capability_request(
        "direct-caller",
        "cli-probe.reverse",
        PolicyContext::default(),
    ));
    assert!(direct_grant.allowed);
    assert_eq!(direct_grant.determining_policy_ids, ["direct-reverse"]);

    let borrowed = engine.authorize(capability_request(
        "direct-caller",
        "cli-probe.reverse",
        via("dekopond-gateway"),
    ));
    assert!(!borrowed.allowed);
}

#[test]
fn agent_prompt_matches_the_named_agent_only() {
    let engine = PolicyEngine::new(
        r#"
        @id("prompt-gate")
        permit(principal == Dekopon::Principal::"cpetersen",
               action == Dekopon::Action::"agent.prompt",
               resource == Dekopon::Agent::"pr-summarizer-linter")
        when { context has via && context.via == "dekopond-gateway" };
        "#,
        &world(),
    )
    .expect("the agent gate validates");

    let allowed = engine.authorize(prompt_request(
        "cpetersen",
        "pr-summarizer-linter",
        via("dekopond-gateway"),
    ));
    assert!(allowed.allowed);
    assert_eq!(allowed.determining_policy_ids, ["prompt-gate"]);

    assert!(
        !engine
            .authorize(prompt_request(
                "cpetersen",
                "some-other-agent",
                via("dekopond-gateway"),
            ))
            .allowed,
        "an agent the policy does not name is a different resource"
    );
    assert!(
        !engine
            .authorize(prompt_request(
                "direct-caller",
                "pr-summarizer-linter",
                via("dekopond-gateway"),
            ))
            .allowed
    );
    assert_eq!(
        engine.referenced_capabilities().count(),
        0,
        "agent.prompt is not a capability and needs no constraint set"
    );
}

#[test]
fn forbid_overrides_permit_and_is_reported_as_the_reason() {
    let engine = PolicyEngine::new(
        r#"
        @id("broad-permit")
        permit(principal == Dekopon::Principal::"cpetersen",
               action in [Dekopon::Action::"cli-probe.upper", Dekopon::Action::"cli-probe.reverse"],
               resource == Dekopon::Provider::"cli-probe");

        @id("no-reverse")
        forbid(principal == Dekopon::Principal::"cpetersen",
               action == Dekopon::Action::"cli-probe.reverse",
               resource == Dekopon::Provider::"cli-probe");
        "#,
        &world(),
    )
    .expect("permit and forbid coexist");

    let permitted = engine.authorize(capability_request(
        "cpetersen",
        "cli-probe.upper",
        PolicyContext::default(),
    ));
    assert!(permitted.allowed);
    assert_eq!(permitted.determining_policy_ids, ["broad-permit"]);

    let forbidden = engine.authorize(capability_request(
        "cpetersen",
        "cli-probe.reverse",
        PolicyContext::default(),
    ));
    assert!(!forbidden.allowed);
    assert_eq!(forbidden.determining_policy_ids, ["no-reverse"]);
}

#[test]
fn referenced_capabilities_cover_every_action_a_policy_names() {
    let engine = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action in [Dekopon::Action::"cli-probe.upper", Dekopon::Action::"cli-probe.reverse"],
                  resource == Dekopon::Provider::"cli-probe");"#,
        &world(),
    )
    .expect("an action list validates");
    assert_eq!(
        engine
            .referenced_capabilities()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>(),
        ["cli-probe.reverse", "cli-probe.upper"]
    );
}

#[test]
fn an_unconstrained_action_scope_names_no_capability() {
    let engine = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"cpetersen", action, resource);"#,
        &world(),
    )
    .expect("an unconstrained scope validates");
    assert_eq!(engine.referenced_capabilities().count(), 0);
    assert!(
        engine
            .authorize(capability_request(
                "cpetersen",
                "cli-probe.upper",
                PolicyContext::default()
            ))
            .allowed
    );
}

#[test]
fn source_and_count_bounds_fail_closed() {
    let oversized = "x".repeat(MAX_POLICY_BYTES + 1);
    assert!(matches!(
        PolicyEngine::new(&oversized, &world()).expect_err("oversized source is refused"),
        PolicyBuildError::PolicyTooLarge { .. }
    ));

    assert!(matches!(
        PolicyEngine::new("this is not cedar", &world())
            .expect_err("unparseable source is refused"),
        PolicyBuildError::Parse { .. }
    ));

    assert!(matches!(
        PolicyEngine::new(
            r#"permit(principal == ?principal,
                      action == Dekopon::Action::"cli-probe.upper",
                      resource == Dekopon::Provider::"cli-probe");"#,
            &world(),
        )
        .expect_err("an unlinked template is refused"),
        PolicyBuildError::TemplateUnsupported
    ));
}

#[test]
fn policy_identifiers_are_bounded_and_unique() {
    let duplicate = PolicyEngine::new(
        r#"
        @id("same")
        permit(principal == Dekopon::Principal::"cpetersen",
               action == Dekopon::Action::"cli-probe.upper",
               resource == Dekopon::Provider::"cli-probe");
        @id("same")
        permit(principal == Dekopon::Principal::"direct-caller",
               action == Dekopon::Action::"cli-probe.upper",
               resource == Dekopon::Provider::"cli-probe");
        "#,
        &world(),
    )
    .expect_err("two policies must not share one name");
    assert!(matches!(
        duplicate,
        PolicyBuildError::DuplicatePolicyId { ref policy } if policy == "same"
    ));

    let invalid = PolicyEngine::new(
        r#"
        @id("has a space")
        permit(principal == Dekopon::Principal::"cpetersen",
               action == Dekopon::Action::"cli-probe.upper",
               resource == Dekopon::Provider::"cli-probe");
        "#,
        &world(),
    )
    .expect_err("a policy name must be a portable identifier");
    assert!(matches!(invalid, PolicyBuildError::InvalidPolicyId { .. }));
}

#[test]
fn digest_is_stable_across_formatting_and_moves_with_meaning() {
    let compact = r#"permit(principal == Dekopon::Principal::"cpetersen",action == Dekopon::Action::"cli-probe.upper",resource == Dekopon::Provider::"cli-probe");"#;
    let spaced = "
        permit(
            principal == Dekopon::Principal::\"cpetersen\",
            action    == Dekopon::Action::\"cli-probe.upper\",
            resource  == Dekopon::Provider::\"cli-probe\"
        );
    ";
    let baseline = PolicyEngine::new(compact, &world()).expect("compact source builds");
    let reformatted = PolicyEngine::new(spaced, &world()).expect("spaced source builds");
    assert_eq!(baseline.digest(), reformatted.digest());
    assert!(baseline.digest().starts_with("sha256:"));
    assert_eq!(baseline.digest().len(), "sha256:".len() + 64);

    let different_policy = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"direct-caller",
                  action == Dekopon::Action::"cli-probe.upper",
                  resource == Dekopon::Provider::"cli-probe");"#,
        &world(),
    )
    .expect("a different policy builds");
    assert_ne!(baseline.digest(), different_policy.digest());

    let wider = PolicyWorld::new(
        [
            "cpetersen".parse().expect("valid principal fixture"),
            "direct-caller".parse().expect("valid principal fixture"),
            "someone-else".parse().expect("valid principal fixture"),
        ],
        [
            (
                "cli-probe.upper".parse().expect("valid capability fixture"),
                "cli-probe".parse().expect("valid provider fixture"),
            ),
            (
                "cli-probe.reverse"
                    .parse()
                    .expect("valid capability fixture"),
                "cli-probe".parse().expect("valid provider fixture"),
            ),
        ],
    )
    .expect("a wider world builds");
    assert_ne!(
        baseline.digest(),
        PolicyEngine::new(compact, &wider)
            .expect("the same text builds against a wider world")
            .digest()
    );

    assert_ne!(
        baseline.digest(),
        PolicyEngine::new("", &world())
            .expect("empty text builds")
            .digest()
    );
}

#[test]
fn world_construction_rejects_duplicates_and_reserved_names() {
    let duplicate = PolicyWorld::new(
        ["cpetersen".parse().expect("valid principal fixture")],
        [
            (
                "cli-probe.upper".parse().expect("valid capability fixture"),
                "cli-probe".parse().expect("valid provider fixture"),
            ),
            (
                "cli-probe.upper".parse().expect("valid capability fixture"),
                "other".parse().expect("valid provider fixture"),
            ),
        ],
    )
    .expect_err("one capability must not route to two providers");
    assert!(matches!(
        duplicate,
        PolicyBuildError::WorldConflicts { reserved, duplicates }
            if reserved.is_empty() && duplicates == vec!["cli-probe.upper".parse().expect("valid id")]
    ));

    for action in [AGENT_PROMPT_ACTION, SECRET_USE_ACTION] {
        let reserved = PolicyWorld::new(
            ["cpetersen".parse().expect("valid principal fixture")],
            [(
                action
                    .parse()
                    .expect("fixed action is a syntactically valid capability id"),
                "agent".parse().expect("valid provider fixture"),
            )],
        )
        .expect_err("a capability must not shadow a fixed action");
        assert!(matches!(reserved,
            PolicyBuildError::WorldConflicts { reserved, duplicates }
                if duplicates.is_empty() && reserved == vec![action.parse().expect("valid id")]
        ));
    }
}

#[test]
fn the_conversation_record_gates_every_action_and_is_absent_for_a_direct_peer() {
    fn chat(kind: &str, container: Option<&str>, id: &str, thread: Option<&str>) -> PolicyContext {
        PolicyContext {
            via: Some("dekopond-gateway".to_owned()),
            transport_kind: Some("discord".to_owned()),
            transport: Some("elote-logs".to_owned()),
            conversation: Some(super::PolicyConversation {
                kind: kind.to_owned(),
                container: container.map(str::to_owned),
                id: id.to_owned(),
                thread: thread.map(str::to_owned),
            }),
            ..PolicyContext::default()
        }
    }

    let source = r#"
@id("images-in-the-routed-channel")
permit(
  principal == Dekopon::Principal::"cpetersen",
  action == Dekopon::Action::"cli-probe.upper",
  resource == Dekopon::Provider::"cli-probe"
) when {
  context has via && context.via == "dekopond-gateway"
  && context has conversation && ["channel", "thread"].contains(context.conversation.kind)
  && context.conversation.id == "1338356895504793623"
};

@id("prompting-inside-the-lange-guild")
permit(
  principal == Dekopon::Principal::"cpetersen",
  action == Dekopon::Action::"agent.prompt",
  resource == Dekopon::Agent::"lange-family"
) when {
  context has conversation
  && context.conversation has container
  && context.conversation.container == "1153119165809434697"
};
"#;
    let engine = PolicyEngine::new(source, &world()).expect("the guarded statements validate");

    for (context, allowed, why) in [
        (
            chat(
                "channel",
                Some("1153119165809434697"),
                "1338356895504793623",
                None,
            ),
            true,
            "a routed channel message",
        ),
        (
            chat(
                "thread",
                Some("1153119165809434697"),
                "1338356895504793623",
                Some("456"),
            ),
            true,
            "a thread under it",
        ),
        (
            chat("directMessage", None, "1338356895504793623", None),
            false,
            "the same id in a direct message is not the channel",
        ),
        (
            chat("channel", Some("1153119165809434697"), "999", None),
            false,
            "another channel",
        ),
        (
            PolicyContext::default(),
            false,
            "a direct peer has no conversation",
        ),
    ] {
        let decision =
            engine.authorize(capability_request("cpetersen", "cli-probe.upper", context));
        assert_eq!(decision.allowed, allowed, "{why}");
        if allowed {
            assert_eq!(
                decision.determining_policy_ids,
                vec!["images-in-the-routed-channel".to_owned()]
            );
        }
    }

    assert!(
        engine
            .authorize(prompt_request(
                "cpetersen",
                "lange-family",
                chat(
                    "channel",
                    Some("1153119165809434697"),
                    "1153119166446981193",
                    None
                ),
            ))
            .allowed
    );
    assert!(
        !engine
            .authorize(prompt_request(
                "cpetersen",
                "lange-family",
                chat("channel", None, "1153119166446981193", None),
            ))
            .allowed,
        "a conversation with no container cannot satisfy a container gate"
    );
}

#[test]
fn owner_policy_can_keep_writes_to_turns_a_person_typed() {
    let source = r#"
@id("typed-only")
permit(
  principal == Dekopon::Principal::"cpetersen",
  action == Dekopon::Action::"cli-probe.upper",
  resource == Dekopon::Provider::"cli-probe"
) when { context has trigger && context.trigger == "message" };
"#;
    let engine = PolicyEngine::new(source, &world()).expect("the trigger gate validates");
    let context = |trigger: &str| PolicyContext {
        via: Some("dekopond-gateway".to_owned()),
        trigger: Some(trigger.to_owned()),
        ..PolicyContext::default()
    };

    assert!(
        engine
            .authorize(capability_request(
                "cpetersen",
                "cli-probe.upper",
                context("message")
            ))
            .allowed
    );
    assert!(
        !engine
            .authorize(capability_request(
                "cpetersen",
                "cli-probe.upper",
                context("wake")
            ))
            .allowed
    );
}

#[test]
fn a_retired_or_unguarded_context_attribute_fails_validation() {
    for (source, why) in [
        (
            r#"permit(principal, action == Dekopon::Action::"cli-probe.upper", resource) when {
                 context has conversation
                 && context.conversation.container == "1153119165809434697"
               };"#,
            "`container` is optional, so it needs `context.conversation has container`",
        ),
        (
            r#"permit(principal, action == Dekopon::Action::"cli-probe.upper", resource) when {
                 context.channel == "1338356895504793623"
               };"#,
            "`context.channel` is gone; the id lives at `context.conversation.id`",
        ),
        (
            r#"permit(principal, action == Dekopon::Action::"cli-probe.upper", resource) when {
                 context has conversation && context.conversation == "1338356895504793623"
               };"#,
            "the conversation is a record, never a string",
        ),
    ] {
        let error = PolicyEngine::new(source, &world()).expect_err(why);
        assert!(
            matches!(error, PolicyBuildError::Validation { .. }),
            "{why}: {error}"
        );
    }
}

#[test]
fn a_has_guarded_read_of_the_retired_channel_attribute_loads_and_then_never_fires() {
    let source = r#"
@id("stale-channel-pin")
permit(
  principal == Dekopon::Principal::"cpetersen",
  action == Dekopon::Action::"cli-probe.upper",
  resource == Dekopon::Provider::"cli-probe"
) when {
  context has via && context.via == "dekopond-gateway"
  && context has channel && context.channel == "1338356895504793623"
};
"#;
    let engine = PolicyEngine::new(source, &world())
        .expect("a guarded read of an absent attribute is not a validation error");
    assert_eq!(engine.policy_count(), 1);

    let context = PolicyContext {
        via: Some("dekopond-gateway".to_owned()),
        conversation: Some(super::PolicyConversation {
            kind: "channel".to_owned(),
            container: None,
            id: "1338356895504793623".to_owned(),
            thread: None,
        }),
        ..PolicyContext::default()
    };
    let decision = engine.authorize(capability_request("cpetersen", "cli-probe.upper", context));
    assert!(
        !decision.allowed,
        "the statement still names an attribute nothing stamps, so it can never permit"
    );
    assert!(
        !decision.errors_present,
        "a short-circuited `has` is a false condition rather than an evaluation failure"
    );
}

#[test]
fn every_action_declares_exactly_these_context_attributes() {
    fn pretty(value: &serde_json::Value) -> String {
        serde_json::to_string_pretty(value).expect("a context record serializes")
    }

    let capability_context = json!({
        "type": "Record",
        "attributes": {
            "via": { "type": "String", "required": false },
            "subject": { "type": "String", "required": false },
            "agent": { "type": "String", "required": false },
            "transportKind": { "type": "String", "required": false },
            "transport": { "type": "String", "required": false },
            "trigger": { "type": "String", "required": false },
            "conversation": {
                "type": "Record",
                "required": false,
                "attributes": {
                    "kind": { "type": "String" },
                    "container": { "type": "String", "required": false },
                    "id": { "type": "String" },
                    "thread": { "type": "String", "required": false },
                },
            },
            "effect": { "type": "String" },
            "risk": { "type": "String" },
        }
    });
    let prompt_context = json!({
        "type": "Record",
        "attributes": {
            "via": { "type": "String", "required": false },
            "subject": { "type": "String", "required": false },
            "agent": { "type": "String", "required": false },
            "transportKind": { "type": "String", "required": false },
            "transport": { "type": "String", "required": false },
            "trigger": { "type": "String", "required": false },
            "conversation": {
                "type": "Record",
                "required": false,
                "attributes": {
                    "kind": { "type": "String" },
                    "container": { "type": "String", "required": false },
                    "id": { "type": "String" },
                    "thread": { "type": "String", "required": false },
                },
            },
        }
    });
    let secret_context = json!({
        "type": "Record",
        "attributes": {
            "via": { "type": "String", "required": false },
            "subject": { "type": "String", "required": false },
            "agent": { "type": "String", "required": false },
            "transportKind": { "type": "String", "required": false },
            "transport": { "type": "String", "required": false },
            "trigger": { "type": "String", "required": false },
            "conversation": {
                "type": "Record",
                "required": false,
                "attributes": {
                    "kind": { "type": "String" },
                    "container": { "type": "String", "required": false },
                    "id": { "type": "String" },
                    "thread": { "type": "String", "required": false },
                },
            },
            "capability": { "type": "String" },
            "provider": { "type": "String" },
            "sink": { "type": "String" },
        }
    });

    let schema = world_with_secret().schema_json();
    let actions = schema["Dekopon"]["actions"]
        .as_object()
        .expect("the schema renders an action map");
    assert_eq!(
        actions.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "cli-probe.reverse",
            "cli-probe.upper",
            "cli-probe:*",
            AGENT_PROMPT_ACTION,
            SECRET_USE_ACTION
        ],
        "the world's two capabilities, their provider group and the two fixed actions"
    );
    for (action, expected) in [
        ("cli-probe.upper", &capability_context),
        ("cli-probe.reverse", &capability_context),
        (AGENT_PROMPT_ACTION, &prompt_context),
        (SECRET_USE_ACTION, &secret_context),
    ] {
        let (rendered, expected) = (
            pretty(&actions[action]["appliesTo"]["context"]),
            pretty(expected),
        );
        assert!(
            rendered == expected,
            "the {action} context record moved\n--- rendered ---\n{rendered}\n--- pinned ---\n{expected}"
        );
    }
}

#[test]
fn debug_output_carries_no_policy_source() {
    let engine = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action == Dekopon::Action::"cli-probe.upper",
                  resource == Dekopon::Provider::"cli-probe");"#,
        &world(),
    )
    .expect("the policy builds");
    let rendered = format!("{engine:?}");
    assert!(rendered.contains(engine.digest()));
    assert!(!rendered.contains("permit"));
    assert!(!rendered.contains("cpetersen"));
}

#[test]
fn tolerating_an_unloaded_capability_leaves_the_rest_of_the_policy_granting() {
    let text = r#"@id("workflow")
        permit(principal == Dekopon::Principal::"cpetersen",
               action in [Dekopon::Action::"cli-probe.upper",
                          Dekopon::Action::"gh.pull-request.approve"],
               resource == Dekopon::Provider::"cli-probe");"#;

    let (engine, unresolved) =
        PolicyEngine::new_lenient(text, &world()).expect("an unloaded capability is tolerated");

    assert_eq!(unresolved.len(), 1, "{unresolved:?}");
    assert_eq!(unresolved[0].name, "gh.pull-request.approve");
    assert_eq!(unresolved[0].kind, UnresolvedKind::Capability);
    assert_eq!(unresolved[0].policy, "workflow");

    assert!(
        engine
            .authorize(capability_request(
                "cpetersen",
                "cli-probe.upper",
                PolicyContext::default()
            ))
            .allowed,
        "the loaded capability in a tolerating policy must still be granted"
    );
}

#[test]
fn a_tolerated_capability_is_never_reported_as_referenced() {
    let (engine, unresolved) = PolicyEngine::new_lenient(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action in [Dekopon::Action::"cli-probe.upper",
                             Dekopon::Action::"gh.pull-request.approve"],
                  resource == Dekopon::Provider::"cli-probe");"#,
        &world(),
    )
    .expect("an unloaded capability is tolerated");

    assert_eq!(unresolved.len(), 1);
    let referenced = engine
        .referenced_capabilities()
        .map(|capability| capability.as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(referenced, ["cli-probe.upper"]);
}

#[test]
fn strict_construction_refuses_precisely_what_lenient_tolerates() {
    let text = r#"permit(principal == Dekopon::Principal::"cpetersen",
                          action == Dekopon::Action::"gh.pull-request.approve",
                          resource == Dekopon::Provider::"gh");"#;

    let strict = PolicyEngine::new(text, &world()).expect_err("strict mode refuses an absent name");
    assert!(
        matches!(
            strict,
            PolicyBuildError::UnknownAction { .. } | PolicyBuildError::UnknownProvider { .. }
        ),
        "{strict:?}"
    );

    let (_, unresolved) =
        PolicyEngine::new_lenient(text, &world()).expect("lenient mode tolerates");
    let mut kinds = unresolved
        .iter()
        .map(|entry| entry.kind)
        .collect::<Vec<_>>();
    kinds.sort_unstable();
    assert_eq!(
        kinds,
        [UnresolvedKind::Capability, UnresolvedKind::Provider],
        "both the action and the resource are provider-derived"
    );
}

#[test]
fn an_unparseable_name_gets_the_specific_error_even_when_lenient() {
    let action = PolicyEngine::new_lenient(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action == Dekopon::Action::"GH.Read",
                  resource == Dekopon::Provider::"cli-probe");"#,
        &world(),
    )
    .expect_err("a name outside the grammar refuses startup even when lenient");
    assert!(
        matches!(action, PolicyBuildError::UnknownAction { ref action, .. } if action == "GH.Read"),
        "{action:?}"
    );

    let provider = PolicyEngine::new_lenient(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action == Dekopon::Action::"cli-probe.upper",
                  resource == Dekopon::Provider::"Not A Provider");"#,
        &world(),
    )
    .expect_err("a provider name outside the grammar refuses startup even when lenient");
    assert!(
        matches!(
            provider,
            PolicyBuildError::UnknownProvider { ref provider, .. } if provider == "Not A Provider"
        ),
        "{provider:?}"
    );

    let (_, unresolved) = PolicyEngine::new_lenient(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action == Dekopon::Action::"gh.pull-request.approve",
                  resource == Dekopon::Provider::"gh");"#,
        &world(),
    )
    .expect("a well-formed absent name is still tolerated");
    assert_eq!(unresolved.len(), 2, "{unresolved:?}");
}

#[test]
fn an_undeclared_principal_stays_fatal_under_leniency() {
    let error = PolicyEngine::new_lenient(
        r#"permit(principal == Dekopon::Principal::"nobody",
                  action == Dekopon::Action::"cli-probe.upper",
                  resource == Dekopon::Provider::"cli-probe");"#,
        &world(),
    )
    .expect_err("an undeclared principal refuses startup even when lenient");
    assert!(
        matches!(error, PolicyBuildError::UnknownPrincipal { ref principal, .. } if principal == "nobody"),
        "{error:?}"
    );
}

#[test]
fn a_malformed_principal_is_not_reported_as_merely_undeclared() {
    let error = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"Ops Team",
                  action == Dekopon::Action::"cli-probe.upper",
                  resource == Dekopon::Provider::"cli-probe");"#,
        &world(),
    )
    .expect_err("a malformed principal must refuse startup");
    let PolicyBuildError::MalformedPrincipal {
        ref principal,
        ref source,
        ..
    } = error
    else {
        panic!("{error:?}");
    };
    assert_eq!(principal, "Ops Team");
    assert!(
        source.to_string().contains('O'),
        "the parse error names the offending character: {source}"
    );
}

#[test]
fn a_request_the_schema_cannot_express_says_so() {
    let engine = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action == Dekopon::Action::"cli-probe.upper",
                  resource == Dekopon::Provider::"cli-probe");"#,
        &world(),
    )
    .expect("the world declares everything this policy names");

    let decision = engine.authorize(capability_request(
        "cpetersen",
        "gh.pull-request.approve",
        PolicyContext::default(),
    ));
    assert!(!decision.allowed);
    assert!(decision.errors_present);
    let refusal = decision
        .refusal
        .expect("a request the schema cannot express explains itself");
    assert!(
        refusal.contains("gh.pull-request.approve"),
        "the refusal names the undeclared action: {refusal}"
    );

    assert!(
        engine
            .authorize(capability_request(
                "direct-caller",
                "cli-probe.upper",
                PolicyContext::default()
            ))
            .refusal
            .is_none()
    );
}

#[test]
fn a_forbid_naming_an_unloaded_capability_applies_once_it_loads() {
    let text = r#"permit(principal == Dekopon::Principal::"cpetersen",
                          action in [Dekopon::Action::"cli-probe.upper",
                                     Dekopon::Action::"cli-probe.reverse"],
                          resource == Dekopon::Provider::"cli-probe");
                  forbid(principal == Dekopon::Principal::"cpetersen",
                         action == Dekopon::Action::"cli-probe.reverse",
                         resource == Dekopon::Provider::"cli-probe");"#;

    let (engine, unresolved) =
        PolicyEngine::new_lenient(text, &world()).expect("world declares all");
    assert!(unresolved.is_empty(), "{unresolved:?}");
    assert!(
        !engine
            .authorize(capability_request(
                "cpetersen",
                "cli-probe.reverse",
                PolicyContext::default()
            ))
            .allowed,
        "a forbid must override the permit it overlaps"
    );
}

#[test]
fn capability_permission_does_not_imply_secret_use() {
    let engine = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action == Dekopon::Action::"cli-probe.upper",
                  resource == Dekopon::Provider::"cli-probe");"#,
        &world_with_secret(),
    )
    .expect("capability-only policy validates");
    assert!(
        engine
            .authorize(capability_request(
                "cpetersen",
                "cli-probe.upper",
                PolicyContext::default()
            ))
            .allowed
    );
    assert!(
        !engine
            .authorize(secret_request("cpetersen", PolicyContext::default()))
            .allowed
    );
}

#[test]
fn secret_use_is_a_separate_exact_resource_decision() {
    let engine = PolicyEngine::new(
        &format!(
            r#"@id("secret-use")
               permit(principal == Dekopon::Principal::"cpetersen",
                      action == Dekopon::Action::"{SECRET_USE_ACTION}",
                      resource == Dekopon::Secret::"drn:com.xrl:secret:prod:api/token")
               when {{ context.capability == "cli-probe.upper"
                    && context.provider == "cli-probe"
                    && context.sink == "httpBearer" }};"#
        ),
        &world_with_secret(),
    )
    .expect("secret policy validates");
    let allowed = engine.authorize(secret_request("cpetersen", PolicyContext::default()));
    assert!(allowed.allowed, "{allowed:?}");
    assert_eq!(allowed.determining_policy_ids, ["secret-use"]);

    let unknown = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action == Dekopon::Action::"secret.use",
                  resource == Dekopon::Secret::"drn:com.xrl:secret:prod:api/typo");"#,
        &world_with_secret(),
    )
    .expect_err("unknown DRN refuses startup");
    assert!(matches!(unknown, PolicyBuildError::UnknownSecret { .. }));
}

#[test]
fn world_refusal_names_all_reserved_and_duplicate_capabilities() {
    let error = PolicyWorld::new(
        [],
        [
            "secret.use",
            "cli-probe.reverse",
            "agent.prompt",
            "cli-probe.count",
            "cli-probe.reverse",
            "cli-probe.count",
        ]
        .map(|name| {
            (
                name.parse().expect("valid id"),
                "cli-probe".parse().expect("valid provider"),
            )
        }),
    )
    .expect_err("all conflicts refuse the world");
    let rendered = error.to_string();
    for name in [
        "agent.prompt",
        "secret.use",
        "cli-probe.count",
        "cli-probe.reverse",
    ] {
        assert!(rendered.contains(name), "missing {name}: {rendered}");
    }
    let PolicyBuildError::WorldConflicts {
        reserved,
        duplicates,
    } = error
    else {
        panic!("expected world conflicts");
    };
    assert_eq!(
        reserved,
        ["agent.prompt", "secret.use"].map(|id| id.parse().expect("valid id"))
    );
    assert_eq!(
        duplicates,
        ["cli-probe.count", "cli-probe.reverse"].map(|id| id.parse().expect("valid id"))
    );
}

fn grouped_world() -> PolicyWorld {
    world()
        .with_group_members([
            (
                "cpetersen".parse().expect("principal"),
                "family".parse().expect("group"),
            ),
            (
                "isaac".parse().expect("principal"),
                "family".parse().expect("group"),
            ),
        ])
        .with_read_only(["cli-probe.upper".parse().expect("capability")])
}

#[test]
fn a_group_grant_reaches_its_members_and_no_one_else() {
    let engine = PolicyEngine::new(
        r#"@id("family-upper")
        permit(principal in Dekopon::Group::"family",
               action == Dekopon::Action::"cli-probe.upper",
               resource);"#,
        &grouped_world(),
    )
    .expect("group policy loads");
    for (principal, allowed) in [
        ("cpetersen", true),
        ("isaac", true),
        ("direct-caller", false),
    ] {
        let decision = engine.authorize(capability_request(
            principal,
            "cli-probe.upper",
            PolicyContext::default(),
        ));
        assert_eq!(decision.allowed, allowed, "{principal}");
    }
}

#[test]
fn a_group_nobody_belongs_to_refuses_construction() {
    let error = PolicyEngine::new(
        r#"@id("typo")
        permit(principal in Dekopon::Group::"famly",
               action == Dekopon::Action::"cli-probe.upper",
               resource);"#,
        &grouped_world(),
    )
    .expect_err("an empty group is a typo");
    assert!(matches!(error, PolicyBuildError::UnknownGroup { group, .. } if group == "famly"));
}

#[test]
fn the_read_only_group_holds_reads_and_never_writes() {
    let engine = PolicyEngine::new(
        r#"@id("reads")
        permit(principal == Dekopon::Principal::"cpetersen",
               action in Dekopon::Action::"cli-probe:read-only",
               resource);"#,
        &grouped_world(),
    )
    .expect("read-only group loads");
    let read = engine.authorize(capability_request(
        "cpetersen",
        "cli-probe.upper",
        PolicyContext::default(),
    ));
    let write = engine.authorize(capability_request(
        "cpetersen",
        "cli-probe.reverse",
        PolicyContext::default(),
    ));
    assert!(read.allowed);
    assert!(!write.allowed);
    assert_eq!(engine.referenced_capabilities().count(), 0);
}

#[test]
fn the_provider_group_holds_every_capability_and_unless_carves_one_out() {
    let engine = PolicyEngine::new(
        r#"@id("all-but-reverse")
        permit(principal in Dekopon::Group::"family",
               action in Dekopon::Action::"cli-probe:*",
               resource)
        unless { action == Dekopon::Action::"cli-probe.reverse" };"#,
        &grouped_world(),
    )
    .expect("provider group loads");
    let upper = engine.authorize(capability_request(
        "isaac",
        "cli-probe.upper",
        PolicyContext::default(),
    ));
    let reverse = engine.authorize(capability_request(
        "isaac",
        "cli-probe.reverse",
        PolicyContext::default(),
    ));
    assert!(upper.allowed);
    assert_eq!(upper.determining_policy_ids, ["all-but-reverse"]);
    assert!(!reverse.allowed);
}

#[test]
fn an_action_group_of_an_unloaded_provider_is_tolerated_only_when_lenient() {
    let policy = r#"@id("absent")
        permit(principal == Dekopon::Principal::"cpetersen",
               action in Dekopon::Action::"absent:read-only",
               resource);"#;
    assert!(matches!(
        PolicyEngine::new(policy, &grouped_world()),
        Err(PolicyBuildError::UnknownAction { .. })
    ));
    let (_, unresolved) =
        PolicyEngine::new_lenient(policy, &grouped_world()).expect("lenient tolerates it");
    assert_eq!(unresolved.len(), 1);
    assert_eq!(unresolved[0].kind, UnresolvedKind::ActionGroup);
}

#[test]
fn the_digest_moves_when_membership_changes() {
    let policy = r#"@id("family-upper")
        permit(principal in Dekopon::Group::"family",
               action == Dekopon::Action::"cli-probe.upper",
               resource);"#;
    let before = PolicyEngine::new(policy, &grouped_world()).expect("loads");
    let after = PolicyEngine::new(
        policy,
        &grouped_world().with_group_members([(
            "direct-caller".parse().expect("principal"),
            "family".parse().expect("group"),
        )]),
    )
    .expect("loads");
    assert_ne!(before.digest(), after.digest());
}
