use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
    time::Duration,
};

use dekopon_core::AgentId;
use serde::Deserialize;
use thiserror::Error;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MemorySettings {
    #[serde(default)]
    pub retention: Retention,
    #[serde(default)]
    pub arena: Arena,
    #[serde(default)]
    pub agents: BTreeMap<AgentId, AgentMemory>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Retention {
    #[serde(default = "default_age")]
    pub max_age_ms: NonZeroU64,
    #[serde(default = "default_agent_bytes")]
    pub max_bytes_per_agent: NonZeroU64,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            max_age_ms: default_age(),
            max_bytes_per_agent: default_agent_bytes(),
        }
    }
}

fn default_age() -> NonZeroU64 {
    NonZeroU64::new(7 * 24 * 60 * 60 * 1000).expect("positive retention age")
}
fn default_agent_bytes() -> NonZeroU64 {
    NonZeroU64::new(64 * 1024 * 1024).expect("positive agent ceiling")
}
fn default_transient_bytes() -> NonZeroU64 {
    NonZeroU64::new(128 * 1024 * 1024).expect("positive transient ceiling")
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Arena {
    #[serde(default = "default_transient_bytes")]
    pub max_transient_bytes: NonZeroU64,
}

impl Default for Arena {
    fn default() -> Self {
        Self {
            max_transient_bytes: default_transient_bytes(),
        }
    }
}

pub const ARENA_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentMemory {
    #[serde(default)]
    pub retention: Option<AgentRetention>,
    #[serde(default)]
    pub routine: Option<String>,
    #[serde(default)]
    pub policy: Option<PolicyConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentRetention {
    pub max_age_ms: Option<NonZeroU64>,
    pub max_bytes: Option<NonZeroU64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PolicyConfig {
    pub capture: Capture,
    pub routines: Vec<Routine>,
    pub retain: Vec<String>,
    pub selection: Selection,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Capture {
    pub user: CaptureBound,
    pub assistant: CaptureBound,
    pub tool_request: CaptureBound,
    pub tool_result: CaptureBound,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(untagged)]
pub enum CaptureBound {
    Bytes(NonZeroU64),
    Word(Uncapped),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub enum Uncapped {
    #[serde(rename = "uncapped")]
    Uncapped,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Routine {
    pub id: String,
    pub kind: RoutineKind,
    pub max_bytes: Option<NonZeroU64>,
    pub model: Option<String>,
    pub parts: Option<Vec<Part>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum RoutineKind {
    Original,
    Prefix,
    ProgressiveOneShot,
    ProgressiveTurducken,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Part {
    pub id: String,
    pub max_bytes: NonZeroU64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Selection {
    pub mode: SelectionMode,
    pub routine: Option<String>,
    pub decision_capability: Option<String>,
    pub fallback: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum SelectionMode {
    Fixed,
    AgentPreference,
    Decider,
}

#[derive(Clone, Debug)]
pub struct ResolvedPolicy {
    pub capture: Capture,
    pub routines: Vec<Routine>,
    pub retain: Vec<String>,
    pub selection: Selection,
    pub preferred_routine: Option<String>,
}

impl Default for ResolvedPolicy {
    fn default() -> Self {
        Self {
            capture: Capture {
                user: CaptureBound::Word(Uncapped::Uncapped),
                assistant: CaptureBound::Word(Uncapped::Uncapped),
                tool_request: CaptureBound::Word(Uncapped::Uncapped),
                tool_result: CaptureBound::Bytes(
                    NonZeroU64::new(256 * 1024).expect("positive shell output ceiling"),
                ),
            },
            routines: vec![Routine {
                id: "original".into(),
                kind: RoutineKind::Original,
                max_bytes: None,
                model: None,
                parts: None,
            }],
            retain: vec!["original".into()],
            selection: Selection {
                mode: SelectionMode::Fixed,
                routine: Some("original".into()),
                decision_capability: None,
                fallback: None,
            },
            preferred_routine: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedSettings {
    pub retention: Retention,
    pub arena: Arena,
    pub sweep_interval: Duration,
    pub agents: BTreeMap<AgentId, ResolvedAgent>,
}

#[derive(Clone, Debug)]
pub struct ResolvedAgent {
    pub max_age: Duration,
    pub max_bytes: NonZeroU64,
}

#[derive(Clone, Debug, Error)]
pub enum PolicyProblem {
    #[error("unknown memory agent {0}")]
    UnknownAgent(AgentId),
    #[error("agent {0} memory retention exceeds global ceiling")]
    Retention(AgentId),
    #[error("memory field {0} exceeds the supported bound")]
    Bound(&'static str),
    #[error("memory policy has an invalid {0}")]
    Invalid(&'static str),
    #[error("memory routine names unknown model {0}")]
    UnknownModel(String),
    #[error("memory routine model {0} is proxy-only")]
    ProxyOnlyModel(String),
}

pub fn resolve_settings(
    settings: &MemorySettings,
    agent_ids: &BTreeSet<AgentId>,
) -> Result<ResolvedSettings, PolicyProblem> {
    let max_age = settings.retention.max_age_ms.get();
    let max_bytes = settings.retention.max_bytes_per_agent.get();
    if max_age > i64::MAX as u64 {
        return Err(PolicyProblem::Bound("retention.maxAgeMs"));
    }
    if max_bytes > usize::MAX as u64 {
        return Err(PolicyProblem::Bound("retention.maxBytesPerAgent"));
    }
    if settings.arena.max_transient_bytes.get() > usize::MAX as u64 {
        return Err(PolicyProblem::Bound("arena.maxTransientBytes"));
    }
    let mut agents = BTreeMap::new();
    for (id, entry) in &settings.agents {
        if !agent_ids.contains(id) {
            return Err(PolicyProblem::UnknownAgent(id.clone()));
        }
        let age = entry
            .retention
            .as_ref()
            .and_then(|r| r.max_age_ms)
            .map_or(max_age, NonZeroU64::get);
        let bytes = entry
            .retention
            .as_ref()
            .and_then(|r| r.max_bytes)
            .unwrap_or(settings.retention.max_bytes_per_agent);
        if age > max_age || bytes.get() > max_bytes {
            return Err(PolicyProblem::Retention(id.clone()));
        }
        agents.insert(
            id.clone(),
            ResolvedAgent {
                max_age: Duration::from_millis(age),
                max_bytes: bytes,
            },
        );
    }
    Ok(ResolvedSettings {
        retention: settings.retention.clone(),
        arena: settings.arena.clone(),
        sweep_interval: ARENA_SWEEP_INTERVAL,
        agents,
    })
}

fn valid_id(id: &str) -> bool {
    !id.trim().is_empty() && id == id.trim()
}

pub fn resolve_policy(
    policy: Option<&PolicyConfig>,
    agent_routine: Option<&str>,
    models: &BTreeSet<String>,
    proxy_only: &BTreeSet<String>,
) -> Result<ResolvedPolicy, PolicyProblem> {
    let Some(policy) = policy else {
        return if agent_routine.is_some() {
            Err(PolicyProblem::Invalid("agent routine"))
        } else {
            Ok(ResolvedPolicy::default())
        };
    };
    let mut ids = BTreeSet::new();
    for routine in &policy.routines {
        if !valid_id(&routine.id) || !ids.insert(routine.id.as_str()) {
            return Err(PolicyProblem::Invalid("routine id"));
        }
        if routine
            .max_bytes
            .is_some_and(|size| size.get() > usize::MAX as u64)
        {
            return Err(PolicyProblem::Bound("routine.maxBytes"));
        }
        match routine.kind {
            RoutineKind::Original
                if routine.max_bytes.is_none()
                    && routine.model.is_none()
                    && routine.parts.is_none() => {}
            RoutineKind::Prefix
                if routine.max_bytes.is_some()
                    && routine.model.is_none()
                    && routine.parts.is_none() => {}
            RoutineKind::ProgressiveOneShot | RoutineKind::ProgressiveTurducken
                if routine.max_bytes.is_none() && routine.model.is_some() =>
            {
                let Some(parts) = &routine.parts else {
                    return Err(PolicyProblem::Invalid("progressive parts"));
                };
                let mut part_ids = BTreeSet::new();
                let mut prior = 0;
                if parts.is_empty() {
                    return Err(PolicyProblem::Invalid("progressive parts"));
                }
                for part in parts {
                    if !valid_id(&part.id)
                        || !part_ids.insert(part.id.as_str())
                        || part.max_bytes.get() <= prior
                    {
                        return Err(PolicyProblem::Invalid("progressive parts"));
                    }
                    if part.max_bytes.get() > usize::MAX as u64 {
                        return Err(PolicyProblem::Bound("part.maxBytes"));
                    }
                    prior = part.max_bytes.get();
                }
                let Some(model) = routine.model.as_ref() else {
                    return Err(PolicyProblem::Invalid("routine model"));
                };
                if !models.contains(model) {
                    return Err(PolicyProblem::UnknownModel(model.clone()));
                }
                if proxy_only.contains(model) {
                    return Err(PolicyProblem::ProxyOnlyModel(model.clone()));
                }
            }
            RoutineKind::Original
            | RoutineKind::Prefix
            | RoutineKind::ProgressiveOneShot
            | RoutineKind::ProgressiveTurducken => {
                return Err(PolicyProblem::Invalid("routine fields"));
            }
        }
    }
    if agent_routine.is_some_and(|name| !ids.contains(name)) {
        return Err(PolicyProblem::Invalid("agent routine"));
    }
    let mut retained = BTreeSet::new();
    for name in &policy.retain {
        if !ids.contains(name.as_str()) || !retained.insert(name.as_str()) {
            return Err(PolicyProblem::Invalid("retain"));
        }
    }
    if retained.is_empty() {
        return Err(PolicyProblem::Invalid("retain"));
    }
    let selected = &policy.selection;
    let preferred_routine = match selected.mode {
        SelectionMode::Fixed
            if selected.decision_capability.is_none()
                && selected.fallback.is_none()
                && selected
                    .routine
                    .as_ref()
                    .is_some_and(|name| retained.contains(name.as_str())) =>
        {
            None
        }
        SelectionMode::AgentPreference
            if selected.decision_capability.is_none()
                && selected.fallback.is_none()
                && selected
                    .routine
                    .as_ref()
                    .is_some_and(|name| retained.contains(name.as_str())) =>
        {
            if let Some(name) = agent_routine
                && !retained.contains(name)
            {
                return Err(PolicyProblem::Invalid("agent routine"));
            }
            agent_routine.map(str::to_owned)
        }
        SelectionMode::Decider
            if selected.routine.is_none()
                && selected
                    .decision_capability
                    .as_deref()
                    .is_some_and(valid_id)
                && selected
                    .fallback
                    .as_ref()
                    .is_some_and(|name| retained.contains(name.as_str())) =>
        {
            None
        }
        SelectionMode::Fixed | SelectionMode::AgentPreference | SelectionMode::Decider => {
            return Err(PolicyProblem::Invalid("selection"));
        }
    };
    Ok(ResolvedPolicy {
        capture: policy.capture.clone(),
        routines: policy.routines.clone(),
        retain: policy.retain.clone(),
        selection: selected.clone(),
        preferred_routine,
    })
}
