//! Bounded, deterministic Cedar authorization adapter for the Dekopon broker.
//!
//! This crate is the only place Cedar appears in the workspace. It wraps `cedar-policy` behind an
//! API with three properties the broker depends on:
//!
//! 1. **Startup-fixed.** A [`PolicyEngine`] parses its policies once, generates a schema from the
//!    deployment's declared world, and validates the policy set against that schema in strict
//!    mode. Nothing is parsed, compiled, or resolved per request.
//! 2. **Deny-by-default at every layer.** Empty policy text is valid and permits nothing. A policy
//!    naming an unknown action, entity type, principal, or provider refuses construction. An
//!    evaluation error at decision time denies.
//! 3. **Explainable without leaking.** A decision carries the identifiers of the policies that
//!    determined it and a flag saying whether Cedar reported evaluation errors. Policy text
//!    reaches a caller only through construction errors, never through a decision.
//!
//! Execution constraints deliberately live outside Cedar. Cedar answers "may this principal do
//! this?"; how narrowly the broker then executes it — timeouts, output ceilings, HTTP destinations,
//! credential binding — stays in owner-authored constraint sets validated against loaded provider
//! manifests. Keeping them apart means a policy edit can never widen an execution bound.
//!
//! # Entity model
//!
//! Everything lives in the `Dekopon` namespace:
//!
//! - `Dekopon::Principal::"<principal-id>"` — enumerated from the deployment's peers and mapped
//!   principals. No attributes.
//! - `Dekopon::Provider::"<provider-id>"` — enumerated from loaded manifests; the resource of every
//!   capability action.
//! - `Dekopon::Agent::"<agent-id>"` — the resource type of [`AGENT_PROMPT_ACTION`]. Instances are
//!   matched by UID and are deliberately not enumerated, because the agent catalog belongs to the
//!   gateway rather than the broker.
//! - `Dekopon::Action::"<capability-id>"` — one action per loaded capability, applying to
//!   `Principal` over `Provider`, plus the fixed `Dekopon::Action::"agent.prompt"` applying to
//!   `Principal` over `Agent`.
//!
//! # Context
//!
//! Capability actions carry `{ via?, subject?, agent?, effect, risk, idempotency }`;
//! `agent.prompt` carries `{ via?, subject?, agent? }`. Every value is rendered from the broker's
//! own trusted state — never from a payload.
//!
//! Message content and provider input are deliberately absent. Conditioning authorization on
//! untrusted input is a possible future addition, but it needs a settled schema treatment for open
//! JSON first; until then a policy cannot accidentally depend on a value the caller controls.
//!
//! ```
//! use dekopon_core::{CapabilityId, PrincipalId, ProviderId};
//! use dekopon_policy::{PolicyContext, PolicyEngine, PolicyRequest, PolicyTarget, PolicyWorld};
//! use dekopon_capability::{EffectKind, Idempotency};
//! use dekopon_core::RiskLevel;
//!
//! let world = PolicyWorld::new(
//!     ["cpetersen".parse::<PrincipalId>()?],
//!     [(
//!         "echo.echo".parse::<CapabilityId>()?,
//!         "echo".parse::<ProviderId>()?,
//!     )],
//! )?;
//! let engine = PolicyEngine::new(
//!     r#"permit(principal == Dekopon::Principal::"cpetersen",
//!               action == Dekopon::Action::"echo.echo",
//!               resource == Dekopon::Provider::"echo");"#,
//!     &world,
//! )?;
//! let decision = engine.authorize(PolicyRequest {
//!     principal: "cpetersen".parse()?,
//!     target: PolicyTarget::Capability {
//!         capability: "echo.echo".parse()?,
//!         provider: "echo".parse()?,
//!         effect: EffectKind::ReadOnly,
//!         risk: RiskLevel::Low,
//!         idempotency: Idempotency::Idempotent,
//!     },
//!     context: PolicyContext::default(),
//! });
//! assert!(decision.allowed);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt,
    str::FromStr as _,
};

use cedar_policy::{
    Authorizer, Context, Decision, Entities, Entity, EntityId, EntityTypeName, EntityUid, PolicyId,
    PolicySet, RestrictedExpression, Schema, ValidationMode, Validator,
};
use dekopon_capability::{EffectKind, Idempotency};
use dekopon_core::{AgentId, CapabilityId, PrincipalId, ProviderId, RiskLevel};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

/// Cedar namespace every Dekopon entity type and action lives in.
pub const NAMESPACE: &str = "Dekopon";
/// The one action that is not a capability: permission for a principal to drive an agent session.
pub const AGENT_PROMPT_ACTION: &str = "agent.prompt";
/// Maximum accepted policy-source bytes.
pub const MAX_POLICY_BYTES: usize = 1024 * 1024;
/// Maximum accepted static policies in one engine.
pub const MAX_POLICIES: usize = 1_024;
/// Maximum accepted bytes in an `@id` policy annotation.
pub const MAX_POLICY_ID_BYTES: usize = 128;

const PRINCIPAL_TYPE: &str = "Dekopon::Principal";
const PROVIDER_TYPE: &str = "Dekopon::Provider";
const AGENT_TYPE: &str = "Dekopon::Agent";
const ACTION_TYPE: &str = "Dekopon::Action";
const DIGEST_DOMAIN: &[u8] = b"dekopon-policy-v1\0";

/// The declared world a policy set is validated against.
///
/// Everything a policy may name has to appear here, which is what turns a typo into a startup
/// refusal instead of a permanently unsatisfiable rule. Principals come from the deployment's peer
/// identities and owner-controlled subject mappings; providers and capabilities come from loaded
/// provider manifests.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PolicyWorld {
    principals: BTreeSet<PrincipalId>,
    providers: BTreeSet<ProviderId>,
    capabilities: BTreeMap<CapabilityId, ProviderId>,
    /// Capability names a policy referenced that no loaded provider routes. See
    /// [`PolicyWorld::with_phantoms`].
    phantom_capabilities: BTreeSet<CapabilityId>,
    /// Provider names a policy referenced that no loaded manifest declares.
    phantom_providers: BTreeSet<ProviderId>,
}

impl PolicyWorld {
    /// Declares the principals a policy may name and the capability-to-provider routes it may act
    /// on.
    ///
    /// Providers are derived from the capability routes: a provider with no loaded capability is
    /// not a resource any action applies to, so naming one is an error rather than a silent
    /// never-match.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyBuildError::DuplicateCapability`] when one capability identifier is declared
    /// twice, and [`PolicyBuildError::ReservedAction`] when a capability collides with
    /// [`AGENT_PROMPT_ACTION`].
    pub fn new(
        principals: impl IntoIterator<Item = PrincipalId>,
        capabilities: impl IntoIterator<Item = (CapabilityId, ProviderId)>,
    ) -> Result<Self, PolicyBuildError> {
        let mut world = Self::default();
        for principal in principals {
            world.principals.insert(principal);
        }
        for (capability, provider) in capabilities {
            if capability.as_str() == AGENT_PROMPT_ACTION {
                return Err(PolicyBuildError::ReservedAction { capability });
            }
            world.providers.insert(provider.clone());
            if world
                .capabilities
                .insert(capability.clone(), provider)
                .is_some()
            {
                return Err(PolicyBuildError::DuplicateCapability { capability });
            }
        }
        Ok(world)
    }

    /// Iterates the declared principals in identifier order.
    pub fn principals(&self) -> impl Iterator<Item = &PrincipalId> {
        self.principals.iter()
    }

    /// Iterates the declared providers in identifier order.
    pub fn providers(&self) -> impl Iterator<Item = &ProviderId> {
        self.providers.iter()
    }

    /// Iterates the declared capability routes in identifier order.
    pub fn capabilities(&self) -> impl Iterator<Item = (&CapabilityId, &ProviderId)> {
        self.capabilities.iter()
    }

    /// Returns the provider a declared capability routes to.
    ///
    /// A phantom name routes nowhere and returns `None`, which is the whole point of keeping
    /// phantoms out of the route map.
    #[must_use]
    pub fn provider_for(&self, capability: &CapabilityId) -> Option<&ProviderId> {
        self.capabilities.get(capability)
    }

    /// Extends this world with names a policy referenced that no loaded provider declares.
    ///
    /// A deployment may ship policy that anticipates a provider it has not dropped in yet. Cedar's
    /// strict validator rejects a policy naming an action outside the schema, so such a name is
    /// registered here as a *phantom*: it exists in the generated schema and nowhere else.
    ///
    /// The alternative — dropping the offending policy — is wrong, and worth saying why. A policy
    /// reading `action in [gh.pull-request.read, gh.issue.create]` with only the first loaded would
    /// lose *both* grants, silently revoking authority the operator still has every reason to
    /// expect. A phantom keeps the policy whole and takes away exactly the missing capability.
    ///
    /// A phantom can never authorize an execution. It routes to no provider, the broker refuses any
    /// constraint set naming an unrouted capability, and an invocation naming one is denied
    /// `unconstrained-capability` before Cedar is consulted at all.
    ///
    /// Every reported name parses: `classify_policies` refuses a literal outside the identifier
    /// grammar in both modes, so nothing silently fails to register here.
    #[must_use]
    fn with_phantoms(&self, unresolved: &[UnresolvedName]) -> Self {
        let mut world = self.clone();
        for entry in unresolved {
            match entry.kind {
                UnresolvedKind::Capability => {
                    if let Ok(capability) = entry.name.parse::<CapabilityId>() {
                        world.phantom_capabilities.insert(capability);
                    }
                }
                UnresolvedKind::Provider => {
                    if let Ok(provider) = entry.name.parse::<ProviderId>() {
                        world.phantom_providers.insert(provider);
                    }
                }
            }
        }
        world
    }

    /// Renders the Cedar schema this world implies.
    fn schema_json(&self) -> serde_json::Value {
        let entity_shape = json!({ "shape": { "type": "Record", "attributes": {} } });
        let capability_context = json!({
            "type": "Record",
            "attributes": {
                "via": { "type": "String", "required": false },
                "subject": { "type": "String", "required": false },
                "agent": { "type": "String", "required": false },
                "transportKind": { "type": "String", "required": false },
                "transport": { "type": "String", "required": false },
                "channel": { "type": "String", "required": false },
                "conversation": { "type": "String", "required": false },
                "effect": { "type": "String" },
                "risk": { "type": "String" },
                "idempotency": { "type": "String" },
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
                "channel": { "type": "String", "required": false },
                "conversation": { "type": "String", "required": false },
            }
        });

        // Phantom capabilities are indistinguishable from routed ones *here*, and only here: the
        // schema is what strict validation checks a policy against, so a phantom is what lets a
        // policy naming an unloaded capability stay whole. Nothing downstream can execute one.
        let mut actions = serde_json::Map::new();
        for capability in self
            .capabilities
            .keys()
            .chain(self.phantom_capabilities.iter())
        {
            actions.insert(
                capability.as_str().to_owned(),
                json!({
                    "appliesTo": {
                        "principalTypes": ["Principal"],
                        "resourceTypes": ["Provider"],
                        "context": capability_context,
                    }
                }),
            );
        }
        actions.insert(
            AGENT_PROMPT_ACTION.to_owned(),
            json!({
                "appliesTo": {
                    "principalTypes": ["Principal"],
                    "resourceTypes": ["Agent"],
                    "context": prompt_context,
                }
            }),
        );

        json!({
            NAMESPACE: {
                "entityTypes": {
                    "Principal": entity_shape,
                    "Provider": entity_shape,
                    "Agent": entity_shape,
                },
                "actions": actions,
            }
        })
    }
}

/// What one authorization request is about.
///
/// The action and its resource travel together because they are not independently valid: a
/// capability always acts on its provider, and `agent.prompt` always acts on an agent. Splitting
/// them would let a caller assemble a pair the schema rejects and turn a programming mistake into a
/// runtime denial.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyTarget {
    /// One provider capability, with the trusted classification the broker will execute it under.
    Capability {
        /// Requested capability, which is also the Cedar action identifier.
        capability: CapabilityId,
        /// Provider the capability routes to, which is the Cedar resource.
        provider: ProviderId,
        /// Trusted effect classification, rendered into `context.effect`.
        effect: EffectKind,
        /// Trusted risk classification, rendered into `context.risk`.
        risk: RiskLevel,
        /// Trusted retry classification, rendered into `context.idempotency`.
        idempotency: Idempotency,
    },
    /// Permission for the principal to drive one agent's session at all.
    AgentPrompt {
        /// The agent being driven, which is the Cedar resource.
        agent: AgentId,
    },
}

impl PolicyTarget {
    /// The Cedar action identifier this target names.
    #[must_use]
    pub fn action(&self) -> &str {
        match self {
            Self::Capability { capability, .. } => capability.as_str(),
            Self::AgentPrompt { .. } => AGENT_PROMPT_ACTION,
        }
    }
}

/// Trusted routing metadata a policy may condition on.
///
/// Every field is derived by the broker from authenticated transport state or owner-controlled
/// configuration. None of it can be set by a request payload.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PolicyContext {
    /// The attestor peer an attested context was derived through; absent for direct peers.
    pub via: Option<String>,
    /// The canonical external subject an attested context stands for.
    pub subject: Option<String>,
    /// The agent identity of an agent actor.
    pub agent: Option<String>,
    /// Chat transport family, absent for legacy operations.
    pub transport_kind: Option<String>,
    /// Owner-configured transport identifier, absent for legacy operations.
    pub transport: Option<String>,
    /// Canonical service channel, absent for legacy operations.
    pub channel: Option<String>,
    /// Canonical service conversation, absent for legacy operations.
    pub conversation: Option<String>,
}

/// One authorization question.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyRequest {
    /// Principal the broker resolved for this request.
    pub principal: PrincipalId,
    /// Action and resource.
    pub target: PolicyTarget,
    /// Trusted routing metadata.
    pub context: PolicyContext,
}

/// One authorization answer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PolicyDecision {
    /// Whether the policy set permits the request. False whenever evaluation reported an error.
    pub allowed: bool,
    /// Identifiers of the policies that determined the answer, in sorted order.
    pub determining_policy_ids: Vec<String>,
    /// Whether Cedar reported any evaluation error while deciding.
    ///
    /// A stable flag rather than the error text: a denial explanation must not become a channel for
    /// policy source or entity data on a per-request path.
    pub errors_present: bool,
}

impl PolicyDecision {
    /// The answer given when a request could not even be constructed.
    fn refused() -> Self {
        Self {
            allowed: false,
            determining_policy_ids: Vec::new(),
            errors_present: true,
        }
    }
}

/// Which kind of declared name a policy referenced but the world does not contain.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum UnresolvedKind {
    /// A Cedar action, which is a capability identifier.
    Capability,
    /// A provider resource.
    Provider,
}

impl UnresolvedKind {
    /// Returns the stable label used in operator-facing diagnostics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Capability => "capability",
            Self::Provider => "provider",
        }
    }
}

/// One provider-derived name a policy references that no loaded provider declares.
///
/// Reported by [`PolicyEngine::new_lenient`] so a deployment can warn about policy that anticipates
/// a provider it has not dropped in yet, instead of refusing to start. Principals are deliberately
/// absent from this type: they come from owner-authored identities rather than a loaded component,
/// so an undeclared principal is a typo and stays fatal in both modes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnresolvedName {
    /// Identifier of the policy that named it.
    pub policy: String,
    /// The undeclared name, exactly as the policy spelled it.
    pub name: String,
    /// Whether it was named as an action or as a resource.
    pub kind: UnresolvedKind,
}

/// How a policy naming a provider-derived entity the world does not declare is handled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Handling {
    /// Refuse to build. Every undeclared name is a [`PolicyBuildError`].
    Refuse,
    /// Register the name as a phantom and report it. See [`PolicyWorld::with_phantoms`].
    Tolerate,
}

/// A validated, startup-fixed Cedar policy set with its generated schema and entity store.
pub struct PolicyEngine {
    policies: PolicySet,
    schema: Schema,
    entities: Entities,
    authorizer: Authorizer,
    referenced_capabilities: BTreeSet<CapabilityId>,
    policy_count: usize,
    digest: String,
}

// Written by hand rather than derived: `PolicySet`'s own `Debug` renders policy source, and this
// value is reachable from `Broker`'s derived `Debug`. A fingerprint and two counts are what an
// operator needs from a log line; the policy text is not.
impl fmt::Debug for PolicyEngine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PolicyEngine")
            .field("digest", &self.digest)
            .field("policies", &self.policy_count)
            .field(
                "referenced_capabilities",
                &self.referenced_capabilities.len(),
            )
            .finish()
    }
}

impl PolicyEngine {
    /// Parses, schema-validates, and freezes one policy set against a declared world.
    ///
    /// Empty (or whitespace-only) policy text is valid and permits nothing, which is the honest
    /// deny-by-default starting point for a deployment that has not written policy yet.
    ///
    /// # Errors
    ///
    /// Returns a [`PolicyBuildError`] when the source exceeds its byte or policy-count bound, fails
    /// to parse, contains a template, fails strict schema validation, or names an entity the world
    /// does not declare.
    pub fn new(policy_text: &str, world: &PolicyWorld) -> Result<Self, PolicyBuildError> {
        let (engine, unresolved) = Self::build(policy_text, world, Handling::Refuse)?;
        debug_assert!(
            unresolved.is_empty(),
            "Handling::Refuse returns an error rather than tolerating a name"
        );
        Ok(engine)
    }

    /// Parses and validates one policy set, tolerating names no loaded provider declares.
    ///
    /// Identical to [`PolicyEngine::new`] except that a policy naming an undeclared capability or
    /// provider is kept and the name reported, rather than refusing to start. This lets a
    /// deployment ship policy that anticipates a provider it has not dropped in yet; the caller is
    /// expected to warn about every returned [`UnresolvedName`].
    ///
    /// Tolerating a name grants nothing. The name is registered as a phantom: it routes to no
    /// provider, the broker refuses any constraint set naming an unrouted capability, and an
    /// invocation naming one is denied `unconstrained-capability` before Cedar is consulted at
    /// all. Dropping the offending policy instead would silently revoke the grants it makes
    /// alongside the missing one.
    ///
    /// # Errors
    ///
    /// The same failures as [`PolicyEngine::new`], minus [`PolicyBuildError::UnknownAction`] and
    /// [`PolicyBuildError::UnknownProvider`] for a name that is a well-formed identifier. An
    /// undeclared *principal* remains an error here: principals come from owner-authored
    /// configuration, not from a loaded component. So does a literal outside the identifier
    /// grammar — `Dekopon::Action::"GH.Read"` can never become a loaded capability however many
    /// providers arrive later, so it gets the same specific error strict mode gives it.
    pub fn new_lenient(
        policy_text: &str,
        world: &PolicyWorld,
    ) -> Result<(Self, Vec<UnresolvedName>), PolicyBuildError> {
        Self::build(policy_text, world, Handling::Tolerate)
    }

    fn build(
        policy_text: &str,
        world: &PolicyWorld,
        handling: Handling,
    ) -> Result<(Self, Vec<UnresolvedName>), PolicyBuildError> {
        if policy_text.len() > MAX_POLICY_BYTES {
            return Err(PolicyBuildError::PolicyTooLarge {
                length: policy_text.len(),
                maximum: MAX_POLICY_BYTES,
            });
        }
        let policies = if policy_text.trim().is_empty() {
            PolicySet::new()
        } else {
            PolicySet::from_str(policy_text).map_err(|source| PolicyBuildError::Parse {
                message: source.to_string(),
            })?
        };
        if policies.num_of_templates() > 0 {
            return Err(PolicyBuildError::TemplateUnsupported);
        }
        if policies.num_of_policies() > MAX_POLICIES {
            return Err(PolicyBuildError::TooManyPolicies {
                count: policies.num_of_policies(),
                maximum: MAX_POLICIES,
            });
        }
        let policies = apply_annotated_ids(&policies)?;

        // Classification runs *before* schema generation, which is the whole reordering. Cedar's
        // strict validator rejects a policy naming an action outside the schema, so a tolerated
        // name has to be in the schema by the time validation runs.
        let (referenced_capabilities, unresolved) = classify_policies(&policies, world, handling)?;
        let effective = world.with_phantoms(&unresolved);

        let schema = Schema::from_json_value(effective.schema_json()).map_err(|source| {
            PolicyBuildError::Schema {
                message: source.to_string(),
            }
        })?;
        let validation = Validator::new(schema.clone()).validate(&policies, ValidationMode::Strict);
        if !validation.validation_passed() {
            let mut messages = validation
                .validation_errors()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            messages.sort();
            return Err(PolicyBuildError::Validation { messages });
        }

        let entities = build_entities(&effective, &schema)?;
        let digest = policy_digest(&policies, world, &unresolved)?;

        Ok((
            Self {
                policy_count: policies.num_of_policies(),
                policies,
                schema,
                entities,
                authorizer: Authorizer::new(),
                referenced_capabilities,
                digest,
            },
            unresolved,
        ))
    }

    /// Decides one request; every failure path denies.
    #[must_use]
    pub fn authorize(&self, request: PolicyRequest) -> PolicyDecision {
        let Ok(cedar_request) = self.build_request(&request) else {
            return PolicyDecision::refused();
        };
        let response =
            self.authorizer
                .is_authorized(&cedar_request, &self.policies, &self.entities);
        let errors_present = response.diagnostics().errors().next().is_some();
        let mut determining_policy_ids = response
            .diagnostics()
            .reason()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        determining_policy_ids.sort();
        determining_policy_ids.dedup();
        PolicyDecision {
            allowed: matches!(response.decision(), Decision::Allow) && !errors_present,
            determining_policy_ids,
            errors_present,
        }
    }

    /// The capabilities some policy in this set names.
    ///
    /// The broker requires an owner-authored constraint set for each of them at startup: a policy
    /// that can permit a capability nothing knows how to execute is a configuration mistake worth
    /// refusing to start over.
    pub fn referenced_capabilities(&self) -> impl Iterator<Item = &CapabilityId> {
        self.referenced_capabilities.iter()
    }

    /// Number of static policies loaded.
    #[must_use]
    pub fn policy_count(&self) -> usize {
        self.policy_count
    }

    /// A `sha256:<hex>` fingerprint of the loaded policy set and the world it was validated
    /// against.
    ///
    /// Domain-separated over canonicalized policy source plus the sorted entity and action
    /// identifiers, so two brokers reporting the same digest evaluated the same authorization
    /// surface. It is a correlation aid for audit records, not a wire-format contract.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    fn build_request(&self, request: &PolicyRequest) -> Result<cedar_policy::Request, ()> {
        let principal = entity_uid(PRINCIPAL_TYPE, request.principal.as_str())?;
        let action = entity_uid(ACTION_TYPE, request.target.action())?;
        let (resource, mut pairs) = match &request.target {
            PolicyTarget::Capability {
                provider,
                effect,
                risk,
                idempotency,
                ..
            } => (
                entity_uid(PROVIDER_TYPE, provider.as_str())?,
                vec![
                    (
                        "effect".to_owned(),
                        RestrictedExpression::new_string(effect.to_string()),
                    ),
                    (
                        "risk".to_owned(),
                        RestrictedExpression::new_string(risk.to_string()),
                    ),
                    (
                        "idempotency".to_owned(),
                        RestrictedExpression::new_string(idempotency.to_string()),
                    ),
                ],
            ),
            PolicyTarget::AgentPrompt { agent } => {
                (entity_uid(AGENT_TYPE, agent.as_str())?, Vec::new())
            }
        };
        for (name, value) in [
            ("via", request.context.via.as_ref()),
            ("subject", request.context.subject.as_ref()),
            ("agent", request.context.agent.as_ref()),
            ("transportKind", request.context.transport_kind.as_ref()),
            ("transport", request.context.transport.as_ref()),
            ("channel", request.context.channel.as_ref()),
            ("conversation", request.context.conversation.as_ref()),
        ] {
            if let Some(value) = value {
                pairs.push((
                    name.to_owned(),
                    RestrictedExpression::new_string(value.clone()),
                ));
            }
        }
        let context = Context::from_pairs(pairs).map_err(|_| ())?;
        cedar_policy::Request::new(principal, action, resource, context, Some(&self.schema))
            .map_err(|_| ())
    }
}

/// Renames each policy to its optional `@id("…")` annotation.
///
/// Cedar names text-parsed policies positionally (`policy0`, `policy1`, …), and those identifiers
/// are what an audit record carries as the reason for a decision. A positional name answers
/// "which line" but not "which rule", and it shifts when an unrelated policy is inserted above it,
/// so an annotation is honored as the stable name instead. Duplicates refuse startup: two policies
/// sharing one name would make an explanation ambiguous.
fn apply_annotated_ids(policies: &PolicySet) -> Result<PolicySet, PolicyBuildError> {
    let mut renamed = PolicySet::new();
    let mut seen = BTreeSet::new();
    for policy in policies.policies() {
        let id = match policy.annotation("id") {
            Some(annotation) => {
                if annotation.is_empty()
                    || annotation.len() > MAX_POLICY_ID_BYTES
                    || !annotation.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')
                    })
                {
                    return Err(PolicyBuildError::InvalidPolicyId {
                        policy: policy.id().to_string(),
                    });
                }
                PolicyId::new(annotation)
            }
            None => policy.id().clone(),
        };
        if !seen.insert(id.to_string()) {
            return Err(PolicyBuildError::DuplicatePolicyId {
                policy: id.to_string(),
            });
        }
        renamed
            .add(policy.new_id(id))
            .map_err(|source| PolicyBuildError::Parse {
                message: source.to_string(),
            })?;
    }
    Ok(renamed)
}

fn entity_uid(type_name: &str, id: &str) -> Result<EntityUid, ()> {
    let type_name = EntityTypeName::from_str(type_name).map_err(|_| ())?;
    Ok(EntityUid::from_type_name_and_id(
        type_name,
        EntityId::new(id),
    ))
}

/// Proves every entity a policy names is one the world declares.
///
/// Cedar's validator checks types, not instances: `principal == Dekopon::Principal::"typo"` is
/// perfectly well typed and simply never matches. That is exactly the failure mode the old exact
/// engine caught with its reachability check, so it is caught here instead — a policy naming an
/// undeclared principal or provider refuses startup rather than becoming latent dead policy.
///
/// Agents are the deliberate exception: the agent catalog belongs to the gateway, so the broker
/// declares the type and matches instances by UID without enumerating them.
/// Classifies every policy's entity literals against the declared world.
///
/// Returns the capabilities the policy set references, plus every provider-derived name the world
/// does not contain. Cedar's own validator checks entity *types*, not instances, which is why this
/// exists at all.
///
/// Two classes of name are treated differently on purpose:
///
/// - **Principals** come from owner-authored identities and subject mappings, never from a loaded
///   component. An undeclared one is a typo, and stays fatal in both modes.
/// - **Actions and providers** are derived from loaded provider manifests. An undeclared one means
///   that provider is not loaded, which is a legitimate state for a deployment whose policy
///   anticipates it. Under [`Handling::Tolerate`] it is reported and registered as a phantom.
///
/// An entity type outside the Dekopon namespace is a grammar error, not an absence, and is fatal
/// in both modes.
fn classify_policies(
    policies: &PolicySet,
    world: &PolicyWorld,
    handling: Handling,
) -> Result<(BTreeSet<CapabilityId>, Vec<UnresolvedName>), PolicyBuildError> {
    let mut referenced_capabilities = BTreeSet::new();
    let mut unresolved = Vec::new();
    for policy in policies.policies() {
        let id = policy.id().to_string();
        for uid in policy.entity_literals() {
            let type_name = uid.type_name().to_string();
            let value = uid.id().unescaped().to_owned();
            match type_name.as_str() {
                PRINCIPAL_TYPE => {
                    let principal = value.parse::<PrincipalId>().map_err(|_| {
                        PolicyBuildError::UnknownPrincipal {
                            policy: id.clone(),
                            principal: value.clone(),
                        }
                    })?;
                    if !world.principals.contains(&principal) {
                        return Err(PolicyBuildError::UnknownPrincipal {
                            policy: id.clone(),
                            principal: value,
                        });
                    }
                }
                PROVIDER_TYPE => {
                    let parsed = value.parse::<ProviderId>().ok();
                    let declared = parsed
                        .as_ref()
                        .is_some_and(|provider| world.providers.contains(provider));
                    if !declared {
                        // A literal outside the identifier grammar can never become a loaded
                        // provider, so it is a typo like a misspelled principal rather than an
                        // anticipated one, and gets the specific error in both modes. Tolerating
                        // it would drop it from the phantom set and surface later as a raw Cedar
                        // validation failure with the `UnresolvedName` report lost.
                        if parsed.is_none() || handling == Handling::Refuse {
                            return Err(PolicyBuildError::UnknownProvider {
                                policy: id.clone(),
                                provider: value,
                            });
                        }
                        unresolved.push(UnresolvedName {
                            policy: id.clone(),
                            name: value,
                            kind: UnresolvedKind::Provider,
                        });
                    }
                }
                ACTION_TYPE => {
                    if value == AGENT_PROMPT_ACTION {
                        continue;
                    }
                    let parsed = value.parse::<CapabilityId>().ok();
                    match parsed
                        .clone()
                        .filter(|capability| world.capabilities.contains_key(capability))
                    {
                        Some(capability) => {
                            referenced_capabilities.insert(capability);
                        }
                        // Same rule as a provider literal: a name outside the identifier grammar
                        // can never become a loaded capability, so it is a typo rather than an
                        // anticipation and stays fatal even under `Tolerate`.
                        None => {
                            if parsed.is_none() || handling == Handling::Refuse {
                                return Err(PolicyBuildError::UnknownAction {
                                    policy: id.clone(),
                                    action: value,
                                });
                            }
                            unresolved.push(UnresolvedName {
                                policy: id.clone(),
                                name: value,
                                kind: UnresolvedKind::Capability,
                            });
                        }
                    }
                }
                // The schema already rejects an unknown entity type, and `Agent` instances are
                // intentionally unenumerated.
                AGENT_TYPE => {}
                other => {
                    return Err(PolicyBuildError::UnknownEntityType {
                        policy: id.clone(),
                        entity_type: other.to_owned(),
                    });
                }
            }
        }
    }
    Ok((referenced_capabilities, unresolved))
}

fn build_entities(world: &PolicyWorld, schema: &Schema) -> Result<Entities, PolicyBuildError> {
    let mut entities = Vec::new();
    for (type_name, ids) in [
        (
            PRINCIPAL_TYPE,
            world
                .principals
                .iter()
                .map(PrincipalId::as_str)
                .collect::<Vec<_>>(),
        ),
        (
            PROVIDER_TYPE,
            world
                .providers
                .iter()
                .chain(world.phantom_providers.iter())
                .map(ProviderId::as_str)
                .collect::<Vec<_>>(),
        ),
    ] {
        for id in ids {
            let uid = entity_uid(type_name, id).map_err(|()| PolicyBuildError::Entities {
                message: format!("could not build entity {type_name}::{id:?}"),
            })?;
            entities.push(Entity::new_no_attrs(uid, HashSet::new()));
        }
    }
    let actions = schema
        .action_entities()
        .map_err(|source| PolicyBuildError::Entities {
            message: source.to_string(),
        })?;
    Entities::from_entities(entities.into_iter().chain(actions), Some(schema)).map_err(|source| {
        PolicyBuildError::Entities {
            message: source.to_string(),
        }
    })
}

fn policy_digest(
    policies: &PolicySet,
    world: &PolicyWorld,
    unresolved: &[UnresolvedName],
) -> Result<String, PolicyBuildError> {
    // Cedar's structural JSON rather than the source text: two spellings of one policy must
    // fingerprint identically, so reformatting a policy file does not look like a policy change.
    // `Display` and `to_cedar` both round-trip the original bytes and would not do that, so a
    // fallback to either would quietly abandon the property — two brokers loading semantically
    // identical files would report different digests in every audit record with nothing saying
    // why. The digest is computed once at startup, so failing closed here is cheap.
    let canonical = policies
        .policies()
        .map(|policy| {
            let json = policy
                .to_json()
                .map_err(|source| PolicyBuildError::Digest {
                    policy: policy.id().to_string(),
                    message: source.to_string(),
                })?
                .to_string();
            Ok((policy.id().to_string(), json))
        })
        .collect::<Result<BTreeMap<_, _>, PolicyBuildError>>()?;

    let mut hasher = Sha256::new();
    hasher.update(DIGEST_DOMAIN);
    hasher.update(b"policies\0");
    for (id, text) in &canonical {
        hasher.update(id.as_bytes());
        hasher.update([0]);
        hasher.update(text.as_bytes());
        hasher.update([0]);
    }
    hasher.update(b"entities\0");
    for principal in &world.principals {
        hasher.update(format!("{PRINCIPAL_TYPE}::{:?}", principal.as_str()).as_bytes());
        hasher.update([0]);
    }
    for provider in &world.providers {
        hasher.update(format!("{PROVIDER_TYPE}::{:?}", provider.as_str()).as_bytes());
        hasher.update([0]);
    }
    hasher.update(b"actions\0");
    let mut actions = world
        .capabilities
        .keys()
        .map(|capability| capability.as_str())
        .collect::<Vec<_>>();
    actions.push(AGENT_PROMPT_ACTION);
    actions.sort_unstable();
    for action in actions {
        hasher.update(action.as_bytes());
        hasher.update([0]);
    }

    // Tolerated names, recorded explicitly. The `actions` section above already moves when a
    // capability stops being loaded, so this is belt-and-braces rather than load-bearing today; it
    // states "this deployment tolerated an absent name" directly instead of leaving it to be
    // inferred from what the world section omits.
    hasher.update(b"phantoms\0");
    let mut phantoms = unresolved
        .iter()
        .map(|entry| format!("{}::{}", entry.kind.label(), entry.name))
        .collect::<Vec<_>>();
    phantoms.sort_unstable();
    phantoms.dedup();
    for phantom in phantoms {
        hasher.update(phantom.as_bytes());
        hasher.update([0]);
    }

    let mut hex = String::with_capacity(64 + 7);
    hex.push_str("sha256:");
    for byte in hasher.finalize() {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        hex.push(char::from(HEX[usize::from(byte >> 4)]));
        hex.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(hex)
}

/// Failure to build a coherent, validated policy engine.
///
/// Every variant is a startup failure. Construction-time detail is deliberately verbose — an
/// operator is holding the policy file — while runtime decisions carry only identifiers and a
/// [`PolicyDecision::errors_present`] flag.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PolicyBuildError {
    /// Policy source exceeded its byte ceiling.
    #[error("policy source is {length} bytes; maximum is {maximum}")]
    PolicyTooLarge {
        /// Actual byte length.
        length: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// Policy set exceeded its count ceiling.
    #[error("policy set contains {count} policies; maximum is {maximum}")]
    TooManyPolicies {
        /// Actual count.
        count: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// Policy source did not parse.
    #[error("policy source could not be parsed: {message}")]
    Parse {
        /// Parser diagnostics.
        message: String,
    },
    /// Policy source declared a template.
    ///
    /// Templates only authorize once linked, and linking is a runtime operation this engine
    /// deliberately does not have. A template would be policy that silently never applies.
    #[error("policy templates are not supported; write static policies instead")]
    TemplateUnsupported,
    /// The world could not be expressed as a Cedar schema.
    #[error("policy schema could not be generated: {message}")]
    Schema {
        /// Schema diagnostics.
        message: String,
    },
    /// The policy set failed strict schema validation.
    #[error("policy set failed strict validation: {}", messages.join("; "))]
    Validation {
        /// Sorted validator diagnostics.
        messages: Vec<String>,
    },
    /// A policy named a principal the world does not declare.
    #[error("policy {policy} names undeclared principal {principal:?}")]
    UnknownPrincipal {
        /// Policy identifier.
        policy: String,
        /// Undeclared principal.
        principal: String,
    },
    /// A policy named a provider the world does not declare.
    #[error("policy {policy} names undeclared provider {provider:?}")]
    UnknownProvider {
        /// Policy identifier.
        policy: String,
        /// Undeclared provider.
        provider: String,
    },
    /// A policy named an action the world does not declare.
    #[error("policy {policy} names undeclared action {action:?}")]
    UnknownAction {
        /// Policy identifier.
        policy: String,
        /// Undeclared action.
        action: String,
    },
    /// A policy named an entity type outside the Dekopon namespace.
    #[error("policy {policy} names unknown entity type {entity_type}")]
    UnknownEntityType {
        /// Policy identifier.
        policy: String,
        /// Unknown entity type.
        entity_type: String,
    },
    /// An `@id` annotation was empty, overlong, or outside the portable identifier alphabet.
    #[error("policy {policy} has an @id annotation that is not a bounded portable identifier")]
    InvalidPolicyId {
        /// Positional identifier of the offending policy.
        policy: String,
    },
    /// Two policies resolved to one identifier.
    #[error("policy identifier {policy:?} is used by more than one policy")]
    DuplicatePolicyId {
        /// Duplicated identifier.
        policy: String,
    },
    /// One capability identifier was declared twice.
    #[error("policy world declares capability {capability} more than once")]
    DuplicateCapability {
        /// Duplicated capability.
        capability: CapabilityId,
    },
    /// A capability collided with the fixed `agent.prompt` action.
    #[error("capability {capability} collides with the reserved agent.prompt action")]
    ReservedAction {
        /// Colliding capability.
        capability: CapabilityId,
    },
    /// The declared world could not be turned into a Cedar entity store.
    #[error("policy entity store could not be built: {message}")]
    Entities {
        /// Entity diagnostics.
        message: String,
    },
    /// A policy could not be rendered as the structural JSON the digest fingerprints.
    ///
    /// The digest deliberately hashes Cedar's structural JSON so two spellings of one policy
    /// fingerprint identically. Degrading to the source text would abandon that property silently,
    /// so construction refuses instead; the digest is computed once at startup.
    #[error("policy {policy} could not be canonicalized for the policy digest: {message}")]
    Digest {
        /// Policy identifier.
        policy: String,
        /// Canonicalization diagnostics.
        message: String,
    },
}

#[cfg(test)]
mod tests;
