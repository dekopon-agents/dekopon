//! Authored OpenRouter settings; omission preserves the upstream default.

use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;
use thiserror::Error;

/// Fixed settings for one configured client, never request-scoped routing affinity.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    /// Optional sampling and output controls.
    pub generation: Option<Generation>,
    /// Optional reasoning effort.
    pub reasoning: Option<Reasoning>,
    /// Optional provider routing constraints.
    pub routing: Option<Routing>,
    /// Optional explicit prefix caching; absence means automatic.
    pub cache: Option<Cache>,
}

/// Authored sampling settings; each absent member remains absent on the wire.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Generation {
    /// Positive output-token ceiling.
    pub max_output_tokens: Option<NonZeroU32>,
    /// Sampling temperature, inclusive 0–2.
    pub temperature: Option<f64>,
    /// Nucleus probability, greater than zero and at most one.
    pub top_p: Option<f64>,
}

/// Authored reasoning policy.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reasoning {
    /// Requested effort; forwarding does not prove upstream support.
    pub effort: Effort,
}

/// Closed spellings supported by the configuration contract.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// No reasoning requested.
    None,
    /// Minimal effort.
    Minimal,
    /// Low effort.
    Low,
    /// Medium effort.
    Medium,
    /// High effort.
    High,
    /// Extra-high effort.
    Xhigh,
    /// Maximum effort.
    Max,
}

/// Authored provider constraints; no defaults are invented.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Routing {
    /// Whether the router may fall back to another provider.
    pub allow_fallbacks: Option<bool>,
    /// Whether selected providers must support all requested parameters.
    pub require_parameters: Option<bool>,
    /// A nonempty allowlist of nonempty provider identifiers.
    pub only: Option<Vec<String>>,
}

/// Prefix cache policy; a TTL is valid only with an explicit prefix.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cache {
    /// Automatic or an explicitly marked leading system prefix.
    pub style: CacheStyle,
    /// Optional retention hint, omitted when not authored.
    pub ttl: Option<Ttl>,
}

/// Cache modes supported by this adapter.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CacheStyle {
    /// No explicit cache marker.
    Automatic,
    /// Mark the last part of the final leading system message.
    ExplicitPrefix,
}

/// Exact upstream retention spellings.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum Ttl {
    /// Five minutes.
    #[serde(rename = "5m")]
    FiveMinutes,
    /// One hour.
    #[serde(rename = "1h")]
    OneHour,
}

/// One semantic problem; callers can collect every authored problem before refusing.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum SettingsProblem {
    /// Temperature bounds, including non-finite values.
    #[error("temperature must be finite and between zero and two")]
    Temperature,
    /// Nucleus bounds, including non-finite values.
    #[error("topP must be greater than zero and at most one")]
    TopP,
    /// The routing allowlist must contain at least one provider.
    #[error("routing.only must not be empty")]
    EmptyOnly,
    /// Each provider identifier must be nonempty.
    #[error("routing.only entries must not be blank")]
    EmptyProvider,
    /// Automatic cache policy has no TTL setting.
    #[error("cache.ttl requires explicitPrefix")]
    AutomaticTtl,
}

impl Settings {
    /// Collects semantic errors without silently clamping or dropping authored controls.
    #[must_use]
    pub fn problems(&self) -> Vec<SettingsProblem> {
        let mut problems = Vec::new();
        if let Some(generation) = &self.generation {
            if generation
                .temperature
                .is_some_and(|n| !(0.0..=2.0).contains(&n))
            {
                problems.push(SettingsProblem::Temperature);
            }
            if generation.top_p.is_some_and(|n| !(n > 0.0 && n <= 1.0)) {
                problems.push(SettingsProblem::TopP);
            }
        }
        if let Some(only) = self
            .routing
            .as_ref()
            .and_then(|routing| routing.only.as_ref())
        {
            if only.is_empty() {
                problems.push(SettingsProblem::EmptyOnly);
            }
            for provider in only {
                if provider.trim().is_empty() {
                    problems.push(SettingsProblem::EmptyProvider);
                }
            }
        }
        if self
            .cache
            .as_ref()
            .is_some_and(|cache| cache.style == CacheStyle::Automatic && cache.ttl.is_some())
        {
            problems.push(SettingsProblem::AutomaticTtl);
        }
        problems
    }
}
