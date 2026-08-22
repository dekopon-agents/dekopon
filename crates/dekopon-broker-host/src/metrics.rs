use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use dekopon_http_host::HttpCallEvidence;
use dekopon_storage_host::StorageEvidence;
use wasmtime::{Error, ResourceLimiter, StoreLimits};

use crate::BrokerHostLimits;

/// A cloneable handle to live Wasmtime host accounting.
///
/// Counters are process-local and reset whenever the broker restarts. They describe host-observed
/// work only; they are operational statistics, not authorization evidence or durable audit.
#[derive(Clone, Debug)]
pub struct BrokerHostMetrics {
    inner: Arc<MetricsInner>,
    limits: BrokerHostLimits,
}

/// One point-in-time view of everything the broker host can observe without guest cooperation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrokerHostStats {
    /// Startup-fixed host ceilings.
    pub limits: BrokerHostLimits,
    /// Components successfully compiled and loaded.
    pub providers_loaded: u64,
    /// Aggregate source artifact bytes inspected at startup.
    pub artifact_bytes: u64,
    /// Successful component compilations.
    pub component_compilations: u64,
    /// Aggregate component compilation wall time.
    pub compilation_micros: u64,
    /// Provider manifest calls completed at startup.
    pub provider_descriptions: u64,
    /// Command-line rewrite calls dispatched to components.
    pub command_resolutions: u64,
    /// Fresh Wasmtime stores created.
    pub stores_created: u64,
    /// Stores currently alive.
    pub active_stores: u64,
    /// Highest concurrently alive store count.
    pub peak_active_stores: u64,
    /// Successful component instantiations.
    pub component_instantiations: u64,
    /// Provider invocations begun.
    pub invocations_started: u64,
    /// Provider invocations that returned successful output.
    pub invocations_succeeded: u64,
    /// Provider invocations that ended in a host, trap, or typed provider failure.
    pub invocations_failed: u64,
    /// Failed invocations whose wall-clock deadline elapsed.
    pub invocations_timed_out: u64,
    /// Serialized provider input bytes accepted by the host.
    pub provider_input_bytes: u64,
    /// Serialized component response bytes observed by the host.
    pub provider_output_bytes: u64,
    /// Stores for which Wasmtime returned a fuel reading.
    pub fuel_observations: u64,
    /// Aggregate fuel supplied to observed stores.
    pub fuel_supplied: u64,
    /// Aggregate fuel consumed by observed stores.
    pub fuel_consumed: u64,
    /// Linear-memory allocation/growth requests seen by the resource limiter.
    pub memory_growth_requests: u64,
    /// Linear-memory growth requests refused by the configured limiter.
    pub memory_growth_denied: u64,
    /// Linear-memory growths Wasmtime could not complete after permission.
    pub memory_growth_failed: u64,
    /// Largest requested size for one guest linear memory.
    pub peak_memory_bytes_requested: u64,
    /// Table allocation/growth requests seen by the resource limiter.
    pub table_growth_requests: u64,
    /// Table growth requests refused by the configured limiter.
    pub table_growth_denied: u64,
    /// Table growths Wasmtime could not complete after permission.
    pub table_growth_failed: u64,
    /// Largest requested size for one guest table.
    pub peak_table_elements_requested: u64,
    /// Authorized HTTP calls that produced evidence.
    pub http_requests: u64,
    /// Guest-authored HTTP request bytes accounted by the native host.
    pub http_request_bytes: u64,
    /// HTTP response bytes accounted by the native host.
    pub http_response_bytes: u64,
    /// Storage-backed invocations that produced content-free evidence.
    pub storage_invocations: u64,
    /// Aggregate storage host-call count.
    pub storage_operations: u64,
    /// Aggregate requested durability barriers.
    pub storage_syncs: u64,
    /// Aggregate quota denials.
    pub storage_quota_denials: u64,
    /// Largest observed powers-of-two read bucket (never exact bytes).
    pub storage_read_bucket_max: u64,
    /// Largest observed powers-of-two write bucket (never exact bytes).
    pub storage_write_bucket_max: u64,
}

#[derive(Debug, Default)]
struct MetricsInner {
    providers_loaded: AtomicU64,
    artifact_bytes: AtomicU64,
    component_compilations: AtomicU64,
    compilation_micros: AtomicU64,
    provider_descriptions: AtomicU64,
    command_resolutions: AtomicU64,
    stores_created: AtomicU64,
    active_stores: AtomicU64,
    peak_active_stores: AtomicU64,
    component_instantiations: AtomicU64,
    invocations_started: AtomicU64,
    invocations_succeeded: AtomicU64,
    invocations_failed: AtomicU64,
    invocations_timed_out: AtomicU64,
    provider_input_bytes: AtomicU64,
    provider_output_bytes: AtomicU64,
    fuel_observations: AtomicU64,
    fuel_supplied: AtomicU64,
    fuel_consumed: AtomicU64,
    memory_growth_requests: AtomicU64,
    memory_growth_denied: AtomicU64,
    memory_growth_failed: AtomicU64,
    peak_memory_bytes_requested: AtomicU64,
    table_growth_requests: AtomicU64,
    table_growth_denied: AtomicU64,
    table_growth_failed: AtomicU64,
    peak_table_elements_requested: AtomicU64,
    http_requests: AtomicU64,
    http_request_bytes: AtomicU64,
    http_response_bytes: AtomicU64,
    storage_invocations: AtomicU64,
    storage_operations: AtomicU64,
    storage_syncs: AtomicU64,
    storage_quota_denials: AtomicU64,
    storage_read_bucket_max: AtomicU64,
    storage_write_bucket_max: AtomicU64,
}

impl BrokerHostMetrics {
    pub(crate) fn new(limits: BrokerHostLimits) -> Self {
        Self {
            inner: Arc::new(MetricsInner::default()),
            limits,
        }
    }

    /// Captures a point-in-time copy of the live counters.
    #[must_use]
    pub fn snapshot(&self) -> BrokerHostStats {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        BrokerHostStats {
            limits: self.limits.clone(),
            providers_loaded: load(&self.inner.providers_loaded),
            artifact_bytes: load(&self.inner.artifact_bytes),
            component_compilations: load(&self.inner.component_compilations),
            compilation_micros: load(&self.inner.compilation_micros),
            provider_descriptions: load(&self.inner.provider_descriptions),
            command_resolutions: load(&self.inner.command_resolutions),
            stores_created: load(&self.inner.stores_created),
            active_stores: load(&self.inner.active_stores),
            peak_active_stores: load(&self.inner.peak_active_stores),
            component_instantiations: load(&self.inner.component_instantiations),
            invocations_started: load(&self.inner.invocations_started),
            invocations_succeeded: load(&self.inner.invocations_succeeded),
            invocations_failed: load(&self.inner.invocations_failed),
            invocations_timed_out: load(&self.inner.invocations_timed_out),
            provider_input_bytes: load(&self.inner.provider_input_bytes),
            provider_output_bytes: load(&self.inner.provider_output_bytes),
            fuel_observations: load(&self.inner.fuel_observations),
            fuel_supplied: load(&self.inner.fuel_supplied),
            fuel_consumed: load(&self.inner.fuel_consumed),
            memory_growth_requests: load(&self.inner.memory_growth_requests),
            memory_growth_denied: load(&self.inner.memory_growth_denied),
            memory_growth_failed: load(&self.inner.memory_growth_failed),
            peak_memory_bytes_requested: load(&self.inner.peak_memory_bytes_requested),
            table_growth_requests: load(&self.inner.table_growth_requests),
            table_growth_denied: load(&self.inner.table_growth_denied),
            table_growth_failed: load(&self.inner.table_growth_failed),
            peak_table_elements_requested: load(&self.inner.peak_table_elements_requested),
            http_requests: load(&self.inner.http_requests),
            http_request_bytes: load(&self.inner.http_request_bytes),
            http_response_bytes: load(&self.inner.http_response_bytes),
            storage_invocations: load(&self.inner.storage_invocations),
            storage_operations: load(&self.inner.storage_operations),
            storage_syncs: load(&self.inner.storage_syncs),
            storage_quota_denials: load(&self.inner.storage_quota_denials),
            storage_read_bucket_max: load(&self.inner.storage_read_bucket_max),
            storage_write_bucket_max: load(&self.inner.storage_write_bucket_max),
        }
    }

    pub(crate) fn record_compilation(&self, elapsed: Duration, artifact_bytes: u64) {
        increment(&self.inner.component_compilations);
        add(&self.inner.compilation_micros, micros(elapsed));
        add(&self.inner.artifact_bytes, artifact_bytes);
    }

    pub(crate) fn record_provider_loaded(&self) {
        increment(&self.inner.providers_loaded);
    }

    pub(crate) fn record_description(&self) {
        increment(&self.inner.provider_descriptions);
    }

    pub(crate) fn record_command_resolution(&self) {
        increment(&self.inner.command_resolutions);
    }

    pub(crate) fn enter_store(&self) -> ActiveStore {
        increment(&self.inner.stores_created);
        let active = self
            .inner
            .active_stores
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        update_max(&self.inner.peak_active_stores, active);
        ActiveStore {
            metrics: self.clone(),
        }
    }

    pub(crate) fn record_instantiation(&self) {
        increment(&self.inner.component_instantiations);
    }

    pub(crate) fn record_fuel(&self, supplied: u64, remaining: u64) {
        increment(&self.inner.fuel_observations);
        add(&self.inner.fuel_supplied, supplied);
        add(
            &self.inner.fuel_consumed,
            supplied.saturating_sub(remaining),
        );
    }

    pub(crate) fn record_invocation_started(&self, input_bytes: usize) {
        increment(&self.inner.invocations_started);
        add(
            &self.inner.provider_input_bytes,
            u64::try_from(input_bytes).unwrap_or(u64::MAX),
        );
    }

    pub(crate) fn record_invocation_finished(
        &self,
        succeeded: bool,
        timed_out: bool,
        output_bytes: usize,
        http_calls: &[HttpCallEvidence],
        storage: Option<&StorageEvidence>,
    ) {
        if succeeded {
            increment(&self.inner.invocations_succeeded);
        } else {
            increment(&self.inner.invocations_failed);
        }
        if timed_out {
            increment(&self.inner.invocations_timed_out);
        }
        add(
            &self.inner.provider_output_bytes,
            u64::try_from(output_bytes).unwrap_or(u64::MAX),
        );
        add(
            &self.inner.http_requests,
            u64::try_from(http_calls.len()).unwrap_or(u64::MAX),
        );
        for call in http_calls {
            add(&self.inner.http_request_bytes, call.request_bytes);
            add(&self.inner.http_response_bytes, call.response_bytes);
        }
        if let Some(storage) = storage {
            increment(&self.inner.storage_invocations);
            add(&self.inner.storage_operations, storage.operations);
            add(&self.inner.storage_syncs, storage.syncs);
            add(&self.inner.storage_quota_denials, storage.quota_denials);
            update_max(
                &self.inner.storage_read_bucket_max,
                u64::from(storage.read_byte_bucket),
            );
            update_max(
                &self.inner.storage_write_bucket_max,
                u64::from(storage.write_byte_bucket),
            );
        }
    }

    fn record_memory_request(&self, desired: usize, allowed: bool) {
        increment(&self.inner.memory_growth_requests);
        update_max(
            &self.inner.peak_memory_bytes_requested,
            u64::try_from(desired).unwrap_or(u64::MAX),
        );
        if !allowed {
            increment(&self.inner.memory_growth_denied);
        }
    }

    fn record_memory_failure(&self) {
        increment(&self.inner.memory_growth_failed);
    }

    fn record_table_request(&self, desired: usize, allowed: bool) {
        increment(&self.inner.table_growth_requests);
        update_max(
            &self.inner.peak_table_elements_requested,
            u64::try_from(desired).unwrap_or(u64::MAX),
        );
        if !allowed {
            increment(&self.inner.table_growth_denied);
        }
    }

    fn record_table_failure(&self) {
        increment(&self.inner.table_growth_failed);
    }
}

/// Keeps the active-store gauge exact on success, error, cancellation, and timeout paths.
pub(crate) struct ActiveStore {
    metrics: BrokerHostMetrics,
}

impl Drop for ActiveStore {
    fn drop(&mut self) {
        self.metrics
            .inner
            .active_stores
            .fetch_sub(1, Ordering::Relaxed);
    }
}

/// Wasmtime's limiter plus observations of the resource requests it receives.
pub(crate) struct TrackingStoreLimits {
    inner: StoreLimits,
    metrics: BrokerHostMetrics,
}

impl TrackingStoreLimits {
    pub(crate) const fn new(inner: StoreLimits, metrics: BrokerHostMetrics) -> Self {
        Self { inner, metrics }
    }
}

impl ResourceLimiter for TrackingStoreLimits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let allowed = self.inner.memory_growing(current, desired, maximum)?;
        self.metrics.record_memory_request(desired, allowed);
        Ok(allowed)
    }

    fn memory_grow_failed(&mut self, error: Error) -> wasmtime::Result<()> {
        self.metrics.record_memory_failure();
        self.inner.memory_grow_failed(error)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let allowed = self.inner.table_growing(current, desired, maximum)?;
        self.metrics.record_table_request(desired, allowed);
        Ok(allowed)
    }

    fn table_grow_failed(&mut self, error: Error) -> wasmtime::Result<()> {
        self.metrics.record_table_failure();
        self.inner.table_grow_failed(error)
    }

    fn instances(&self) -> usize {
        self.inner.instances()
    }

    fn tables(&self) -> usize {
        self.inner.tables()
    }

    fn memories(&self) -> usize {
        self.inner.memories()
    }
}

fn increment(counter: &AtomicU64) {
    add(counter, 1);
}

fn add(counter: &AtomicU64, value: u64) {
    #[allow(
        clippy::let_underscore_must_use,
        reason = "the closure always returns `Some`, so `fetch_update` cannot report failure here"
    )]
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(value))
    });
}

fn update_max(counter: &AtomicU64, candidate: u64) {
    #[allow(
        clippy::let_underscore_must_use,
        reason = "the closure returns `None` exactly when the observed maximum already wins, so \
                  the `Err` is the intended no-op rather than a failure"
    )]
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        (candidate > current).then_some(candidate)
    });
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}
