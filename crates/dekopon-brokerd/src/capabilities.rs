use std::collections::BTreeMap;

use dekopon_broker::{CapabilityRoute, ConstraintSet};
use dekopon_capability::{
    AssetConstraints, EffectKind, ExecutionConstraints, HttpConstraints, SecretUseGrant,
    StorageConstraints,
};
use dekopon_core::{CapabilityId, ProviderId, RiskLevel};
use serde::Deserialize;
use thiserror::Error;

/// One provider's execution bounds. Only the capabilities listed under `capabilities` run, each
/// inheriting `constraints` and `credential`, so a capability a provider upgrade adds reaches
/// nobody until the owner lists it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct ProviderCapabilities {
    pub credential: Option<String>,
    pub constraints: Option<ConstraintsPatch>,
    pub capabilities: BTreeMap<CapabilityId, CapabilityConfig>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct CapabilityConfig {
    pub route: CapabilityRoute,
    pub credential: Option<String>,
    pub constraints: ConstraintsPatch,
}

/// A field-wise mirror of `ExecutionConstraints`: an override replaces a field whole, lists
/// included, so a capability can never widen `allowedHosts` by appending to its provider's.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct ConstraintsPatch {
    pub timeout_ms: Option<u64>,
    pub max_output_bytes: Option<u64>,
    pub http: Option<HttpPatch>,
    pub storage: Option<StorageConstraints>,
    pub asset: Option<AssetConstraints>,
    pub secret_use: Option<SecretUseGrant>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct HttpPatch {
    pub allowed_hosts: Option<Vec<String>>,
    pub allowed_methods: Option<Vec<String>>,
    pub max_requests: Option<u32>,
    pub max_request_bytes: Option<u64>,
    pub max_response_bytes: Option<u64>,
    pub allow_plaintext_loopback: Option<bool>,
    pub propagate_trace: Option<bool>,
}

impl ConstraintsPatch {
    fn over(&self, base: &Self) -> Self {
        Self {
            timeout_ms: self.timeout_ms.or(base.timeout_ms),
            max_output_bytes: self.max_output_bytes.or(base.max_output_bytes),
            http: match (&self.http, &base.http) {
                (Some(top), Some(bottom)) => Some(top.over(bottom)),
                (top, bottom) => top.clone().or_else(|| bottom.clone()),
            },
            storage: self.storage.clone().or_else(|| base.storage.clone()),
            asset: self.asset.clone().or_else(|| base.asset.clone()),
            secret_use: self.secret_use.clone().or_else(|| base.secret_use.clone()),
        }
    }

    fn complete(
        self,
        capability: &CapabilityId,
    ) -> Result<ExecutionConstraints, CapabilityProblem> {
        let missing = |field: &'static str| CapabilityProblem::MissingField {
            capability: capability.clone(),
            field,
        };
        Ok(ExecutionConstraints {
            timeout_ms: self.timeout_ms.ok_or_else(|| missing("timeoutMs"))?,
            max_output_bytes: self
                .max_output_bytes
                .ok_or_else(|| missing("maxOutputBytes"))?,
            http: self
                .http
                .map(|http| {
                    Ok::<_, CapabilityProblem>(HttpConstraints {
                        allowed_hosts: http
                            .allowed_hosts
                            .ok_or_else(|| missing("http.allowedHosts"))?,
                        allowed_methods: http
                            .allowed_methods
                            .ok_or_else(|| missing("http.allowedMethods"))?,
                        max_requests: http
                            .max_requests
                            .ok_or_else(|| missing("http.maxRequests"))?,
                        max_request_bytes: http
                            .max_request_bytes
                            .ok_or_else(|| missing("http.maxRequestBytes"))?,
                        max_response_bytes: http
                            .max_response_bytes
                            .ok_or_else(|| missing("http.maxResponseBytes"))?,
                        allow_plaintext_loopback: http.allow_plaintext_loopback.unwrap_or(false),
                        propagate_trace: http.propagate_trace.unwrap_or(false),
                    })
                })
                .transpose()?,
            storage: self.storage,
            asset: self.asset,
            secret_use: self.secret_use,
        })
    }
}

impl HttpPatch {
    fn over(&self, base: &Self) -> Self {
        Self {
            allowed_hosts: self
                .allowed_hosts
                .clone()
                .or_else(|| base.allowed_hosts.clone()),
            allowed_methods: self
                .allowed_methods
                .clone()
                .or_else(|| base.allowed_methods.clone()),
            max_requests: self.max_requests.or(base.max_requests),
            max_request_bytes: self.max_request_bytes.or(base.max_request_bytes),
            max_response_bytes: self.max_response_bytes.or(base.max_response_bytes),
            allow_plaintext_loopback: self
                .allow_plaintext_loopback
                .or(base.allow_plaintext_loopback),
            propagate_trace: self.propagate_trace.or(base.propagate_trace),
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CapabilityProblem {
    #[error("capabilities.{provider} names a provider no loaded manifest declares")]
    UnknownProvider { provider: ProviderId },
    #[error(
        "capabilities.{provider}.capabilities names {capability}, which that provider does not declare"
    )]
    UnknownCapability {
        provider: ProviderId,
        capability: CapabilityId,
    },
    #[error("capability {capability} has no {field}")]
    MissingField {
        capability: CapabilityId,
        field: &'static str,
    },
}

impl CapabilityProblem {
    #[must_use]
    pub const fn is_unloaded_name(&self) -> bool {
        matches!(
            self,
            Self::UnknownProvider { .. } | Self::UnknownCapability { .. }
        )
    }
}

pub struct ManifestCapability<'a> {
    pub id: &'a CapabilityId,
    pub effect: EffectKind,
    pub risk: RiskLevel,
}

pub fn constraint_sets<'a>(
    providers: &BTreeMap<ProviderId, ProviderCapabilities>,
    manifest: impl Fn(&ProviderId) -> Option<Vec<ManifestCapability<'a>>>,
) -> (
    BTreeMap<CapabilityId, ConstraintSet>,
    Vec<CapabilityProblem>,
) {
    let mut sets = BTreeMap::new();
    let mut problems = Vec::new();
    for (provider, block) in providers {
        let Some(declared) = manifest(provider) else {
            problems.push(CapabilityProblem::UnknownProvider {
                provider: provider.clone(),
            });
            continue;
        };
        for capability in block.capabilities.keys() {
            if !declared.iter().any(|candidate| candidate.id == capability) {
                problems.push(CapabilityProblem::UnknownCapability {
                    provider: provider.clone(),
                    capability: capability.clone(),
                });
            }
        }
        for capability in declared {
            let Some(listed) = block.capabilities.get(capability.id) else {
                continue;
            };
            let base = block.constraints.clone().unwrap_or_default();
            match listed.constraints.over(&base).complete(capability.id) {
                Ok(constraints) => {
                    sets.insert(
                        capability.id.clone(),
                        ConstraintSet {
                            route: listed.route,
                            provider: provider.clone(),
                            effect: capability.effect,
                            risk: capability.risk,
                            credential: listed
                                .credential
                                .clone()
                                .or_else(|| block.credential.clone()),
                            constraints,
                        },
                    );
                }
                Err(problem) => problems.push(problem),
            }
        }
    }
    (sets, problems)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use dekopon_capability::EffectKind;
    use dekopon_core::{CapabilityId, ProviderId, RiskLevel};

    use super::{CapabilityProblem, ManifestCapability, ProviderCapabilities, constraint_sets};

    fn id(value: &str) -> CapabilityId {
        value.parse().expect("capability")
    }

    fn resolve(
        yaml: &str,
    ) -> (
        BTreeMap<CapabilityId, dekopon_broker::ConstraintSet>,
        Vec<CapabilityProblem>,
    ) {
        let providers: BTreeMap<ProviderId, ProviderCapabilities> =
            serde_yaml::from_str(yaml).expect("capabilities parse");
        let (read, write) = (id("gh.repo.read"), id("gh.issue.comment"));
        constraint_sets(&providers, |provider| {
            (provider.as_str() == "gh").then(|| {
                vec![
                    ManifestCapability {
                        id: &read,
                        effect: EffectKind::ReadOnly,
                        risk: RiskLevel::Low,
                    },
                    ManifestCapability {
                        id: &write,
                        effect: EffectKind::ExternalWrite,
                        risk: RiskLevel::Medium,
                    },
                ]
            })
        })
    }

    const DEFAULTS: &str = r"
gh:
  credential: github-pat
  constraints:
    timeoutMs: 10000
    maxOutputBytes: 4096
    http:
      allowedHosts: [api.github.com]
      allowedMethods: [GET]
      maxRequests: 1
      maxRequestBytes: 1024
      maxResponseBytes: 4096
";

    #[test]
    fn only_listed_capabilities_run_and_each_inherits_its_providers_bounds() {
        let (sets, problems) = resolve(DEFAULTS);
        assert!(problems.is_empty());
        assert!(sets.is_empty());

        let (sets, problems) = resolve(&format!(
            "{DEFAULTS}  capabilities:\n    gh.repo.read: {{}}\n    gh.issue.comment:\n      constraints: {{ http: {{ allowedMethods: [GET, POST], maxRequests: 2 }} }}\n"
        ));
        assert!(problems.is_empty());
        let read = &sets[&id("gh.repo.read")];
        assert_eq!(read.effect, EffectKind::ReadOnly);
        assert_eq!(read.credential.as_deref(), Some("github-pat"));
        let write = &sets[&id("gh.issue.comment")];
        let http = write.constraints.http.as_ref().expect("inherited http");
        assert_eq!(write.effect, EffectKind::ExternalWrite);
        assert_eq!(http.allowed_methods, ["GET", "POST"]);
        assert_eq!(http.allowed_hosts, ["api.github.com"]);
        assert_eq!(http.max_requests, 2);
    }

    #[test]
    fn an_override_list_replaces_the_providers_rather_than_extending_it() {
        let (sets, _) = resolve(&format!(
            "{DEFAULTS}  capabilities:\n    gh.repo.read:\n      constraints: {{ http: {{ allowedHosts: [uploads.github.com] }} }}\n"
        ));
        let http = sets[&id("gh.repo.read")]
            .constraints
            .http
            .clone()
            .expect("http");
        assert_eq!(http.allowed_hosts, ["uploads.github.com"]);
    }

    #[test]
    fn trace_propagation_defaults_off_and_capabilities_override_the_provider_opt_in() {
        for (provider, capability, expected) in [
            ("", "{}", false),
            ("      propagateTrace: true\n", "{}", true),
            (
                "      propagateTrace: true\n",
                "{propagateTrace: false}",
                false,
            ),
            ("", "{propagateTrace: true}", true),
        ] {
            let (sets, problems) = resolve(&format!(
                "{DEFAULTS}{provider}  capabilities:\n    gh.repo.read:\n      constraints: {{ http: {capability} }}\n"
            ));
            assert!(problems.is_empty());
            let http = sets[&id("gh.repo.read")]
                .constraints
                .http
                .as_ref()
                .expect("http");
            assert_eq!(http.propagate_trace, expected);
            let serialized = serde_json::to_value(http).expect("HTTP constraints serialize");
            assert_eq!(
                serialized.get("propagateTrace"),
                expected.then_some(&serde_json::Value::Bool(true))
            );
            assert_eq!(
                serde_json::from_value::<dekopon_capability::HttpConstraints>(serialized)
                    .expect("HTTP constraints round-trip"),
                *http
            );
        }
    }

    #[test]
    fn every_missing_field_and_unknown_name_is_reported_at_once() {
        let (_, problems) = resolve(
            r"
gh:
  constraints: { maxOutputBytes: 4096 }
  capabilities:
    gh.repo.read: {}
    gh.repo.delete: {}
absent:
  constraints: { timeoutMs: 1, maxOutputBytes: 1 }
",
        );
        assert!(problems.contains(&CapabilityProblem::MissingField {
            capability: id("gh.repo.read"),
            field: "timeoutMs",
        }));
        assert!(problems.contains(&CapabilityProblem::UnknownCapability {
            provider: "gh".parse().expect("provider"),
            capability: id("gh.repo.delete"),
        }));
        assert!(problems.contains(&CapabilityProblem::UnknownProvider {
            provider: "absent".parse().expect("provider"),
        }));
    }
}
