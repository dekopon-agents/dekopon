use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;
use thiserror::Error;

#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub generation: Option<Generation>,
    pub reasoning: Option<Reasoning>,
    pub routing: Option<Routing>,
    pub cache: Option<Cache>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Generation {
    pub max_output_tokens: Option<NonZeroU32>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reasoning {
    pub effort: Effort,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Routing {
    pub allow_fallbacks: Option<bool>,
    pub require_parameters: Option<bool>,
    pub only: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cache {
    pub style: CacheStyle,
    pub ttl: Option<Ttl>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CacheStyle {
    Automatic,
    ExplicitPrefix,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum Ttl {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum SettingsProblem {
    #[error("temperature must be finite and between zero and two")]
    Temperature,
    #[error("topP must be greater than zero and at most one")]
    TopP,
    #[error("routing.only must not be empty")]
    EmptyOnly,
    #[error("routing.only entries must not be blank")]
    EmptyProvider,
    #[error("cache.ttl requires explicitPrefix")]
    AutomaticTtl,
}

impl Settings {
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
