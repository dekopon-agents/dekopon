use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use dekopon_core::AgentId;
use parking_lot::Mutex;
use tracing::field::{Field, Visit};
use tracing_subscriber::{Layer, layer::SubscriberExt as _};

use crate::{
    Budget, Call, Estimate, HistoryRow, InputHint, Meter, MeterKind, MeterSpec, Metering,
    ModelUsage, Outcome, Refusal, Retry, Sizes, Tokens, UnixMillis, Verdict, Via,
};

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;

fn hours(count: u64) -> Duration {
    Duration::from_secs(count * 3_600)
}

fn at(millis: i64) -> UnixMillis {
    UnixMillis(millis)
}

fn agent() -> AgentId {
    "gylmar".parse().unwrap()
}

fn specs() -> [MeterSpec; 4] {
    [
        MeterSpec::Fixed {
            limit: Tokens(100),
            period: hours(24),
        },
        MeterSpec::Rolling {
            limit: Tokens(100),
            period: hours(5),
        },
        MeterSpec::Session {
            limit: Tokens(100),
            length: hours(5),
        },
        MeterSpec::Credit {
            capacity: Tokens(100),
            refill: Tokens(100),
            per: hours(1),
            initial: Tokens(100),
        },
    ]
}

#[test]
fn every_kind_allows_up_to_its_limit_and_refuses_one_token_past_it() {
    for spec in specs() {
        let mut meter = Meter::new(spec, at(0));
        meter.charge(at(1), Tokens(60));
        assert_eq!(meter.check(at(1), Tokens(40)), Verdict::Allow, "{spec:?}");
        assert!(
            matches!(meter.check(at(1), Tokens(41)), Verdict::Wait(_)),
            "{spec:?}"
        );
    }
}

#[test]
fn every_kind_reports_never_when_the_request_exceeds_its_whole_limit() {
    for spec in specs() {
        let meter = Meter::new(spec, at(0));
        assert_eq!(meter.check(at(0), Tokens(101)), Verdict::Never, "{spec:?}");
    }
}

#[test]
fn every_kind_reports_its_exact_wait() {
    let waits = [
        // Fixed: until the UTC day ends.
        Duration::from_millis(u64::try_from(24 * HOUR - 10 * MINUTE).unwrap()),
        // Rolling: the 5-minute bucket holding the charge leaves the 5-hour window.
        Duration::from_millis(u64::try_from(5 * HOUR + 5 * MINUTE).unwrap()),
        // Session: the window that opened on the charge ends.
        Duration::from_millis(u64::try_from(5 * HOUR).unwrap()),
        // Credit: 100 tokens an hour refill the one missing token in 36 seconds.
        Duration::from_secs(36),
    ];
    for (spec, expected) in specs().into_iter().zip(waits) {
        let mut meter = Meter::new(spec, at(0));
        meter.charge(at(10 * MINUTE), Tokens(100));
        assert_eq!(
            meter.check(at(10 * MINUTE), Tokens(1)),
            Verdict::Wait(expected),
            "{spec:?}"
        );
    }
}

#[test]
fn every_kind_turns_overdraw_into_debt_that_delays_the_next_allow() {
    for spec in specs() {
        let mut meter = Meter::new(spec, at(0));
        meter.charge(at(MINUTE), Tokens(150));
        assert_eq!(meter.status(at(MINUTE)).remaining, -50, "{spec:?}");
        let Verdict::Wait(debt_wait) = meter.check(at(MINUTE), Tokens(1)) else {
            panic!("{spec:?} allowed while in debt");
        };
        let mut fresh = Meter::new(spec, at(0));
        fresh.charge(at(MINUTE), Tokens(100));
        let Verdict::Wait(full_wait) = fresh.check(at(MINUTE), Tokens(1)) else {
            panic!("{spec:?} allowed while full");
        };
        assert!(debt_wait >= full_wait, "{spec:?}");
    }
    let mut credit = Meter::new(specs()[3], at(0));
    credit.charge(at(0), Tokens(150));
    assert_eq!(
        credit.check(at(0), Tokens(1)),
        Verdict::Wait(Duration::from_secs(36 * 51))
    );
}

#[test]
fn a_fixed_window_resets_at_the_epoch_multiple() {
    let mut meter = Meter::new(specs()[0], at(0));
    meter.charge(at(24 * HOUR - 1), Tokens(100));
    assert!(matches!(
        meter.check(at(24 * HOUR - 1), Tokens(1)),
        Verdict::Wait(_)
    ));
    assert_eq!(meter.check(at(24 * HOUR), Tokens(100)), Verdict::Allow);
}

#[test]
fn rolling_spend_expires_bucket_by_bucket() {
    let mut meter = Meter::new(specs()[1], at(0));
    meter.charge(at(0), Tokens(40));
    meter.charge(at(HOUR), Tokens(60));
    let first_expires = 5 * HOUR + 5 * MINUTE;
    assert_eq!(meter.status(at(first_expires - 1)).used, Tokens(100));
    assert_eq!(meter.status(at(first_expires)).used, Tokens(60));
    assert_eq!(meter.status(at(first_expires + HOUR)).used, Tokens(0));
}

#[test]
fn a_session_opens_on_a_charge_and_never_on_a_refused_check() {
    let mut meter = Meter::new(specs()[2], at(0));
    assert_eq!(meter.check(at(0), Tokens(101)), Verdict::Never);
    assert_eq!(meter.status(at(0)).used, Tokens(0));
    meter.charge(at(2 * HOUR), Tokens(100));
    assert_eq!(
        meter.check(at(2 * HOUR), Tokens(1)),
        Verdict::Wait(hours(5))
    );
    assert_eq!(meter.check(at(7 * HOUR), Tokens(100)), Verdict::Allow);
}

#[test]
fn credit_caps_at_capacity_and_spend_below_the_refill_rate_never_refuses() {
    let mut meter = Meter::new(specs()[3], at(0));
    assert_eq!(meter.status(at(10 * HOUR)).remaining, 100);
    for minute in 0..600 {
        let now = at(minute * MINUTE);
        assert_eq!(meter.check(now, Tokens(1)), Verdict::Allow);
        meter.charge(now, Tokens(1));
    }
}

#[test]
fn time_going_backwards_does_not_unspend() {
    let mut meter = Meter::new(specs()[1], at(0));
    meter.charge(at(10 * HOUR), Tokens(100));
    meter.charge(at(HOUR), Tokens(1));
    assert_eq!(meter.status(at(0)).used, Tokens(101));
    assert!(matches!(meter.check(at(0), Tokens(1)), Verdict::Wait(_)));
}

fn budget(specs: &[MeterSpec]) -> Budget {
    Budget::new(agent(), None, specs, at(0))
}

#[test]
fn a_budget_refuses_when_any_meter_refuses_and_names_the_longest_wait() {
    let mut budget = budget(&[
        MeterSpec::Rolling {
            limit: Tokens(100),
            period: hours(1),
        },
        MeterSpec::Session {
            limit: Tokens(100),
            length: hours(5),
        },
        MeterSpec::Credit {
            capacity: Tokens(1_000),
            refill: Tokens(1),
            per: hours(1),
            initial: Tokens(1_000),
        },
    ]);
    let reservation = budget.reserve(at(0), Tokens(100)).unwrap();
    budget.settle(reservation, at(0), Tokens(100));
    let refusal = budget.explain(at(0), Tokens(10)).unwrap();
    assert_eq!(refusal.meter, MeterKind::Session { length: hours(5) });
    assert_eq!(refusal.retry, Retry::After(hours(5)));
    let never = budget.explain(at(0), Tokens(500)).unwrap();
    assert_eq!(never.retry, Retry::Never);
    assert!(!never.fits_ever());
}

#[test]
fn a_second_caller_sees_the_first_callers_reservation() {
    let mut budget = budget(&specs()[1..2]);
    let first = budget.reserve(at(0), Tokens(70)).unwrap();
    assert!(budget.reserve(at(0), Tokens(31)).is_err());
    budget.settle(first, at(0), Tokens(20));
    assert!(budget.reserve(at(0), Tokens(80)).is_ok());
}

#[test]
fn a_refusal_counts_reservations_in_flight_as_spent() {
    let mut budget = budget(&specs()[1..2]);
    let _first = budget.reserve(at(0), Tokens(70)).unwrap();
    let refusal = budget.reserve(at(0), Tokens(40)).unwrap_err();
    assert_eq!(refusal.remaining, 30);
}

#[test]
fn settling_under_the_estimate_frees_the_difference() {
    let mut budget = budget(&specs()[1..2]);
    let reservation = budget.reserve(at(0), Tokens(90)).unwrap();
    budget.settle(reservation, at(0), Tokens(30));
    assert_eq!(budget.statuses(at(0))[0].1.remaining, 70);
    assert!(budget.explain(at(0), Tokens(70)).is_none());
}

#[test]
fn restore_never_clamps_history_to_boot() {
    let boot = 10 * HOUR;
    let mut budget = budget(&specs()[1..2]);
    budget.begin_restore();
    let reservation = budget.reserve(at(boot), Tokens(10)).unwrap();
    budget.settle(reservation, at(boot), Tokens(10));
    let old = boot - 4 * HOUR;
    budget.restore(at(boot - 6 * HOUR), [(at(old), Tokens(50))].into_iter());
    assert_eq!(budget.statuses(at(boot))[0].1.used, Tokens(60));
    let expires = old + 5 * HOUR + 5 * MINUTE;
    assert_eq!(budget.statuses(at(expires - 1))[0].1.used, Tokens(60));
    assert_eq!(budget.statuses(at(expires))[0].1.used, Tokens(10));
}

/// splitmix64, so the sequence is the same on every run.
struct Seeded(u64);

impl Seeded {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }
}

#[test]
fn restored_history_plus_the_live_tail_matches_live_charging_within_tolerance() {
    let bucket = 5 * MINUTE;
    for seed in 0..50 {
        let mut random = Seeded(seed);
        for spec in specs() {
            let boot = 40 * HOUR;
            let mut live = Meter::new(spec, at(0));
            let mut restored = budget(&[spec]);
            restored.begin_restore();
            let mut history = Vec::new();
            let mut now = 0;
            while now < boot + 30 * MINUTE {
                let idle = if random.next().is_multiple_of(10) {
                    6 * HOUR
                } else {
                    0
                };
                now += idle
                    + i64::try_from(random.next() % u64::try_from(90 * MINUTE).unwrap()).unwrap();
                let spent = Tokens(random.next() % 40);
                live.charge(at(now), spent);
                if now < boot {
                    history.push((at(now.div_euclid(bucket) * bucket), spent));
                } else {
                    let reservation = restored.reserve(at(now), Tokens(0)).unwrap();
                    restored.settle(reservation, at(now), spent);
                }
            }
            let since = boot - i64::try_from(spec_lookback(spec).as_millis()).unwrap();
            if let MeterSpec::Session { length, .. } = spec {
                let length = i64::try_from(length.as_millis()).unwrap();
                let fetched = history
                    .iter()
                    .filter(|(time, _)| time.0 >= since)
                    .map(|(time, _)| time.0)
                    .collect::<Vec<_>>();
                if !fetched.windows(2).any(|pair| pair[1] - pair[0] >= length) {
                    continue;
                }
            }
            restored.restore(
                at(since),
                history.into_iter().filter(|(time, _)| time.0 >= since),
            );
            let expected = live.status(at(now)).used.0;
            let got = restored.statuses(at(now))[0].1.used.0;
            let tolerance = match spec {
                MeterSpec::Fixed { .. } | MeterSpec::Rolling { .. } => 80,
                MeterSpec::Session { .. } | MeterSpec::Credit { .. } => 100,
            };
            assert!(
                expected.abs_diff(got) <= tolerance,
                "seed {seed} {spec:?}: live {expected}, restored {got}"
            );
        }
    }
}

fn spec_lookback(spec: MeterSpec) -> Duration {
    Meter::new(spec, at(0)).horizon(at(40 * HOUR))
}

fn refusal(meter: MeterKind, limit: u64, remaining: i64, requested: u64, retry: Retry) -> Refusal {
    Refusal {
        agent: agent(),
        meter,
        limit: Tokens(limit),
        remaining,
        requested: Tokens(requested),
        retry,
    }
}

#[test]
fn the_refusal_reads_as_one_of_three_sentences() {
    assert_eq!(
        refusal(
            MeterKind::Rolling { period: hours(5) },
            1_000,
            10,
            100,
            Retry::After(Duration::from_secs(14 * 60 + 20))
        )
        .to_string(),
        "I'm at 99% of my token budget (5-hour rolling window): 10 tokens left, this message needs about 100. Try again in 15 minutes."
    );
    assert_eq!(
        refusal(
            MeterKind::Fixed { period: hours(24) },
            100_000,
            -1_200,
            100,
            Retry::After(Duration::from_secs(2 * 3_600 + 60))
        )
        .to_string(),
        "I'm over my token budget (daily window) by 1,200 tokens. It resets in 3 hours."
    );
    assert_eq!(
        refusal(MeterKind::Credit, 100_000, 100_000, 180_000, Retry::Never).to_string(),
        "This message needs about 180,000 tokens, more than my whole 100,000-token budget (credit bucket). It can't run as is."
    );
}

type RecordFields = Vec<(String, String)>;

#[derive(Clone, Default)]
struct MeterRecords(Arc<Mutex<Vec<RecordFields>>>);

struct Fields<'a>(&'a mut Vec<(String, String)>);

impl Visit for Fields<'_> {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0
            .push((field.name().to_owned(), format!("i64:{value}")));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push((field.name().to_owned(), value.to_owned()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push((field.name().to_owned(), format!("{value:?}")));
    }
}

impl<S: tracing::Subscriber> Layer<S> for MeterRecords {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() == "meter" {
            let mut fields = Vec::new();
            event.record(&mut Fields(&mut fields));
            self.0.lock().push(fields);
        }
    }
}

impl MeterRecords {
    fn field(&self, index: usize, name: &str) -> String {
        self.0.lock()[index]
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    }

    fn len(&self) -> usize {
        self.0.lock().len()
    }
}

fn capture() -> (MeterRecords, tracing::subscriber::DefaultGuard) {
    let records = MeterRecords::default();
    let guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(records.clone()));
    (records, guard)
}

fn metering(limit: u64) -> (Arc<Metering>, Arc<AtomicI64>) {
    let clock = Arc::new(AtomicI64::new(HOUR));
    let now = Arc::clone(&clock);
    let budget = Budget::new(
        agent(),
        Some(BTreeSet::from(["astra".to_owned()])),
        &[MeterSpec::Rolling {
            limit: Tokens(limit),
            period: hours(5),
        }],
        at(0),
    );
    let metering = Metering::new(
        vec![budget],
        Arc::new(move || UnixMillis(now.load(Ordering::Relaxed))),
    );
    (Arc::new(metering), clock)
}

fn call(model: &str) -> Call {
    Call {
        agent: agent(),
        model: model.to_owned(),
        backend: "codex",
        via: Via::Agent,
    }
}

fn estimate(input: u64) -> Estimate {
    Estimate {
        input: Tokens(input),
        output_reserve: Tokens(10),
    }
}

fn used(metering: &Metering) -> u64 {
    metering.statuses(&agent()).unwrap()[0].1.used.0
}

#[test]
fn a_settled_call_charges_once_and_emits_one_record() {
    let (records, _guard) = capture();
    let (metering, _) = metering(1_000);
    let admission = metering.admit(call("astra"), estimate(100)).unwrap();
    admission.observe_usage(ModelUsage {
        input_tokens: Some(120),
        output_tokens: Some(30),
        cached_input_tokens: Some(100),
        ..ModelUsage::default()
    });
    admission.settle(Outcome::Succeeded);
    assert_eq!(used(&metering), 150);
    assert_eq!(records.len(), 1);
    assert_eq!(records.field(0, "usage.input_tokens"), "i64:120");
    assert_eq!(records.field(0, "usage.output_tokens"), "i64:30");
    assert_eq!(records.field(0, "usage.cached_input_tokens"), "i64:100");
    assert_eq!(records.field(0, "usage.cache_write_tokens"), "i64:0");
    assert_eq!(records.field(0, "meter.schema"), "i64:1");
    assert_eq!(records.field(0, "meter.estimate.input_tokens"), "i64:100");
    assert_eq!(records.field(0, "usage.source"), "reported");
    assert_eq!(records.field(0, "outcome"), "succeeded");
    assert_eq!(records.field(0, "meter.via"), "agent");
    assert_eq!(records.field(0, "model.name"), "astra");
}

#[test]
fn usage_then_a_cancel_settles_reported_input_and_estimated_output() {
    let (records, _guard) = capture();
    let (metering, _) = metering(1_000);
    let admission = metering.admit(call("astra"), estimate(100)).unwrap();
    admission.observe_usage(ModelUsage {
        input_tokens: Some(80),
        ..ModelUsage::default()
    });
    admission.observe_text(41);
    admission.settle(Outcome::Cancelled);
    assert_eq!(used(&metering), 80 + 11);
    assert_eq!(records.field(0, "outcome"), "cancelled");
    assert_eq!(records.field(0, "usage.source"), "partial");
}

#[test]
fn an_unsettled_drop_charges_the_estimate() {
    let (records, _guard) = capture();
    let (metering, _) = metering(1_000);
    drop(metering.admit(call("astra"), estimate(100)).unwrap());
    assert_eq!(used(&metering), 100);
    assert_eq!(records.len(), 1);
    assert_eq!(records.field(0, "outcome"), "cancelled");
    assert_eq!(records.field(0, "usage.source"), "estimated");
}

#[test]
fn a_call_not_sent_charges_nothing() {
    let (records, _guard) = capture();
    let (metering, _) = metering(1_000);
    let admission = metering.admit(call("astra"), estimate(100)).unwrap();
    admission.settle(Outcome::NotSent);
    assert_eq!(used(&metering), 0);
    assert_eq!(records.field(0, "outcome"), "failed");
    assert_eq!(records.field(0, "usage.input_tokens"), "i64:0");
}

#[test]
fn a_refused_call_emits_a_zero_refused_record() {
    let (records, _guard) = capture();
    let (metering, _) = metering(100);
    let refusal = metering
        .admit(call("astra"), estimate(200))
        .err()
        .expect("over the limit");
    assert_eq!(refusal.retry, Retry::Never);
    assert_eq!(records.len(), 1);
    assert_eq!(records.field(0, "outcome"), "refused");
    assert_eq!(records.field(0, "usage.input_tokens"), "i64:0");
}

#[test]
fn a_model_outside_the_budget_filter_is_recorded_but_never_refused() {
    let (records, _guard) = capture();
    let (metering, _) = metering(100);
    let admission = metering.admit(call("terra"), estimate(10_000)).unwrap();
    admission.settle(Outcome::Failed);
    assert_eq!(used(&metering), 0);
    assert_eq!(records.len(), 1);
}

#[test]
fn an_agent_without_a_budget_is_never_refused() {
    let (metering, _) = metering(100);
    let mut other = call("astra");
    other.agent = "other".parse().unwrap();
    metering
        .admit(other, estimate(1_000_000))
        .unwrap()
        .settle(Outcome::Succeeded);
}

#[test]
fn an_admission_is_send() {
    fn send<T: Send + 'static>() {}
    send::<crate::Admission>();
}

#[tokio::test]
async fn an_admission_held_across_an_await_and_a_blocking_call_share_one_budget() {
    let (metering, _) = metering(1_000);
    let gate = Arc::new(tokio::sync::Notify::new());
    let held = {
        let metering = Arc::clone(&metering);
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            let admission = metering.admit(call("astra"), estimate(600)).unwrap();
            gate.notified().await;
            admission.settle(Outcome::Succeeded);
        })
    };
    tokio::task::yield_now().await;
    while metering.admit(call("astra"), estimate(390)).is_ok() {
        tokio::task::yield_now().await;
    }
    let blocking = Arc::clone(&metering);
    let second = tokio::task::spawn_blocking(move || {
        blocking
            .admit(call("astra"), estimate(390))
            .map(|admission| admission.settle(Outcome::NotSent))
            .is_err()
    })
    .await
    .unwrap();
    assert!(
        second,
        "the blocking caller saw the async caller's reservation"
    );
    gate.notify_one();
    held.await.unwrap();
}

#[tokio::test]
async fn an_admission_aborted_mid_await_charges_what_it_observed_once() {
    let (records, _guard) = capture();
    let (metering, _) = metering(1_000);
    let admission = metering.admit(call("astra"), estimate(100)).unwrap();
    admission.observe_text(400);
    let task = tokio::spawn(async move {
        let _admission = admission;
        std::future::pending::<()>().await;
    });
    tokio::task::yield_now().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(used(&metering), 200);
    assert_eq!(records.len(), 1);
    assert_eq!(records.field(0, "outcome"), "cancelled");
}

#[test]
fn restore_applies_only_matching_rows_and_replays_the_live_tail() {
    let (metering, clock) = metering(1_000);
    metering.begin_restore();
    metering
        .admit(call("astra"), estimate(100))
        .unwrap()
        .settle(Outcome::Succeeded);
    let rows = [
        HistoryRow {
            agent: agent(),
            model: "astra".to_owned(),
            at: at(0),
            tokens: Tokens(300),
        },
        HistoryRow {
            agent: agent(),
            model: "terra".to_owned(),
            at: at(0),
            tokens: Tokens(500),
        },
    ];
    metering.restore(at(0), &rows);
    assert_eq!(used(&metering), 400);
    clock.store(2 * HOUR, Ordering::Relaxed);
    assert_eq!(
        metering.lookback(at(2 * HOUR)),
        hours(5) + Duration::from_secs(300)
    );
}

#[test]
fn the_estimate_takes_the_larger_of_the_bytes_and_the_hint() {
    let hint = InputHint::default();
    let whole = |images| Sizes { bytes: 400, images };
    let fresh = |images| Sizes { bytes: 40, images };
    assert_eq!(
        Estimate::from_sizes(whole(1), Some((&hint, fresh(1))), Tokens(5)).input,
        Tokens(1_100)
    );
    hint.observe(&ModelUsage {
        input_tokens: Some(5_000),
        ..ModelUsage::default()
    });
    assert_eq!(
        Estimate::from_sizes(whole(0), Some((&hint, fresh(0))), Tokens(5)).input,
        Tokens(5_010)
    );
    assert_eq!(
        Estimate::from_sizes(whole(1), Some((&hint, fresh(1))), Tokens(5)).input,
        Tokens(6_010)
    );
    assert_eq!(Estimate::output_reserve(None, true), Tokens(4_096));
    assert_eq!(Estimate::output_reserve(Some(7), true), Tokens(7));
    assert_eq!(Estimate::output_reserve(None, false), Tokens(1_024));
}
