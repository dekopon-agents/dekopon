use std::{collections::BTreeSet, fmt, time::Duration};

use dekopon_broker_host::BrokerHostLimits;
use dekopon_capability::{ExecutionConstraints, SignedDownload};
use dekopon_core::SubjectKind;
use dekopon_policy::{PolicyRequest, PolicyTarget, RequestOutcome};
use dekopon_storage_host::{ContinuityCheck, LegacyContinuity, StorageLimits, UnprovenDisposition};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use super::{
    AuditLog, AuthenticatedContext, AuthorityEncoder, Broker, BrokerError, ChatMemoryConfig,
    ChatTransportKind, encode_asset_constraints, encode_host_limits, encode_memory_config,
    encode_request_templates, encode_storage_constraints, encode_storage_limits, policy_context,
    policy_contexts,
};

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("a continuity digest is `sha256:` followed by 64 lowercase hexadecimal digits")]
pub struct ContinuityDigestError;

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContinuityDigest(String);

impl ContinuityDigest {
    fn of(domain: &str, bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(domain.as_bytes());
        hasher.update([0]);
        hasher.update(bytes);
        Self(format!("sha256:{}", lowercase_hex(&hasher.finalize())))
    }
}

fn lowercase_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl TryFrom<String> for ContinuityDigest {
    type Error = ContinuityDigestError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let valid = value.strip_prefix("sha256:").is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        if valid {
            Ok(Self(value))
        } else {
            Err(ContinuityDigestError)
        }
    }
}

impl From<ContinuityDigest> for String {
    fn from(digest: ContinuityDigest) -> Self {
        digest.0
    }
}

impl fmt::Display for ContinuityDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ContinuityMigration {
    #[serde(default)]
    pub on_unproven: UnprovenDisposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority_digest: Option<ContinuityDigest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<ContinuityCandidate>,
}

impl ContinuityMigration {
    pub(super) fn valid(&self) -> bool {
        self.authority_digest.is_some() != self.candidates.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ContinuityCandidate {
    pub resolved_digest: ContinuityDigest,
    #[serde(default)]
    pub overrides: OperationalOverrides,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OperationalOverrides {
    #[serde(default)]
    pub storage: StorageOverrides,
    #[serde(default)]
    pub host_limits: HostLimitOverrides,
    #[serde(default)]
    pub memory: MemoryOverrides,
}

macro_rules! overrides {
    ($name:ident for $target:ty { $($field:ident: $kind:ty),* $(,)? }) => {
        #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        pub struct $name {
            $(
                #[serde(default, skip_serializing_if = "Option::is_none")]
                pub $field: Option<$kind>,
            )*
        }

        impl $name {
            fn apply(&self, target: &mut $target) {
                $(
                    if let Some(value) = self.$field {
                        target.$field = value;
                    }
                )*
            }
        }
    };
}

overrides!(StorageOverrides for StorageLimits {
    max_root_bytes: u64,
    max_namespaces: u64,
    max_namespace_bytes: u64,
    max_files_per_namespace: u64,
    max_file_bytes: u64,
    max_open_handles: u64,
    max_handles_per_invocation: u64,
    max_host_calls_per_invocation: u64,
    max_read_bytes_per_call: u64,
    max_read_bytes_per_invocation: u64,
    max_write_bytes_per_call: u64,
    max_write_bytes_per_invocation: u64,
    max_entropy_bytes_per_call: u64,
    max_entropy_bytes_per_invocation: u64,
    lock_timeout_ms: u64,
    finalization_budget_ms: u64,
    max_active_invocations: u64,
    startup_max_entries: u64,
});

overrides!(HostLimitFields for BrokerHostLimits {
    max_memory_bytes: usize,
    max_table_elements: usize,
    max_instances: usize,
    max_tables: usize,
    max_memories: usize,
    max_input_bytes: usize,
    max_output_bytes: usize,
    max_http_requests: u32,
    max_http_request_bytes: u64,
    max_http_response_bytes: u64,
    max_http_headers: usize,
    max_http_header_bytes: usize,
    fuel: u64,
});

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HostLimitOverrides {
    #[serde(flatten)]
    pub fields: HostLimitFields,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_timeout_ms: Option<u64>,
}

overrides!(MemoryOverrides for ChatMemoryConfig {
    max_lookback_turns: u32,
    max_recent_turns: u32,
    max_search_results: u32,
    max_query_bytes: u64,
    max_result_bytes: u64,
    max_turn_bytes: u64,
    compaction_target_bytes: u64,
    compaction_threshold_bytes: u64,
});

#[derive(Clone, Debug)]
pub(super) struct OperationalSnapshot {
    pub(super) host: BrokerHostLimits,
    pub(super) storage: StorageLimits,
    pub(super) memory: Option<ChatMemoryConfig>,
}

impl OperationalSnapshot {
    fn with_overrides(&self, overrides: &OperationalOverrides) -> Self {
        let mut snapshot = self.clone();
        overrides.storage.apply(&mut snapshot.storage);
        overrides.host_limits.fields.apply(&mut snapshot.host);
        if let Some(milliseconds) = overrides.host_limits.max_timeout_ms {
            snapshot.host.max_timeout = Duration::from_millis(milliseconds);
        }
        if let Some(memory) = snapshot.memory.as_mut() {
            overrides.memory.apply(memory);
        }
        snapshot
    }

    fn resolved_digest(&self) -> ContinuityDigest {
        let mut encoded = AuthorityEncoder::new();
        encode_host_limits(&mut encoded, &self.host);
        encode_storage_limits(&mut encoded, &self.storage);
        if let Some(memory) = &self.memory {
            encode_memory_config(&mut encoded, memory);
        }
        ContinuityDigest::of("dekopon-continuity-operational-v1", &encoded.finish())
    }
}

#[derive(Debug)]
pub(super) struct ContinuityState {
    authority_digest: ContinuityDigest,
    routing_dependent: Vec<String>,
    candidates: Vec<OperationalSnapshot>,
    report: ContinuityReport,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContinuityReport {
    pub legacy_authority_digest: ContinuityDigest,
    pub routing_dependent_policies: Vec<String>,
    pub current_resolved_digest: ContinuityDigest,
    pub candidates: Vec<CandidateReport>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateReport {
    pub configured: ContinuityDigest,
    pub resolved: ContinuityDigest,
}

impl<A> Broker<A>
where
    A: AuditLog,
{
    pub(super) fn continuity_state(
        &self,
        current: &OperationalSnapshot,
        migration: Option<&ContinuityMigration>,
    ) -> ContinuityState {
        let authority_digest = self.legacy_authority_digest();
        let routing_dependent = self.policy.routing_dependent_policies();
        let mut candidates = Vec::new();
        let mut reports = Vec::new();
        for candidate in migration
            .map(|migration| &migration.candidates[..])
            .unwrap_or_default()
        {
            let snapshot = current.with_overrides(&candidate.overrides);
            let resolved = snapshot.resolved_digest();
            if resolved == candidate.resolved_digest {
                candidates.push(snapshot);
            } else {
                tracing::warn!(
                    event = "storage_continuity_candidate_stale",
                    category = "storage",
                    storage.continuity.configured = %candidate.resolved_digest,
                    storage.continuity.resolved = %resolved,
                    "storage.continuity.candidate_stale"
                );
            }
            reports.push(CandidateReport {
                configured: candidate.resolved_digest.clone(),
                resolved,
            });
        }
        ContinuityState {
            report: ContinuityReport {
                legacy_authority_digest: authority_digest.clone(),
                routing_dependent_policies: routing_dependent.clone(),
                current_resolved_digest: current.resolved_digest(),
                candidates: reports,
            },
            authority_digest,
            routing_dependent,
            candidates,
        }
    }

    #[must_use]
    pub fn continuity_report(&self) -> Option<&ContinuityReport> {
        self.continuity.as_ref().map(|state| &state.report)
    }

    fn legacy_authority_digest(&self) -> ContinuityDigest {
        let mut encoded = AuthorityEncoder::new();
        encoded.text("policyDigest", &self.policy_digest);
        let identities = self.identities.mappings();
        encoded.number("identityCount", identities.len() as u128);
        for (subject, principal) in identities {
            encoded.text("identitySubject", &subject.canonical());
            encoded.text("identityPrincipal", principal.as_str());
        }
        let http = self.registry.http_destination_options();
        encoded.number(
            "plaintextHostCount",
            http.plaintext_hosts.iter().len() as u128,
        );
        for host in http.plaintext_hosts.iter().collect::<BTreeSet<_>>() {
            encoded.text("plaintextHost", host);
        }
        let non_public = http
            .non_public_https
            .iter()
            .map(|entry| entry.authority.as_str())
            .collect::<BTreeSet<_>>();
        encoded.number("nonPublicHttpsCount", non_public.len() as u128);
        for authority in non_public {
            encoded.text("nonPublicHttps", authority);
        }
        encode_trust_anchors(&mut encoded, http.extra_ca_bundles);
        ContinuityDigest::of("dekopon-continuity-legacy-authority-v1", &encoded.finish())
    }

    pub(super) fn legacy_continuity(
        &self,
        context: &AuthenticatedContext,
    ) -> Result<LegacyContinuity, BrokerError> {
        let configured = self
            .chat_memory
            .as_ref()
            .and_then(|memory| memory.continuity_migration.as_ref())
            .and_then(|migration| migration.authority_digest.as_ref());
        let (Some(state), Some(configured)) = (self.continuity.as_ref(), configured) else {
            return Ok(LegacyContinuity::Unproven(ContinuityCheck::Absent));
        };
        let check = if &state.authority_digest != configured {
            Some(ContinuityCheck::DigestMismatch)
        } else if self.space_capable_channel_is_mapped(context) {
            Some(ContinuityCheck::SpaceBound)
        } else if !state.routing_dependent.is_empty() {
            Some(ContinuityCheck::RoutingDependent)
        } else if !context
            .attested_subject()
            .is_some_and(|subject| self.identities.resolve(subject) == Some(context.principal()))
        {
            Some(ContinuityCheck::IdOnlySubject)
        } else if state.candidates.is_empty() {
            Some(ContinuityCheck::NoCandidate)
        } else {
            None
        };
        if let Some(check) = check {
            return Ok(LegacyContinuity::Unproven(check));
        }
        state
            .candidates
            .iter()
            .map(|candidate| {
                self.authority_surface_v1(
                    context,
                    dekopon_capability::StorageInterface::Jsonl,
                    true,
                    candidate,
                )
            })
            .collect::<Result<_, _>>()
            .map(LegacyContinuity::Candidates)
    }

    // Namespace identity binds the channel id but not the conversation kind or container that
    // select a space pass, so any mapped space channel with this id may have applied historically.
    fn space_capable_channel_is_mapped(&self, context: &AuthenticatedContext) -> bool {
        let Some(scope) = context.chat_scope() else {
            return false;
        };
        let kind = match scope.kind {
            ChatTransportKind::Fauxlack => SubjectKind::FauxlackChannel,
            ChatTransportKind::Slack => SubjectKind::SlackChannel,
            ChatTransportKind::Discord => SubjectKind::DiscordChannel,
            ChatTransportKind::Telegram
            | ChatTransportKind::Whatsapp
            | ChatTransportKind::Github
            | ChatTransportKind::Local => return false,
        };
        self.identities.mappings().any(|(subject, _)| {
            subject.kind() == kind && subject.id().eq_ignore_ascii_case(&scope.conversation.id)
        })
    }

    pub(super) fn memory_surface_v2(
        &self,
        context: &AuthenticatedContext,
    ) -> Result<Vec<u8>, BrokerError> {
        let mut encoded = AuthorityEncoder::new();
        encoded.text("format", "dekopon-authority-surface-v2");
        encoded.text("backend", "jsonl@0.1.0/chat-memory-format-v1");
        let effective = self.effective_authority(context, true);
        let effective_capabilities = effective
            .iter()
            .map(|(capability, _)| (*capability).clone())
            .collect::<BTreeSet<_>>();
        let mut reachable = BTreeSet::new();
        let mut reaches_http = false;
        encoded.number("capabilityCount", effective.len() as u128);
        for (capability, set) in &effective {
            encoded.text("capability", capability.as_str());
            encoded.text("provider", set.provider.as_str());
            match self.effective_default_proposal(context.actor(), capability) {
                Some(default) => {
                    encoded.byte("defaultSecret.present", 1);
                    encoded.text("defaultSecretDrn", default.secret().as_str());
                    encoded.text("defaultSecretSink", &default.sink().to_string());
                    encoded.optional_text("defaultSecretBasicUsername", default.username());
                }
                None => encoded.byte("defaultSecret.present", 0),
            }
            encode_authority_constraints(&mut encoded, &set.constraints);
            // Request policies run only through the HTTP authorizer a constraint set with HTTP
            // receives, so no other capability has request authority to fingerprint.
            if let Some(http) = &set.constraints.http {
                reaches_http = true;
                reachable.extend(http.allowed_hosts.iter().cloned());
                self.encode_request_outcomes(
                    &mut encoded,
                    context,
                    PolicyTarget::Capability {
                        capability: (*capability).clone(),
                        provider: set.provider.clone(),
                        effect: set.effect,
                        risk: set.risk,
                    },
                );
            }
        }

        let mut bindings = self
            .authorized_bindings(context, &effective_capabilities)
            .into_iter()
            .map(|(binding, capability)| {
                let sorted = |values: &[String]| {
                    values
                        .iter()
                        .map(|value| value.to_ascii_lowercase())
                        .collect::<BTreeSet<_>>()
                };
                let paths = binding
                    .allowed_paths
                    .iter()
                    .map(|rule| match rule {
                        dekopon_capability::HttpPathRule::Exact { path } => format!("exact:{path}"),
                        dekopon_capability::HttpPathRule::SegmentPrefix { path } => {
                            format!("segment-prefix:{path}")
                        }
                    })
                    .collect::<BTreeSet<_>>();
                (
                    (
                        binding.secret.as_str().to_owned(),
                        capability.clone(),
                        binding.sink.to_string(),
                        binding.basic_username.clone(),
                        sorted(&binding.allowed_hosts),
                        sorted(&binding.allowed_methods),
                        paths,
                        binding.allow_query,
                    ),
                    binding,
                )
            })
            .collect::<Vec<_>>();
        bindings.sort_by(|left, right| left.0.cmp(&right.0));
        encoded.number("secretBindingCount", bindings.len() as u128);
        for ((secret, capability, sink, username, hosts, methods, paths, allow_query), _) in
            bindings
        {
            encoded.text("secretDrn", &secret);
            encoded.text("secretCapability", capability.as_str());
            encoded.text("secretSink", &sink);
            encoded.optional_text("secretBasicUsername", username.as_deref());
            encoded.number("secretHostCount", hosts.len() as u128);
            for host in &hosts {
                encoded.text("secretHost", host);
            }
            reachable.extend(hosts);
            encoded.number("secretMethodCount", methods.len() as u128);
            for method in methods {
                encoded.text("secretMethod", &method);
            }
            encoded.number("secretPathCount", paths.len() as u128);
            for path in paths {
                encoded.text("secretPath", &path);
            }
            encoded.boolean("secretAllowQuery", allow_query);
        }

        let http = self.registry.http_destination_options();
        let projected = dekopon_broker_host::reachable_destination_settings(
            reachable.iter().map(String::as_str),
            http.plaintext_hosts,
            http.non_public_https,
        );
        encoded.number(
            "plaintextHostCount",
            projected.plaintext_hosts.len() as u128,
        );
        for host in &projected.plaintext_hosts {
            encoded.text("plaintextHost", host);
        }
        encoded.number(
            "nonPublicHttpsCount",
            projected.non_public_https.len() as u128,
        );
        for authority in &projected.non_public_https {
            encoded.text("nonPublicHttps", authority);
        }
        encode_trust_anchors(
            &mut encoded,
            if reaches_http {
                http.extra_ca_bundles
            } else {
                &[]
            },
        );
        Ok(encoded.finish())
    }

    fn encode_request_outcomes(
        &self,
        encoded: &mut AuthorityEncoder,
        context: &AuthenticatedContext,
        target: PolicyTarget,
    ) {
        let request = PolicyRequest {
            principal: context.principal().clone(),
            target,
            context: policy_context(context),
        };
        let github = context.chat_scope().and_then(|scope| scope.github.as_ref());
        let outcomes = match policy_contexts(&request.context, github) {
            Ok(passes) => passes
                .into_iter()
                .flat_map(|(pass_context, _)| {
                    let asker = PolicyRequest {
                        context: pass_context,
                        ..request.clone()
                    };
                    let space = context
                        .space
                        .as_ref()
                        .filter(|space| **space != asker.principal)
                        .map(|space| PolicyRequest {
                            principal: space.clone(),
                            ..asker.clone()
                        });
                    [Some(asker), space]
                        .into_iter()
                        .flatten()
                        .map(|pass| self.policy.request_outcome(pass))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>(),
            Err(_) => vec![RequestOutcome::Denied],
        };
        encoded.number("requestOutcomeCount", outcomes.len() as u128);
        for outcome in outcomes {
            match outcome {
                RequestOutcome::Denied => encoded.byte("requestOutcome", 0),
                RequestOutcome::Open {
                    permit_satisfied,
                    residuals,
                } => {
                    encoded.byte("requestOutcome", 1);
                    encoded.boolean("requestPermitSatisfied", permit_satisfied);
                    encoded.number("requestResidualCount", residuals.len() as u128);
                    for residual in residuals {
                        encoded.text("requestResidual", &residual);
                    }
                }
            }
        }
    }
}

fn encode_trust_anchors(encoded: &mut AuthorityEncoder, bundles: &[Vec<u8>]) {
    let anchors = bundles
        .iter()
        .map(|bundle| lowercase_hex(&Sha256::digest(bundle)))
        .collect::<BTreeSet<_>>();
    encoded.number("trustAnchorCount", anchors.len() as u128);
    for anchor in anchors {
        encoded.text("trustAnchorSha256", &anchor);
    }
}

fn encode_authority_constraints(
    encoded: &mut AuthorityEncoder,
    constraints: &ExecutionConstraints,
) {
    if let Some(http) = &constraints.http {
        encoded.byte("execution.http.present", 1);
        let hosts = http
            .allowed_hosts
            .iter()
            .map(|host| host.to_ascii_lowercase())
            .collect::<BTreeSet<_>>();
        encoded.number("execution.http.allowedHostCount", hosts.len() as u128);
        for host in &hosts {
            encoded.text("execution.http.allowedHost", host);
        }
        let methods = http
            .allowed_methods
            .iter()
            .map(|method| method.to_ascii_uppercase())
            .collect::<BTreeSet<_>>();
        encoded.number("execution.http.allowedMethodCount", methods.len() as u128);
        for method in &methods {
            encoded.text("execution.http.allowedMethod", method);
        }
        encoded.boolean(
            "execution.http.allowPlaintextLoopback",
            http.allow_plaintext_loopback,
        );
        encoded.text(
            "execution.http.signedDownload",
            match http.signed_download {
                None => "none",
                Some(SignedDownload::GithubActionsLogsV1) => "github-actions-logs-v1",
            },
        );
        encode_request_templates(encoded, &http.request_templates);
    } else {
        encoded.byte("execution.http.present", 0);
    }
    encode_storage_constraints(encoded, constraints.storage.as_ref());
    encode_asset_constraints(encoded, constraints.asset.as_ref());
}
