use std::{
    collections::{BTreeMap, BTreeSet},
    ops::ControlFlow,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use dekopon_core::AgentId;
use dekopon_model::{
    TurnEvent,
    error::InferenceError,
    model::{AssistantTurn, ChatModel, CompletionOptions, ContentPart, ModelMessage, ModelTool},
};
use dekopon_model_token_governor::{
    Budget, Call, Estimate, InputHint, MeterSpec, Metering, Outcome, Sizes, Tokens, Via,
};
use serde::Deserialize;
use thiserror::Error;

use crate::{config::ModelConfig, session::SharedModel};

const MIN_PERIOD: Duration = Duration::from_secs(60);
const MAX_PERIOD: Duration = Duration::from_secs(366 * 86_400);

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MeteringConfig {
    #[serde(default)]
    pub budgets: BTreeMap<String, BudgetConfig>,
    #[serde(default)]
    pub restore: Option<RestoreConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BudgetConfig {
    #[serde(default)]
    pub models: Option<Vec<String>>,
    pub meters: Vec<MeterConfig>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(
    tag = "kind",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum MeterConfig {
    Fixed {
        limit: u64,
        period: HumanDuration,
    },
    Rolling {
        limit: u64,
        period: HumanDuration,
    },
    Session {
        limit: u64,
        length: HumanDuration,
    },
    Credit {
        capacity: u64,
        refill: u64,
        per: HumanDuration,
        initial: Initial,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(untagged)]
pub enum Initial {
    Full(Full),
    Tokens(u64),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum Full {
    Full,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HumanDuration(pub Duration);

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        humantime::parse_duration(&text)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(
    tag = "kind",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RestoreConfig {
    Openobserve {
        endpoint: String,
        org: String,
        stream: String,
        auth_env: String,
        #[serde(default)]
        delay: RestoreDelay,
        #[serde(default)]
        timeout: RestoreTimeout,
        #[serde(default)]
        lookback_max: LookbackMax,
    },
    Quickwit {
        endpoint: String,
        index: String,
        #[serde(default)]
        delay: RestoreDelay,
        #[serde(default)]
        timeout: RestoreTimeout,
        #[serde(default)]
        lookback_max: LookbackMax,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(transparent)]
pub struct RestoreDelay(pub HumanDuration);

impl Default for RestoreDelay {
    fn default() -> Self {
        Self(HumanDuration(Duration::from_secs(30)))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(transparent)]
pub struct RestoreTimeout(pub HumanDuration);

impl Default for RestoreTimeout {
    fn default() -> Self {
        Self(HumanDuration(Duration::from_secs(10)))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(transparent)]
pub struct LookbackMax(pub HumanDuration);

impl Default for LookbackMax {
    fn default() -> Self {
        Self(HumanDuration(Duration::from_secs(7 * 86_400)))
    }
}

impl RestoreConfig {
    pub(crate) const fn delay(&self) -> Duration {
        match self {
            Self::Openobserve { delay, .. } | Self::Quickwit { delay, .. } => delay.0.0,
        }
    }

    pub(crate) const fn lookback_max(&self) -> Duration {
        match self {
            Self::Openobserve { lookback_max, .. } | Self::Quickwit { lookback_max, .. } => {
                lookback_max.0.0
            }
        }
    }

    pub(crate) const fn timeout(&self) -> Duration {
        match self {
            Self::Openobserve { timeout, .. } | Self::Quickwit { timeout, .. } => timeout.0.0,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MeteringProblem {
    #[error("metering budget names {agent:?}, which no route serves")]
    UnknownAgent { agent: String },
    #[error("metering budget for {agent:?} names unknown model {model:?}")]
    UnknownModel { agent: String, model: String },
    #[error("metering budget for {agent:?} lists no meters")]
    EmptyMeters { agent: String },
    #[error("metering budget for {agent:?} lists no models; omit models to cover every model")]
    EmptyModels { agent: String },
    #[error("metering budget for {agent:?}: meter {meter} must have a limit above zero")]
    ZeroLimit { agent: String, meter: usize },
    #[error(
        "metering budget for {agent:?}: meter {meter} starts with more credit than its capacity"
    )]
    InitialAboveCapacity { agent: String, meter: usize },
    #[error("metering budget for {agent:?}: meter {meter} has a period shorter than one minute")]
    PeriodTooShort { agent: String, meter: usize },
    #[error("metering budget for {agent:?}: meter {meter} has a period longer than 366 days")]
    PeriodTooLong { agent: String, meter: usize },
    #[error("metering.restore reads records this gateway never exports; add a telemetry block")]
    RestoreWithoutTelemetry,
    #[error("metering.restore timeout must be greater than zero")]
    ZeroRestoreTimeout,
}

#[derive(Clone, Debug, Default)]
pub struct ResolvedMetering {
    pub budgets: Vec<ResolvedBudget>,
    pub restore: Option<RestoreConfig>,
}

#[derive(Clone, Debug)]
pub struct ResolvedBudget {
    pub agent: AgentId,
    pub models: Option<BTreeSet<String>>,
    pub meters: Vec<MeterSpec>,
}

pub(crate) fn resolve(
    config: Option<MeteringConfig>,
    agents: &BTreeSet<String>,
    models: &BTreeSet<String>,
    telemetry: bool,
    problems: &mut Vec<MeteringProblem>,
) -> ResolvedMetering {
    let Some(config) = config else {
        return ResolvedMetering::default();
    };
    let mut budgets = Vec::with_capacity(config.budgets.len());
    for (agent, budget) in config.budgets {
        let id = agent
            .parse::<AgentId>()
            .ok()
            .filter(|_| agents.contains(&agent));
        if id.is_none() {
            problems.push(MeteringProblem::UnknownAgent {
                agent: agent.clone(),
            });
        }
        if budget.meters.is_empty() {
            problems.push(MeteringProblem::EmptyMeters {
                agent: agent.clone(),
            });
        }
        if budget.models.as_ref().is_some_and(Vec::is_empty) {
            problems.push(MeteringProblem::EmptyModels {
                agent: agent.clone(),
            });
        }
        for model in budget.models.iter().flatten() {
            if !models.contains(model) {
                problems.push(MeteringProblem::UnknownModel {
                    agent: agent.clone(),
                    model: model.clone(),
                });
            }
        }
        let meters = budget
            .meters
            .iter()
            .enumerate()
            .map(|(meter, config)| spec(&agent, meter, *config, problems))
            .collect();
        if let Some(agent) = id {
            budgets.push(ResolvedBudget {
                agent,
                models: budget.models.map(|models| models.into_iter().collect()),
                meters,
            });
        }
    }
    if let Some(restore) = &config.restore {
        if !telemetry {
            problems.push(MeteringProblem::RestoreWithoutTelemetry);
        }
        if restore.timeout().is_zero() {
            problems.push(MeteringProblem::ZeroRestoreTimeout);
        }
    }
    ResolvedMetering {
        budgets,
        restore: config.restore,
    }
}

fn spec(
    agent: &str,
    meter: usize,
    config: MeterConfig,
    problems: &mut Vec<MeteringProblem>,
) -> MeterSpec {
    let (limit, period, spec) = match config {
        MeterConfig::Fixed { limit, period } => (
            limit,
            period.0,
            MeterSpec::Fixed {
                limit: Tokens(limit),
                period: period.0,
            },
        ),
        MeterConfig::Rolling { limit, period } => (
            limit,
            period.0,
            MeterSpec::Rolling {
                limit: Tokens(limit),
                period: period.0,
            },
        ),
        MeterConfig::Session { limit, length } => (
            limit,
            length.0,
            MeterSpec::Session {
                limit: Tokens(limit),
                length: length.0,
            },
        ),
        MeterConfig::Credit {
            capacity,
            refill,
            per,
            initial,
        } => {
            let initial = match initial {
                Initial::Full(Full::Full) => capacity,
                Initial::Tokens(tokens) => tokens,
            };
            if initial > capacity {
                problems.push(MeteringProblem::InitialAboveCapacity {
                    agent: agent.to_owned(),
                    meter,
                });
            }
            (
                capacity.min(refill),
                per.0,
                MeterSpec::Credit {
                    capacity: Tokens(capacity),
                    refill: Tokens(refill),
                    per: per.0,
                    initial: Tokens(initial),
                },
            )
        }
    };
    if limit == 0 {
        problems.push(MeteringProblem::ZeroLimit {
            agent: agent.to_owned(),
            meter,
        });
    }
    if period < MIN_PERIOD {
        problems.push(MeteringProblem::PeriodTooShort {
            agent: agent.to_owned(),
            meter,
        });
    }
    if period > MAX_PERIOD {
        problems.push(MeteringProblem::PeriodTooLong {
            agent: agent.to_owned(),
            meter,
        });
    }
    spec
}

pub(crate) fn build(
    resolved: &ResolvedMetering,
    now: dekopon_model_token_governor::Clock,
) -> Metering {
    let since = now();
    let budgets = resolved
        .budgets
        .iter()
        .map(|budget| {
            Budget::new(
                budget.agent.clone(),
                budget.models.clone(),
                &budget.meters,
                since,
            )
        })
        .collect();
    let metering = Metering::new(budgets, now);
    if resolved.restore.is_some() {
        metering.begin_restore();
    }
    metering
}

impl ModelConfig {
    #[must_use]
    pub const fn backend(&self) -> &'static str {
        match self {
            Self::OpenaiCompatible { .. } => "openai-compatible",
            Self::Openrouter { .. } => "openrouter",
            Self::ChatgptSubscription { .. } => "codex",
            Self::Anthropic { .. } => "anthropic",
        }
    }

    pub(crate) fn output_reserve(&self) -> Tokens {
        let max_output = match self {
            Self::Openrouter {
                generation: Some(generation),
                ..
            } => generation
                .max_output_tokens
                .map(|tokens| u64::from(tokens.get())),
            Self::Openrouter { .. }
            | Self::OpenaiCompatible { .. }
            | Self::ChatgptSubscription { .. }
            | Self::Anthropic { .. } => None,
        };
        Estimate::output_reserve(
            max_output,
            self.classes().iter().any(|class| class == "reasoning"),
        )
    }
}

pub(crate) struct ChatProviderGovernor {
    inner: SharedModel,
    metering: Arc<Metering>,
    call: Call,
    reserve: Tokens,
    hint: InputHint,
    seen: AtomicUsize,
}

impl ChatProviderGovernor {
    pub(crate) fn wrap(
        inner: SharedModel,
        metering: &Arc<Metering>,
        agent: &AgentId,
        model: &ModelConfig,
    ) -> SharedModel {
        Arc::new(Self {
            inner,
            metering: Arc::clone(metering),
            call: Call {
                agent: agent.clone(),
                model: model.name().to_owned(),
                backend: model.backend(),
                via: Via::Agent,
            },
            reserve: model.output_reserve(),
            hint: InputHint::default(),
            seen: AtomicUsize::new(0),
        })
    }
}

impl ChatModel for ChatProviderGovernor {
    fn complete(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
        on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
    ) -> Result<AssistantTurn, InferenceError> {
        let seen = self.seen.swap(messages.len(), Ordering::Relaxed);
        let fresh = messages.get(seen..).unwrap_or(messages);
        let estimate = Estimate::from_sizes(
            Sizes {
                bytes: json_bytes(messages).saturating_add(json_bytes(tools)),
                images: images(messages),
            },
            Some((
                &self.hint,
                Sizes {
                    bytes: json_bytes(fresh),
                    images: images(fresh),
                },
            )),
            self.reserve,
        );
        let admission = self
            .metering
            .admit(self.call.clone(), estimate)
            .map_err(InferenceError::OverBudget)?;
        let result = self.inner.complete(messages, tools, options, &mut |event| {
            if let TurnEvent::TextDelta(text) = &event {
                admission.observe_text(text.len());
            }
            on_event(event)
        });
        if let Ok(AssistantTurn {
            usage: Some(usage), ..
        }) = &result
        {
            admission.observe_usage(*usage);
            self.hint.observe(usage);
        }
        admission.settle(outcome(&result));
        result
    }
}

fn outcome(result: &Result<AssistantTurn, InferenceError>) -> Outcome {
    match result {
        Ok(_) => Outcome::Succeeded,
        Err(
            InferenceError::InvalidRequest(_)
            | InferenceError::Authentication(_)
            | InferenceError::Attachment(_)
            | InferenceError::RateLimited(_)
            | InferenceError::OverBudget(_),
        ) => Outcome::NotSent,
        Err(InferenceError::Cancelled) => Outcome::Cancelled,
        Err(
            InferenceError::DeadlineExceeded
            | InferenceError::Transport(_)
            | InferenceError::Protocol(_)
            | InferenceError::Provider(_),
        ) => Outcome::Failed,
    }
}

struct ByteCount(usize);

impl std::io::Write for ByteCount {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Image and file bytes serialize as a short summary, so this counts text and structure only.
fn json_bytes<T: serde::Serialize + ?Sized>(value: &T) -> usize {
    let mut count = ByteCount(0);
    match serde_json::to_writer(&mut count, value) {
        Ok(()) => count.0,
        Err(error) => {
            tracing::debug!(target: "meter", error = %error, "request size estimate fell back to zero");
            0
        }
    }
}

fn images(messages: &[ModelMessage]) -> usize {
    messages
        .iter()
        .filter_map(ModelMessage::parts)
        .flatten()
        .filter(|part| match part {
            ContentPart::Image { .. } => true,
            ContentPart::Text(_) | ContentPart::File { .. } => false,
        })
        .count()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicI64;

    use dekopon_model::error::{ProtocolFailure, RequestError};
    use dekopon_model_token_governor::{ModelUsage, Retry, UnixMillis};
    use dekopon_test_support::{OPENAI_CHAT_COMPLETIONS_TWO_DELTAS, ScriptedStreamModel};

    use super::*;

    const LIMIT: u64 = 100_000;

    fn metering(limit: u64) -> Arc<Metering> {
        let clock = Arc::new(AtomicI64::new(1_000_000));
        let budget = Budget::new(
            "reviewer".parse().unwrap(),
            None,
            &[MeterSpec::Rolling {
                limit: Tokens(limit),
                period: Duration::from_secs(5 * 3_600),
            }],
            UnixMillis(0),
        );
        Arc::new(Metering::new(
            vec![budget],
            Arc::new(move || UnixMillis(clock.load(Ordering::Relaxed))),
        ))
    }

    fn model() -> ModelConfig {
        serde_json::from_value(serde_json::json!({"kind":"openaiCompatible", "name":"local", "endpoint":"http://127.0.0.1:9", "model":"fixture", "timeoutMs":2000})).unwrap()
    }

    fn used(metering: &Metering) -> u64 {
        metering.statuses(&"reviewer".parse().unwrap()).unwrap()[0]
            .1
            .used
            .0
    }

    fn governed(inner: SharedModel, metering: &Arc<Metering>) -> SharedModel {
        ChatProviderGovernor::wrap(inner, metering, &"reviewer".parse().unwrap(), &model())
    }

    fn turn(usage: Option<ModelUsage>) -> AssistantTurn {
        AssistantTurn::new(Some("one two".to_owned()), Vec::new(), usage)
    }

    fn streaming(usage: Option<ModelUsage>) -> Arc<ScriptedStreamModel> {
        let model = Arc::new(
            ScriptedStreamModel::from_transcript(OPENAI_CHAT_COMPLETIONS_TWO_DELTAS, turn(usage))
                .unwrap(),
        );
        for _ in 0..8 {
            model.release_next();
        }
        model
    }

    fn ask(model: &SharedModel) -> Result<AssistantTurn, InferenceError> {
        model.complete(
            &[ModelMessage::user("x".repeat(400))],
            &[],
            &CompletionOptions::default(),
            &mut |_| ControlFlow::Continue(()),
        )
    }

    #[test]
    fn a_refused_call_never_reaches_the_inner_model() {
        let metering = metering(10);
        let inner = streaming(None);
        let error = ask(&governed(Arc::clone(&inner) as SharedModel, &metering)).unwrap_err();
        assert!(matches!(
            error,
            InferenceError::OverBudget(ref refusal) if refusal.retry == Retry::Never
        ));
        assert_eq!(inner.emitted(), 0);
    }

    #[test]
    fn success_settles_to_the_reported_usage() {
        let metering = metering(LIMIT);
        let usage = ModelUsage {
            input_tokens: Some(700),
            output_tokens: Some(9),
            ..ModelUsage::default()
        };
        ask(&governed(streaming(Some(usage)), &metering)).unwrap();
        assert_eq!(used(&metering), 709);
    }

    #[test]
    fn usage_none_settles_the_estimate_and_the_streamed_text() {
        let metering = metering(LIMIT);
        ask(&governed(streaming(None), &metering)).unwrap();
        let input = Estimate::from_sizes(
            Sizes {
                bytes: json_bytes(&[ModelMessage::user("x".repeat(400))])
                    + json_bytes::<[ModelTool]>(&[]),
                images: 0,
            },
            None,
            Tokens(0),
        )
        .input
        .0;
        assert_eq!(
            used(&metering),
            input + Tokens::from_bytes("Echoed hello.".len()).0
        );
    }

    #[test]
    fn a_cancelled_stream_charges_the_text_it_already_streamed() {
        let metering = metering(LIMIT);
        let model = governed(streaming(None), &metering);
        let error = model
            .complete(
                &[ModelMessage::user("hi")],
                &[],
                &CompletionOptions::default(),
                &mut |_| ControlFlow::Break(()),
            )
            .unwrap_err();
        assert!(matches!(error, InferenceError::Cancelled));
        let streamed = Tokens::from_bytes("Echoed ".len()).0;
        assert!(
            used(&metering) > streamed,
            "the input estimate is charged too"
        );
    }

    struct Failing(fn() -> InferenceError);

    impl ChatModel for Failing {
        fn complete(
            &self,
            _: &[ModelMessage],
            _: &[ModelTool],
            _: &CompletionOptions,
            _: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        ) -> Result<AssistantTurn, InferenceError> {
            Err((self.0)())
        }
    }

    #[test]
    fn each_failure_charges_per_whether_the_request_was_sent() {
        let not_sent: [fn() -> InferenceError; 2] = [
            || InferenceError::InvalidRequest(RequestError::EmptyModel),
            || InferenceError::InvalidRequest(RequestError::ZeroTimeout),
        ];
        for failure in not_sent {
            let metering = metering(LIMIT);
            ask(&governed(Arc::new(Failing(failure)), &metering)).unwrap_err();
            assert_eq!(used(&metering), 0);
        }
        let sent: [fn() -> InferenceError; 3] = [
            || InferenceError::DeadlineExceeded,
            || InferenceError::Cancelled,
            || InferenceError::Protocol(ProtocolFailure::MissingTerminal),
        ];
        for failure in sent {
            let metering = metering(LIMIT);
            ask(&governed(Arc::new(Failing(failure)), &metering)).unwrap_err();
            assert!(used(&metering) >= 100, "the input estimate is charged");
        }
    }

    fn parse(yaml: &str) -> Result<MeteringConfig, serde_yaml::Error> {
        serde_yaml::from_str(yaml)
    }

    #[test]
    fn every_meter_kind_parses() {
        let config = parse(
            "budgets:\n  reviewer:\n    models: [local]\n    meters:\n    - {kind: rolling, limit: 200000, period: 5h}\n    - {kind: fixed, limit: 1000000, period: 1d}\n    - {kind: session, limit: 300000, length: 5h}\n    - {kind: credit, capacity: 2000000, refill: 500000, per: 1d, initial: full}\n    - {kind: credit, capacity: 20, refill: 5, per: 1h, initial: 3}\n",
        )
        .unwrap();
        let mut problems = Vec::new();
        let resolved = resolve(
            Some(config),
            &BTreeSet::from(["reviewer".to_owned()]),
            &BTreeSet::from(["local".to_owned()]),
            false,
            &mut problems,
        );
        assert_eq!(problems, []);
        assert_eq!(
            resolved.budgets[0].meters[3],
            MeterSpec::Credit {
                capacity: Tokens(2_000_000),
                refill: Tokens(500_000),
                per: Duration::from_secs(86_400),
                initial: Tokens(2_000_000),
            }
        );
        assert_eq!(resolved.budgets[0].meters.len(), 5);
        let restore = parse(
            "restore: {kind: quickwit, endpoint: 'http://quickwit:7280', index: otel-logs-v0_9, delay: 45s, timeout: 3s}",
        )
        .unwrap()
        .restore
        .unwrap();
        assert_eq!(restore.delay(), Duration::from_secs(45));
        assert_eq!(restore.timeout(), Duration::from_secs(3));
        assert!(
            parse("restore: {kind: quickwit, endpoint: 'http://quickwit:7280', index: otel-logs-v0_9, delayMs: 30000}").is_err()
        );
        assert!(
            parse(
                "budgets: {reviewer: {meters: [{kind: rolling, limit: 1, period: 5h, extra: 1}]}}"
            )
            .is_err()
        );
        assert!(
            parse("budgets: {reviewer: {meters: [{kind: rolling, limit: 1, period: soon}]}}")
                .is_err()
        );
    }

    #[test]
    fn every_metering_problem_is_reported_at_once() {
        let config = parse(
            "budgets:\n  ghost:\n    meters: [{kind: rolling, limit: 10, period: 5h}]\n  idle:\n    models: []\n    meters: []\n  reviewer:\n    models: [nope]\n    meters:\n    - {kind: fixed, limit: 0, period: 1d}\n    - {kind: rolling, limit: 10, period: 30s}\n    - {kind: credit, capacity: 10, refill: 1, per: 1h, initial: 11}\n    - {kind: session, limit: 10, length: 400d}\nrestore: {kind: quickwit, endpoint: 'http://quickwit:7280', index: otel-logs-v0_9, timeout: 0s}\n",
        )
        .unwrap();
        let mut problems = Vec::new();
        resolve(
            Some(config),
            &BTreeSet::from(["idle".to_owned(), "reviewer".to_owned()]),
            &BTreeSet::from(["local".to_owned()]),
            false,
            &mut problems,
        );
        assert_eq!(
            problems,
            [
                MeteringProblem::UnknownAgent {
                    agent: "ghost".to_owned()
                },
                MeteringProblem::EmptyMeters {
                    agent: "idle".to_owned()
                },
                MeteringProblem::EmptyModels {
                    agent: "idle".to_owned()
                },
                MeteringProblem::UnknownModel {
                    agent: "reviewer".to_owned(),
                    model: "nope".to_owned()
                },
                MeteringProblem::ZeroLimit {
                    agent: "reviewer".to_owned(),
                    meter: 0
                },
                MeteringProblem::PeriodTooShort {
                    agent: "reviewer".to_owned(),
                    meter: 1
                },
                MeteringProblem::InitialAboveCapacity {
                    agent: "reviewer".to_owned(),
                    meter: 2
                },
                MeteringProblem::PeriodTooLong {
                    agent: "reviewer".to_owned(),
                    meter: 3
                },
                MeteringProblem::RestoreWithoutTelemetry,
                MeteringProblem::ZeroRestoreTimeout,
            ]
        );
    }
}
