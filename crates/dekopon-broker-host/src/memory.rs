//! Conservative, per-memory observations at the synchronous Wasmtime limiter boundary.

use dekopon_provider_sdk::host::StoreLimits;
use wasmtime::ResourceLimiter;

/// Delegates every enforcement decision to the existing limiter; no guest memory is read.
///
/// Wasmtime has no success callback. Permitted sizes are candidates until instantiation
/// succeeds. A grow-failed callback can also occur *without* a preceding growing callback,
/// so after any such callback only confirmed observations are reported, never candidates.
/// Initial allocation failures have no failure callback, hence the instantiation gate.
pub(super) struct MemoryLimiter {
    limits: wasmtime::StoreLimits,
    per_memory_limit: usize,
    candidate_peak: Option<usize>,
    confirmed_peak: Option<usize>,
    instantiated: bool,
    denied: u64,
    failed: u64,
    report: Option<Report>,
}

struct Report {
    span: tracing::Span,
    provider: String,
    capability: String,
    outcome: &'static str,
    initial_fuel: Option<u64>,
    remaining_fuel: Option<u64>,
}

impl MemoryLimiter {
    pub(super) fn new(bounds: StoreLimits) -> Self {
        Self {
            limits: bounds.store_limits(),
            per_memory_limit: bounds.max_memory_bytes,
            candidate_peak: None,
            confirmed_peak: None,
            instantiated: false,
            denied: 0,
            failed: 0,
            report: None,
        }
    }

    pub(super) fn observe_invocation(
        &mut self,
        provider: &str,
        capability: &str,
        initial_fuel: Option<u64>,
    ) {
        self.report = Some(Report {
            span: tracing::Span::current(),
            provider: provider.to_owned(),
            capability: capability.to_owned(),
            outcome: "cancelled",
            initial_fuel,
            remaining_fuel: None,
        });
    }

    pub(super) fn record_remaining_fuel(&mut self, remaining: Option<u64>) {
        if let Some(report) = &mut self.report {
            report.remaining_fuel = remaining;
        }
    }

    pub(super) fn instantiated(&mut self) {
        self.instantiated = true;
        if self.failed == 0 {
            self.confirmed_peak = self.confirmed_peak.max(self.candidate_peak);
        }
    }

    pub(super) fn finish(&mut self, outcome: &'static str) {
        if let Some(report) = &mut self.report {
            report.outcome = outcome;
        }
    }

    fn complete(&self) -> bool {
        self.instantiated && self.failed == 0
    }

    fn observed_peak(&self) -> Option<usize> {
        if self.complete() {
            self.confirmed_peak.max(self.candidate_peak)
        } else {
            self.confirmed_peak
        }
    }
}

impl ResourceLimiter for MemoryLimiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        // A positive current size proves that memory exists even if instantiation later fails.
        // current=0 is also used for initial allocation, which has not yet succeeded.
        if current > 0 {
            self.confirmed_peak = self.confirmed_peak.max(Some(current));
        }
        let allowed = self.limits.memory_growing(current, desired, maximum)?;
        if allowed {
            self.candidate_peak = self.candidate_peak.max(Some(desired));
        } else {
            self.denied = self.denied.saturating_add(1);
        }
        Ok(allowed)
    }

    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.failed = self.failed.saturating_add(1);
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
        if let Some(report) = &self.report {
            // Explicit parenting alone does not activate the native OTel context used by
            // the log bridge. Re-enter synchronously, including on out-of-context drops.
            report.span.in_scope(|| tracing::info!(
                parent: &report.span,
                event = "provider.memory",
                operation = "invoke",
                provider = %report.provider,
                capability = %report.capability,
                outcome = report.outcome,
                fuel.initial = report.initial_fuel,
                fuel.remaining = report.remaining_fuel,
                fuel.consumed = report.initial_fuel.zip(report.remaining_fuel)
                    .and_then(|(initial, remaining)| initial.checked_sub(remaining)),
                memory.max_individual_observed_bytes = self.observed_peak().map(|bytes| bytes as u64),
                memory.observation_complete = self.complete(),
                memory.per_memory_limit_bytes = self.per_memory_limit as u64,
                memory.growth_denied = self.denied,
                memory.growth_failed = self.failed,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::{Engine, Instance, Module, Store};

    const PAGE: usize = 65_536;

    fn store() -> Store<MemoryLimiter> {
        let mut store = Store::new(
            &Engine::default(),
            MemoryLimiter::new(StoreLimits {
                max_memory_bytes: 4 * PAGE,
                ..StoreLimits::default()
            }),
        );
        store.limiter(|limiter| limiter);
        store
    }

    fn instantiate(store: &mut Store<MemoryLimiter>, wat: &str) -> Instance {
        let module = Module::new(store.engine(), wat).expect("synthetic module");
        let instance = Instance::new(&mut *store, &module, &[]).expect("instantiate");
        store.data_mut().instantiated();
        instance
    }

    #[test]
    fn initial_and_growing_memories_report_largest_individual_size_not_sum() {
        let mut store = store();
        let instance = instantiate(
            &mut store,
            r#"(module
                (memory $a 1) (memory $b 2)
                (func (export "grow") (result i32) i32.const 2 memory.grow $a))"#,
        );
        assert_eq!(store.data().observed_peak(), Some(2 * PAGE));
        let grow = instance
            .get_typed_func::<(), i32>(&mut store, "grow")
            .unwrap();
        assert_eq!(grow.call(&mut store, ()).unwrap(), 1);
        assert_eq!(store.data().observed_peak(), Some(3 * PAGE));
        assert!(store.data().complete());
        // A second core instance does not reset the observation or turn it into a sum.
        instantiate(&mut store, "(module (memory 2))");
        assert_eq!(store.data().observed_peak(), Some(3 * PAGE));
    }

    #[test]
    fn caught_denied_growth_does_not_count_as_consumed_memory() {
        for maximum in ["", " 1"] {
            let mut store = store();
            let instance = instantiate(
                &mut store,
                &format!(
                    r#"(module (memory 1{maximum})
                    (func (export "grow") (result i32) i32.const 4 memory.grow))"#
                ),
            );
            let grow = instance
                .get_typed_func::<(), i32>(&mut store, "grow")
                .unwrap();
            assert_eq!(grow.call(&mut store, ()).unwrap(), -1);
            assert_eq!(store.data().observed_peak(), Some(PAGE));
            assert_eq!(store.data().denied, 1);
            assert!(store.data().complete());
        }
    }

    #[test]
    fn unpaired_failure_after_successful_growth_is_only_a_lower_bound() {
        let mut store = store();
        let instance = instantiate(
            &mut store,
            r#"(module (memory 1)
                (func (export "grow")
                    i32.const 1 memory.grow drop))"#,
        );
        instance
            .get_typed_func::<(), ()>(&mut store, "grow")
            .unwrap()
            .call(&mut store, ())
            .unwrap();
        // Replay Wasmtime's unpaired type-limit failure path; ordinary wasm32
        // oversized grows may instead be rejected before reaching the limiter.
        store
            .data_mut()
            .memory_grow_failed(wasmtime::Error::msg("synthetic type-limit failure"))
            .unwrap();
        assert_eq!(store.data().failed, 1);
        assert!(!store.data().complete());
        // Wasmtime can report a type-limit failure without a new growing callback.
        // The preceding two-page candidate is not safe to promote from callbacks alone.
        assert_eq!(store.data().observed_peak(), Some(PAGE));
    }

    #[test]
    fn permitted_allocation_failure_never_promotes_the_attempt() {
        let mut limiter = MemoryLimiter::new(StoreLimits::default());
        assert!(limiter.memory_growing(0, PAGE, None).unwrap());
        limiter.instantiated();
        assert!(limiter.memory_growing(PAGE, 2 * PAGE, None).unwrap());
        // Deterministic OS-failure callback: exhausting the test machine is not a test.
        limiter
            .memory_grow_failed(wasmtime::Error::msg("synthetic allocation failure"))
            .unwrap();
        assert_eq!(limiter.observed_peak(), Some(PAGE));
        assert!(!limiter.complete());
    }

    #[test]
    fn failed_initial_allocation_or_instantiation_is_not_confirmed() {
        let mut limiter = MemoryLimiter::new(StoreLimits::default());
        assert!(limiter.memory_growing(0, PAGE, None).unwrap());
        // Initial OS allocation failure has no callback in Wasmtime. No completed
        // instantiation means this candidate remains unknown, not zero or one page used.
        assert_eq!(limiter.observed_peak(), None);
        assert!(!limiter.complete());

        for wat in [
            "(module (memory 5))",
            "(module (memory 1) (func $start unreachable) (start $start))",
        ] {
            let mut store = store();
            let module = Module::new(store.engine(), wat).unwrap();
            assert!(Instance::new(&mut store, &module, &[]).is_err());
            assert_eq!(store.data().observed_peak(), None);
            assert!(!store.data().complete());
        }
    }

    #[test]
    fn incomplete_reports_omit_unknown_sizes_and_keep_confirmed_lower_bounds() {
        use dekopon_test_support::CaptureLayer;
        use tracing_subscriber::layer::SubscriberExt as _;

        let capture = CaptureLayer::workspace();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::with_default(subscriber, || {
            for initialized in [false, true] {
                let mut limiter = MemoryLimiter::new(StoreLimits::default());
                let span = tracing::info_span!("provider.invoke");
                span.in_scope(|| {
                    limiter.observe_invocation("probe", "probe.run", initialized.then_some(200))
                });
                limiter.record_remaining_fuel(Some(50));
                assert!(limiter.memory_growing(0, PAGE, None).unwrap());
                if initialized {
                    limiter.instantiated();
                    assert!(limiter.memory_growing(PAGE, 2 * PAGE, None).unwrap());
                    limiter
                        .memory_grow_failed(wasmtime::Error::msg("private-error-sentinel"))
                        .unwrap();
                    // A guest may swallow a growth failure and still succeed.
                    limiter.finish("succeeded");
                } else {
                    limiter.finish("instantiation-error");
                }
                // No entered span: the stored explicit parent must still correlate the event.
                drop(limiter);
            }
        });
        let events = capture.events();
        assert_eq!(events.len(), 2);
        for (fields, parent) in &events {
            assert_eq!(parent.as_deref(), Some("provider.invoke"));
            assert!(
                fields.contains("memory.observation_complete=false"),
                "{fields}"
            );
            assert!(!fields.contains("private-error-sentinel"), "{fields}");
        }
        assert!(
            !events[0]
                .0
                .contains("memory.max_individual_observed_bytes=")
        );
        assert!(events[0].0.contains("outcome=\"instantiation-error\""));
        assert!(
            events[1]
                .0
                .contains("memory.max_individual_observed_bytes=65536")
        );
        assert!(events[1].0.contains("outcome=\"succeeded\""));
        assert!(events[1].0.contains("memory.growth_failed=1"));
        // Fuel is independently observable even when memory is incomplete.
        assert!(!events[0].0.contains("fuel.initial="));
        assert!(!events[0].0.contains("fuel.consumed="));
        assert!(events[1].0.contains("fuel.initial=200"));
        assert!(events[1].0.contains("fuel.remaining=50"));
        assert!(events[1].0.contains("fuel.consumed=150"));
    }

    #[test]
    fn table_and_resource_count_enforcement_is_delegated_unchanged() {
        let bounds = StoreLimits {
            max_table_elements: 3,
            max_tables: 2,
            max_instances: 5,
            max_memories: 4,
            ..StoreLimits::default()
        };
        let mut limiter = MemoryLimiter::new(bounds);
        let mut original = bounds.store_limits();
        for (current, desired, maximum) in [(0, 2, None), (2, 4, None), (2, 3, Some(2))] {
            assert_eq!(
                limiter.table_growing(current, desired, maximum).unwrap(),
                original.table_growing(current, desired, maximum).unwrap()
            );
        }
        assert_eq!(limiter.instances(), original.instances());
        assert_eq!(limiter.tables(), original.tables());
        assert_eq!(limiter.memories(), original.memories());
    }
}
