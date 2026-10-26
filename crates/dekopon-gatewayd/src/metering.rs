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
    #[error("metering.budgets entry #{entry} names an agent no route serves")]
    UnknownAgent { entry: usize },
    #[error("metering.budgets entry #{entry}{} names an unknown model at models[{model}]", agent.as_ref().map_or_else(String::new, |agent| format!(" (agent {agent})")))]
    UnknownModel {
        entry: usize,
        agent: Option<AgentId>,
        model: usize,
    },
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
    for (index, (agent, budget)) in config.budgets.into_iter().enumerate() {
        let id = agent
            .parse::<AgentId>()
            .ok()
            .filter(|_| agents.contains(&agent));
        if id.is_none() {
            problems.push(MeteringProblem::UnknownAgent { entry: index + 1 });
        }
        // Every validation branch uses the position for an unserved entry, never its raw key.
        let label = id
            .as_ref()
            .map_or_else(|| format!("entry #{}", index + 1), |_| agent.clone());
        if budget.meters.is_empty() {
            problems.push(MeteringProblem::EmptyMeters {
                agent: label.clone(),
            });
        }
        if budget.models.as_ref().is_some_and(Vec::is_empty) {
            problems.push(MeteringProblem::EmptyModels {
                agent: label.clone(),
            });
        }
        for (model, name) in budget.models.iter().flatten().enumerate() {
            if !models.contains(name) {
                problems.push(MeteringProblem::UnknownModel {
                    entry: index + 1,
                    agent: id.clone(),
                    model,
                });
            }
        }
        let meters = budget
            .meters
            .iter()
            .enumerate()
            .map(|(meter, config)| spec(&label, meter, *config, problems))
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
    fn model_key(&self) -> dekopon_model::model::ModelKey {
        self.inner.model_key()
    }

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
                bytes: json_bytes(messages).saturating_add(serialized_bytes(tools)),
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
            match &event {
                TurnEvent::TextDelta(text) => admission.observe_text(text.len()),
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
        match &result {
            Ok(_) => admission.settle(outcome(&result)),
            Err(error) => admission.settle_with_kind(outcome(&result), error.kind().as_str()),
        }
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

struct MeteredMessages<'a>(&'a [ModelMessage]);

impl serde::Serialize for MeteredMessages<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq as _;
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for message in self.0 {
            sequence.serialize_element(&message.for_metering())?;
        }
        sequence.end()
    }
}

fn json_bytes(messages: &[ModelMessage]) -> usize {
    serialized_bytes(&MeteredMessages(messages))
}

fn serialized_bytes<T: serde::Serialize + ?Sized>(value: &T) -> usize {
    let mut count = ByteCount(0);
    match serde_json::to_writer(&mut count, value) {
        Ok(()) => count.0,
        Err(error) => {
            tracing::debug!(target: "meter", error = %error, "meter.estimate.fallback");
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
    use dekopon_test_support::{
        CaptureLayer, OPENAI_CHAT_COMPLETIONS_TWO_DELTAS, ScriptedStreamModel,
    };
    use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

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
            ScriptedStreamModel::from_transcript(
                OPENAI_CHAT_COMPLETIONS_TWO_DELTAS,
                dekopon_model::TranscriptDialect::ChatCompletions,
                turn(usage),
            )
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
    fn reading_images_does_not_change_metered_json_bytes() {
        let data: dekopon_model::asset::BlobReference =
            dekopon_model::asset::DiskBlob::from_bytes(b"pixels")
                .unwrap()
                .into();
        let image = ContentPart::Image {
            mime: "image/png".to_owned(),
            data: data.clone(),
        };
        let messages = [ModelMessage::user_with_parts(vec![
            image.clone(),
            image,
            ContentPart::File {
                name: "pixels.bin".to_owned(),
                mime: "application/octet-stream".to_owned(),
                data: data.clone(),
            },
            ContentPart::Text(format!(" sha256:{}", "0".repeat(64))),
        ])];
        let before = json_bytes(&messages);
        assert_eq!(before, serialized_bytes(&messages));
        data.read().unwrap();
        assert_eq!(serialized_bytes(&messages), before + 2 * 72);
        assert_eq!(json_bytes(&messages), before);
    }

    #[tokio::test]
    async fn concurrent_digest_initialization_does_not_change_metered_json_bytes() {
        use dekopon_model::asset::{BlobError, BlobReference, BlobSource, DiskBlob};
        use std::sync::mpsc;

        struct PausedRead {
            blob: DiskBlob,
            entered: mpsc::SyncSender<()>,
            resume: parking_lot::Mutex<mpsc::Receiver<()>>,
        }
        impl BlobSource for PausedRead {
            fn pin(&self) -> Result<DiskBlob, BlobError> {
                Ok(self.blob.clone())
            }
            fn read(&self) -> Result<Vec<u8>, BlobError> {
                self.entered.send(()).unwrap();
                self.resume
                    .lock()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
                self.blob.read()
            }
        }
        struct InitializeDuringText {
            count: ByteCount,
            initialize: Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>,
        }
        impl std::io::Write for InitializeDuringText {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if bytes.len() >= 65_536
                    && let Some((resume, initialized)) = self.initialize.take()
                {
                    resume.send(()).unwrap();
                    initialized.recv_timeout(Duration::from_secs(10)).unwrap();
                }
                std::io::Write::write(&mut self.count, bytes)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                std::io::Write::flush(&mut self.count)
            }
        }
        let (entered, started) = mpsc::sync_channel(1);
        let (resume, resumed) = mpsc::sync_channel(1);
        let (initialized, completed) = mpsc::sync_channel(1);
        let data = BlobReference::new(
            Arc::new(PausedRead {
                blob: DiskBlob::from_bytes(b"pixels").unwrap(),
                entered,
                resume: parking_lot::Mutex::new(resumed),
            }),
            6,
            1,
        );
        let messages = [
            ModelMessage::user("history"),
            ModelMessage::user_with_parts(vec![
                ContentPart::Text("x".repeat(65_536)),
                ContentPart::Image {
                    mime: "image/png".into(),
                    data: data.clone(),
                },
            ]),
        ];
        let whole_before = json_bytes(&messages);
        let fresh_before = json_bytes(&messages[1..]);
        assert_eq!(serialized_bytes(&messages), whole_before);
        let read = tokio::task::spawn_blocking(move || {
            let bytes = data.read().unwrap();
            initialized.send(()).unwrap();
            bytes
        });
        started.recv_timeout(Duration::from_secs(10)).unwrap();
        read.abort();
        let mut writer = InitializeDuringText {
            count: ByteCount(0),
            initialize: Some((resume, completed)),
        };
        serde_json::to_writer(&mut writer, &MeteredMessages(&messages)).unwrap();
        assert!(writer.initialize.is_none());
        assert_eq!(writer.count.0, whole_before);
        assert_eq!(json_bytes(&messages), whole_before);
        assert_eq!(json_bytes(&messages[1..]), fresh_before);
        assert_eq!(serialized_bytes(&messages), whole_before + 72);
        assert_eq!(read.await.unwrap(), b"pixels");
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
                    + serialized_bytes::<[ModelTool]>(&[]),
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

    #[test]
    fn returned_model_failures_charge_one_typed_meter_record() {
        type FailureCase = (
            fn() -> InferenceError,
            &'static str,
            &'static str,
            &'static str,
            u64,
        );
        let estimated_input = Estimate::from_sizes(
            Sizes {
                bytes: json_bytes(&[ModelMessage::user("x".repeat(400))])
                    + serialized_bytes::<[ModelTool]>(&[]),
                images: 0,
            },
            None,
            Tokens(0),
        )
        .input
        .0;
        let cases: [FailureCase; 2] = [
            (
                || InferenceError::Cancelled,
                "cancelled",
                "cancelled",
                "estimated",
                estimated_input,
            ),
            (
                || InferenceError::InvalidRequest(RequestError::EmptyModel),
                "invalid-request",
                "failed",
                "estimated",
                0,
            ),
        ];
        for (failure, kind, outcome, source, expected_input) in cases {
            let capture = CaptureLayer::new();
            let _guard = tracing_subscriber::registry()
                .with(capture.clone())
                .set_default();
            let metering = metering(LIMIT);
            let error = ask(&governed(Arc::new(Failing(failure)), &metering)).unwrap_err();
            assert_eq!(error.kind().as_str(), kind);
            let records = capture.events_where("meter", &[("message", "model call charged")]);
            assert_eq!(records.len(), 1);
            let charge = &records[0];
            assert_eq!(charge.field("error.kind"), Some(kind));
            assert_eq!(charge.field("outcome"), Some(outcome));
            assert_eq!(charge.field("usage.source"), Some(source));
            assert_eq!(charge.field("meter.schema"), Some("1"));
            let input = expected_input.to_string();
            assert_eq!(charge.field("usage.input_tokens"), Some(input.as_str()));
            assert_eq!(charge.field("usage.output_tokens"), Some("0"));
        }
    }

    struct TimedOutModel;

    impl ChatModel for TimedOutModel {
        fn complete(
            &self,
            _: &[ModelMessage],
            _: &[ModelTool],
            _: &CompletionOptions,
            _: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        ) -> Result<AssistantTurn, InferenceError> {
            let (_sender, receiver) = std::sync::mpsc::channel::<()>();
            match receiver.recv_timeout(Duration::from_millis(10)) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    Err(InferenceError::DeadlineExceeded)
                }
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    unreachable!("sender remains held until return")
                }
            }
        }
    }

    #[test]
    fn a_timed_out_model_call_charges_its_deadline_kind() {
        let capture = CaptureLayer::new();
        let _guard = tracing_subscriber::registry()
            .with(capture.clone())
            .set_default();
        let metering = metering(LIMIT);
        let error = ask(&governed(Arc::new(TimedOutModel), &metering)).unwrap_err();
        assert!(matches!(error, InferenceError::DeadlineExceeded));
        let charges = capture.events_where("meter", &[("message", "model call charged")]);
        assert_eq!(charges.len(), 1);
        assert_eq!(charges[0].field("error.kind"), Some("deadline-exceeded"));
        assert_eq!(charges[0].field("outcome"), Some("failed"));
        assert_eq!(charges[0].field("usage.source"), Some("estimated"));
        assert_eq!(
            charges[0].field("usage.input_tokens"),
            Some(used(&metering).to_string().as_str())
        );
        assert_eq!(charges[0].field("usage.output_tokens"), Some("0"));
    }

    #[test]
    fn success_and_pre_admission_refusal_have_no_error_kind() {
        let capture = CaptureLayer::new();
        let _guard = tracing_subscriber::registry()
            .with(capture.clone())
            .set_default();
        let allowed = metering(LIMIT);
        ask(&governed(
            streaming(Some(ModelUsage {
                input_tokens: Some(700),
                output_tokens: Some(9),
                ..ModelUsage::default()
            })),
            &allowed,
        ))
        .unwrap();
        let refused = metering(10);
        let error = ask(&governed(streaming(None), &refused)).unwrap_err();
        assert!(matches!(error, InferenceError::OverBudget(_)));
        let records = capture.events_where("meter", &[("message", "model call charged")]);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].field("error.kind"), None);
        assert_eq!(records[0].field("outcome"), Some("succeeded"));
        assert_eq!(records[0].field("usage.source"), Some("reported"));
        assert_eq!(records[0].field("usage.input_tokens"), Some("700"));
        assert_eq!(records[0].field("usage.output_tokens"), Some("9"));
        assert_eq!(records[1].field("error.kind"), None);
        assert_eq!(records[1].field("outcome"), Some("refused"));
        assert_eq!(records[1].field("usage.source"), Some("estimated"));
        assert_eq!(records[1].field("usage.input_tokens"), Some("0"));
        assert_eq!(records[1].field("usage.output_tokens"), Some("0"));
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
                MeteringProblem::UnknownAgent { entry: 1 },
                MeteringProblem::EmptyMeters {
                    agent: "idle".to_owned()
                },
                MeteringProblem::EmptyModels {
                    agent: "idle".to_owned()
                },
                MeteringProblem::UnknownModel {
                    entry: 3,
                    agent: Some("reviewer".parse().unwrap()),
                    model: 0
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
