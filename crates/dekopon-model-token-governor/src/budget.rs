use std::{collections::BTreeSet, fmt, time::Duration};

use dekopon_core::AgentId;

use crate::meter::{Meter, MeterKind, MeterSpec, Tokens, UnixMillis, Verdict, grouped};

#[derive(Clone, Debug)]
pub struct Budget {
    agent: AgentId,
    models: Option<BTreeSet<String>>,
    meters: Vec<Meter>,
    reserved: Tokens,
    live_since_boot: Option<Vec<(UnixMillis, Tokens)>>,
}

#[must_use]
#[derive(Debug)]
pub struct Reservation {
    tokens: Tokens,
}

impl Budget {
    #[must_use]
    pub fn new(
        agent: AgentId,
        models: Option<BTreeSet<String>>,
        specs: &[MeterSpec],
        now: UnixMillis,
    ) -> Self {
        Self {
            agent,
            models,
            meters: specs.iter().map(|spec| Meter::new(*spec, now)).collect(),
            reserved: Tokens(0),
            live_since_boot: None,
        }
    }

    #[must_use]
    pub const fn agent(&self) -> &AgentId {
        &self.agent
    }

    #[must_use]
    pub fn covers(&self, model: &str) -> bool {
        self.models
            .as_ref()
            .is_none_or(|models| models.contains(model))
    }

    pub fn reserve(&mut self, now: UnixMillis, estimate: Tokens) -> Result<Reservation, Refusal> {
        if let Some(refusal) = self.explain(now, estimate) {
            return Err(refusal);
        }
        self.reserved = self.reserved.saturating_add(estimate);
        Ok(Reservation { tokens: estimate })
    }

    pub fn settle(&mut self, reservation: Reservation, at: UnixMillis, actual: Tokens) {
        self.reserved = Tokens(self.reserved.0.saturating_sub(reservation.tokens.0));
        self.charge(at, actual);
    }

    pub(crate) fn charge(&mut self, at: UnixMillis, actual: Tokens) {
        for meter in &mut self.meters {
            meter.charge(at, actual);
        }
        if actual.0 > 0
            && let Some(live) = self.live_since_boot.as_mut()
        {
            live.push((at, actual));
        }
    }

    /// The meter with the longest wait names the refusal, and a meter the request can never fit
    /// outranks every wait.
    #[must_use]
    pub fn explain(&self, now: UnixMillis, want: Tokens) -> Option<Refusal> {
        let mut worst: Option<(&Meter, Retry)> = None;
        for meter in &self.meters {
            let retry = match meter.verdict(now, self.reserved, want) {
                Verdict::Allow => continue,
                Verdict::Never => Retry::Never,
                Verdict::Wait(wait) => Retry::After(wait),
            };
            let worse = match (&worst, retry) {
                (None, _) => true,
                (Some((_, Retry::Never)), _) => false,
                (Some((_, Retry::After(_))), Retry::Never) => true,
                (Some((_, Retry::After(current))), Retry::After(wait)) => wait > *current,
            };
            if worse {
                worst = Some((meter, retry));
            }
        }
        worst.map(|(meter, retry)| {
            let status = meter.status(now);
            Refusal {
                agent: self.agent.clone(),
                meter: meter.spec().kind(),
                limit: status.limit,
                remaining: status.remaining,
                requested: want,
                retry,
            }
        })
    }

    #[must_use]
    pub fn statuses(&self, now: UnixMillis) -> Vec<(MeterKind, crate::MeterStatus)> {
        self.meters
            .iter()
            .map(|meter| (meter.spec().kind(), meter.status(now)))
            .collect()
    }

    #[must_use]
    pub fn horizon(&self, now: UnixMillis) -> Duration {
        self.meters
            .iter()
            .map(|meter| meter.horizon(now))
            .max()
            .unwrap_or_default()
    }

    pub(crate) fn begin_restore(&mut self) {
        self.live_since_boot = Some(Vec::new());
    }

    pub(crate) fn abandon_restore(&mut self) {
        self.live_since_boot = None;
    }

    /// Builds fresh meters from `history`, which starts at `since`, then replays the charges seen
    /// live since boot on top; reservations stay on the budget, so calls in flight are unaffected.
    pub fn restore(
        &mut self,
        since: UnixMillis,
        history: impl Iterator<Item = (UnixMillis, Tokens)>,
    ) {
        let mut charges = history.collect::<Vec<_>>();
        charges.sort_by_key(|(at, _)| *at);
        let mut meters = self
            .meters
            .iter()
            .map(|meter| Meter::new(*meter.spec(), since))
            .collect::<Vec<_>>();
        let live = self.live_since_boot.take().unwrap_or_default();
        for (at, spent) in charges.into_iter().chain(live) {
            for meter in &mut meters {
                meter.charge(at, spent);
            }
        }
        self.meters = meters;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Retry {
    After(Duration),
    Never,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Refusal {
    pub agent: AgentId,
    pub meter: MeterKind,
    pub limit: Tokens,
    pub remaining: i64,
    pub requested: Tokens,
    pub retry: Retry,
}

impl Refusal {
    #[must_use]
    pub const fn retry_after(&self) -> Option<Duration> {
        match self.retry {
            Retry::After(wait) => Some(wait),
            Retry::Never => None,
        }
    }

    #[must_use]
    pub const fn fits_ever(&self) -> bool {
        match self.retry {
            Retry::After(_) => true,
            Retry::Never => false,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let window = WindowName(self.meter);
        match self.retry {
            Retry::Never => write!(
                formatter,
                "This message needs about {} tokens, more than my whole {}-token budget ({window}). It can't run as is.",
                self.requested, self.limit
            ),
            Retry::After(wait) if self.remaining < 0 => write!(
                formatter,
                "I'm over my token budget ({window}) by {} tokens. It resets in {}.",
                grouped(-i128::from(self.remaining)),
                Rounded(wait)
            ),
            Retry::After(wait) => {
                let used = i128::from(self.limit.0) - i128::from(self.remaining);
                let percent = (used * 100)
                    .checked_div(i128::from(self.limit.0))
                    .unwrap_or(100)
                    .clamp(0, 100);
                write!(
                    formatter,
                    "I'm at {percent}% of my token budget ({window}): {} tokens left, this message needs about {}. Try again in {}.",
                    grouped(i128::from(self.remaining)),
                    self.requested,
                    Rounded(wait)
                )
            }
        }
    }
}

struct WindowName(MeterKind);

impl fmt::Display for WindowName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            MeterKind::Fixed { period } => match period.as_secs() {
                3_600 => formatter.write_str("hourly window"),
                86_400 => formatter.write_str("daily window"),
                604_800 => formatter.write_str("weekly window"),
                _ => write!(formatter, "{} window", Span(period)),
            },
            MeterKind::Rolling { period } => write!(formatter, "{} rolling window", Span(period)),
            MeterKind::Session { length } => write!(formatter, "{} session window", Span(length)),
            MeterKind::Credit => formatter.write_str("credit bucket"),
        }
    }
}

const UNITS: [(u64, &str); 4] = [
    (86_400, "day"),
    (3_600, "hour"),
    (60, "minute"),
    (1, "second"),
];

/// A window length as an adjective, "5-hour", in its coarsest exact unit.
struct Span(Duration);

impl fmt::Display for Span {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seconds = self.0.as_secs().max(1);
        let (size, unit) = UNITS
            .into_iter()
            .find(|(size, _)| seconds.is_multiple_of(*size))
            .unwrap_or((1, "second"));
        write!(formatter, "{}-{unit}", seconds / size)
    }
}

/// A wait rounded up to the coarsest whole unit it reaches: "15 minutes", "3 hours".
struct Rounded(Duration);

impl fmt::Display for Rounded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seconds = self.0.as_secs() + u64::from(self.0.subsec_nanos() > 0);
        let seconds = seconds.max(1);
        let mut chosen = (seconds, "second");
        for (index, (size, unit)) in UNITS.into_iter().enumerate() {
            if seconds >= size {
                let count = seconds.div_ceil(size);
                chosen = match index.checked_sub(1).map(|coarser| UNITS[coarser]) {
                    Some((coarser, coarser_unit)) if count * size == coarser => (1, coarser_unit),
                    Some(_) | None => (count, unit),
                };
                break;
            }
        }
        let (count, unit) = chosen;
        let plural = if count == 1 { "" } else { "s" };
        write!(formatter, "{count} {unit}{plural}")
    }
}
