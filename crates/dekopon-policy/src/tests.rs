use dekopon_capability::{EffectKind, Idempotency};
use dekopon_core::{RiskLevel, SecretSinkKind};

use super::{
    AGENT_PROMPT_ACTION, MAX_POLICY_BYTES, PolicyBuildError, PolicyContext, PolicyDecision,
    PolicyEngine, PolicyRequest, PolicyTarget, PolicyWorld, SECRET_USE_ACTION, UnresolvedKind,
};

/// The workflow world: two principals, two echo capabilities.
fn world() -> PolicyWorld {
    PolicyWorld::new(
        [
            "cpetersen".parse().expect("valid principal fixture"),
            "direct-caller".parse().expect("valid principal fixture"),
        ],
        [
            (
                "echo.echo".parse().expect("valid capability fixture"),
                "echo".parse().expect("valid provider fixture"),
            ),
            (
                "echo.reverse".parse().expect("valid capability fixture"),
                "echo".parse().expect("valid provider fixture"),
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
            provider: "echo".parse().expect("valid provider fixture"),
            effect: EffectKind::ReadOnly,
            risk: RiskLevel::Low,
            idempotency: Idempotency::Idempotent,
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
            capability: "echo.echo".parse().expect("capability"),
            provider: "echo".parse().expect("provider"),
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

/// An empty policy set is a valid deployment that permits nothing. This is the deny-by-default
/// floor: a broker that has not been given policy yet must still start and still refuse.
#[test]
fn empty_policy_text_is_valid_and_permits_nothing() {
    for source in ["", "   \n\t  "] {
        let engine = PolicyEngine::new(source, &world()).expect("empty policy text is valid");
        assert_eq!(engine.policy_count(), 0);
        assert_eq!(engine.referenced_capabilities().count(), 0);
        let decision = engine.authorize(capability_request(
            "cpetersen",
            "echo.echo",
            PolicyContext::default(),
        ));
        assert_eq!(decision, PolicyDecision::default());
        assert!(!decision.allowed);
        assert!(!decision.errors_present);
    }
}

/// The three ways a policy can name something that does not exist, each of which the exact engine
/// used to catch structurally. A rule nothing can satisfy is a configuration mistake, and finding
/// it at startup beats finding it in a denial log.
#[test]
fn undeclared_names_refuse_construction() {
    let unknown_principal = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"nobody",
                  action == Dekopon::Action::"echo.echo",
                  resource == Dekopon::Provider::"echo");"#,
        &world(),
    )
    .expect_err("an undeclared principal must refuse startup");
    assert!(matches!(
        unknown_principal,
        PolicyBuildError::UnknownPrincipal { ref principal, .. } if principal == "nobody"
    ));

    let unknown_provider = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action == Dekopon::Action::"echo.echo",
                  resource == Dekopon::Provider::"github");"#,
        &world(),
    )
    .expect_err("an undeclared provider must refuse startup");
    // Cedar's own validator reaches this one first: `echo.echo` does not apply to a resource type
    // it has never seen paired with that action.
    assert!(matches!(
        unknown_provider,
        PolicyBuildError::Validation { .. } | PolicyBuildError::UnknownProvider { .. }
    ));

    let unknown_action = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action == Dekopon::Action::"gh.pull-request.approve",
                  resource == Dekopon::Provider::"echo");"#,
        &world(),
    )
    .expect_err("an undeclared action must refuse startup");
    assert!(matches!(
        unknown_action,
        PolicyBuildError::Validation { .. } | PolicyBuildError::UnknownAction { .. }
    ));

    let unknown_type = PolicyEngine::new(
        r#"permit(principal == Dekopon::Robot::"hal",
                  action == Dekopon::Action::"echo.echo",
                  resource == Dekopon::Provider::"echo");"#,
        &world(),
    )
    .expect_err("an undeclared entity type must refuse startup");
    // Classification now runs before schema generation, so our own check reaches this before
    // Cedar's validator does and reports the more specific variant.
    assert!(matches!(
        unknown_type,
        PolicyBuildError::Validation { .. } | PolicyBuildError::UnknownEntityType { .. }
    ));
}

/// Strict validation is what keeps a policy from reading an attribute the request will never carry.
/// `agent.prompt` has no capability classification, so a policy that inspects one is refused rather
/// than silently erroring — and therefore denying — on every request.
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

    // The mirror image: a required attribute may be read without a `has` guard, because every
    // capability request carries it.
    PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action == Dekopon::Action::"echo.echo",
                  resource == Dekopon::Provider::"echo")
           when { context.effect == "read-only" };"#,
        &world(),
    )
    .expect("a capability action always carries its classification");
}

/// `via` is the hinge that keeps attested and direct authority disjoint, and it has to hold in both
/// directions: a policy written for a gateway must not authorize a direct peer, and vice versa.
#[test]
fn context_conditions_isolate_attested_and_direct_authority() {
    let engine = PolicyEngine::new(
        r#"
        @id("attested-echo")
        permit(principal == Dekopon::Principal::"cpetersen",
               action == Dekopon::Action::"echo.echo",
               resource == Dekopon::Provider::"echo")
        when { context has via && context.via == "dekopond-gateway" };

        @id("direct-reverse")
        permit(principal == Dekopon::Principal::"direct-caller",
               action == Dekopon::Action::"echo.reverse",
               resource == Dekopon::Provider::"echo")
        unless { context has via };
        "#,
        &world(),
    )
    .expect("the workflow policy set validates");

    let attested = engine.authorize(capability_request(
        "cpetersen",
        "echo.echo",
        via("dekopond-gateway"),
    ));
    assert!(attested.allowed);
    assert_eq!(attested.determining_policy_ids, ["attested-echo"]);

    // The same principal, the same capability, arriving directly.
    let direct = engine.authorize(capability_request(
        "cpetersen",
        "echo.echo",
        PolicyContext::default(),
    ));
    assert!(!direct.allowed);
    assert!(direct.determining_policy_ids.is_empty());

    // A different gateway is not this gateway.
    let other_gateway = engine.authorize(capability_request(
        "cpetersen",
        "echo.echo",
        via("someone-elses-gateway"),
    ));
    assert!(!other_gateway.allowed);

    let direct_grant = engine.authorize(capability_request(
        "direct-caller",
        "echo.reverse",
        PolicyContext::default(),
    ));
    assert!(direct_grant.allowed);
    assert_eq!(direct_grant.determining_policy_ids, ["direct-reverse"]);

    // And the direct grant cannot be borrowed by an attested context.
    let borrowed = engine.authorize(capability_request(
        "direct-caller",
        "echo.reverse",
        via("dekopond-gateway"),
    ));
    assert!(!borrowed.allowed);
}

/// The session gate: permitting a principal to talk to an agent is its own explicit statement, and
/// naming a different agent is not that statement.
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

/// `forbid` beats `permit`, and the forbid is what the explanation names.
#[test]
fn forbid_overrides_permit_and_is_reported_as_the_reason() {
    let engine = PolicyEngine::new(
        r#"
        @id("broad-permit")
        permit(principal == Dekopon::Principal::"cpetersen",
               action in [Dekopon::Action::"echo.echo", Dekopon::Action::"echo.reverse"],
               resource == Dekopon::Provider::"echo");

        @id("no-reverse")
        forbid(principal == Dekopon::Principal::"cpetersen",
               action == Dekopon::Action::"echo.reverse",
               resource == Dekopon::Provider::"echo");
        "#,
        &world(),
    )
    .expect("permit and forbid coexist");

    let permitted = engine.authorize(capability_request(
        "cpetersen",
        "echo.echo",
        PolicyContext::default(),
    ));
    assert!(permitted.allowed);
    assert_eq!(permitted.determining_policy_ids, ["broad-permit"]);

    let forbidden = engine.authorize(capability_request(
        "cpetersen",
        "echo.reverse",
        PolicyContext::default(),
    ));
    assert!(!forbidden.allowed);
    assert_eq!(forbidden.determining_policy_ids, ["no-reverse"]);
}

/// Every capability an `action in [...]` list names is reported, because each one needs an
/// owner-authored constraint set before the broker will start.
#[test]
fn referenced_capabilities_cover_every_action_a_policy_names() {
    let engine = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action in [Dekopon::Action::"echo.echo", Dekopon::Action::"echo.reverse"],
                  resource == Dekopon::Provider::"echo");"#,
        &world(),
    )
    .expect("an action list validates");
    assert_eq!(
        engine
            .referenced_capabilities()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>(),
        ["echo.echo", "echo.reverse"]
    );
}

/// A policy that constrains no action names none, so nothing forces a constraint set into
/// existence. The broker's decision-time `unconstrained-capability` refusal is what covers this,
/// and this test pins the fact that the startup half cannot.
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
                "echo.echo",
                PolicyContext::default()
            ))
            .allowed
    );
}

/// Bounds are startup-fixed, so a policy file cannot become an unbounded parse cost.
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
                      action == Dekopon::Action::"echo.echo",
                      resource == Dekopon::Provider::"echo");"#,
            &world(),
        )
        .expect_err("an unlinked template is refused"),
        PolicyBuildError::TemplateUnsupported
    ));
}

/// Policy identifiers are the explanation an audit record carries, so they must be unambiguous and
/// bounded.
#[test]
fn policy_identifiers_are_bounded_and_unique() {
    let duplicate = PolicyEngine::new(
        r#"
        @id("same")
        permit(principal == Dekopon::Principal::"cpetersen",
               action == Dekopon::Action::"echo.echo",
               resource == Dekopon::Provider::"echo");
        @id("same")
        permit(principal == Dekopon::Principal::"direct-caller",
               action == Dekopon::Action::"echo.echo",
               resource == Dekopon::Provider::"echo");
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
               action == Dekopon::Action::"echo.echo",
               resource == Dekopon::Provider::"echo");
        "#,
        &world(),
    )
    .expect_err("a policy name must be a portable identifier");
    assert!(matches!(invalid, PolicyBuildError::InvalidPolicyId { .. }));
}

/// The digest fingerprints the authorization surface: the same policies over the same world hash
/// identically regardless of formatting, and any change to either side moves it.
#[test]
fn digest_is_stable_across_formatting_and_moves_with_meaning() {
    let compact = r#"permit(principal == Dekopon::Principal::"cpetersen",action == Dekopon::Action::"echo.echo",resource == Dekopon::Provider::"echo");"#;
    let spaced = "
        permit(
            principal == Dekopon::Principal::\"cpetersen\",
            action    == Dekopon::Action::\"echo.echo\",
            resource  == Dekopon::Provider::\"echo\"
        );
    ";
    let baseline = PolicyEngine::new(compact, &world()).expect("compact source builds");
    let reformatted = PolicyEngine::new(spaced, &world()).expect("spaced source builds");
    assert_eq!(baseline.digest(), reformatted.digest());
    assert!(baseline.digest().starts_with("sha256:"));
    assert_eq!(baseline.digest().len(), "sha256:".len() + 64);

    let different_policy = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"direct-caller",
                  action == Dekopon::Action::"echo.echo",
                  resource == Dekopon::Provider::"echo");"#,
        &world(),
    )
    .expect("a different policy builds");
    assert_ne!(baseline.digest(), different_policy.digest());

    // The world is part of the fingerprint: the same text over a larger surface is not the same
    // authorization decision procedure.
    let wider = PolicyWorld::new(
        [
            "cpetersen".parse().expect("valid principal fixture"),
            "direct-caller".parse().expect("valid principal fixture"),
            "someone-else".parse().expect("valid principal fixture"),
        ],
        [
            (
                "echo.echo".parse().expect("valid capability fixture"),
                "echo".parse().expect("valid provider fixture"),
            ),
            (
                "echo.reverse".parse().expect("valid capability fixture"),
                "echo".parse().expect("valid provider fixture"),
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

    // Empty policy text still has a digest, and it is not the digest of a granted one.
    assert_ne!(
        baseline.digest(),
        PolicyEngine::new("", &world())
            .expect("empty text builds")
            .digest()
    );
}

/// The world declares what a policy may name, so its own construction has to fail closed too.
#[test]
fn world_construction_rejects_duplicates_and_reserved_names() {
    let duplicate = PolicyWorld::new(
        ["cpetersen".parse().expect("valid principal fixture")],
        [
            (
                "echo.echo".parse().expect("valid capability fixture"),
                "echo".parse().expect("valid provider fixture"),
            ),
            (
                "echo.echo".parse().expect("valid capability fixture"),
                "other".parse().expect("valid provider fixture"),
            ),
        ],
    )
    .expect_err("one capability must not route to two providers");
    assert!(matches!(
        duplicate,
        PolicyBuildError::DuplicateCapability { .. }
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
        assert!(matches!(reserved, PolicyBuildError::ReservedAction { .. }));
    }
}

/// Debug output is reachable from the broker's own `Debug`; it must fingerprint the policy set
/// rather than reproduce it.
#[test]
fn debug_output_carries_no_policy_source() {
    let engine = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action == Dekopon::Action::"echo.echo",
                  resource == Dekopon::Provider::"echo");"#,
        &world(),
    )
    .expect("the policy builds");
    let rendered = format!("{engine:?}");
    assert!(rendered.contains(engine.digest()));
    assert!(!rendered.contains("permit"));
    assert!(!rendered.contains("cpetersen"));
}

/// The reason a policy naming an unloaded capability is *kept* rather than dropped.
///
/// A grant reading `action in [a, b]` with only `a` loaded must keep granting `a`. Dropping the
/// whole policy would silently revoke authority the operator has every reason to still expect,
/// turning "one provider is missing" into "this agent can do nothing".
#[test]
fn tolerating_an_unloaded_capability_leaves_the_rest_of_the_policy_granting() {
    let text = r#"@id("workflow")
        permit(principal == Dekopon::Principal::"cpetersen",
               action in [Dekopon::Action::"echo.echo",
                          Dekopon::Action::"gh.pull-request.approve"],
               resource == Dekopon::Provider::"echo");"#;

    let (engine, unresolved) =
        PolicyEngine::new_lenient(text, &world()).expect("an unloaded capability is tolerated");

    assert_eq!(unresolved.len(), 1, "{unresolved:?}");
    assert_eq!(unresolved[0].name, "gh.pull-request.approve");
    assert_eq!(unresolved[0].kind, UnresolvedKind::Capability);
    assert_eq!(unresolved[0].policy, "workflow");

    // The surviving half of the same policy still grants.
    assert!(
        engine
            .authorize(capability_request(
                "cpetersen",
                "echo.echo",
                PolicyContext::default()
            ))
            .allowed,
        "the loaded capability in a tolerating policy must still be granted"
    );
}

/// A tolerated name is not a referenced capability, so it never reaches the broker's requirement
/// that every capability a policy could permit have an owner-authored constraint set.
#[test]
fn a_tolerated_capability_is_never_reported_as_referenced() {
    let (engine, unresolved) = PolicyEngine::new_lenient(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action in [Dekopon::Action::"echo.echo",
                             Dekopon::Action::"gh.pull-request.approve"],
                  resource == Dekopon::Provider::"echo");"#,
        &world(),
    )
    .expect("an unloaded capability is tolerated");

    assert_eq!(unresolved.len(), 1);
    let referenced = engine
        .referenced_capabilities()
        .map(|capability| capability.as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(referenced, ["echo.echo"]);
}

/// Leniency is a startup posture, not a weakening of the grammar. Everything strict mode refuses
/// for a *provider-derived* reason is exactly what lenient mode tolerates, and nothing else.
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

/// A literal outside the identifier grammar is a typo, not an anticipation.
///
/// `Dekopon::Action::"GH.Read"` can never become a loaded capability however many providers arrive
/// later, so tolerating it is meaningless. It used to be pushed to `unresolved`, skipped by
/// `with_phantoms` because it does not parse, and then rejected by Cedar's strict validator — so
/// the tolerant default produced a raw `Validation` error carrying Cedar text while strict mode
/// gave the clearer `UnknownAction` for the same input, and the `UnresolvedName` report was lost
/// with the `Err`.
#[test]
fn an_unparseable_name_gets_the_specific_error_even_when_lenient() {
    let action = PolicyEngine::new_lenient(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action == Dekopon::Action::"GH.Read",
                  resource == Dekopon::Provider::"echo");"#,
        &world(),
    )
    .expect_err("a name outside the grammar refuses startup even when lenient");
    assert!(
        matches!(action, PolicyBuildError::UnknownAction { ref action, .. } if action == "GH.Read"),
        "{action:?}"
    );

    let provider = PolicyEngine::new_lenient(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action == Dekopon::Action::"echo.echo",
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

    // A well-formed name that is merely absent is still tolerated.
    let (_, unresolved) = PolicyEngine::new_lenient(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action == Dekopon::Action::"gh.pull-request.approve",
                  resource == Dekopon::Provider::"gh");"#,
        &world(),
    )
    .expect("a well-formed absent name is still tolerated");
    assert_eq!(unresolved.len(), 2, "{unresolved:?}");
}

/// Principals come from owner-authored identities, never from a loaded component, so an undeclared
/// one is a typo in any mode. Leniency must not turn a misspelled principal into a silent no-match.
#[test]
fn an_undeclared_principal_stays_fatal_under_leniency() {
    let error = PolicyEngine::new_lenient(
        r#"permit(principal == Dekopon::Principal::"nobody",
                  action == Dekopon::Action::"echo.echo",
                  resource == Dekopon::Provider::"echo");"#,
        &world(),
    )
    .expect_err("an undeclared principal refuses startup even when lenient");
    assert!(
        matches!(error, PolicyBuildError::UnknownPrincipal { ref principal, .. } if principal == "nobody"),
        "{error:?}"
    );
}

/// "Undeclared" and "could never be a principal" are different diagnoses with different fixes, and
/// collapsing the second into the first sends an operator to add an identity they cannot spell.
#[test]
fn a_malformed_principal_is_not_reported_as_merely_undeclared() {
    let error = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"Ops Team",
                  action == Dekopon::Action::"echo.echo",
                  resource == Dekopon::Provider::"echo");"#,
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

/// A capability the world never declared cannot even be phrased as a Cedar question. The answer is
/// still a denial — but a blanket denial that explains itself, rather than one indistinguishable
/// from a deployment that simply granted nothing.
#[test]
fn a_request_the_schema_cannot_express_says_so() {
    let engine = PolicyEngine::new(
        r#"permit(principal == Dekopon::Principal::"cpetersen",
                  action == Dekopon::Action::"echo.echo",
                  resource == Dekopon::Provider::"echo");"#,
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

    // Every decision Cedar actually reached leaves the field alone, denials included.
    assert!(
        engine
            .authorize(capability_request(
                "direct-caller",
                "echo.echo",
                PolicyContext::default()
            ))
            .refusal
            .is_none()
    );
}

/// A `forbid` naming an unloaded capability must not fail open once that provider is loaded.
///
/// Keeping the policy whole is what makes this safe: the same text refuses the capability the
/// moment the world declares it, with no restart-ordering subtlety.
#[test]
fn a_forbid_naming_an_unloaded_capability_applies_once_it_loads() {
    let text = r#"permit(principal == Dekopon::Principal::"cpetersen",
                          action in [Dekopon::Action::"echo.echo",
                                     Dekopon::Action::"echo.reverse"],
                          resource == Dekopon::Provider::"echo");
                  forbid(principal == Dekopon::Principal::"cpetersen",
                         action == Dekopon::Action::"echo.reverse",
                         resource == Dekopon::Provider::"echo");"#;

    let (engine, unresolved) =
        PolicyEngine::new_lenient(text, &world()).expect("world declares all");
    assert!(unresolved.is_empty(), "{unresolved:?}");
    assert!(
        !engine
            .authorize(capability_request(
                "cpetersen",
                "echo.reverse",
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
                  action == Dekopon::Action::"echo.echo",
                  resource == Dekopon::Provider::"echo");"#,
        &world_with_secret(),
    )
    .expect("capability-only policy validates");
    assert!(
        engine
            .authorize(capability_request(
                "cpetersen",
                "echo.echo",
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
               when {{ context.capability == "echo.echo"
                    && context.provider == "echo"
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
fn core_control_actions_are_reserved_and_have_exact_typed_agent_context() {
    use super::{AGENT_EFFORT_SET_ACTION, AGENT_MODEL_SELECT_ACTION, AgentControlAction};
    use dekopon_core::{Effort, ModelSelection};
    for action in [AGENT_MODEL_SELECT_ACTION, AGENT_EFFORT_SET_ACTION] {
        assert!(matches!(
            PolicyWorld::new([], [(action.parse().unwrap(), "provider".parse().unwrap())]),
            Err(PolicyBuildError::ReservedAction { .. })
        ));
    }
    let policy = r#"
        permit(principal == Dekopon::Principal::"cpetersen",
               action == Dekopon::Action::"agent.model.select",
               resource == Dekopon::Agent::"reviewer")
        when { context.agent == "reviewer" && context.fromModel == "baseline"
            && context.toModel == "gpt-5.6-sol" && context.fromEffort == "low"
            && context.toEffort == "high" && context has via && context.via == "gateway" };
    "#;
    let engine = PolicyEngine::new(policy, &world()).unwrap();
    assert_eq!(engine.referenced_capabilities().count(), 0);
    let request = PolicyRequest {
        principal: "cpetersen".parse().unwrap(),
        target: PolicyTarget::AgentControl {
            agent: "reviewer".parse().unwrap(),
            action: AgentControlAction::ModelSelect,
            from: ModelSelection {
                model: "baseline".parse().unwrap(),
                effort: Effort::Low,
            },
            to: ModelSelection {
                model: "gpt-5.6-sol".parse().unwrap(),
                effort: Effort::High,
            },
        },
        context: via("gateway"),
    };
    assert!(engine.authorize(request.clone()).allowed);
    let mut direct = request.clone();
    direct.context.via = None;
    assert!(!engine.authorize(direct).allowed);
    let mut effort = request;
    let PolicyTarget::AgentControl { ref mut action, .. } = effort.target else {
        panic!()
    };
    *action = AgentControlAction::EffortSet;
    assert!(!engine.authorize(effort).allowed);
    for field in ["history", "spend", "provider", "input", "endpoint"] {
        let forbidden = format!(
            "permit(principal, action == Dekopon::Action::\"agent.model.select\", resource) when {{ context.{field} == \"value\" }};"
        );
        assert!(PolicyEngine::new(&forbidden, &world()).is_err(), "{field}");
    }
    let wrong_resource = "permit(principal, action == Dekopon::Action::\"agent.model.select\", resource == Dekopon::Provider::\"echo\");";
    assert!(PolicyEngine::new(wrong_resource, &world()).is_err());
}
