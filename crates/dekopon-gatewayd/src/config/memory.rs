use std::{
    collections::{BTreeMap, BTreeSet},
    num::{NonZeroU32, NonZeroU64, NonZeroUsize},
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
    pub summaries: SummaryEngine,
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SummaryEngine {
    #[serde(default = "default_concurrency")]
    pub concurrency: NonZeroUsize,
    #[serde(default = "default_queue")]
    pub queue: NonZeroUsize,
}

impl Default for SummaryEngine {
    fn default() -> Self {
        Self {
            concurrency: default_concurrency(),
            queue: default_queue(),
        }
    }
}

fn default_concurrency() -> NonZeroUsize {
    NonZeroUsize::MIN
}
fn default_queue() -> NonZeroUsize {
    NonZeroUsize::new(64).expect("positive summary queue")
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentMemory {
    #[serde(default)]
    pub retention: Option<AgentRetention>,
    #[serde(default)]
    pub hot_turns: Option<NonZeroUsize>,
    #[serde(default)]
    pub summaries: Option<SummariesConfig>,
    #[serde(default)]
    pub capture: Option<CaptureConfig>,
    #[serde(default)]
    pub selection: Option<SelectionConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentRetention {
    pub max_age_ms: Option<NonZeroU64>,
    pub max_bytes: Option<NonZeroU64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(untagged)]
pub enum SummariesConfig {
    Off(Off),
    On(SummaryParts),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub enum Off {
    #[serde(rename = "off")]
    Off,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SummaryParts {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_parts")]
    pub parts: Vec<NonZeroU32>,
    #[serde(default = "default_max_input_bytes")]
    pub max_input_bytes: NonZeroU32,
    #[serde(default)]
    pub attachments: AttachmentSummaries,
}

impl Default for SummaryParts {
    fn default() -> Self {
        Self {
            model: None,
            parts: default_parts(),
            max_input_bytes: default_max_input_bytes(),
            attachments: AttachmentSummaries::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AttachmentSummaries {
    #[serde(default)]
    pub images: Toggle,
    #[serde(default)]
    pub documents: Toggle,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Toggle {
    #[default]
    Off,
    Summarize,
}

impl AttachmentSummaries {
    pub(crate) fn summarizes(self, kind: dekopon_core::asset::AssetKind) -> bool {
        use dekopon_core::asset::AssetKind;
        match match kind {
            AssetKind::Image => self.images,
            AssetKind::TextDocument | AssetKind::BinaryDocument => self.documents,
            AssetKind::Other => Toggle::Off,
        } {
            Toggle::Off => false,
            Toggle::Summarize => true,
        }
    }
}

fn default_parts() -> Vec<NonZeroU32> {
    [200, 1024, 4096]
        .into_iter()
        .filter_map(NonZeroU32::new)
        .collect()
}
fn default_max_input_bytes() -> NonZeroU32 {
    NonZeroU32::new(128 * 1024).expect("positive summary input")
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CaptureConfig {
    pub max_item_bytes: NonZeroU32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(untagged)]
pub enum SelectionConfig {
    Ladder(Ladder),
    Decider(DeciderSelection),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub enum Ladder {
    #[serde(rename = "ladder")]
    Ladder,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DeciderSelection {
    pub decider: Decider,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Decider {
    pub capability: String,
    pub backend: String,
    pub model: String,
    pub require_probabilities: bool,
}

/// The route-level memory keys that override the agent's; each is optional on both.
#[derive(Clone, Debug, Default)]
pub struct KnobOverrides {
    pub hot_turns: Option<NonZeroUsize>,
    pub summaries: Option<SummariesConfig>,
    pub capture: Option<CaptureConfig>,
    pub selection: Option<SelectionConfig>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SummaryModel {
    Route,
    Named(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSummaries {
    pub model: SummaryModel,
    pub parts: Vec<NonZeroU32>,
    pub max_input_bytes: NonZeroU32,
    pub attachments: AttachmentSummaries,
}

impl ResolvedSummaries {
    pub(crate) fn validate_attachments(
        &self,
        model: Option<&super::ModelConfig>,
        queue: NonZeroUsize,
    ) -> Vec<PolicyProblem> {
        let mut problems = Vec::new();
        let enabled = match (self.attachments.images, self.attachments.documents) {
            (Toggle::Off, Toggle::Off) => false,
            (Toggle::Summarize, Toggle::Off | Toggle::Summarize)
            | (Toggle::Off, Toggle::Summarize) => true,
        };
        if enabled && queue.get() < 2 {
            problems.push(PolicyProblem::AttachmentSummariesNeedQueue { queue: queue.get() });
        }
        match self.attachments.images {
            Toggle::Off => {}
            Toggle::Summarize => {
                if let Some(model) = model
                    && !model.accepts_images()
                {
                    problems.push(PolicyProblem::ImageSummariesNeedImageModel {
                        model: model.name().to_owned(),
                    });
                }
            }
        }
        problems
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Selection {
    Ladder,
    Decider(Decider),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryKnobs {
    pub hot_turns: NonZeroUsize,
    pub summaries: Option<ResolvedSummaries>,
    pub capture_max_item_bytes: NonZeroU32,
    pub selection: Selection,
}

pub const DEFAULT_HOT_TURNS: usize = 10;
pub const DEFAULT_CAPTURE_MAX_ITEM_BYTES: u32 = 256 * 1024;
pub const JOURNAL_RECALL_MAX_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug)]
pub struct ResolvedSettings {
    pub retention: Retention,
    pub summaries: SummaryEngine,
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
    #[error("hotTurns {hot_turns} exceeds maxTurns {max_turns}")]
    HotTurns { hot_turns: usize, max_turns: usize },
    #[error("summaries.parts must hold 1 to 255 entries")]
    Parts,
    #[error("summaries.model names an unknown model")]
    UnknownModel,
    #[error("summaries.model {0} is proxy-only")]
    ProxyOnlyModel(String),
    #[error("image attachment summaries need an image-capable summaries model: {model}")]
    ImageSummariesNeedImageModel { model: String },
    #[error("attachment summaries need a summary queue of at least two, got {queue}")]
    AttachmentSummariesNeedQueue { queue: usize },
    #[error("selection.decider has an empty {0}")]
    Decider(&'static str),
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
    if settings.summaries.queue.get() > tokio::sync::Semaphore::MAX_PERMITS {
        return Err(PolicyProblem::Bound("summaries.queue"));
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
        summaries: settings.summaries,
        agents,
    })
}

pub struct KnobInputs<'a> {
    pub route: &'a KnobOverrides,
    pub agent: Option<&'a AgentMemory>,
    pub max_turns: usize,
    pub journal_recall: JournalRecall,
    pub models: &'a BTreeSet<String>,
    pub proxy_only: &'a BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalRecall {
    Replays,
    Never,
}

pub fn resolve_knobs(inputs: &KnobInputs<'_>) -> Result<MemoryKnobs, PolicyProblem> {
    let agent = inputs.agent;
    let hot_turns = match inputs
        .route
        .hot_turns
        .or_else(|| agent.and_then(|agent| agent.hot_turns))
    {
        Some(hot_turns) if hot_turns.get() > inputs.max_turns => {
            return Err(PolicyProblem::HotTurns {
                hot_turns: hot_turns.get(),
                max_turns: inputs.max_turns,
            });
        }
        Some(hot_turns) => hot_turns,
        None => {
            NonZeroUsize::new(DEFAULT_HOT_TURNS.min(inputs.max_turns)).unwrap_or(NonZeroUsize::MIN)
        }
    };
    let summaries = inputs
        .route
        .summaries
        .clone()
        .or_else(|| agent.and_then(|agent| agent.summaries.clone()))
        .unwrap_or_else(|| SummariesConfig::On(SummaryParts::default()));
    let summaries = match (summaries, inputs.journal_recall) {
        (SummariesConfig::Off(Off::Off), _) | (SummariesConfig::On(_), JournalRecall::Never) => {
            None
        }
        (SummariesConfig::On(parts), JournalRecall::Replays) => {
            if parts.parts.is_empty() || parts.parts.len() > usize::from(u8::MAX) {
                return Err(PolicyProblem::Parts);
            }
            let model = match parts.model {
                None => SummaryModel::Route,
                Some(name) if !inputs.models.contains(&name) => {
                    return Err(PolicyProblem::UnknownModel);
                }
                Some(name) if inputs.proxy_only.contains(&name) => {
                    return Err(PolicyProblem::ProxyOnlyModel(name));
                }
                Some(name) => SummaryModel::Named(name),
            };
            Some(ResolvedSummaries {
                model,
                parts: parts.parts,
                max_input_bytes: parts.max_input_bytes,
                attachments: parts.attachments,
            })
        }
    };
    let capture_max_item_bytes = inputs
        .route
        .capture
        .or_else(|| agent.and_then(|agent| agent.capture))
        .map_or(
            NonZeroU32::new(DEFAULT_CAPTURE_MAX_ITEM_BYTES).expect("positive capture cap"),
            |capture| capture.max_item_bytes,
        );
    let selection = match inputs
        .route
        .selection
        .clone()
        .or_else(|| agent.and_then(|agent| agent.selection.clone()))
    {
        None | Some(SelectionConfig::Ladder(Ladder::Ladder)) => Selection::Ladder,
        Some(SelectionConfig::Decider(DeciderSelection { decider })) => {
            for (field, value) in [
                ("capability", &decider.capability),
                ("backend", &decider.backend),
                ("model", &decider.model),
            ] {
                if value.trim().is_empty() || value.trim() != value {
                    return Err(PolicyProblem::Decider(field));
                }
            }
            Selection::Decider(decider)
        }
    };
    Ok(MemoryKnobs {
        hot_turns,
        summaries,
        capture_max_item_bytes,
        selection,
    })
}
