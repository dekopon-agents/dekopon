use std::{
    collections::VecDeque,
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Tokens(pub u64);

impl Tokens {
    #[must_use]
    pub const fn from_bytes(bytes: usize) -> Self {
        Self((bytes as u64).div_ceil(4))
    }

    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }
}

impl fmt::Display for Tokens {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&grouped(i128::from(self.0)))
    }
}

pub(crate) fn grouped(value: i128) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if value < 0 {
        out.push('-');
    }
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UnixMillis(pub i64);

impl UnixMillis {
    #[must_use]
    pub fn now() -> Self {
        Self(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, millis),
        )
    }

    #[must_use]
    pub const fn saturating_sub(self, duration: Duration) -> Self {
        Self(self.0.saturating_sub(millis(duration)))
    }
}

const fn millis(duration: Duration) -> i64 {
    let value = duration.as_millis();
    if value > i64::MAX as u128 {
        i64::MAX
    } else {
        value as i64
    }
}

const fn positive_millis(duration: Duration) -> i64 {
    let value = millis(duration);
    if value < 1 { 1 } else { value }
}

fn wait(from: i64, until: i64) -> Duration {
    Duration::from_millis(u64::try_from(until.saturating_sub(from)).unwrap_or(0))
}

/// A meter that refuses only because concurrent reservations hold its room asks for this wait;
/// those calls settle within a turn.
const RESERVATION_WAIT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MeterSpec {
    Fixed {
        limit: Tokens,
        period: Duration,
    },
    Rolling {
        limit: Tokens,
        period: Duration,
    },
    Session {
        limit: Tokens,
        length: Duration,
    },
    Credit {
        capacity: Tokens,
        refill: Tokens,
        per: Duration,
        initial: Tokens,
    },
}

impl MeterSpec {
    #[must_use]
    pub const fn kind(&self) -> MeterKind {
        match *self {
            Self::Fixed { period, .. } => MeterKind::Fixed { period },
            Self::Rolling { period, .. } => MeterKind::Rolling { period },
            Self::Session { length, .. } => MeterKind::Session { length },
            Self::Credit { .. } => MeterKind::Credit,
        }
    }

    #[must_use]
    pub const fn limit(&self) -> Tokens {
        match *self {
            Self::Fixed { limit, .. }
            | Self::Rolling { limit, .. }
            | Self::Session { limit, .. } => limit,
            Self::Credit { capacity, .. } => capacity,
        }
    }

    fn horizon(&self, now: i64) -> Duration {
        match *self {
            Self::Fixed { period, .. } => {
                let period = positive_millis(period);
                wait(now.div_euclid(period) * period, now)
            }
            Self::Rolling { period, .. } => period.saturating_add(rolling_bucket(period)),
            Self::Session { length, .. } => length.saturating_mul(2),
            Self::Credit {
                capacity,
                refill,
                per,
                ..
            } => {
                let scaled = i128::from(capacity.0) * i128::from(positive_millis(per));
                let millis = scaled / i128::from(refill.0.max(1));
                Duration::from_millis(u64::try_from(millis).unwrap_or(u64::MAX))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MeterKind {
    Fixed { period: Duration },
    Rolling { period: Duration },
    Session { length: Duration },
    Credit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    Allow,
    Wait(Duration),
    Never,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MeterStatus {
    pub limit: Tokens,
    pub used: Tokens,
    pub remaining: i64,
}

#[derive(Clone, Debug)]
enum State {
    Fixed {
        period: i64,
        window: i64,
        used: u64,
    },
    Rolling {
        period: Duration,
        buckets: VecDeque<(i64, u64)>,
    },
    Session {
        length: i64,
        open: Option<(i64, u64)>,
    },
    Credit {
        capacity: u64,
        refill: u64,
        per: i128,
        scaled: i128,
        at: i64,
    },
}

#[derive(Clone, Debug)]
pub struct Meter {
    spec: MeterSpec,
    state: State,
    last_seen: i64,
}

fn rolling_bucket(period: Duration) -> Duration {
    Duration::from_millis(
        u64::try_from(positive_millis(period) / 60)
            .unwrap_or(1)
            .max(1),
    )
}

impl Meter {
    #[must_use]
    pub fn new(spec: MeterSpec, since: UnixMillis) -> Self {
        let state = match spec {
            MeterSpec::Fixed { period, .. } => State::Fixed {
                period: positive_millis(period),
                window: i64::MIN,
                used: 0,
            },
            MeterSpec::Rolling { period, .. } => State::Rolling {
                period,
                buckets: VecDeque::new(),
            },
            MeterSpec::Session { length, .. } => State::Session {
                length: positive_millis(length),
                open: None,
            },
            MeterSpec::Credit {
                capacity,
                refill,
                per,
                initial,
            } => {
                let per = i128::from(positive_millis(per));
                State::Credit {
                    capacity: capacity.0,
                    refill: refill.0,
                    per,
                    scaled: i128::from(initial.0) * per,
                    at: since.0,
                }
            }
        };
        Self {
            spec,
            state,
            last_seen: since.0,
        }
    }

    #[must_use]
    pub const fn spec(&self) -> &MeterSpec {
        &self.spec
    }

    #[must_use]
    pub fn check(&self, now: UnixMillis, want: Tokens) -> Verdict {
        self.verdict(now, Tokens(0), want)
    }

    pub(crate) fn verdict(&self, now: UnixMillis, held: Tokens, want: Tokens) -> Verdict {
        let now = now.0.max(self.last_seen);
        let limit = self.spec.limit();
        if want > limit {
            return Verdict::Never;
        }
        let asked = u128::from(held.0) + u128::from(want.0);
        let fits = |used: u64| u128::from(used) + asked <= u128::from(limit.0);
        match &self.state {
            State::Fixed {
                period,
                window,
                used,
                ..
            } => {
                let current = now.div_euclid(*period) * period;
                let used = if *window == current { *used } else { 0 };
                if fits(used) {
                    Verdict::Allow
                } else if fits(0) {
                    Verdict::Wait(wait(now, current.saturating_add(*period)))
                } else {
                    Verdict::Wait(RESERVATION_WAIT)
                }
            }
            State::Rolling {
                period, buckets, ..
            } => {
                let expiry = rolling_expiry(*period);
                let live = buckets.iter().filter(|(start, _)| start + expiry > now);
                let mut used = live
                    .clone()
                    .map(|(_, spent)| *spent)
                    .fold(0u64, u64::saturating_add);
                if fits(used) {
                    return Verdict::Allow;
                }
                for (start, spent) in live {
                    used = used.saturating_sub(*spent);
                    if fits(used) {
                        return Verdict::Wait(wait(now, start + expiry));
                    }
                }
                Verdict::Wait(RESERVATION_WAIT)
            }
            State::Session { length, open, .. } => {
                match open.filter(|(start, _)| start + length > now) {
                    Some((_, used)) if fits(used) => Verdict::Allow,
                    Some((start, _)) if fits(0) => Verdict::Wait(wait(now, start + length)),
                    None if fits(0) => Verdict::Allow,
                    Some(_) | None => Verdict::Wait(RESERVATION_WAIT),
                }
            }
            State::Credit {
                capacity,
                refill,
                per,
                scaled,
                at,
            } => {
                let balance = credit_balance(*scaled, *at, now, *capacity, *refill, *per);
                let need = i128::try_from(asked)
                    .unwrap_or(i128::MAX)
                    .saturating_mul(*per);
                if balance >= need {
                    Verdict::Allow
                } else if *refill == 0 {
                    Verdict::Never
                } else {
                    let refill = i128::from(*refill);
                    let millis = (need - balance + refill - 1) / refill;
                    Verdict::Wait(Duration::from_millis(
                        u64::try_from(millis).unwrap_or(u64::MAX),
                    ))
                }
            }
        }
    }

    pub fn charge(&mut self, at: UnixMillis, spent: Tokens) {
        let at = at.0.max(self.last_seen);
        self.last_seen = at;
        if spent.0 == 0 {
            return;
        }
        match &mut self.state {
            State::Fixed {
                period,
                window,
                used,
                ..
            } => {
                let current = at.div_euclid(*period) * *period;
                if *window != current {
                    *window = current;
                    *used = 0;
                }
                *used = used.saturating_add(spent.0);
            }
            State::Rolling {
                period, buckets, ..
            } => {
                let expiry = rolling_expiry(*period);
                let width = positive_millis(rolling_bucket(*period));
                while buckets
                    .front()
                    .is_some_and(|(start, _)| start + expiry <= at)
                {
                    buckets.pop_front();
                }
                let key = at.div_euclid(width) * width;
                match buckets.back_mut() {
                    Some((start, total)) if *start == key => {
                        *total = total.saturating_add(spent.0);
                    }
                    Some(_) | None => buckets.push_back((key, spent.0)),
                }
            }
            State::Session { length, open, .. } => {
                *open = match open.filter(|(start, _)| start + *length > at) {
                    Some((start, used)) => Some((start, used.saturating_add(spent.0))),
                    None => Some((at, spent.0)),
                };
            }
            State::Credit {
                capacity,
                refill,
                per,
                scaled,
                at: since,
            } => {
                *scaled = credit_balance(*scaled, *since, at, *capacity, *refill, *per)
                    - i128::from(spent.0) * *per;
                *since = at;
            }
        }
    }

    #[must_use]
    pub fn status(&self, now: UnixMillis) -> MeterStatus {
        let now = now.0.max(self.last_seen);
        let limit = self.spec.limit();
        let used = match &self.state {
            State::Fixed {
                period,
                window,
                used,
                ..
            } => {
                if *window == now.div_euclid(*period) * period {
                    i128::from(*used)
                } else {
                    0
                }
            }
            State::Rolling {
                period, buckets, ..
            } => {
                let expiry = rolling_expiry(*period);
                buckets
                    .iter()
                    .filter(|(start, _)| start + expiry > now)
                    .map(|(_, spent)| i128::from(*spent))
                    .sum()
            }
            State::Session { length, open, .. } => open
                .filter(|(start, _)| start + length > now)
                .map_or(0, |(_, used)| i128::from(used)),
            State::Credit {
                capacity,
                refill,
                per,
                scaled,
                at,
            } => {
                let balance = credit_balance(*scaled, *at, now, *capacity, *refill, *per);
                i128::from(*capacity) - balance.div_euclid(*per)
            }
        };
        let remaining = i128::from(limit.0) - used;
        MeterStatus {
            limit,
            used: Tokens(u64::try_from(used.max(0)).unwrap_or(u64::MAX)),
            remaining: i64::try_from(remaining).unwrap_or(if remaining < 0 {
                i64::MIN
            } else {
                i64::MAX
            }),
        }
    }

    #[must_use]
    pub fn horizon(&self, now: UnixMillis) -> Duration {
        self.spec.horizon(now.0.max(self.last_seen))
    }
}

/// A rolling bucket counts until its end leaves the window, so spend never expires early.
fn rolling_expiry(period: Duration) -> i64 {
    positive_millis(period).saturating_add(positive_millis(rolling_bucket(period)))
}

fn credit_balance(
    scaled: i128,
    since: i64,
    now: i64,
    capacity: u64,
    refill: u64,
    per: i128,
) -> i128 {
    let cap = i128::from(capacity) * per;
    if scaled >= cap {
        return scaled;
    }
    let accrued = i128::from(now.saturating_sub(since).max(0)) * i128::from(refill);
    scaled.saturating_add(accrued).min(cap)
}
