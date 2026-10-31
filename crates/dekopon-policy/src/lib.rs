//! ```
//! use dekopon_core::{CapabilityId, PrincipalId, ProviderId};
//! use dekopon_policy::{PolicyContext, PolicyEngine, PolicyRequest, PolicyTarget, PolicyWorld};
//! use dekopon_capability::EffectKind;
//! use dekopon_core::RiskLevel;
//!
//! let world = PolicyWorld::new(
//!     ["cpetersen".parse::<PrincipalId>()?],
//!     [(
//!         "cli-probe.upper".parse::<CapabilityId>()?,
//!         "cli-probe".parse::<ProviderId>()?,
//!     )],
//! )?;
//! let engine = PolicyEngine::new(
//!     r#"@id("cpetersen-may-cli-probe-upper")
//!        permit(principal == Dekopon::Principal::"cpetersen",
//!               action == Dekopon::Action::"cli-probe.upper",
//!               resource == Dekopon::Provider::"cli-probe");"#,
//!     &world,
//! )?;
//! let decision = engine.authorize(PolicyRequest {
//!     principal: "cpetersen".parse()?,
//!     target: PolicyTarget::Capability {
//!         capability: "cli-probe.upper".parse()?,
//!         provider: "cli-probe".parse()?,
//!         effect: EffectKind::ReadOnly,
//!         risk: RiskLevel::Low,
//!     },
//!     context: PolicyContext {
//!         via: Some("dekopon-gatewayd".to_owned()),
//!         agent: Some("reviewer".to_owned()),
//!         ..PolicyContext::default()
//!     },
//! });
//! assert!(decision.allowed);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt,
    str::FromStr as _,
};

use cedar_policy::{
    ActionConstraint, Authorizer, Context, Decision, Effect, Entities, Entity, EntityId,
    EntityTypeName, EntityUid, PolicyId, PolicySet, RestrictedExpression, Schema, ValidationMode,
    Validator,
};
use dekopon_capability::EffectKind;
use dekopon_core::{
    AgentId, CapabilityId, ExternalSubject, GroupId, IdentifierError, PrincipalId, PrincipalKind,
    ProviderId, RiskLevel, SecretDrn, SecretSinkKind,
};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

mod github;
mod request;
pub use request::{HttpRequestFacts, OpaqueBody, RequestBody};

pub const NAMESPACE: &str = "Dekopon";
pub const AGENT_PROMPT_ACTION: &str = "agent.prompt";
pub const SECRET_USE_ACTION: &str = "secret.use";
pub const MAX_POLICY_BYTES: usize = 1024 * 1024;
pub const MAX_POLICIES: usize = 1_024;
pub const MAX_POLICY_ID_BYTES: usize = 128;

const PRINCIPAL_TYPE: &str = "Dekopon::Principal";
const PROVIDER_TYPE: &str = "Dekopon::Provider";
const AGENT_TYPE: &str = "Dekopon::Agent";
const SECRET_TYPE: &str = "Dekopon::Secret";
const ACTION_TYPE: &str = "Dekopon::Action";
const GROUP_TYPE: &str = "Dekopon::Group";
const PROVIDER_GROUP_SUFFIX: &str = ":*";
const READ_ONLY_GROUP_SUFFIX: &str = ":read-only";
const DIGEST_DOMAIN: &[u8] = b"dekopon-policy-v1\0";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PolicyWorld {
    principals: BTreeSet<PrincipalId>,
    memberships: BTreeMap<PrincipalId, BTreeSet<GroupId>>,
    subjects: BTreeMap<ExternalSubject, PrincipalId>,
    providers: BTreeSet<ProviderId>,
    capabilities: BTreeMap<CapabilityId, ProviderId>,
    read_only: BTreeSet<CapabilityId>,
    secrets: BTreeSet<SecretDrn>,
    phantom_capabilities: BTreeSet<CapabilityId>,
    phantom_providers: BTreeSet<ProviderId>,
    phantom_action_groups: BTreeSet<String>,
}

impl PolicyWorld {
    pub fn new(
        principals: impl IntoIterator<Item = PrincipalId>,
        capabilities: impl IntoIterator<Item = (CapabilityId, ProviderId)>,
    ) -> Result<Self, PolicyBuildError> {
        let mut world = Self::default();
        for principal in principals {
            world.principals.insert(principal);
        }
        let mut reserved = BTreeSet::new();
        let mut duplicates = BTreeSet::new();
        for (capability, provider) in capabilities {
            if matches!(capability.as_str(), AGENT_PROMPT_ACTION | SECRET_USE_ACTION) {
                reserved.insert(capability.clone());
            }
            world.providers.insert(provider.clone());
            if world
                .capabilities
                .insert(capability.clone(), provider)
                .is_some()
            {
                duplicates.insert(capability);
            }
        }
        if !reserved.is_empty() || !duplicates.is_empty() {
            return Err(PolicyBuildError::WorldConflicts {
                reserved: reserved.into_iter().collect(),
                duplicates: duplicates.into_iter().collect(),
            });
        }
        Ok(world)
    }

    pub fn principals(&self) -> impl Iterator<Item = &PrincipalId> {
        self.principals.iter()
    }

    pub fn providers(&self) -> impl Iterator<Item = &ProviderId> {
        self.providers.iter()
    }

    pub fn capabilities(&self) -> impl Iterator<Item = (&CapabilityId, &ProviderId)> {
        self.capabilities.iter()
    }

    #[must_use]
    pub fn with_secrets(mut self, secrets: impl IntoIterator<Item = SecretDrn>) -> Self {
        self.secrets.extend(secrets);
        self
    }

    pub fn secrets(&self) -> impl Iterator<Item = &SecretDrn> {
        self.secrets.iter()
    }

    #[must_use]
    pub fn with_group_members(
        mut self,
        members: impl IntoIterator<Item = (PrincipalId, GroupId)>,
    ) -> Self {
        for (principal, group) in members {
            self.principals.insert(principal.clone());
            self.memberships.entry(principal).or_default().insert(group);
        }
        self
    }

    #[must_use]
    pub fn with_read_only(mut self, capabilities: impl IntoIterator<Item = CapabilityId>) -> Self {
        self.read_only.extend(capabilities);
        self
    }

    fn groups(&self) -> BTreeSet<&GroupId> {
        self.memberships.values().flatten().collect()
    }

    fn action_groups_of(&self, capability: &CapabilityId, provider: &ProviderId) -> Vec<String> {
        let mut groups = vec![format!("{provider}{PROVIDER_GROUP_SUFFIX}")];
        if self.read_only.contains(capability) {
            groups.push(format!("{provider}{READ_ONLY_GROUP_SUFFIX}"));
        }
        groups
    }

    fn action_groups(&self) -> BTreeMap<String, BTreeSet<&CapabilityId>> {
        let mut groups = BTreeMap::<String, BTreeSet<&CapabilityId>>::new();
        for (capability, provider) in &self.capabilities {
            for group in self.action_groups_of(capability, provider) {
                groups.entry(group).or_default().insert(capability);
            }
        }
        for phantom in &self.phantom_action_groups {
            groups.entry(phantom.clone()).or_default();
        }
        groups
    }

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
                UnresolvedKind::ActionGroup => {
                    world.phantom_action_groups.insert(entry.name.clone());
                }
            }
        }
        world
    }

    #[must_use]
    pub fn with_subjects(
        mut self,
        subjects: impl IntoIterator<Item = (ExternalSubject, PrincipalId)>,
    ) -> Self {
        self.subjects.extend(subjects);
        self
    }

    #[must_use]
    pub fn principal_kind(&self, principal: &PrincipalId) -> PrincipalKind {
        self.subjects
            .iter()
            .find_map(|(subject, mapped)| (mapped == principal).then(|| subject.kind()))
            .unwrap_or(PrincipalKind::Service)
    }

    #[must_use]
    pub fn identity_digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"dekopon-identity-v1\0");
        hasher.update(b"subjects\0");
        for (subject, principal) in &self.subjects {
            hasher.update(subject.canonical().as_bytes());
            hasher.update([0]);
            hasher.update(principal.as_str().as_bytes());
            hasher.update([0]);
        }
        hasher.update(b"memberships\0");
        for principal in &self.principals {
            hasher.update(b"principal\0");
            hasher.update(principal.as_str().as_bytes());
            hasher.update([0]);
            for group in self.memberships.get(principal).into_iter().flatten() {
                hasher.update(b"group\0");
                hasher.update(group.as_str().as_bytes());
                hasher.update([0]);
            }
        }
        finish_digest(hasher)
    }

    fn schema_json(&self) -> serde_json::Value {
        // Every routing attribute must be declared optional in the schema, or Cedar's strict
        // validator rejects any policy referencing an absent one.
        const ROUTING: [&str; 5] = [
            "subject",
            "transportKind",
            "transport",
            "trigger",
            "startedBy",
        ];
        let mut routing_attributes = serde_json::Map::from_iter(ROUTING.map(|name| {
            (
                name.to_owned(),
                json!({ "type": "String", "required": false }),
            )
        }));
        // Conversation is a nested record, not flat strings, so a policy reading container must
        // guard with has first or Cedar's strict validator refuses it at load time.
        routing_attributes.insert(
            "conversation".to_owned(),
            json!({
                "type": "Record",
                "required": false,
                "attributes": {
                    "kind": { "type": "String" },
                    "container": { "type": "String", "required": false },
                    "id": { "type": "String" },
                    "thread": { "type": "String", "required": false },
                },
            }),
        );
        routing_attributes.insert("github".to_owned(), github::schema());
        // Fields added here must be schema-required only if the broker always stamps them on the
        // request, or Cedar evaluation can fail.
        let context = |required: &[&str]| {
            let mut attributes = routing_attributes.clone();
            attributes.extend(
                ["via", "agent"].map(|name| (name.to_owned(), json!({ "type": "String" }))),
            );
            attributes.extend(
                required
                    .iter()
                    .map(|name| ((*name).to_owned(), json!({ "type": "String" }))),
            );
            json!({ "type": "Record", "attributes": attributes })
        };

        let entity_shape = json!({ "shape": { "type": "Record", "attributes": {} } });
        let capability_context = context(&["effect", "risk"]);
        let prompt_context = context(&[]);
        // Capability, provider and sink are required together on secret.use so a request cannot
        // satisfy policy via a binding different from the one it is actually trusted for.
        let secret_context = context(&["capability", "provider", "sink"]);

        let mut actions = serde_json::Map::new();
        for (capability, groups) in self
            .capabilities
            .iter()
            .map(|(capability, provider)| (capability, self.action_groups_of(capability, provider)))
            .chain(
                self.phantom_capabilities
                    .iter()
                    .map(|capability| (capability, Vec::new())),
            )
        {
            let member_of = groups
                .into_iter()
                .map(|group| json!({ "id": group }))
                .collect::<Vec<_>>();
            actions.insert(
                capability.as_str().to_owned(),
                json!({
                    "memberOf": member_of,
                    "appliesTo": {
                        "principalTypes": ["Principal"],
                        "resourceTypes": ["Provider"],
                        "context": capability_context,
                    }
                }),
            );
        }
        for group in self.action_groups().into_keys() {
            actions.insert(group, json!({}));
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
        let mut entity_types = serde_json::Map::from_iter([
            (
                "Principal".to_owned(),
                json!({ "memberOfTypes": ["Group"], "shape": { "type": "Record", "attributes": { "kind": { "type": "String" } } } }),
            ),
            ("Group".to_owned(), entity_shape.clone()),
            ("Provider".to_owned(), entity_shape.clone()),
            ("Agent".to_owned(), entity_shape),
        ]);
        if !self.secrets.is_empty() {
            actions.insert(
                SECRET_USE_ACTION.to_owned(),
                json!({
                    "appliesTo": {
                        "principalTypes": ["Principal"],
                        "resourceTypes": ["Secret"],
                        "context": secret_context,
                    }
                }),
            );
            entity_types.insert(
                "Secret".to_owned(),
                json!({ "shape": { "type": "Record", "attributes": {} } }),
            );
        }

        json!({
            NAMESPACE: {
                "entityTypes": entity_types,
                "actions": actions,
            }
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyTarget {
    Capability {
        capability: CapabilityId,
        provider: ProviderId,
        effect: EffectKind,
        risk: RiskLevel,
    },
    AgentPrompt {
        agent: AgentId,
    },
    SecretUse {
        secret: SecretDrn,
        capability: CapabilityId,
        provider: ProviderId,
        sink: SecretSinkKind,
    },
}

impl PolicyTarget {
    #[must_use]
    pub fn action(&self) -> &str {
        match self {
            Self::Capability { capability, .. } => capability.as_str(),
            Self::AgentPrompt { .. } => AGENT_PROMPT_ACTION,
            Self::SecretUse { .. } => SECRET_USE_ACTION,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PolicyContext {
    pub via: Option<String>,
    pub subject: Option<String>,
    pub agent: Option<String>,
    pub transport_kind: Option<String>,
    pub transport: Option<String>,
    pub trigger: Option<String>,
    pub started_by: Option<String>,
    pub conversation: Option<PolicyConversation>,
    pub github: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyConversation {
    pub kind: String,
    pub container: Option<String>,
    pub id: String,
    pub thread: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyRequest {
    pub principal: PrincipalId,
    pub target: PolicyTarget,
    pub context: PolicyContext,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PolicyDecision {
    pub allowed: bool,
    pub determining_policy_ids: Vec<String>,
    pub erroring_policy_ids: Vec<String>,
    pub errors_present: bool,
    pub refusal: Option<String>,
    pub opaque_body: Option<OpaqueBody>,
}

impl PolicyDecision {
    fn refused(error: &RequestError) -> Self {
        Self {
            allowed: false,
            determining_policy_ids: Vec::new(),
            erroring_policy_ids: Vec::new(),
            errors_present: true,
            refusal: Some(error.to_string()),
            opaque_body: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum UnresolvedKind {
    Capability,
    Provider,
    ActionGroup,
}

impl UnresolvedKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Capability => "capability",
            Self::Provider => "provider",
            Self::ActionGroup => "action group",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnresolvedName {
    pub policy: String,
    pub name: String,
    pub kind: UnresolvedKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Handling {
    Refuse,
    Tolerate,
}

pub struct PolicyEngine {
    policies: PolicySet,
    schema: Schema,
    entities: Entities,
    entity_types: EntityTypes,
    authorizer: Authorizer,
    referenced_capabilities: BTreeSet<CapabilityId>,
    permit_reach: PermitReach,
    policy_count: usize,
    digest: String,
}

enum PermitReach {
    AnyAction,
    Named(BTreeSet<CapabilityId>),
}

#[derive(Debug)]
struct EntityTypes {
    principal: EntityTypeName,
    action: EntityTypeName,
    provider: EntityTypeName,
    agent: EntityTypeName,
    secret: EntityTypeName,
}

impl EntityTypes {
    fn parse() -> Result<Self, PolicyBuildError> {
        Ok(Self {
            principal: entity_type_name(PRINCIPAL_TYPE)?,
            action: entity_type_name(ACTION_TYPE)?,
            provider: entity_type_name(PROVIDER_TYPE)?,
            agent: entity_type_name(AGENT_TYPE)?,
            secret: entity_type_name(SECRET_TYPE)?,
        })
    }
}

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
    pub fn new(policy_text: &str, world: &PolicyWorld) -> Result<Self, PolicyBuildError> {
        let (engine, unresolved) = Self::build(policy_text, world, Handling::Refuse)?;
        debug_assert!(
            unresolved.is_empty(),
            "Handling::Refuse returns an error rather than tolerating a name"
        );
        Ok(engine)
    }

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
                messages: source.iter().map(ToString::to_string).collect(),
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
        let (ordinary_policies, invalid_request_shapes) = inspect_request_shapes(&policies)?;
        if !invalid_request_shapes.is_empty() {
            return Err(PolicyBuildError::InvalidRequestShapes {
                policies: invalid_request_shapes,
            });
        }

        // Classification must run before schema generation; a tolerated name needs to already be in
        // the schema before Cedar's strict validator runs.
        let (referenced_capabilities, unresolved) = classify_policies(&policies, world, handling)?;
        let permit_reach = permit_reach(&policies, world);
        let effective = world.with_phantoms(&unresolved);

        let schema = Schema::from_json_value(effective.schema_json()).map_err(|source| {
            PolicyBuildError::Schema {
                message: source.to_string(),
            }
        })?;
        let validation =
            Validator::new(schema.clone()).validate(&ordinary_policies, ValidationMode::Strict);
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
                entity_types: EntityTypes::parse()?,
                authorizer: Authorizer::new(),
                referenced_capabilities,
                permit_reach,
                digest,
            },
            unresolved,
        ))
    }

    #[must_use]
    pub fn authorize(&self, request: PolicyRequest) -> PolicyDecision {
        self.authorize_with_facts(request, None)
    }

    #[must_use]
    pub fn authorize_http(
        &self,
        request: PolicyRequest,
        facts: HttpRequestFacts,
    ) -> PolicyDecision {
        self.authorize_with_facts(request, Some(facts))
    }

    fn authorize_with_facts(
        &self,
        request: PolicyRequest,
        facts: Option<HttpRequestFacts>,
    ) -> PolicyDecision {
        let (cedar_request, opaque_body) = match self.build_request(request, facts) {
            Ok(built) => built,
            Err(error) => return PolicyDecision::refused(&error),
        };
        let response =
            self.authorizer
                .is_authorized(&cedar_request, &self.policies, &self.entities);
        let mut erroring_policy_ids = response
            .diagnostics()
            .errors()
            .map(|error| match error {
                cedar_policy::AuthorizationError::PolicyEvaluationError(error) => {
                    error.policy_id().to_string()
                }
            })
            .collect::<Vec<_>>();
        erroring_policy_ids.sort();
        erroring_policy_ids.dedup();
        let errors_present = !erroring_policy_ids.is_empty();
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
            erroring_policy_ids,
            errors_present,
            refusal: None,
            opaque_body,
        }
    }

    pub fn referenced_capabilities(&self) -> impl Iterator<Item = &CapabilityId> {
        self.referenced_capabilities.iter()
    }

    #[must_use]
    pub fn names_in_a_permit(&self, capability: &CapabilityId) -> bool {
        match &self.permit_reach {
            PermitReach::AnyAction => true,
            PermitReach::Named(named) => named.contains(capability),
        }
    }

    #[must_use]
    pub fn policy_count(&self) -> usize {
        self.policy_count
    }

    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    fn build_request(
        &self,
        request: PolicyRequest,
        request_facts: Option<HttpRequestFacts>,
    ) -> Result<(cedar_policy::Request, Option<OpaqueBody>), RequestError> {
        let PolicyRequest {
            principal,
            target,
            context,
        } = request;
        let has_request = request_facts.is_some();
        let action = entity_uid(&self.entity_types.action, target.action());
        let principal = entity_uid(&self.entity_types.principal, principal.as_str());
        let (resource, mut attributes) = match target {
            PolicyTarget::Capability {
                provider,
                effect,
                risk,
                ..
            } => (
                entity_uid(&self.entity_types.provider, provider.as_str()),
                serde_json::Map::from_iter([
                    ("effect".to_owned(), json!(effect.to_string())),
                    ("risk".to_owned(), json!(risk.to_string())),
                ]),
            ),
            PolicyTarget::AgentPrompt { agent } => (
                entity_uid(&self.entity_types.agent, agent.as_str()),
                serde_json::Map::new(),
            ),
            PolicyTarget::SecretUse {
                secret,
                capability,
                provider,
                sink,
            } => (
                entity_uid(&self.entity_types.secret, secret.as_str()),
                serde_json::Map::from_iter([
                    ("capability".to_owned(), json!(capability.to_string())),
                    ("provider".to_owned(), json!(provider.to_string())),
                    ("sink".to_owned(), json!(sink.to_string())),
                ]),
            ),
        };
        for (name, value) in [
            ("via", context.via),
            ("subject", context.subject),
            ("agent", context.agent),
            ("transportKind", context.transport_kind),
            ("transport", context.transport),
            ("trigger", context.trigger),
            ("startedBy", context.started_by),
        ] {
            if let Some(value) = value {
                attributes.insert(name.to_owned(), json!(value));
            }
        }
        if let Some(conversation) = context.conversation {
            let mut fields = serde_json::Map::from_iter([
                ("kind".to_owned(), json!(conversation.kind)),
                ("id".to_owned(), json!(conversation.id)),
            ]);
            if let Some(container) = conversation.container {
                fields.insert("container".to_owned(), json!(container));
            }
            if let Some(thread) = conversation.thread {
                fields.insert("thread".to_owned(), json!(thread));
            }
            attributes.insert("conversation".to_owned(), fields.into());
        }
        let github_schema = if let Some(github) = context.github {
            attributes.insert("github".to_owned(), github);
            Some((&self.schema, &action))
        } else {
            None
        };
        let context =
            Context::from_json_value(attributes.into(), github_schema).map_err(|source| {
                RequestError::Context {
                    message: source.to_string(),
                }
            })?;
        let (context, opaque_body) = if let Some(facts) = request_facts {
            let (request, opaque_body) = facts.context()?;
            let context = context
                .merge(request)
                .map_err(|source| RequestError::Context {
                    message: source.to_string(),
                })?;
            (context, opaque_body)
        } else {
            (context, None)
        };
        let schema = if has_request {
            None
        } else {
            Some(&self.schema)
        };
        cedar_policy::Request::new(principal, action, resource, context, schema)
            .map(|request| (request, opaque_body))
            .map_err(|source| RequestError::Schema {
                message: source.to_string(),
            })
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
enum RequestError {
    #[error("trusted routing context could not be assembled: {message}")]
    Context { message: String },
    #[error("request does not validate against the policy schema: {message}")]
    Schema { message: String },
}

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
            None => {
                return Err(PolicyBuildError::MissingPolicyId {
                    policy: policy.id().to_string(),
                });
            }
        };
        if !seen.insert(id.to_string()) {
            return Err(PolicyBuildError::DuplicatePolicyId {
                policy: id.to_string(),
            });
        }
        renamed
            .add(policy.new_id(id))
            .map_err(|source| PolicyBuildError::Parse {
                messages: vec![source.to_string()],
            })?;
    }
    Ok(renamed)
}

fn inspect_request_shapes(
    policies: &PolicySet,
) -> Result<(PolicySet, Vec<String>), PolicyBuildError> {
    let mut ordinary = PolicySet::new();
    let mut invalid = Vec::new();
    for policy in policies.policies() {
        let est = policy.to_json().map_err(|source| PolicyBuildError::Parse {
            messages: vec![source.to_string()],
        })?;
        if !references_request(&est) {
            ordinary
                .add(policy.clone())
                .map_err(|source| PolicyBuildError::Parse {
                    messages: vec![source.to_string()],
                })?;
            continue;
        }
        let guard = json!({"kind": "when", "body": {"has": {
            "left": {"Var": "context"}, "attr": "request"
        }}});
        if policy.effect() != Effect::Forbid || est["conditions"][0] != guard {
            invalid.push(policy.id().to_string());
        }
    }
    invalid.sort();
    Ok((ordinary, invalid))
}

fn references_request(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(fields) => {
            let refers = [".", "has"].into_iter().any(|op| {
                fields.get(op).is_some_and(|expr| {
                    expr.get("left") == Some(&json!({"Var": "context"}))
                        && (expr.get("attr") == Some(&json!("request"))
                            || expr
                                .get("attr")
                                .and_then(serde_json::Value::as_array)
                                .and_then(|path| path.first())
                                == Some(&json!("request")))
                })
            });
            refers || fields.values().any(references_request)
        }
        serde_json::Value::Array(values) => values.iter().any(references_request),
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => false,
    }
}

fn entity_type_name(type_name: &str) -> Result<EntityTypeName, PolicyBuildError> {
    EntityTypeName::from_str(type_name).map_err(|source| PolicyBuildError::Entities {
        message: format!("could not parse entity type {type_name}: {source}"),
    })
}

fn entity_uid(type_name: &EntityTypeName, id: &str) -> EntityUid {
    EntityUid::from_type_name_and_id(type_name.clone(), EntityId::new(id))
}

fn classify_policies(
    policies: &PolicySet,
    world: &PolicyWorld,
    handling: Handling,
) -> Result<(BTreeSet<CapabilityId>, Vec<UnresolvedName>), PolicyBuildError> {
    let mut referenced_capabilities = BTreeSet::new();
    let mut unresolved = Vec::new();
    let action_groups = world.action_groups();
    for policy in policies.policies() {
        let id = policy.id().to_string();
        for uid in policy.entity_literals() {
            let type_name = uid.type_name().to_string();
            let value = uid.id().unescaped().to_owned();
            match type_name.as_str() {
                PRINCIPAL_TYPE => {
                    let principal = value.parse::<PrincipalId>().map_err(|source| {
                        PolicyBuildError::MalformedPrincipal {
                            policy: id.clone(),
                            principal: value.clone(),
                            source,
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
                        // A literal outside the identifier grammar is treated as a typo, not
                        // tolerated, since tolerating it would drop it from the phantom set and
                        // surface as an unexplained raw Cedar failure.
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
                GROUP_TYPE => {
                    let group = value.parse::<GroupId>().map_err(|source| {
                        PolicyBuildError::MalformedGroup {
                            policy: id.clone(),
                            group: value.clone(),
                            source,
                        }
                    })?;
                    if !world.groups().contains(&group) {
                        return Err(PolicyBuildError::UnknownGroup {
                            policy: id.clone(),
                            group: value,
                        });
                    }
                }
                SECRET_TYPE => {
                    let secret = value.parse::<SecretDrn>().map_err(|source| {
                        PolicyBuildError::MalformedSecret {
                            policy: id.clone(),
                            secret: value.clone(),
                            source,
                        }
                    })?;
                    if !world.secrets.contains(&secret) {
                        return Err(PolicyBuildError::UnknownSecret {
                            policy: id.clone(),
                            secret: value,
                        });
                    }
                }
                ACTION_TYPE => {
                    if matches!(value.as_str(), AGENT_PROMPT_ACTION | SECRET_USE_ACTION) {
                        continue;
                    }
                    if action_group_provider(&value).is_some() {
                        if action_groups.contains_key(&value) {
                            continue;
                        }
                        if handling == Handling::Refuse {
                            return Err(PolicyBuildError::UnknownAction {
                                policy: id.clone(),
                                action: value,
                            });
                        }
                        unresolved.push(UnresolvedName {
                            policy: id.clone(),
                            name: value,
                            kind: UnresolvedKind::ActionGroup,
                        });
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

fn permit_reach(policies: &PolicySet, world: &PolicyWorld) -> PermitReach {
    let groups = world.action_groups();
    let mut named = BTreeSet::new();
    for policy in policies.policies() {
        match policy.effect() {
            Effect::Permit => {}
            Effect::Forbid => continue,
        }
        match policy.action_constraint() {
            ActionConstraint::Any => return PermitReach::AnyAction,
            ActionConstraint::In(_) | ActionConstraint::Eq(_) => {}
        }
        for uid in policy.entity_literals() {
            if uid.type_name().to_string() != ACTION_TYPE {
                continue;
            }
            let value = uid.id().unescaped();
            if let Some(members) = groups.get(value) {
                named.extend(members.iter().map(|&capability| capability.clone()));
            } else if let Ok(capability) = value.parse::<CapabilityId>()
                && world.capabilities.contains_key(&capability)
            {
                named.insert(capability);
            }
        }
    }
    PermitReach::Named(named)
}

fn action_group_provider(value: &str) -> Option<ProviderId> {
    value
        .strip_suffix(PROVIDER_GROUP_SUFFIX)
        .or_else(|| value.strip_suffix(READ_ONLY_GROUP_SUFFIX))?
        .parse()
        .ok()
}

fn build_entities(world: &PolicyWorld, schema: &Schema) -> Result<Entities, PolicyBuildError> {
    let principal_type = entity_type_name(PRINCIPAL_TYPE)?;
    let group_type = entity_type_name(GROUP_TYPE)?;
    let mut entities = Vec::new();
    for principal in &world.principals {
        let parents = world
            .memberships
            .get(principal)
            .into_iter()
            .flatten()
            .map(|group| entity_uid(&group_type, group.as_str()))
            .collect::<HashSet<_>>();
        entities.push(
            Entity::new(
                entity_uid(&principal_type, principal.as_str()),
                [(
                    "kind".to_owned(),
                    RestrictedExpression::new_string(
                        world.principal_kind(principal).as_str().to_owned(),
                    ),
                )]
                .into_iter()
                .collect(),
                parents,
            )
            .map_err(|source| PolicyBuildError::Entities {
                message: source.to_string(),
            })?,
        );
    }
    for group in world.groups() {
        entities.push(Entity::new_no_attrs(
            entity_uid(&group_type, group.as_str()),
            HashSet::new(),
        ));
    }
    for (type_name, ids) in [
        (
            PROVIDER_TYPE,
            world
                .providers
                .iter()
                .chain(world.phantom_providers.iter())
                .map(ProviderId::as_str)
                .collect::<Vec<_>>(),
        ),
        (
            SECRET_TYPE,
            world
                .secrets
                .iter()
                .map(SecretDrn::as_str)
                .collect::<Vec<_>>(),
        ),
    ] {
        let type_name = entity_type_name(type_name)?;
        for id in ids {
            entities.push(Entity::new_no_attrs(
                entity_uid(&type_name, id),
                HashSet::new(),
            ));
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
        hasher.update(world.principal_kind(principal).as_str().as_bytes());
        hasher.update([0]);
        for group in world.memberships.get(principal).into_iter().flatten() {
            hasher.update(format!("in {GROUP_TYPE}::{:?}", group.as_str()).as_bytes());
            hasher.update([0]);
        }
    }
    for provider in &world.providers {
        hasher.update(format!("{PROVIDER_TYPE}::{:?}", provider.as_str()).as_bytes());
        hasher.update([0]);
    }
    for secret in &world.secrets {
        hasher.update(format!("{SECRET_TYPE}::{:?}", secret.as_str()).as_bytes());
        hasher.update([0]);
    }
    hasher.update(b"actions\0");
    let mut actions = world
        .capabilities
        .keys()
        .map(|capability| capability.as_str())
        .collect::<Vec<_>>();
    actions.push(AGENT_PROMPT_ACTION);
    if !world.secrets.is_empty() {
        actions.push(SECRET_USE_ACTION);
    }
    actions.sort_unstable();
    for action in actions {
        hasher.update(action.as_bytes());
        hasher.update([0]);
    }
    hasher.update(b"action-groups\0");
    for (group, members) in world.action_groups() {
        hasher.update(group.as_bytes());
        hasher.update([0]);
        for member in members {
            hasher.update(member.as_str().as_bytes());
            hasher.update([0]);
        }
    }

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

    Ok(finish_digest(hasher))
}

fn finish_digest(hasher: Sha256) -> String {
    let mut hex = String::with_capacity(64 + 7);
    hex.push_str("sha256:");
    for byte in hasher.finalize() {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        hex.push(char::from(HEX[usize::from(byte >> 4)]));
        hex.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    hex
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PolicyBuildError {
    #[error("policy source is {length} bytes; maximum is {maximum}")]
    PolicyTooLarge { length: usize, maximum: usize },
    #[error("policy set contains {count} policies; maximum is {maximum}")]
    TooManyPolicies { count: usize, maximum: usize },
    #[error("policy source could not be parsed: {}", messages.join("; "))]
    Parse { messages: Vec<String> },
    #[error("policy templates are not supported; write static policies instead")]
    TemplateUnsupported,
    #[error("policy schema could not be generated: {message}")]
    Schema { message: String },
    #[error("policy set failed strict validation: {}", messages.join("; "))]
    Validation { messages: Vec<String> },
    #[error("request policy shape invalid for IDs: {}", policies.join(", "))]
    InvalidRequestShapes { policies: Vec<String> },
    #[error("policy {policy} names undeclared principal {principal:?}")]
    UnknownPrincipal { policy: String, principal: String },
    #[error("policy {policy} names malformed principal {principal:?}")]
    MalformedPrincipal {
        policy: String,
        principal: String,
        #[source]
        source: IdentifierError,
    },
    #[error("policy {policy} names undeclared secret {secret:?}")]
    UnknownSecret { policy: String, secret: String },
    #[error("policy {policy} names malformed secret DRN {secret:?}")]
    MalformedSecret {
        policy: String,
        secret: String,
        #[source]
        source: dekopon_core::SecretDrnError,
    },
    #[error("policy {policy} names group {group:?}, which no principal belongs to")]
    UnknownGroup { policy: String, group: String },
    #[error("policy {policy} names malformed group {group:?}")]
    MalformedGroup {
        policy: String,
        group: String,
        #[source]
        source: IdentifierError,
    },
    #[error("policy {policy} names undeclared provider {provider:?}")]
    UnknownProvider { policy: String, provider: String },
    #[error("policy {policy} names undeclared action {action:?}")]
    UnknownAction { policy: String, action: String },
    #[error("policy {policy} names unknown entity type {entity_type}")]
    UnknownEntityType { policy: String, entity_type: String },
    #[error("policy {policy} has an @id annotation that is not a bounded portable identifier")]
    InvalidPolicyId { policy: String },
    #[error(
        "policy {policy} has no @id annotation; every statement needs one so audit records name it \
         stably"
    )]
    MissingPolicyId { policy: String },
    #[error("policy identifier {policy:?} is used by more than one policy")]
    DuplicatePolicyId { policy: String },
    #[error(
        "policy world conflicts: reserved actions {reserved:?}; duplicate capabilities {duplicates:?}"
    )]
    WorldConflicts {
        reserved: Vec<CapabilityId>,
        duplicates: Vec<CapabilityId>,
    },
    #[error("policy entity store could not be built: {message}")]
    Entities { message: String },
    #[error("policy {policy} could not be canonicalized for the policy digest: {message}")]
    Digest { policy: String, message: String },
}

#[cfg(test)]
mod github_fixture_tests;
#[cfg(test)]
mod tests;
