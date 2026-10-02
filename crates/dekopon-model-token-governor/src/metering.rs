use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use dekopon_core::AgentId;
use parking_lot::Mutex;

use crate::{
    ModelUsage,
    budget::{Budget, Refusal, Reservation},
    meter::{Tokens, UnixMillis},
};

pub type Clock = Arc<dyn Fn() -> UnixMillis + Send + Sync>;

pub const IMAGE_TOKENS: u64 = 1_000;
pub const DEFAULT_OUTPUT_RESERVE: Tokens = Tokens(1_024);
pub const REASONING_OUTPUT_RESERVE: Tokens = Tokens(4_096);

pub struct Metering {
    budgets: HashMap<AgentId, Mutex<Budget>>,
    now: Clock,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Via {
    Agent,
    Proxy,
}

impl Via {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Proxy => "proxy",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Call {
    pub agent: AgentId,
    pub model: String,
    pub backend: &'static str,
    pub via: Via,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Estimate {
    pub input: Tokens,
    pub output_reserve: Tokens,
}

impl Estimate {
    #[must_use]
    pub fn from_sizes(
        request_bytes: usize,
        images: usize,
        hint: Option<(&InputHint, usize)>,
        reserve: Tokens,
    ) -> Self {
        let images = (images as u64).saturating_mul(IMAGE_TOKENS);
        let whole = Tokens::from_bytes(request_bytes).0.saturating_add(images);
        let hinted = hint.map_or(0, |(hint, new_bytes)| {
            hint.last().saturating_add(Tokens::from_bytes(new_bytes).0)
        });
        Self {
            input: Tokens(whole.max(hinted)),
            output_reserve: reserve,
        }
    }

    #[must_use]
    pub const fn output_reserve(max_output_tokens: Option<u64>, reasoning: bool) -> Tokens {
        match max_output_tokens {
            Some(tokens) => Tokens(tokens),
            None if reasoning => REASONING_OUTPUT_RESERVE,
            None => DEFAULT_OUTPUT_RESERVE,
        }
    }

    const fn total(self) -> Tokens {
        self.input.saturating_add(self.output_reserve)
    }
}

/// The last settled input of one session, so the next call's estimate covers history the request
/// bytes do not show (Codex's hidden items).
#[derive(Debug, Default)]
pub struct InputHint(AtomicU64);

impl InputHint {
    pub fn observe(&self, usage: &ModelUsage) {
        if let Some(input) = usage.input_tokens {
            self.0.store(input, Ordering::Relaxed);
        }
    }

    fn last(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Succeeded,
    Failed,
    Cancelled,
    NotSent,
}

impl Outcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed | Self::NotSent => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Default)]
struct Observed {
    usage: Option<ModelUsage>,
    text_bytes: usize,
}

/// Holds no lock guard, so it can live across an `.await`; dropping it unsettled settles it as
/// cancelled, which is the proxy's client-disconnect path.
#[must_use]
pub struct Admission {
    call: Call,
    estimate: Estimate,
    reservation: Option<Reservation>,
    observed: Mutex<Observed>,
    metering: Arc<Metering>,
    settled: bool,
}

impl Metering {
    #[must_use]
    pub fn new(budgets: Vec<Budget>, now: Clock) -> Self {
        Self {
            budgets: budgets
                .into_iter()
                .map(|budget| (budget.agent().clone(), Mutex::new(budget)))
                .collect(),
            now,
        }
    }

    #[must_use]
    pub fn system_clock() -> Clock {
        Arc::new(UnixMillis::now)
    }

    #[must_use]
    pub fn now(&self) -> UnixMillis {
        (self.now)()
    }

    #[must_use]
    pub fn has_budgets(&self) -> bool {
        !self.budgets.is_empty()
    }

    pub fn admit(self: &Arc<Self>, call: Call, estimate: Estimate) -> Result<Admission, Refusal> {
        let reservation = match self.budgets.get(&call.agent) {
            Some(budget) => {
                let mut budget = budget.lock();
                if budget.covers(&call.model) {
                    let now = self.now();
                    match budget.reserve(now, estimate.total()) {
                        Ok(reservation) => Some(reservation),
                        Err(refusal) => {
                            drop(budget);
                            record(&call, estimate, &Charge::refused());
                            return Err(refusal);
                        }
                    }
                } else {
                    None
                }
            }
            None => None,
        };
        Ok(Admission {
            call,
            estimate,
            reservation,
            observed: Mutex::new(Observed::default()),
            metering: Arc::clone(self),
            settled: false,
        })
    }

    /// Every budget appends its live charges until `restore` or `abandon_restore` runs.
    pub fn begin_restore(&self) {
        for budget in self.budgets.values() {
            budget.lock().begin_restore();
        }
    }

    pub fn abandon_restore(&self) {
        for budget in self.budgets.values() {
            budget.lock().abandon_restore();
        }
    }

    #[must_use]
    pub fn lookback(&self, now: UnixMillis) -> Duration {
        self.budgets
            .values()
            .map(|budget| budget.lock().horizon(now))
            .max()
            .unwrap_or_default()
    }

    pub fn restore(&self, since: UnixMillis, rows: &[HistoryRow]) {
        for (agent, budget) in &self.budgets {
            let mut budget = budget.lock();
            let history = rows
                .iter()
                .filter(|row| &row.agent == agent && budget.covers(&row.model))
                .map(|row| (row.at, row.tokens))
                .collect::<Vec<_>>();
            budget.restore(since, history.into_iter());
        }
    }

    #[must_use]
    pub fn statuses(&self, agent: &AgentId) -> Option<Vec<(crate::MeterKind, crate::MeterStatus)>> {
        let now = self.now();
        self.budgets
            .get(agent)
            .map(|budget| budget.lock().statuses(now))
    }

    fn settle(&self, call: &Call, reservation: Option<Reservation>, actual: Tokens) {
        let Some(budget) = self.budgets.get(&call.agent) else {
            return;
        };
        let now = self.now();
        let mut budget = budget.lock();
        match reservation {
            Some(reservation) => budget.settle(reservation, now, actual),
            None if budget.covers(&call.model) => budget.charge(now, actual),
            None => {}
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryRow {
    pub agent: AgentId,
    pub model: String,
    pub at: UnixMillis,
    pub tokens: Tokens,
}

impl Admission {
    pub fn observe_usage(&self, usage: ModelUsage) {
        let mut observed = self.observed.lock();
        observed.usage = Some(observed.usage.unwrap_or_default().merged(usage));
    }

    pub fn observe_text(&self, bytes: usize) {
        let mut observed = self.observed.lock();
        observed.text_bytes = observed.text_bytes.saturating_add(bytes);
    }

    pub fn settle(mut self, outcome: Outcome) {
        self.finish(outcome);
    }

    fn finish(&mut self, outcome: Outcome) {
        if self.settled {
            return;
        }
        self.settled = true;
        let charge = {
            let observed = self.observed.lock();
            Charge::of(outcome, self.estimate, &observed)
        };
        self.metering.settle(
            &self.call,
            self.reservation.take(),
            Tokens(charge.input.saturating_add(charge.output)),
        );
        record(&self.call, self.estimate, &charge);
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.finish(Outcome::Cancelled);
    }
}

struct Charge {
    input: u64,
    output: u64,
    usage: ModelUsage,
    source: &'static str,
    outcome: &'static str,
}

impl Charge {
    const fn refused() -> Self {
        Self {
            input: 0,
            output: 0,
            usage: ModelUsage {
                input_tokens: None,
                cached_input_tokens: None,
                cache_write_tokens: None,
                output_tokens: None,
                reasoning_output_tokens: None,
                total_tokens: None,
            },
            source: "estimated",
            outcome: "refused",
        }
    }

    fn of(outcome: Outcome, estimate: Estimate, observed: &Observed) -> Self {
        let usage = observed.usage.unwrap_or_default();
        if outcome == Outcome::NotSent {
            return Self {
                outcome: outcome.as_str(),
                usage,
                ..Self::refused()
            };
        }
        let source = match (usage.input_tokens, usage.output_tokens) {
            (Some(_), Some(_)) => "reported",
            (None, None) => "estimated",
            (Some(_), None) | (None, Some(_)) => "partial",
        };
        Self {
            input: usage.input_tokens.unwrap_or(estimate.input.0),
            output: usage
                .output_tokens
                .unwrap_or_else(|| Tokens::from_bytes(observed.text_bytes).0),
            usage,
            source,
            outcome: outcome.as_str(),
        }
    }
}

fn int(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn record(call: &Call, estimate: Estimate, charge: &Charge) {
    tracing::info!(
        target: "meter",
        {
        meter.schema = 1_i64,
        agent = call.agent.as_str(),
        model.name = call.model.as_str(),
        model.backend = call.backend,
        meter.via = call.via.as_str(),
        usage.input_tokens = int(charge.input),
        usage.output_tokens = int(charge.output),
        usage.cached_input_tokens = int(charge.usage.cached_input_tokens.unwrap_or(0)),
        usage.cache_write_tokens = int(charge.usage.cache_write_tokens.unwrap_or(0)),
        usage.reasoning_output_tokens = int(charge.usage.reasoning_output_tokens.unwrap_or(0)),
        usage.source = charge.source,
        meter.estimate.input_tokens = int(estimate.input.0),
        outcome = charge.outcome,
        },
        "model call charged"
    );
}
