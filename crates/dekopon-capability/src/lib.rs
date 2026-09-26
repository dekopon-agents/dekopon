//! Capability metadata and invocation-state types for Dekopon.
//!
//! The central API distinction is between [`ProposedInvocation`] and
//! [`AuthorizedInvocation`]. A model-facing tool call can create only the former. The
//! latter has private fields and can be produced only by the broker-oriented transition
//! in this crate. The broker-owned execution boundary creates and consumes this state;
//! its serialized representation is inert audit/evidence data, not transferable authority.
//!
//! ```compile_fail
//! use dekopon_capability::{AuthorizedInvocation, ExecutionConstraints, ProposedInvocation};
//!
//! fn fabricate(proposal: ProposedInvocation, constraints: ExecutionConstraints) {
//!     // Ordinary callers cannot use a struct literal to cross the authority boundary.
//!     let _forged = AuthorizedInvocation {
//!         proposal,
//!         provider: todo!(),
//!         receipt: todo!(),
//!         constraints,
//!     };
//! }
//! ```
//!
//! Serialized authorization state cannot be decoded into executable authority:
//!
//! ```compile_fail
//! use dekopon_capability::AuthorizedInvocation;
//! use serde::de::DeserializeOwned;
//!
//! fn require_deserializable<T: DeserializeOwned>() {}
//!
//! fn main() {
//!     require_deserializable::<AuthorizedInvocation>();
//! }
//! ```
//!
//! Authorization is also intentionally single-use at the type boundary:
//!
//! ```compile_fail
//! use dekopon_capability::AuthorizedInvocation;
//!
//! fn require_clone<T: Clone>() {}
//!
//! fn main() {
//!     require_clone::<AuthorizedInvocation>();
//! }
//! ```
//!
//! Rust visibility is defense in depth. It is not a substitute for process isolation,
//! authenticated broker messages, authorization policy, or credential separation.

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
use std::fmt;

use dekopon_core::{
    Actor, CapabilityId, InvocationId, PrincipalId, ProviderFailureDetail, ProviderId, SecretDrn,
    SecretSinkKind, SecretUseProposal, TraceId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EffectKind {
    ReadOnly,
    LocalWrite,
    ExternalWrite,
}

impl fmt::Display for EffectKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::ReadOnly => "read-only",
            Self::LocalWrite => "local-write",
            Self::ExternalWrite => "external-write",
        };
        formatter.write_str(value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Permission {
    pub operation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

/// Not Deserialize: the broker converts this from a separate wire type only after authenticating
/// the envelope, so no caller can decode one directly.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProposedInvocation {
    pub id: InvocationId,
    pub capability: CapabilityId,
    pub actor: Actor,
    pub trace: TraceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_use: Option<SecretUseProposal>,
    pub input: Value,
}

impl ProposedInvocation {
    #[must_use]
    pub fn new(
        id: InvocationId,
        capability: CapabilityId,
        actor: Actor,
        trace: TraceId,
        input: Value,
    ) -> Self {
        Self {
            id,
            capability,
            actor,
            trace,
            secret_use: None,
            input,
        }
    }

    #[must_use]
    pub fn with_secret_use(mut self, secret_use: Option<SecretUseProposal>) -> Self {
        self.secret_use = secret_use;
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HttpConstraints {
    pub allowed_hosts: Vec<String>,
    pub allowed_methods: Vec<String>,
    pub max_requests: u32,
    pub max_request_bytes: u64,
    pub max_response_bytes: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_plaintext_loopback: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub propagate_trace: bool,
}

const fn is_false(value: &bool) -> bool {
    !*value
}

pub const MAX_HTTP_SCOPE_ENTRIES: usize = 64;
pub const MAX_HTTP_HOST_BYTES: usize = 512;
pub const MAX_HTTP_METHOD_BYTES: usize = 64;

impl HttpConstraints {
    /// This is the one definition of the grant's entry grammar; validating here catches a bad grant
    /// before it silently denies every call at runtime.
    pub fn validate(&self) -> Result<(), HttpConstraintsError> {
        if self.allowed_hosts.is_empty() {
            return Err(HttpConstraintsError::NoHosts);
        }
        if self.allowed_methods.is_empty() {
            return Err(HttpConstraintsError::NoMethods);
        }
        if self.allowed_hosts.len() > MAX_HTTP_SCOPE_ENTRIES
            || self.allowed_methods.len() > MAX_HTTP_SCOPE_ENTRIES
        {
            return Err(HttpConstraintsError::TooManyEntries {
                maximum: MAX_HTTP_SCOPE_ENTRIES,
            });
        }
        if let Some(value) = self
            .allowed_hosts
            .iter()
            .find(|value| !is_authority_scope(value))
        {
            return Err(HttpConstraintsError::InvalidHost {
                value: value.clone(),
            });
        }
        if let Some(value) = self
            .allowed_methods
            .iter()
            .find(|value| value.len() > MAX_HTTP_METHOD_BYTES || !is_http_token(value))
        {
            return Err(HttpConstraintsError::InvalidMethod {
                value: value.clone(),
            });
        }
        if self.max_requests == 0 || self.max_request_bytes == 0 || self.max_response_bytes == 0 {
            return Err(HttpConstraintsError::ZeroLimit);
        }
        Ok(())
    }
}

fn is_authority_scope(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_HTTP_HOST_BYTES
        && value.trim() == value
        && !value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        && !value.contains(['/', '?', '#', '@', '*'])
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum HttpConstraintsError {
    #[error("HTTP authorization requires at least one allowed host")]
    NoHosts,
    #[error("HTTP authorization requires at least one allowed method")]
    NoMethods,
    #[error("HTTP authorization allows at most {maximum} host or method entries")]
    TooManyEntries { maximum: usize },
    #[error("HTTP allowed host {value:?} is not an exact authority")]
    InvalidHost { value: String },
    #[error("HTTP allowed method {value:?} is not an exact HTTP method token")]
    InvalidMethod { value: String },
    #[error("HTTP authorization limits must be greater than zero")]
    ZeroLimit,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "match", rename_all = "camelCase", deny_unknown_fields)]
pub enum HttpPathRule {
    Exact { path: String },
    SegmentPrefix { path: String },
}

impl HttpPathRule {
    pub fn validate(&self) -> Result<(), SecretUseGrantError> {
        let path = self.path();
        if !canonical_secret_path(path) {
            return Err(SecretUseGrantError::InvalidPath {
                path: path.to_owned(),
            });
        }
        Ok(())
    }

    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Exact { path } | Self::SegmentPrefix { path } => path,
        }
    }

    #[must_use]
    pub fn matches(&self, candidate: &str) -> bool {
        match self {
            Self::Exact { path } => candidate == path,
            Self::SegmentPrefix { path } if path == "/" => candidate.starts_with('/'),
            Self::SegmentPrefix { path } => {
                candidate == path
                    || candidate
                        .strip_prefix(path)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            }
        }
    }
}

fn canonical_secret_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= 4096
        && !path.contains(['%', '\\', '?', '#'])
        && !path.contains("//")
        && !path
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        && path
            .split('/')
            .skip(1)
            .all(|segment| !matches!(segment, "." | ".."))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SecretUseGrant {
    pub secret: SecretDrn,
    pub sink: SecretSinkKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basic_username: Option<String>,
    pub allowed_hosts: Vec<String>,
    pub allowed_methods: Vec<String>,
    pub allowed_paths: Vec<HttpPathRule>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_query: bool,
    pub max_injections: u32,
    pub binding_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map_revision: Option<String>,
}

impl SecretUseGrant {
    pub fn validate(&self) -> Result<(), SecretUseGrantError> {
        if self.allowed_hosts.is_empty()
            || self.allowed_methods.is_empty()
            || self.allowed_paths.is_empty()
        {
            return Err(SecretUseGrantError::EmptyScope);
        }
        if self.max_injections == 0 {
            return Err(SecretUseGrantError::ZeroInjections);
        }
        if self.binding_id.is_empty()
            || self.binding_id.len() > 128
            || !self
                .binding_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            return Err(SecretUseGrantError::InvalidBindingId);
        }
        if self.map_revision.as_ref().is_some_and(|revision| {
            revision.is_empty()
                || revision.len() > 128
                || revision.trim() != revision
                || revision.bytes().any(|byte| byte.is_ascii_control())
        }) {
            return Err(SecretUseGrantError::InvalidMapRevision);
        }
        if self.allowed_hosts.len() > MAX_HTTP_SCOPE_ENTRIES
            || self.allowed_methods.len() > MAX_HTTP_SCOPE_ENTRIES
            || self.allowed_paths.len() > MAX_HTTP_SCOPE_ENTRIES
            || self
                .allowed_hosts
                .iter()
                .any(|value| !is_authority_scope(value))
            || self
                .allowed_methods
                .iter()
                .any(|value| value.len() > MAX_HTTP_METHOD_BYTES || !is_http_token(value))
        {
            return Err(SecretUseGrantError::InvalidScope);
        }
        for path in &self.allowed_paths {
            path.validate()?;
        }
        match (self.sink, self.basic_username.as_deref()) {
            (SecretSinkKind::HttpBasic, Some(username))
                if !username.is_empty()
                    && username.len() <= dekopon_core::MAX_SECRET_USERNAME_LENGTH
                    && !username.contains(':')
                    && !username.bytes().any(|byte| byte.is_ascii_control()) => {}
            (SecretSinkKind::HttpBearer, None) => {}
            _ => return Err(SecretUseGrantError::InvalidUsername),
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SecretUseGrantError {
    #[error("secret use requires nonempty host, method, and path scopes")]
    EmptyScope,
    #[error("secret use scopes are oversized or structurally invalid")]
    InvalidScope,
    #[error("secret path {path:?} is not canonical")]
    InvalidPath { path: String },
    #[error("secret use maximum injections must be greater than zero")]
    ZeroInjections,
    #[error("secret use binding identifier is invalid")]
    InvalidBindingId,
    #[error("secret use private-map revision is invalid")]
    InvalidMapRevision,
    #[error("secret sink and Basic username do not agree")]
    InvalidUsername,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StorageInterface {
    Jsonl,
    DurableFiles,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StorageAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StorageNamespace {
    Chat,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StorageConstraints {
    pub interface: StorageInterface,
    pub access: StorageAccess,
    /// Namespace is broker-owned; guests can never supply or influence it themselves.
    pub namespace: StorageNamespace,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AssetConstraints {
    #[serde(default)]
    pub attach: bool,
    #[serde(default)]
    pub remove: bool,
    #[serde(default)]
    pub send: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ExecutionConstraints {
    pub timeout_ms: u64,
    pub max_output_bytes: u64,
    /// Its absence means no HTTP host calls are permitted at all, not unrestricted access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpConstraints>,
    /// HTTP and storage grants are mutually exclusive; a capability cannot combine both in v1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<StorageConstraints>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<AssetConstraints>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_use: Option<SecretUseGrant>,
}

impl Default for ExecutionConstraints {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            max_output_bytes: 1_048_576,
            http: None,
            storage: None,
            asset: None,
            secret_use: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationReceipt {
    decision_id: String,
    authorized_by: PrincipalId,
    policy_revision: String,
}

impl AuthorizationReceipt {
    #[must_use]
    pub fn decision_id(&self) -> &str {
        &self.decision_id
    }

    #[must_use]
    pub fn authorized_by(&self) -> &PrincipalId {
        &self.authorized_by
    }

    #[must_use]
    pub fn policy_revision(&self) -> &str {
        &self.policy_revision
    }
}

/// Not Clone or Deserialize: the broker creates and consumes this once, and its serialized form
/// must never be treated as a reusable bearer grant.
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizedInvocation {
    proposal: ProposedInvocation,
    provider: ProviderId,
    receipt: AuthorizationReceipt,
    constraints: ExecutionConstraints,
}

impl AuthorizedInvocation {
    #[must_use]
    pub fn proposal(&self) -> &ProposedInvocation {
        &self.proposal
    }

    #[must_use]
    pub fn provider(&self) -> &ProviderId {
        &self.provider
    }

    #[must_use]
    pub fn receipt(&self) -> &AuthorizationReceipt {
        &self.receipt
    }

    #[must_use]
    pub fn constraints(&self) -> &ExecutionConstraints {
        &self.constraints
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DecisionReference {
    pub decision_id: String,
    pub authorized_by: PrincipalId,
    pub policy_revision: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Evidence {
    pub kind: String,
    pub digest: String,
    pub media_type: String,
    /// This is only a durable reference; secrets must never be embedded in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum InvocationOutcome {
    Succeeded,
    Denied,
    Failed,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InvocationResult {
    pub invocation: InvocationId,
    pub decision: DecisionReference,
    pub outcome: InvocationOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<ProviderFailureDetail>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum AuthorizationError {
    #[error("authorization decision identifier must not be empty")]
    EmptyDecisionId,
    #[error("authorization policy revision must not be empty")]
    EmptyPolicyRevision,
    #[error("authorization timeout must be greater than zero")]
    ZeroTimeout,
    #[error("authorization output limit must be greater than zero")]
    ZeroOutputLimit,
    #[error(transparent)]
    InvalidHttp(#[from] HttpConstraintsError),
    #[error("HTTP and storage authority cannot coexist in one capability")]
    MixedHttpAndStorage,
    #[error("authorization secret-use proposal does not match its effective grant")]
    SecretUseMismatch,
    #[error(transparent)]
    InvalidSecretUse(#[from] SecretUseGrantError),
}

/// Public so a separate broker adapter can own the transition, but construction authenticates
/// nothing; never drive this directly from model-provided data.
pub mod broker {
    use dekopon_core::{PrincipalId, ProviderId};

    use super::{
        AuthorizationError, AuthorizationReceipt, AuthorizedInvocation, ExecutionConstraints,
        ProposedInvocation,
    };

    #[derive(Debug)]
    pub struct AuthorizationGate {
        _private: (),
    }

    #[allow(
        clippy::new_without_default,
        reason = "authority transitions should require an explicit broker-owned constructor"
    )]
    impl AuthorizationGate {
        #[must_use]
        pub const fn new() -> Self {
            Self { _private: () }
        }

        pub fn authorize(
            &self,
            proposal: ProposedInvocation,
            provider: ProviderId,
            decision_id: String,
            authorized_by: PrincipalId,
            policy_revision: String,
            constraints: ExecutionConstraints,
        ) -> Result<AuthorizedInvocation, AuthorizationError> {
            if decision_id.trim().is_empty() {
                return Err(AuthorizationError::EmptyDecisionId);
            }
            if policy_revision.trim().is_empty() {
                return Err(AuthorizationError::EmptyPolicyRevision);
            }
            if constraints.timeout_ms == 0 {
                return Err(AuthorizationError::ZeroTimeout);
            }
            if constraints.max_output_bytes == 0 {
                return Err(AuthorizationError::ZeroOutputLimit);
            }
            if constraints.http.is_some() && constraints.storage.is_some() {
                return Err(AuthorizationError::MixedHttpAndStorage);
            }
            if let Some(http) = &constraints.http {
                http.validate()?;
            }
            if let Some(secret) = &constraints.secret_use {
                secret.validate()?;
            }
            let secret_matches = match (&proposal.secret_use, &constraints.secret_use) {
                (None, None) => true,
                (Some(proposal), Some(grant)) => {
                    proposal.secret() == &grant.secret
                        && proposal.sink() == grant.sink
                        && proposal.username() == grant.basic_username.as_deref()
                        && constraints.http.is_some()
                }
                _ => false,
            };
            if !secret_matches {
                return Err(AuthorizationError::SecretUseMismatch);
            }

            Ok(AuthorizedInvocation {
                proposal,
                provider,
                receipt: AuthorizationReceipt {
                    decision_id,
                    authorized_by,
                    policy_revision,
                },
                constraints,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use dekopon_core::{Actor, AgentId, CapabilityId, InvocationId, PrincipalId, TraceId};
    use serde_json::json;

    use super::{
        AuthorizationError, EffectKind, ExecutionConstraints, HttpConstraints,
        HttpConstraintsError, MAX_HTTP_SCOPE_ENTRIES, ProposedInvocation, broker,
    };

    fn proposal() -> ProposedInvocation {
        ProposedInvocation::new(
            "invoke-1".parse::<InvocationId>().expect("valid fixture"),
            "github.pull-request.comment"
                .parse::<CapabilityId>()
                .expect("valid fixture"),
            Actor::Agent {
                agent: "reviewer".parse::<AgentId>().expect("valid fixture"),
            },
            "0000000000000000000000000000f1c7"
                .parse::<TraceId>()
                .expect("valid fixture"),
            json!({"body": "Looks good"}),
        )
    }

    #[test]
    fn display_matches_the_serde_rendering_for_every_variant() {
        for effect in [
            EffectKind::ReadOnly,
            EffectKind::LocalWrite,
            EffectKind::ExternalWrite,
        ] {
            assert_eq!(
                serde_json::to_value(effect).expect("effect serializes"),
                json!(effect.to_string()),
            );
        }
    }

    #[test]
    fn broker_gate_performs_explicit_authority_transition() {
        let authorized = broker::AuthorizationGate::new()
            .authorize(
                proposal(),
                "github".parse().expect("valid provider fixture"),
                "decision-1".to_owned(),
                "broker".parse::<PrincipalId>().expect("valid fixture"),
                "policy-1".to_owned(),
                ExecutionConstraints::default(),
            )
            .expect("valid broker decision");

        assert_eq!(authorized.proposal().id.as_str(), "invoke-1");
        assert_eq!(authorized.provider().as_str(), "github");
        assert_eq!(authorized.receipt().decision_id(), "decision-1");
        assert_eq!(authorized.constraints().timeout_ms, 30_000);
    }

    #[test]
    fn broker_gate_rejects_unbounded_execution() {
        let constraints = ExecutionConstraints {
            timeout_ms: 0,
            ..ExecutionConstraints::default()
        };
        let error = broker::AuthorizationGate::new()
            .authorize(
                proposal(),
                "github".parse().expect("valid provider fixture"),
                "decision-1".to_owned(),
                "broker".parse::<PrincipalId>().expect("valid fixture"),
                "policy-1".to_owned(),
                constraints,
            )
            .expect_err("zero timeout must fail");

        assert_eq!(error, AuthorizationError::ZeroTimeout);
    }

    #[test]
    fn broker_gate_rejects_incomplete_http_authority() {
        let valid = HttpConstraints {
            allowed_hosts: vec!["api.example.test".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            max_requests: 1,
            max_request_bytes: 1024,
            max_response_bytes: 1024,
            allow_plaintext_loopback: false,
            propagate_trace: false,
        };
        let cases = [
            (
                HttpConstraints {
                    allowed_hosts: Vec::new(),
                    ..valid.clone()
                },
                HttpConstraintsError::NoHosts,
            ),
            (
                HttpConstraints {
                    allowed_methods: Vec::new(),
                    ..valid.clone()
                },
                HttpConstraintsError::NoMethods,
            ),
            (
                HttpConstraints {
                    max_requests: 0,
                    ..valid.clone()
                },
                HttpConstraintsError::ZeroLimit,
            ),
            (
                HttpConstraints {
                    allowed_hosts: vec![" api.github.com".to_owned()],
                    ..valid.clone()
                },
                HttpConstraintsError::InvalidHost {
                    value: " api.github.com".to_owned(),
                },
            ),
            (
                HttpConstraints {
                    allowed_hosts: vec!["*".to_owned()],
                    ..valid.clone()
                },
                HttpConstraintsError::InvalidHost {
                    value: "*".to_owned(),
                },
            ),
            (
                HttpConstraints {
                    allowed_hosts: vec!["a/b".to_owned()],
                    ..valid.clone()
                },
                HttpConstraintsError::InvalidHost {
                    value: "a/b".to_owned(),
                },
            ),
            (
                HttpConstraints {
                    allowed_hosts: vec![String::new()],
                    ..valid.clone()
                },
                HttpConstraintsError::InvalidHost {
                    value: String::new(),
                },
            ),
            (
                HttpConstraints {
                    allowed_methods: vec!["GET POST".to_owned()],
                    ..valid.clone()
                },
                HttpConstraintsError::InvalidMethod {
                    value: "GET POST".to_owned(),
                },
            ),
            (
                HttpConstraints {
                    allowed_hosts: (0..=MAX_HTTP_SCOPE_ENTRIES)
                        .map(|index| format!("host{index}.example.test"))
                        .collect(),
                    ..valid
                },
                HttpConstraintsError::TooManyEntries {
                    maximum: MAX_HTTP_SCOPE_ENTRIES,
                },
            ),
        ];

        for (http, expected) in cases {
            let error = broker::AuthorizationGate::new()
                .authorize(
                    proposal(),
                    "github".parse().expect("valid provider fixture"),
                    "decision-1".to_owned(),
                    "broker".parse::<PrincipalId>().expect("valid fixture"),
                    "policy-1".to_owned(),
                    ExecutionConstraints {
                        http: Some(http),
                        ..ExecutionConstraints::default()
                    },
                )
                .expect_err("incomplete HTTP authority must fail");
            assert_eq!(error, AuthorizationError::InvalidHttp(expected));
        }
    }

    #[test]
    fn broker_gate_accepts_exact_http_authority() {
        let http = HttpConstraints {
            allowed_hosts: vec!["api.example.test".to_owned(), "127.0.0.1:8080".to_owned()],
            allowed_methods: vec![
                "GET".to_owned(),
                "POST".to_owned(),
                "PATCH".to_owned(),
                "DELETE".to_owned(),
            ],
            max_requests: 4,
            max_request_bytes: 65_536,
            max_response_bytes: 1_048_576,
            allow_plaintext_loopback: true,
            propagate_trace: false,
        };

        http.validate().expect("an exact grant is enforceable");
    }

    #[test]
    fn authorized_invocation_serialization_preserves_linkage() {
        let constraints = ExecutionConstraints {
            http: Some(HttpConstraints {
                allowed_hosts: vec!["api.github.com".to_owned()],
                allowed_methods: vec!["POST".to_owned()],
                max_requests: 1,
                max_request_bytes: 65_536,
                max_response_bytes: 1_048_576,
                allow_plaintext_loopback: false,
                propagate_trace: false,
            }),
            ..ExecutionConstraints::default()
        };
        let authorized = broker::AuthorizationGate::new()
            .authorize(
                proposal(),
                "github".parse().expect("valid provider fixture"),
                "decision-1".to_owned(),
                "broker".parse::<PrincipalId>().expect("valid fixture"),
                "policy-1".to_owned(),
                constraints,
            )
            .expect("valid broker decision");
        let value = serde_json::to_value(authorized).expect("authorization serializes");

        assert_eq!(
            value,
            json!({
                "proposal": {
                    "id": "invoke-1",
                    "capability": "github.pull-request.comment",
                    "actor": {"type": "agent", "agent": "reviewer"},
                    "trace": "0000000000000000000000000000f1c7",
                    "input": {"body": "Looks good"}
                },
                "provider": "github",
                "receipt": {
                    "decisionId": "decision-1",
                    "authorizedBy": "broker",
                    "policyRevision": "policy-1"
                },
                "constraints": {
                    "timeoutMs": 30_000,
                    "maxOutputBytes": 1_048_576,
                    "http": {
                        "allowedHosts": ["api.github.com"],
                        "allowedMethods": ["POST"],
                        "maxRequests": 1,
                        "maxRequestBytes": 65_536,
                        "maxResponseBytes": 1_048_576
                    }
                }
            })
        );
    }
}
