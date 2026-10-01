use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use crate::host::StoreLimits;
use wasmtime::ResourceLimiter;

pub(super) struct MemoryBudget {
    pub(super) maximum: usize,
    used: AtomicUsize,
}

impl MemoryBudget {
    pub(super) fn new(maximum: usize) -> Self {
        Self {
            maximum,
            used: AtomicUsize::new(0),
        }
    }

    fn charge(&self, bytes: usize) -> Result<usize, usize> {
        self.used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current
                    .checked_add(bytes)
                    .filter(|next| *next <= self.maximum)
            })
    }
}

struct MemoryCharge {
    budget: Option<Arc<MemoryBudget>>,
    peak: usize,
}

impl MemoryCharge {
    fn new(budget: Option<Arc<MemoryBudget>>) -> Self {
        Self { budget, peak: 0 }
    }

    fn grow(&mut self, desired: usize) -> Result<(), BudgetRefusal> {
        if desired <= self.peak {
            return Ok(());
        }
        if let Some(budget) = &self.budget {
            let requested = desired - self.peak;
            budget.charge(requested).map_err(|used| BudgetRefusal {
                requested,
                maximum: budget.maximum,
                used,
            })?;
        }
        self.peak = desired;
        Ok(())
    }

    fn used(&self) -> Option<usize> {
        self.budget
            .as_ref()
            .map(|budget| budget.used.load(Ordering::Relaxed))
    }
}

impl Drop for MemoryCharge {
    fn drop(&mut self) {
        if let Some(budget) = &self.budget {
            budget.used.fetch_sub(self.peak, Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct BudgetRefusal {
    pub(super) requested: usize,
    pub(super) maximum: usize,
    used: usize,
}

pub(super) struct MemoryLimiter {
    limits: wasmtime::StoreLimits,
    charge: MemoryCharge,
    refusal: Option<BudgetRefusal>,
    report: Report,
}

struct Report {
    span: tracing::Span,
    provider: String,
    capability: Option<String>,
    outcome: Option<&'static str>,
    initial_fuel: Option<u64>,
    remaining_fuel: Option<u64>,
}

fn recorded(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn fuel(value: Option<u64>) -> Option<i64> {
    value.map(|value| i64::try_from(value).unwrap_or(i64::MAX))
}

fn detail() -> &'static str {
    if tracing::enabled!(target: "memory", tracing::Level::TRACE) {
        "full"
    } else if tracing::enabled!(target: "memory", tracing::Level::DEBUG) {
        "standard"
    } else {
        "drip"
    }
}

impl MemoryLimiter {
    pub(super) fn new(
        bounds: StoreLimits,
        budget: Option<Arc<MemoryBudget>>,
        provider: &str,
    ) -> Self {
        Self {
            limits: bounds.store_limits(),
            charge: MemoryCharge::new(budget),
            refusal: None,
            report: Report {
                span: tracing::Span::current(),
                provider: provider.to_owned(),
                capability: None,
                outcome: None,
                initial_fuel: None,
                remaining_fuel: None,
            },
        }
    }

    pub(super) fn observe_invocation(&mut self, capability: &str, initial_fuel: Option<u64>) {
        self.report.capability = Some(capability.to_owned());
        self.report.outcome = Some("cancelled");
        self.report.initial_fuel = initial_fuel;
    }

    pub(super) fn refusal(&self) -> Option<BudgetRefusal> {
        self.refusal
    }

    pub(super) fn record_remaining_fuel(&mut self, remaining: Option<u64>) {
        self.report.remaining_fuel = remaining;
    }

    pub(super) fn finish(&mut self, outcome: &'static str) {
        self.report.outcome = Some(outcome);
    }
}

impl ResourceLimiter for MemoryLimiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if !self.limits.memory_growing(current, desired, maximum)? {
            return Ok(false);
        }
        if let Err(refusal) = self.charge.grow(desired) {
            if self.refusal.is_none() {
                self.refusal = Some(refusal);
                tracing::warn!(
                    name: "memory.refused",
                    target: "memory",
                    { telemetry.detail = detail(), provider = %self.report.provider,
                      capability = self.report.capability.as_deref(),
                      memory.request.bytes = recorded(refusal.requested),
                      memory.budget.bytes = recorded(refusal.maximum),
                      memory.budget.used.bytes = recorded(refusal.used) },
                    "memory.refused"
                );
            }
            return Ok(false);
        }
        Ok(true)
    }

    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.limits.memory_grow_failed(error)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.limits.table_growing(current, desired, maximum)
    }

    fn table_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.limits.table_grow_failed(error)
    }

    fn instances(&self) -> usize {
        self.limits.instances()
    }

    fn tables(&self) -> usize {
        self.limits.tables()
    }

    fn memories(&self) -> usize {
        self.limits.memories()
    }
}

impl Drop for MemoryLimiter {
    fn drop(&mut self) {
        let report = &self.report;
        // The span must be re-entered so the log bridge sees its OTel context on out-of-context drops.
        report.span.in_scope(|| {
            tracing::info!(
                name: "memory.store",
                target: "memory",
                { telemetry.detail = detail(), provider = %report.provider,
                  capability = report.capability.as_deref(), outcome = report.outcome,
                  fuel.initial = fuel(report.initial_fuel),
                  fuel.remaining = fuel(report.remaining_fuel),
                  fuel.consumed = fuel(report.initial_fuel.zip(report.remaining_fuel)
                      .and_then(|(initial, remaining)| initial.checked_sub(remaining))),
                  memory.peak.bytes = recorded(self.charge.peak),
                  memory.budget.bytes = self.charge.budget.as_ref().map(|b| recorded(b.maximum)),
                  memory.budget.used.bytes = self.charge.used().map(recorded) },
                "memory.store"
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dekopon_test_support::{CaptureLayer, Record};
    use tracing_subscriber::layer::SubscriberExt as _;
    use wasmtime::{Engine, Instance, Module, Store};

    const PAGE: usize = 65_536;

    fn store(engine: &Engine, budget: Option<Arc<MemoryBudget>>) -> Store<MemoryLimiter> {
        let mut store = Store::new(
            engine,
            MemoryLimiter::new(
                StoreLimits {
                    max_memory_bytes: 4 * PAGE,
                    ..StoreLimits::default()
                },
                budget,
                "probe",
            ),
        );
        store.limiter(|limiter| limiter);
        store
    }

    fn instantiate(store: &mut Store<MemoryLimiter>, wat: &str) -> Instance {
        let module = Module::new(store.engine(), wat).expect("synthetic module");
        Instance::new(&mut *store, &module, &[]).expect("instantiate")
    }

    #[test]
    fn four_concurrent_small_stores_fit() {
        let engine = Engine::default();
        let budget = Arc::new(MemoryBudget::new(8 * PAGE));
        let stores = (0..4)
            .map(|_| {
                let mut store = store(&engine, Some(Arc::clone(&budget)));
                instantiate(&mut store, "(module (memory 1))");
                store
            })
            .collect::<Vec<_>>();
        assert_eq!(budget.used.load(Ordering::Relaxed), 4 * PAGE);
        drop(stores);
        assert_eq!(budget.used.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn budget_refusal_is_a_guest_result_and_one_event() {
        let capture = CaptureLayer::workspace();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::with_default(subscriber, || {
            let engine = Engine::default();
            let budget = Arc::new(MemoryBudget::new(4 * PAGE));
            let mut holder = store(&engine, Some(Arc::clone(&budget)));
            instantiate(&mut holder, "(module (memory 3))");
            let mut guest = store(&engine, Some(budget));
            let instance = instantiate(
                &mut guest,
                "(module (memory 1) (func (export \"grow\") (result i32) i32.const 1 memory.grow))",
            );
            let grow = instance
                .get_typed_func::<(), i32>(&mut guest, "grow")
                .unwrap();
            for _ in 0..2 {
                assert_eq!(grow.call(&mut guest, ()).unwrap(), -1);
            }
            assert_eq!(guest.data().refusal().map(|r| r.requested), Some(PAGE));
        });
        assert!(capture.records().iter().any(|record| matches!(record, Record::Event { target, fields, level, .. } if target == "memory" && level == &"WARN" && fields.contains("message=memory.refused"))));
        let refusals = capture
            .events()
            .into_iter()
            .filter(|(fields, _)| fields.contains("memory.refused"))
            .collect::<Vec<_>>();
        assert_eq!(refusals.len(), 1, "{}", capture.text());
        for field in [
            "memory.request.bytes=65536",
            "memory.budget.bytes=262144",
            "memory.budget.used.bytes=262144",
            "provider=probe",
            "telemetry.detail=\"full\"",
        ] {
            assert!(refusals[0].0.contains(field), "{}", refusals[0].0);
        }
    }

    #[test]
    fn failed_allocation_stays_charged_until_drop() {
        let budget = Arc::new(MemoryBudget::new(4 * PAGE));
        let mut limiter =
            MemoryLimiter::new(StoreLimits::default(), Some(Arc::clone(&budget)), "probe");
        assert!(limiter.memory_growing(0, PAGE, None).unwrap());
        assert!(limiter.memory_growing(PAGE, 2 * PAGE, None).unwrap());
        limiter
            .memory_grow_failed(wasmtime::Error::msg("synthetic failure"))
            .unwrap();
        assert_eq!(budget.used.load(Ordering::Relaxed), 2 * PAGE);
        drop(limiter);
        assert_eq!(budget.used.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn smaller_second_memory_does_not_increase_high_water() {
        let engine = Engine::default();
        let budget = Arc::new(MemoryBudget::new(4 * PAGE));
        let mut store = store(&engine, Some(Arc::clone(&budget)));
        instantiate(&mut store, "(module (memory 2))");
        instantiate(&mut store, "(module (memory 1))");
        assert_eq!(budget.used.load(Ordering::Relaxed), 2 * PAGE);
        drop(store);
        assert_eq!(budget.used.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn per_store_denial_never_takes_a_budget_charge() {
        let engine = Engine::default();
        let budget = Arc::new(MemoryBudget::new(4 * PAGE));
        let mut store = store(&engine, Some(Arc::clone(&budget)));
        let instance = instantiate(
            &mut store,
            "(module (memory 1) (func (export \"grow\") (result i32) i32.const 4 memory.grow))",
        );
        assert_eq!(
            instance
                .get_typed_func::<(), i32>(&mut store, "grow")
                .unwrap()
                .call(&mut store, ())
                .unwrap(),
            -1
        );
        assert!(store.data().refusal().is_none());
        assert_eq!(budget.used.load(Ordering::Relaxed), PAGE);
    }

    #[test]
    fn unbudgeted_store_omits_budget_fields() {
        let capture = CaptureLayer::workspace();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::with_default(subscriber, || {
            let engine = Engine::default();
            let mut store = store(&engine, None);
            instantiate(&mut store, "(module (memory 1))");
        });
        let events = capture.events();
        assert_eq!(events.len(), 1);
        assert!(events[0].0.contains("memory.peak.bytes=65536"));
        assert!(!events[0].0.contains("memory.budget."));
    }

    #[test]
    fn store_record_has_contract_fields_once_without_payload() {
        let capture = CaptureLayer::workspace();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::with_default(subscriber, || {
            let budget = Arc::new(MemoryBudget::new(4 * PAGE));
            let span = tracing::info_span!("provider.invoke");
            let mut limiter =
                span.in_scope(|| MemoryLimiter::new(StoreLimits::default(), Some(budget), "probe"));
            limiter.observe_invocation("probe.run", Some(200));
            assert!(limiter.memory_growing(0, PAGE, None).unwrap());
            limiter.record_remaining_fuel(Some(50));
            limiter.finish("succeeded");
            drop(limiter);
        });
        assert!(capture.records().iter().any(|record| matches!(record, Record::Event { target, fields, level, .. } if target == "memory" && level == &"INFO" && fields.contains("message=memory.store"))));
        let events = capture.events();
        assert_eq!(events.len(), 1, "{}", capture.text());
        let (fields, parent) = &events[0];
        assert_eq!(parent.as_deref(), Some("provider.invoke"));
        for field in [
            "memory.store",
            "provider=probe",
            "capability=\"probe.run\"",
            "outcome=\"succeeded\"",
            "memory.peak.bytes=65536",
            "memory.budget.bytes=262144",
            "memory.budget.used.bytes=65536",
            "fuel.initial=200",
            "fuel.remaining=50",
            "fuel.consumed=150",
            "telemetry.detail=\"full\"",
        ] {
            assert!(fields.contains(field), "missing {field}: {fields}");
        }
        assert!(!fields.contains("private-input"));
    }
}
