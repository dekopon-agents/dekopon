//! Broker-owned bounded asynchronous WebAssembly provider hosting.
//!
//! The current immediate host intentionally has an empty linker. This crate is the privileged
//! counterpart intended only for a separately deployed broker: it accepts an
//! [`AuthorizedInvocation`], links only the project-owned buffered HTTP and namespace-bound
//! storage interfaces, and applies the invocation's exact host-call constraints in a fresh store.

#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    fmt,
    io::Read as _,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use dekopon_capability::{AuthorizedInvocation, ExecutionConstraints};
use dekopon_core::{CapabilityId, ProviderId};
use dekopon_provider_sdk::host::CommandExport;
pub use dekopon_provider_sdk::host::ProviderConflicts;
use dekopon_provider_sdk::host::{
    self, CommandExportProblem, ConflictScan, ConflictWording, EngineError, RESOLVE_COMMAND_EXPORT,
    RUN_COMMAND_EXPORT, StoreLimits, check_command_export, command_export, command_input_bytes,
    parse_command_run,
};
pub use dekopon_provider_sdk::{
    CommandResolution, CommandRunOutcome, ComponentFailure, ComponentResponse, ProviderApiVersion,
    ProviderCapability, ProviderManifest,
};
use dekopon_storage_host::{StorageEvidence, StorageGrant, StorageHost};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tokio::time::timeout;
use tracing::Instrument as _;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Engine, Store};

mod http;
mod metadata;
mod metrics;
mod storage;
pub use http::{BoundCredential, HttpCallEvidence, HttpConfigurationError};
use http::{HttpCeilings, HttpState};
pub use metadata::{ComponentInterfaceItem, LoadedProviderMetadata};
use metadata::{component_interface, identify_bytes};
use metrics::{ActiveStore, TrackingStoreLimits};
pub use metrics::{BrokerHostMetrics, BrokerHostStats};

pub(crate) mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "provider",
        imports: { default: async | trappable },
        exports: { default: async },
        with: {
            "dekopon:storage/durable-files/file": crate::storage::FileResource,
        },
    });
}

/// Provider export package mirrored into the broker host bindings.
pub const PROVIDER_WIT: &str = include_str!("../wit/deps/provider.wit");
/// Buffered HTTP package mirrored into the broker host bindings.
pub const HTTP_WIT: &str = include_str!("../wit/deps/http.wit");
/// Namespace-bound storage package mirrored into the broker host bindings.
pub const STORAGE_WIT: &str = include_str!("../wit/deps/storage.wit");

/// Hard maximum source bytes for one provider component (64 MiB).
pub const HARD_MAX_PROVIDER_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;
/// Default maximum size of each linear memory in one store (64 MiB).
#[deprecated(since = "0.12.0", note = "moved to dekopon_provider_sdk::host")]
pub const DEFAULT_MAX_MEMORY_BYTES: usize = host::DEFAULT_MAX_MEMORY_BYTES;
/// Default maximum elements in each Wasm table.
#[deprecated(since = "0.12.0", note = "moved to dekopon_provider_sdk::host")]
pub const DEFAULT_MAX_TABLE_ELEMENTS: usize = host::DEFAULT_MAX_TABLE_ELEMENTS;
/// Default maximum core instances in one store.
#[deprecated(since = "0.12.0", note = "moved to dekopon_provider_sdk::host")]
pub const DEFAULT_MAX_INSTANCES: usize = host::DEFAULT_MAX_INSTANCES;
/// Default maximum tables in one store.
#[deprecated(since = "0.12.0", note = "moved to dekopon_provider_sdk::host")]
pub const DEFAULT_MAX_TABLES: usize = host::DEFAULT_MAX_TABLES;
/// Default maximum linear memories in one store.
#[deprecated(since = "0.12.0", note = "moved to dekopon_provider_sdk::host")]
pub const DEFAULT_MAX_MEMORIES: usize = host::DEFAULT_MAX_MEMORIES;
/// Default maximum serialized provider input size (1 MiB).
#[deprecated(since = "0.12.0", note = "moved to dekopon_provider_sdk::host")]
pub const DEFAULT_MAX_INPUT_BYTES: usize = host::DEFAULT_MAX_INPUT_BYTES;
/// Default maximum serialized provider output or manifest size (1 MiB).
#[deprecated(since = "0.12.0", note = "moved to dekopon_provider_sdk::host")]
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = host::DEFAULT_MAX_OUTPUT_BYTES;
/// Default maximum HTTP calls in one invocation.
pub const DEFAULT_MAX_HTTP_REQUESTS: u32 = 32;
/// Default maximum accounted HTTP request bytes (1 MiB).
pub const DEFAULT_MAX_HTTP_REQUEST_BYTES: u64 = 1024 * 1024;
/// Default maximum accounted HTTP response bytes (4 MiB).
pub const DEFAULT_MAX_HTTP_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
/// Default maximum HTTP header count in either direction.
pub const DEFAULT_MAX_HTTP_HEADERS: usize = 128;
/// Default maximum aggregate HTTP header bytes in either direction (64 KiB).
pub const DEFAULT_MAX_HTTP_HEADER_BYTES: usize = 64 * 1024;
/// Default Wasm instruction fuel supplied to each store.
///
/// Durable-memory compaction parses the bounded near-threshold turn and dedup files and emits one
/// multi-megabyte replacement inside the guest. The independent wall-clock deadline still bounds
/// execution; this ceiling keeps the default valid memory limits from deterministically trapping
/// on their first full compaction.
pub const DEFAULT_FUEL: u64 = 8_000_000_000;
/// Host ceiling for one provider description or invocation.
pub const DEFAULT_MAX_TIMEOUT: Duration = Duration::from_secs(30);

/// Broker-owned ceilings that authorization may narrow but never widen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrokerHostLimits {
    /// Maximum linear memory in one fresh store.
    pub max_memory_bytes: usize,
    /// Maximum elements in each Wasm table.
    pub max_table_elements: usize,
    /// Maximum core instances in one store.
    pub max_instances: usize,
    /// Maximum tables in one store.
    pub max_tables: usize,
    /// Maximum linear memories in one store.
    pub max_memories: usize,
    /// Maximum serialized invocation input.
    pub max_input_bytes: usize,
    /// Maximum serialized manifest or invocation output.
    pub max_output_bytes: usize,
    /// Maximum HTTP calls accepted in one invocation.
    pub max_http_requests: u32,
    /// Maximum authorized accounted request bytes.
    pub max_http_request_bytes: u64,
    /// Maximum authorized accounted response bytes.
    pub max_http_response_bytes: u64,
    /// Maximum HTTP header count in a request or response.
    pub max_http_headers: usize,
    /// Maximum aggregate HTTP header bytes in a request or response.
    pub max_http_header_bytes: usize,
    /// Wasm fuel in one fresh store.
    pub fuel: u64,
    /// Maximum wall-clock duration accepted from an authorization.
    pub max_timeout: Duration,
}

impl Default for BrokerHostLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: host::DEFAULT_MAX_MEMORY_BYTES,
            max_table_elements: host::DEFAULT_MAX_TABLE_ELEMENTS,
            max_instances: host::DEFAULT_MAX_INSTANCES,
            max_tables: host::DEFAULT_MAX_TABLES,
            max_memories: host::DEFAULT_MAX_MEMORIES,
            max_input_bytes: host::DEFAULT_MAX_INPUT_BYTES,
            max_output_bytes: host::DEFAULT_MAX_OUTPUT_BYTES,
            max_http_requests: DEFAULT_MAX_HTTP_REQUESTS,
            max_http_request_bytes: DEFAULT_MAX_HTTP_REQUEST_BYTES,
            max_http_response_bytes: DEFAULT_MAX_HTTP_RESPONSE_BYTES,
            max_http_headers: DEFAULT_MAX_HTTP_HEADERS,
            max_http_header_bytes: DEFAULT_MAX_HTTP_HEADER_BYTES,
            fuel: DEFAULT_FUEL,
            max_timeout: DEFAULT_MAX_TIMEOUT,
        }
    }
}

/// Operational broker-host settings that are deliberately not part of the authority surface.
///
/// Nothing here narrows or widens what an authorization may do, which is why it is separate from
/// [`BrokerHostLimits`]: the broker commits its host ceilings into the effective-authority
/// generation, and pointing a compilation cache at a different directory must not rotate that.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BrokerHostOptions {
    /// Absolute directory for Wasmtime's content-addressed compilation cache.
    ///
    /// `None` recompiles every provider with Cranelift at every start. A deployment whose pods roll
    /// inside a startup-probe budget wants this pointed at durable broker-owned state; the
    /// directory must be writable only by the broker, because its contents are compiled code.
    pub compile_cache_dir: Option<PathBuf>,
    /// Aggregate guest linear memory reservable across concurrently live stores.
    ///
    /// [`BrokerHostLimits::max_memory_bytes`] bounds one invocation; this bounds all of them at
    /// once, so a daemon that accepts many connections refuses cleanly instead of being OOM-killed.
    /// `None` leaves the aggregate unbounded, which is only safe when the connection ceiling
    /// multiplied by the per-store ceiling still fits the container.
    pub max_total_memory_bytes: Option<usize>,
}

/// One content-locked provider component the broker may compile.
///
/// The expected identity is checked against the exact buffer passed to Wasmtime, rather than a
/// separate preflight read. This closes the gap between a generated provider lock and the bytes the
/// privileged host actually executes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockedProviderSource {
    path: PathBuf,
    artifact_bytes: u64,
    artifact_sha256: String,
    provider_id: ProviderId,
}

impl LockedProviderSource {
    /// Creates a locked source after validating the canonical lowercase SHA-256 spelling.
    ///
    /// # Errors
    ///
    /// Returns [`BrokerHostError::InvalidArtifactSize`] when the expected byte length is zero or
    /// above the hard source ceiling, and [`BrokerHostError::InvalidArtifactDigest`] when
    /// `artifact_sha256` is not exactly sixty-four lowercase hexadecimal characters.
    pub fn new(
        path: impl Into<PathBuf>,
        artifact_bytes: u64,
        artifact_sha256: impl Into<String>,
        provider_id: ProviderId,
    ) -> Result<Self, BrokerHostError> {
        let artifact_sha256 = artifact_sha256.into();
        if artifact_bytes == 0 || artifact_bytes > HARD_MAX_PROVIDER_COMPONENT_BYTES {
            return Err(BrokerHostError::InvalidArtifactSize {
                size: artifact_bytes,
                maximum: HARD_MAX_PROVIDER_COMPONENT_BYTES,
            });
        }
        if artifact_sha256.len() != 64
            || !artifact_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(BrokerHostError::InvalidArtifactDigest);
        }
        Ok(Self {
            path: path.into(),
            artifact_bytes,
            artifact_sha256,
            provider_id,
        })
    }

    /// Returns the local component path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the expected component byte length.
    #[must_use]
    pub const fn artifact_bytes(&self) -> u64 {
        self.artifact_bytes
    }

    /// Returns the expected lowercase SHA-256 digest.
    #[must_use]
    pub fn artifact_sha256(&self) -> &str {
        &self.artifact_sha256
    }

    /// Returns the provider identity the component must describe.
    #[must_use]
    pub fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }
}

#[derive(Clone)]
struct ProviderSource {
    path: PathBuf,
    expected: Option<LockedProviderSource>,
}

impl From<PathBuf> for ProviderSource {
    fn from(path: PathBuf) -> Self {
        Self {
            path,
            expected: None,
        }
    }
}

impl From<LockedProviderSource> for ProviderSource {
    fn from(expected: LockedProviderSource) -> Self {
        Self {
            path: expected.path.clone(),
            expected: Some(expected),
        }
    }
}

/// Upper bound on the fuel a store may burn between async yields.
///
/// A store holding billions of units of fuel must still hand the executor back often enough for the
/// wall-clock deadline to fire, so the interval is capped independently of the fuel ceiling. Kept
/// private so the policy has exactly one definition: [`BrokerHostLimits::fuel_yield_interval`].
const MAX_FUEL_YIELD_INTERVAL: u64 = 10_000;

impl BrokerHostLimits {
    /// Returns the fuel interval every store built from these limits actually yields on.
    ///
    /// An operational view that re-derived this from [`fuel`](Self::fuel) would keep displaying the
    /// old formula after the policy changed, with no compile error and no failing test.
    #[must_use]
    pub const fn fuel_yield_interval(&self) -> u64 {
        if self.fuel < MAX_FUEL_YIELD_INTERVAL {
            self.fuel
        } else {
            MAX_FUEL_YIELD_INTERVAL
        }
    }

    /// The subset of these ceilings Wasmtime enforces on one fresh store.
    fn store_bounds(&self) -> StoreLimits {
        StoreLimits {
            max_memory_bytes: self.max_memory_bytes,
            max_table_elements: self.max_table_elements,
            max_instances: self.max_instances,
            max_tables: self.max_tables,
            max_memories: self.max_memories,
        }
    }
}

/// Failed broker-provider invocation and the evidence for calls that already executed.
///
/// An invocation can fail after the guest has already dispatched authorized HTTP requests, so
/// the terminal failure carries the same sanitized metadata a success would have carried. The
/// evidence is empty only when the failure preceded any dispatch.
#[derive(Debug)]
pub struct BrokerInvocationFailure {
    /// Host failure that ended the invocation.
    pub error: Box<BrokerHostError>,
    /// Sanitized metadata for every HTTP call dispatched before the failure.
    pub http_calls: Vec<HttpCallEvidence>,
    /// Content-free storage evidence when a storage transaction began.
    pub storage: Option<StorageEvidence>,
}

impl From<BrokerHostError> for BrokerInvocationFailure {
    fn from(error: BrokerHostError) -> Self {
        Self {
            error: Box::new(error),
            http_calls: Vec::new(),
            storage: None,
        }
    }
}

impl fmt::Display for BrokerInvocationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for BrokerInvocationFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// Successful broker-provider output and bounded HTTP evidence metadata.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrokerInvocationOutput {
    /// Provider selected by the trusted capability route.
    pub provider: ProviderId,
    /// Invoked capability.
    pub capability: CapabilityId,
    /// Valid JSON returned by the provider.
    pub output: Value,
    /// Sanitized HTTP metadata emitted by the host.
    pub http_calls: Vec<HttpCallEvidence>,
    /// Content-free storage evidence when this invocation used storage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage: Option<StorageEvidence>,
}

/// Aggregate guest-memory reservation shared by every live store in one runtime.
///
/// Reservation is pessimistic — a store books its whole per-invocation ceiling whether or not the
/// guest ever grows into it — because the point is to refuse before the allocation exists.
#[derive(Debug)]
struct MemoryBudget {
    maximum: usize,
    reserved: AtomicUsize,
}

impl MemoryBudget {
    fn reserve(self: &Arc<Self>, bytes: usize) -> Option<MemoryReservation> {
        self.reserved
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current
                    .checked_add(bytes)
                    .filter(|next| *next <= self.maximum)
            })
            .ok()?;
        Some(MemoryReservation {
            budget: Arc::clone(self),
            bytes,
        })
    }
}

/// Releases one store's reservation on every path a store can end on, including cancellation.
struct MemoryReservation {
    budget: Arc<MemoryBudget>,
    bytes: usize,
}

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        self.budget
            .reserved
            .fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

struct Runtime {
    engine: Engine,
    // One linker for the whole process. Its contents are fixed by the generated bindings, so
    // rebuilding it per call only re-registered the same host functions and forced every
    // instantiation to resolve imports from scratch.
    linker: Linker<StoreState>,
    limits: BrokerHostLimits,
    metrics: BrokerHostMetrics,
    memory_budget: Option<Arc<MemoryBudget>>,
}

impl Runtime {
    fn new(limits: BrokerHostLimits, options: &BrokerHostOptions) -> Result<Self, BrokerHostError> {
        validate_limits(&limits)?;
        if options
            .max_total_memory_bytes
            .is_some_and(|maximum| maximum < limits.max_memory_bytes)
        {
            return Err(BrokerHostError::InvalidLimit {
                name: "max_total_memory_bytes",
            });
        }
        let metrics = BrokerHostMetrics::new(limits.clone());
        let mut config = host::config();
        // Asynchronous execution: the guest yields on a fuel interval so a Tokio deadline can
        // cancel it without a process-wide epoch interrupt.
        config.async_support(true);
        let engine = host::engine(config, options.compile_cache_dir.as_deref()).map_err(
            |error| match error {
                EngineError::CompileCache { path, source } => {
                    BrokerHostError::CompileCache { path, source }
                }
                EngineError::Engine { source } => BrokerHostError::Engine { source },
            },
        )?;
        let mut linker = Linker::new(&engine);
        bindings::Provider::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(|source| BrokerHostError::Linker { source })?;
        Ok(Self {
            engine,
            linker,
            limits,
            metrics,
            memory_budget: options.max_total_memory_bytes.map(|maximum| {
                Arc::new(MemoryBudget {
                    maximum,
                    reserved: AtomicUsize::new(0),
                })
            }),
        })
    }

    fn store(
        &self,
        http: HttpState,
        storage: storage::StorageState,
    ) -> Result<Store<StoreState>, BrokerHostError> {
        let reserved = match &self.memory_budget {
            Some(budget) => Some(budget.reserve(self.limits.max_memory_bytes).ok_or(
                BrokerHostError::MemoryBudgetExhausted {
                    requested: self.limits.max_memory_bytes,
                    maximum: budget.maximum,
                },
            )?),
            None => None,
        };
        let limits = self.limits.store_bounds().store_limits();
        let active = self.metrics.enter_store();
        let mut store = Store::new(
            &self.engine,
            StoreState {
                limits: TrackingStoreLimits::new(limits, self.metrics.clone()),
                http,
                storage,
                table: storage::new_table(),
                fuel_recorded: false,
                provider_output_bytes: 0,
                _active: active,
                _reserved: reserved,
            },
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(self.limits.fuel)
            .map_err(|source| BrokerHostError::Store { source })?;
        store
            .fuel_async_yield_interval(Some(self.limits.fuel_yield_interval()))
            .map_err(|source| BrokerHostError::Store { source })?;
        Ok(store)
    }

    fn record_fuel(&self, store: &mut Store<StoreState>) {
        if store.data().fuel_recorded {
            return;
        }
        let remaining = store.get_fuel();
        store.data_mut().fuel_recorded = true;
        if let Ok(remaining) = remaining {
            self.metrics.record_fuel(self.limits.fuel, remaining);
        }
    }

    fn http_ceilings(&self) -> HttpCeilings {
        HttpCeilings {
            max_requests: self.limits.max_http_requests,
            max_request_bytes: self.limits.max_http_request_bytes,
            max_response_bytes: self.limits.max_http_response_bytes,
            max_headers: self.limits.max_http_headers,
            max_header_bytes: self.limits.max_http_header_bytes,
        }
    }
}

struct StoreState {
    limits: TrackingStoreLimits,
    http: HttpState,
    storage: storage::StorageState,
    table: wasmtime::component::ResourceTable,
    fuel_recorded: bool,
    provider_output_bytes: usize,
    _active: ActiveStore,
    _reserved: Option<MemoryReservation>,
}

impl bindings::dekopon::http::client::Host for StoreState {
    async fn send(
        &mut self,
        request: bindings::dekopon::http::client::Request,
    ) -> wasmtime::Result<
        Result<
            bindings::dekopon::http::client::Response,
            bindings::dekopon::http::client::HttpError,
        >,
    > {
        Ok(self.http.send(request).await)
    }
}

/// One provider component after Cranelift, before it has described itself.
struct CompiledComponent {
    source: PathBuf,
    expected_provider_id: Option<ProviderId>,
    artifact_bytes: u64,
    artifact_sha256: String,
    compile_ms: u64,
    pre: bindings::ProviderPre<StoreState>,
    imports: Vec<ComponentInterfaceItem>,
    exports: Vec<ComponentInterfaceItem>,
    interface_truncated: bool,
    command_export: CommandExport,
}

/// One provider component compiled by the broker host.
pub struct BrokerWasmProvider {
    runtime: Arc<Runtime>,
    // Imports are resolved and type-checked once here rather than on every call. Wasmtime's own
    // shape for repeated instantiation, and the difference between a link per chat-message
    // capability call and none.
    pre: bindings::ProviderPre<StoreState>,
    source: PathBuf,
    artifact_bytes: u64,
    artifact_sha256: String,
    imports: Vec<ComponentInterfaceItem>,
    exports: Vec<ComponentInterfaceItem>,
    interface_truncated: bool,
    manifest: ProviderManifest,
    /// Which command export the component offers, read once from its type at compile.
    command_export: CommandExport,
}

impl fmt::Debug for BrokerWasmProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrokerWasmProvider")
            .field("source", &self.source)
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

/// Compiles one provider component, off the asynchronous runtime.
///
/// Everything here is CPU-bound Cranelift work with no `await` in it, which is why the registry
/// hands it to the blocking pool: three providers on a four-core host should not compile one at a
/// time while the socket stays unbound.
fn compile_component(
    runtime: &Runtime,
    source: ProviderSource,
) -> Result<CompiledComponent, BrokerHostError> {
    // Open and read once. A digest taken from a second read cannot prove it describes the bytes
    // Cranelift consumed. Locked metadata is checked on this descriptor before allocation, then the
    // read itself is capped at one byte beyond the applicable limit so concurrent growth cannot
    // turn a trusted startup input into an unbounded allocation.
    let file =
        std::fs::File::open(&source.path).map_err(|error| BrokerHostError::ArtifactMetadata {
            path: source.path.clone(),
            source: error,
        })?;
    let metadata = file
        .metadata()
        .map_err(|error| BrokerHostError::ArtifactMetadata {
            path: source.path.clone(),
            source: error,
        })?;
    let maximum = source
        .expected
        .as_ref()
        .map_or(HARD_MAX_PROVIDER_COMPONENT_BYTES, |expected| {
            expected.artifact_bytes
        });
    if let Some(expected) = &source.expected
        && metadata.len() != expected.artifact_bytes
    {
        return Err(BrokerHostError::ArtifactSizeMismatch {
            path: source.path,
            expected: expected.artifact_bytes,
            actual: metadata.len(),
        });
    }
    if metadata.len() > maximum {
        return Err(BrokerHostError::ArtifactTooLarge {
            path: source.path,
            actual: metadata.len(),
            maximum,
        });
    }
    let mut bytes = Vec::new();
    file.take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| BrokerHostError::ArtifactMetadata {
            path: source.path.clone(),
            source: error,
        })?;
    let actual = bytes.len() as u64;
    if actual > maximum {
        return match &source.expected {
            Some(expected) => Err(BrokerHostError::ArtifactSizeMismatch {
                path: source.path,
                expected: expected.artifact_bytes,
                actual,
            }),
            None => Err(BrokerHostError::ArtifactTooLarge {
                path: source.path,
                actual,
                maximum,
            }),
        };
    }
    let artifact = identify_bytes(&bytes);
    if let Some(expected) = &source.expected {
        if artifact.bytes != expected.artifact_bytes {
            return Err(BrokerHostError::ArtifactSizeMismatch {
                path: source.path,
                expected: expected.artifact_bytes,
                actual: artifact.bytes,
            });
        }
        if artifact.sha256 != expected.artifact_sha256 {
            return Err(BrokerHostError::ArtifactDigestMismatch {
                path: source.path,
                expected: expected.artifact_sha256.clone(),
                actual: artifact.sha256,
            });
        }
    }
    let expected_provider_id = source.expected.map(|expected| expected.provider_id);
    let source = source.path;
    // Compilation happens once per provider at startup rather than per invocation, so this span
    // answers "why was the broker slow to become ready", not "why was that call slow".
    let compile = tracing::info_span!(
        "provider.compile",
        path = %source.display(),
        artifact_bytes = artifact.bytes,
        elapsed_ms = tracing::field::Empty,
    );
    let started = Instant::now();
    let component = compile
        .in_scope(|| Component::new(&runtime.engine, &bytes))
        .map_err(|error| BrokerHostError::Compile {
            path: source.clone(),
            source: error,
        })?;
    let elapsed = started.elapsed();
    let compile_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    compile.record("elapsed_ms", compile_ms);
    runtime.metrics.record_compilation(elapsed, artifact.bytes);
    let (imports, exports, interface_truncated) = component_interface(&runtime.engine, &component);
    let command_export = command_export(&runtime.engine, &component);
    let pre = runtime
        .linker
        .instantiate_pre(&component)
        .and_then(bindings::ProviderPre::new)
        .map_err(|error| BrokerHostError::Instantiate {
            path: source.clone(),
            source: error,
        })?;
    Ok(CompiledComponent {
        source,
        expected_provider_id,
        artifact_bytes: artifact.bytes,
        artifact_sha256: artifact.sha256,
        compile_ms,
        pre,
        imports,
        exports,
        interface_truncated,
        command_export,
    })
}

impl BrokerWasmProvider {
    async fn load(
        runtime: Arc<Runtime>,
        compiled: CompiledComponent,
    ) -> Result<Self, BrokerHostError> {
        let CompiledComponent {
            source,
            expected_provider_id,
            artifact_bytes,
            artifact_sha256,
            compile_ms,
            pre,
            imports,
            exports,
            interface_truncated,
            command_export,
        } = compiled;
        let manifest_json = describe_component(&runtime, &pre, &source).await?;
        if manifest_json.len() > runtime.limits.max_output_bytes {
            return Err(BrokerHostError::OutputTooLarge {
                provider: source.display().to_string(),
                length: manifest_json.len(),
                maximum: runtime.limits.max_output_bytes,
            });
        }
        let manifest =
            serde_json::from_str::<ProviderManifest>(&manifest_json).map_err(|error| {
                BrokerHostError::InvalidManifest {
                    path: source.clone(),
                    source: error,
                }
            })?;
        validate_manifest(&manifest, &source)?;
        if let Some(expected) = expected_provider_id
            && manifest.id != expected
        {
            return Err(BrokerHostError::ProviderIdentityMismatch {
                path: source,
                expected,
                actual: manifest.id,
            });
        }
        // A manifest that promises command words the component cannot run would fail at the
        // first `gh …` a model typed, in a session, hours later. Prove it at load instead — from
        // the component's own type, which distinguishes "no such export" from "wrong signature".
        if let Err(problem) = check_command_export(&manifest, &command_export) {
            return Err(match problem {
                CommandExportProblem::Missing => BrokerHostError::MissingCommandExport {
                    provider: manifest.id.clone(),
                    path: source.clone(),
                },
                CommandExportProblem::Mismatched { name, found } => {
                    BrokerHostError::CommandExportSignature {
                        provider: manifest.id.clone(),
                        path: source.clone(),
                        name,
                        found,
                    }
                }
            });
        }
        runtime.metrics.record_provider_loaded();
        tracing::info!(
            provider = %manifest.id,
            path = %source.display(),
            artifact_bytes,
            artifact_sha256 = &artifact_sha256[..artifact_sha256.len().min(12)],
            compile_ms,
            capabilities = manifest.capabilities.len(),
            command_words = manifest.command_words.len(),
            command_export = command_export_name(&command_export),
            "loaded broker provider"
        );
        Ok(Self {
            runtime,
            pre,
            source,
            artifact_bytes,
            artifact_sha256,
            imports,
            exports,
            interface_truncated,
            manifest,
            command_export,
        })
    }

    /// Runs one command word's argv inside the guest and returns the JSON it produced.
    ///
    /// Bounded exactly as `describe` is: import-free, timed out, input- and output-capped. The run
    /// happens *before* authorization, so a component that reaches for a host import here is
    /// refused rather than trusted. Which export is called was decided once at load from the
    /// component's type: `run-command` receives `argv` and `stdin`; a legacy `resolve-command`
    /// guest receives only `argv`, because its contract has no piped value — `stdin` is dropped for
    /// it by contract, not lost.
    ///
    /// # Errors
    ///
    /// Returns [`BrokerHostError::CommandInputTooLarge`] before instantiation when `argv` plus
    /// `stdin` exceed the host's input bound, and any instantiation, trap, timeout, host-import, or
    /// output-size failure from the run itself.
    pub async fn run_command(
        &self,
        argv: &[String],
        stdin: Option<&str>,
    ) -> Result<String, BrokerHostError> {
        let length = command_input_bytes(argv, stdin);
        if length > self.runtime.limits.max_input_bytes {
            return Err(BrokerHostError::CommandInputTooLarge {
                provider: self.manifest.id.clone(),
                length,
                maximum: self.runtime.limits.max_input_bytes,
            });
        }
        let export_name = match &self.command_export {
            CommandExport::RunCommand => RUN_COMMAND_EXPORT,
            CommandExport::ResolveCommand => RESOLVE_COMMAND_EXPORT,
            CommandExport::Absent => {
                return Err(BrokerHostError::MissingCommandExport {
                    provider: self.manifest.id.clone(),
                    path: self.source.clone(),
                });
            }
            CommandExport::Mismatched { name, found } => {
                return Err(BrokerHostError::CommandExportSignature {
                    provider: self.manifest.id.clone(),
                    path: self.source.clone(),
                    name,
                    found: found.clone(),
                });
            }
        };
        self.runtime.metrics.record_command_resolution();
        let operation_timeout = self.runtime.limits.max_timeout;
        let http = HttpState::describe(self.runtime.http_ceilings(), operation_timeout)
            .map_err(|source| BrokerHostError::HttpConfiguration { source })?;
        let mut store = self
            .runtime
            .store(http, storage::StorageState::disabled())?;
        let argv = argv.to_vec();
        let stdin = stdin.map(str::to_owned);
        let signature = |source: wasmtime::Error| BrokerHostError::CommandExportSignature {
            provider: self.manifest.id.clone(),
            path: self.source.clone(),
            name: export_name,
            found: source.to_string(),
        };
        let failed = |source: wasmtime::Error| BrokerHostError::RunCommand {
            provider: self.manifest.id.clone(),
            source,
        };
        let operation = async {
            let instance = self
                .pre
                .instance_pre()
                .instantiate_async(&mut store)
                .await
                .map_err(|source| BrokerHostError::Instantiate {
                    path: self.source.clone(),
                    source,
                })?;
            self.runtime.metrics.record_instantiation();
            let output = if export_name == RUN_COMMAND_EXPORT {
                let function = instance
                    .get_typed_func::<(Vec<String>, Option<String>), (String,)>(
                        &mut store,
                        RUN_COMMAND_EXPORT,
                    )
                    .map_err(signature)?;
                let (output,) = function
                    .call_async(&mut store, (argv, stdin))
                    .await
                    .map_err(failed)?;
                function
                    .post_return_async(&mut store)
                    .await
                    .map_err(failed)?;
                output
            } else {
                let function = instance
                    .get_typed_func::<(Vec<String>,), (String,)>(&mut store, RESOLVE_COMMAND_EXPORT)
                    .map_err(signature)?;
                let (output,) = function
                    .call_async(&mut store, (argv,))
                    .await
                    .map_err(failed)?;
                function
                    .post_return_async(&mut store)
                    .await
                    .map_err(failed)?;
                output
            };
            Ok::<_, BrokerHostError>(output)
        };
        #[allow(
            clippy::map_err_ignore,
            reason = "`tokio::time::error::Elapsed` carries only \"deadline has elapsed\"; the \
                      Timeout variant already names the operation and the budget it exceeded"
        )]
        let output =
            timeout(operation_timeout, operation)
                .await
                .map_err(|_| BrokerHostError::Timeout {
                    operation: format!("{export_name} {}", self.manifest.id),
                    timeout_ms: operation_timeout.as_millis() as u64,
                });
        self.runtime.record_fuel(&mut store);
        let output = output??;
        if store.data().http.attempted() || store.data().storage.attempted() {
            return Err(BrokerHostError::RunCommandUsedHostImport {
                path: self.source.clone(),
            });
        }
        if output.len() > self.runtime.limits.max_output_bytes {
            return Err(BrokerHostError::OutputTooLarge {
                provider: self.manifest.id.to_string(),
                length: output.len(),
                maximum: self.runtime.limits.max_output_bytes,
            });
        }
        Ok(output)
    }

    async fn invoke(
        &self,
        capability: &CapabilityId,
        input: &Value,
        constraints: &ExecutionConstraints,
        credential: Option<BoundCredential>,
        storage_transaction: Option<dekopon_storage_host::StorageTransaction>,
    ) -> Result<BrokerInvocationOutput, BrokerInvocationFailure> {
        validate_authorized_constraints(constraints, &self.runtime.limits)?;
        if !self
            .manifest
            .capabilities
            .iter()
            .any(|candidate| &candidate.id == capability)
        {
            return Err(BrokerHostError::ProviderDoesNotImplement {
                provider: self.manifest.id.clone(),
                capability: capability.clone(),
            }
            .into());
        }
        if !input.is_object() {
            return Err(BrokerHostError::InputNotObject {
                capability: capability.clone(),
            }
            .into());
        }
        let input_json = serde_json::to_string(input)
            .map_err(|source| BrokerHostError::SerializeInput { source })?;
        if input_json.len() > self.runtime.limits.max_input_bytes {
            return Err(BrokerHostError::InputTooLarge {
                capability: capability.clone(),
                length: input_json.len(),
                maximum: self.runtime.limits.max_input_bytes,
            }
            .into());
        }

        let storage_backed = storage_transaction.is_some();
        self.runtime
            .metrics
            .record_invocation_started(if storage_backed { 0 } else { input_json.len() });
        let operation_timeout = Duration::from_millis(constraints.timeout_ms);
        let http = match HttpState::invoke(
            constraints.http.clone(),
            constraints.secret_use.clone(),
            credential,
            self.runtime.http_ceilings(),
            operation_timeout,
        ) {
            Ok(http) => http,
            Err(source) => {
                self.runtime
                    .metrics
                    .record_invocation_finished(false, false, 0, &[], None);
                return Err(BrokerHostError::HttpConfiguration { source }.into());
            }
        };
        let storage_state = storage_transaction.map_or_else(
            storage::StorageState::disabled,
            storage::StorageState::active,
        );
        let mut store = match self.runtime.store(http, storage_state) {
            Ok(store) => store,
            Err(error) => {
                self.runtime
                    .metrics
                    .record_invocation_finished(false, false, 0, &[], None);
                return Err(error.into());
            }
        };
        // The store outlives the guest on every path, including the one where the timeout drops
        // the operation future, so evidence for dispatched calls is harvested exactly once and
        // reaches the caller whether the invocation succeeded or failed.
        let mut executed = self
            .execute_in_store(
                &mut store,
                capability,
                &input_json,
                constraints,
                operation_timeout,
            )
            .await;
        let commit = executed.is_ok();
        let storage_output = executed
            .as_ref()
            .ok()
            .and_then(|output| serde_json::to_vec(output).ok());
        if let Err(source) = store
            .data_mut()
            .storage
            .finish(commit, storage_output)
            .await
        {
            executed = Err(BrokerHostError::Storage { source });
        }
        self.runtime.record_fuel(&mut store);
        let output_bytes = if storage_backed {
            0
        } else {
            store.data().provider_output_bytes
        };
        let timed_out = matches!(&executed, Err(BrokerHostError::Timeout { .. }));
        let mut data = store.into_data();
        let storage = data.storage.take_evidence();
        let http_calls = data.http.into_evidence();
        self.runtime.metrics.record_invocation_finished(
            executed.is_ok(),
            timed_out,
            output_bytes,
            &http_calls,
            storage.as_ref(),
        );
        match executed {
            Ok(output) => Ok(BrokerInvocationOutput {
                provider: self.manifest.id.clone(),
                capability: capability.clone(),
                output,
                http_calls,
                storage,
            }),
            Err(error) => Err(BrokerInvocationFailure {
                error: Box::new(error),
                http_calls,
                storage,
            }),
        }
    }

    /// Runs one guest invocation to a terminal outcome, leaving its store to the caller.
    async fn execute_in_store(
        &self,
        store: &mut Store<StoreState>,
        capability: &CapabilityId,
        input_json: &str,
        constraints: &ExecutionConstraints,
        operation_timeout: Duration,
    ) -> Result<Value, BrokerHostError> {
        let operation = async {
            let bindings = self
                .pre
                .instantiate_async(&mut *store)
                .await
                .map_err(|source| BrokerHostError::Instantiate {
                    path: self.source.clone(),
                    source,
                })?;
            self.runtime.metrics.record_instantiation();
            bindings
                .call_invoke(&mut *store, capability.as_str(), input_json)
                .await
                .map_err(|source| BrokerHostError::Invoke {
                    provider: self.manifest.id.clone(),
                    capability: capability.clone(),
                    source,
                })
        };
        #[allow(
            clippy::map_err_ignore,
            reason = "`tokio::time::error::Elapsed` carries only \"deadline has elapsed\"; the \
                      Timeout variant already names the operation and the budget it exceeded"
        )]
        let operation_result =
            timeout(operation_timeout, operation)
                .await
                .map_err(|_| BrokerHostError::Timeout {
                    operation: format!("invoke {capability}"),
                    timeout_ms: constraints.timeout_ms,
                })?;
        // Sticky host authority wins even when the guest catches the typed error or a failing
        // resource destructor turns it into a component trap. Inspect policy state before
        // propagating the guest call result.
        if let Some(reason) = store.data().http.policy_violation() {
            return Err(BrokerHostError::HostCallRejected {
                provider: self.manifest.id.clone(),
                capability: capability.clone(),
                reason,
            });
        }
        if let Some(reason) = store.data().storage.violation() {
            return Err(BrokerHostError::StorageCallRejected {
                provider: self.manifest.id.clone(),
                capability: capability.clone(),
                reason,
            });
        }
        let output_json = operation_result?;
        store.data_mut().provider_output_bytes = output_json.len();

        let maximum_output = usize::try_from(constraints.max_output_bytes)
            .unwrap_or(usize::MAX)
            .min(self.runtime.limits.max_output_bytes);
        if output_json.len() > maximum_output {
            return Err(BrokerHostError::OutputTooLarge {
                provider: self.manifest.id.to_string(),
                length: output_json.len(),
                maximum: maximum_output,
            });
        }
        let response =
            serde_json::from_str::<ComponentResponse>(&output_json).map_err(|source| {
                BrokerHostError::InvalidOutput {
                    provider: self.manifest.id.clone(),
                    capability: capability.clone(),
                    source,
                }
            })?;
        match response {
            ComponentResponse::Succeeded { output } => Ok(output),
            ComponentResponse::Failed { error } => Err(BrokerHostError::ProviderFailure {
                provider: self.manifest.id.clone(),
                capability: capability.clone(),
                code: error.code,
                message: error.message,
            }),
        }
    }
}

/// How this host addresses an operator in a conflict report.
///
/// The broker starts from a configured provider directory; the immediate host loads a component set
/// named on the command line. Everything else in the report is shared.
const CONFLICT_WORDING: ConflictWording = ConflictWording {
    refusing_to: "start",
    duplicate_provider_remedy: "remove one, or drop it from the provider search path",
};

/// Deterministic capability registry owned by a privileged broker.
#[derive(Debug)]
pub struct BrokerProviderRegistry {
    providers: Vec<BrokerWasmProvider>,
    routes: BTreeMap<CapabilityId, usize>,
    storage_host: Option<StorageHost>,
}

impl BrokerProviderRegistry {
    /// Compiles and validates provider components using one shared asynchronous engine.
    pub async fn load<I, P>(sources: I, limits: BrokerHostLimits) -> Result<Self, BrokerHostError>
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        Self::load_with_storage(sources, limits, None).await
    }

    /// Compiles providers with an optional broker-owned storage engine.
    pub async fn load_with_storage<I, P>(
        sources: I,
        limits: BrokerHostLimits,
        storage_host: Option<StorageHost>,
    ) -> Result<Self, BrokerHostError>
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        Self::load_with_options(sources, limits, storage_host, &BrokerHostOptions::default()).await
    }

    /// Compiles providers with operational settings the authority surface does not commit.
    pub async fn load_with_options<I, P>(
        sources: I,
        limits: BrokerHostLimits,
        storage_host: Option<StorageHost>,
        options: &BrokerHostOptions,
    ) -> Result<Self, BrokerHostError>
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        Self::load_sources(
            sources
                .into_iter()
                .map(|source| ProviderSource::from(source.into())),
            limits,
            storage_host,
            options,
        )
        .await
    }

    /// Compiles content-locked providers and compares each expected digest, length, and provider
    /// identity with the exact bytes and manifest the host consumes.
    pub async fn load_locked_with_options<I>(
        sources: I,
        limits: BrokerHostLimits,
        storage_host: Option<StorageHost>,
        options: &BrokerHostOptions,
    ) -> Result<Self, BrokerHostError>
    where
        I: IntoIterator<Item = LockedProviderSource>,
    {
        Self::load_sources(
            sources.into_iter().map(ProviderSource::from),
            limits,
            storage_host,
            options,
        )
        .await
    }

    async fn load_sources<I>(
        sources: I,
        limits: BrokerHostLimits,
        storage_host: Option<StorageHost>,
        options: &BrokerHostOptions,
    ) -> Result<Self, BrokerHostError>
    where
        I: IntoIterator<Item = ProviderSource>,
    {
        let sources = sources.into_iter().collect::<Vec<_>>();
        if sources.is_empty() {
            return Err(BrokerHostError::NoProviders);
        }
        let runtime = Arc::new(Runtime::new(limits, options)?);
        // Cranelift is the whole of a cold start and it is pure CPU, so every provider is
        // dispatched at once instead of one core compiling three components while the socket stays
        // unbound. The results are consumed in source order below, so the conflict report and the
        // first reported failure are exactly what the serial load produced.
        let compiling = sources
            .iter()
            .map(|source| {
                let runtime = Arc::clone(&runtime);
                let source = source.clone();
                tokio::task::spawn_blocking(move || compile_component(&runtime, source))
            })
            .collect::<Vec<_>>();
        let mut providers = Vec::with_capacity(sources.len());
        // Every conflict, then one failure. Returning on the first would make fixing a provider
        // directory take one restart per mistake; an operator should see the whole picture once.
        let mut scan = ConflictScan::new(CONFLICT_WORDING);
        for (source, compiling) in sources.into_iter().zip(compiling) {
            // A compilation task that panicked used to report itself as a fabricated "did not
            // complete", sending an operator to look for a truncated artifact. The join failure
            // says which it was — a panic and its message, or a cancellation — so it is kept as
            // the cause rather than replaced.
            let compiled = compiling.await.map_err(|join| BrokerHostError::Compile {
                path: source.path,
                source: wasmtime::Error::new(join),
            })??;
            let provider = BrokerWasmProvider::load(Arc::clone(&runtime), compiled).await?;
            scan.record(&provider.manifest, providers.len());
            providers.push(provider);
        }

        let routes = scan
            .finish()
            .map_err(|report| BrokerHostError::ConflictingProviders {
                report: Box::new(report),
            })?;
        Ok(Self {
            providers,
            routes,
            storage_host,
        })
    }

    /// Returns each provider's command words, in load order.
    #[must_use]
    pub fn command_words_by_provider(&self) -> Vec<(&ProviderId, &[String])> {
        self.providers
            .iter()
            .filter(|provider| !provider.manifest.command_words.is_empty())
            .map(|provider| {
                (
                    &provider.manifest.id,
                    provider.manifest.command_words.as_slice(),
                )
            })
            .collect()
    }

    /// Returns every command word the loaded providers contribute, in identifier order.
    #[must_use]
    pub fn command_words(&self) -> Vec<String> {
        let mut words = self
            .providers
            .iter()
            .flat_map(|provider| provider.manifest.command_words.iter().cloned())
            .collect::<Vec<_>>();
        words.sort();
        words.dedup();
        words
    }

    /// Runs one command word's argv through the provider that declared it.
    ///
    /// The provider's `run-command` export receives `argv` and `stdin`; a legacy `resolve-command`
    /// export receives `argv` alone, and its answer is adapted into the same
    /// [`CommandRunOutcome`], so a caller handles one type whichever export the component has.
    ///
    /// # Errors
    ///
    /// Returns [`BrokerHostError::UnknownCommandWord`] when no loaded provider declared it,
    /// [`BrokerHostError::CommandInputTooLarge`] when `argv` plus `stdin` exceed the input bound,
    /// [`BrokerHostError::InvalidCommandRun`] when the guest's answer is not its wire type, and any
    /// guest failure from the run itself.
    pub async fn run_command(
        &self,
        word: &str,
        argv: &[String],
        stdin: Option<&str>,
    ) -> Result<CommandRunOutcome, BrokerHostError> {
        let provider = self
            .providers
            .iter()
            .find(|provider| {
                provider
                    .manifest
                    .command_words
                    .iter()
                    .any(|candidate| candidate == word)
            })
            .ok_or_else(|| BrokerHostError::UnknownCommandWord {
                word: word.to_owned(),
            })?;
        // Parsed here rather than handed onward as JSON: guest output is this crate's concern, and
        // a daemon that never sees the raw string cannot accidentally forward it to a caller.
        //
        // `argv` and `stdin` are deliberately not span fields for the same reason the invoke span
        // omits input: they are model-authored text.
        let json = provider
            .run_command(argv, stdin)
            .instrument(tracing::info_span!(
                "provider.run_command",
                provider = %provider.manifest.id,
                word,
                command.export = command_export_name(&provider.command_export),
            ))
            .await?;
        parse_command_run(&provider.command_export, &json).map_err(|source| {
            BrokerHostError::InvalidCommandRun {
                provider: provider.manifest.id.clone(),
                source,
            }
        })
    }

    /// Returns validated manifests in component load order.
    pub fn manifests(&self) -> impl ExactSizeIterator<Item = &ProviderManifest> {
        self.providers.iter().map(|provider| &provider.manifest)
    }

    /// Returns owned documentation metadata for every component loaded into this registry.
    pub fn loaded_provider_metadata(
        &self,
    ) -> impl ExactSizeIterator<Item = LoadedProviderMetadata> + '_ {
        self.providers
            .iter()
            .map(|provider| LoadedProviderMetadata {
                source: provider.source.clone(),
                artifact_bytes: provider.artifact_bytes,
                artifact_sha256: provider.artifact_sha256.clone(),
                manifest: provider.manifest.clone(),
                imports: provider.imports.clone(),
                exports: provider.exports.clone(),
                interface_truncated: provider.interface_truncated,
            })
    }

    /// Returns the independent component-host ceilings used for authority commitments.
    #[must_use]
    pub fn host_limits(&self) -> &BrokerHostLimits {
        &self
            .providers
            .first()
            .expect("a registry is constructed with at least one provider")
            .runtime
            .limits
    }

    /// Returns the configured storage host handle, when storage is enabled.
    #[must_use]
    pub fn storage_host(&self) -> Option<StorageHost> {
        self.storage_host.clone()
    }

    /// Returns a cloneable handle to live Wasmtime host counters.
    #[must_use]
    pub fn metrics(&self) -> BrokerHostMetrics {
        self.providers
            .first()
            .expect("a registry is constructed with at least one provider")
            .runtime
            .metrics
            .clone()
    }

    /// Returns one routed capability and the provider declaring it, by identifier.
    ///
    /// Routes are already keyed by capability, so a caller filtering constraint sets does not have
    /// to scan every route per set.
    #[must_use]
    pub fn capability(
        &self,
        capability: &CapabilityId,
    ) -> Option<(&ProviderId, &ProviderCapability)> {
        let provider = &self.providers[*self.routes.get(capability)?];
        let capability = provider
            .manifest
            .capabilities
            .iter()
            .find(|candidate| &candidate.id == capability)
            .expect("routes originate from validated provider manifests");
        Some((&provider.manifest.id, capability))
    }

    /// Returns capabilities in deterministic identifier order.
    pub fn capabilities(&self) -> impl Iterator<Item = (&ProviderId, &ProviderCapability)> {
        self.routes.iter().map(|(capability_id, provider_index)| {
            let provider = &self.providers[*provider_index];
            let capability = provider
                .manifest
                .capabilities
                .iter()
                .find(|candidate| &candidate.id == capability_id)
                .expect("routes originate from validated provider manifests");
            (&provider.manifest.id, capability)
        })
    }

    /// Validates one policy grant against the component host's independent ceilings.
    pub fn validate_constraints(
        &self,
        constraints: &ExecutionConstraints,
    ) -> Result<(), BrokerHostError> {
        if constraints.storage.is_some() && self.storage_host.is_none() {
            return Err(BrokerHostError::StorageDisabled);
        }
        let runtime = &self
            .providers
            .first()
            .expect("a registry is constructed with at least one provider")
            .runtime;
        validate_authorized_constraints(constraints, &runtime.limits)
    }

    /// Consumes one broker-authorized proposal through its trusted capability route.
    ///
    /// `credential` rides alongside the authorization rather than inside it: an
    /// `AuthorizedInvocation` is inert-serializable as evidence, and a secret must never share a
    /// container with anything that can be rendered. The host injects it at the native HTTP
    /// boundary only for destinations inside its binding; the guest never observes it.
    pub async fn invoke(
        &self,
        authorized: AuthorizedInvocation,
        credential: Option<BoundCredential>,
    ) -> Result<BrokerInvocationOutput, BrokerInvocationFailure> {
        self.invoke_with_storage(authorized, credential, None).await
    }

    /// Consumes an invocation plus its exact non-forgeable storage grant when storage is enabled.
    pub async fn invoke_with_storage(
        &self,
        authorized: AuthorizedInvocation,
        credential: Option<BoundCredential>,
        storage_grant: Option<StorageGrant>,
    ) -> Result<BrokerInvocationOutput, BrokerInvocationFailure> {
        let storage_backed = authorized.constraints().storage.is_some();
        let proposal = authorized.proposal();
        let provider_index = self
            .routes
            .get(&proposal.capability)
            .copied()
            .ok_or_else(|| BrokerHostError::UnknownCapability {
                capability: proposal.capability.clone(),
            })?;
        let provider = &self.providers[provider_index];
        if &provider.manifest.id != authorized.provider() {
            return Err(BrokerHostError::AuthorizedProviderMismatch {
                capability: proposal.capability.clone(),
                authorized: authorized.provider().clone(),
                routed: provider.manifest.id.clone(),
            }
            .into());
        }
        let secret_grant = authorized.constraints().secret_use.as_ref();
        if credential
            .as_ref()
            .is_some_and(|credential| !credential.matches_secret_grant(secret_grant))
            || (secret_grant.is_some() && credential.is_none())
        {
            return Err(BrokerHostError::SecretCredentialMismatch.into());
        }
        let storage_transaction = match (&authorized.constraints().storage, storage_grant) {
            (None, None) => None,
            (None, Some(_)) => return Err(BrokerHostError::UnexpectedStorageGrant.into()),
            (Some(_), None) => return Err(BrokerHostError::MissingStorageGrant.into()),
            #[allow(
                clippy::map_err_ignore,
                reason = "a `JoinError` from `spawn_blocking` distinguishes only a panic from \
                          runtime cancellation, and the panic hook has already printed the panic \
                          with its location; `storage::StorageState::call` classes the same \
                          failure as `Io` for the same reason"
            )]
            (Some(constraints), Some(grant)) => {
                if grant.invocation() != &proposal.id
                    || grant.capability() != &proposal.capability
                    || grant.provider() != authorized.provider()
                    || grant.interface() != constraints.interface
                    || grant.access() != constraints.access
                    || grant.namespace() != constraints.namespace
                {
                    return Err(BrokerHostError::StorageGrantMismatch.into());
                }
                let host = self
                    .storage_host
                    .as_ref()
                    .ok_or(BrokerHostError::StorageDisabled)?
                    .clone();
                Some(
                    tokio::task::spawn_blocking(move || host.begin(grant))
                        .await
                        .map_err(|_| BrokerHostError::Storage {
                            source: dekopon_storage_host::StorageHostError::Io,
                        })?
                        .map_err(|source| BrokerHostError::Storage { source })?,
                )
            }
        };
        // `proposal.input` is deliberately not a field here. It is the untrusted payload the whole
        // authority boundary exists to contain, and a span is not a safer place for it than an
        // audit record.
        provider
            .invoke(
                &proposal.capability,
                &proposal.input,
                authorized.constraints(),
                credential,
                storage_transaction,
            )
            .instrument(if storage_backed {
                tracing::info_span!("provider.invoke", storage = true)
            } else {
                let span = tracing::info_span!(
                    "provider.invoke",
                    capability = %proposal.capability,
                    provider = %provider.manifest.id,
                    input = tracing::field::Empty,
                );
                if dekopon_core::telemetry_payloads() {
                    span.record("input", tracing::field::display(&proposal.input));
                }
                span
            })
            .await
    }
}

/// The export name a load line and a run span report for one component.
///
/// A mismatched export is reported as `none` here: nothing callable exists under it, and a manifest
/// declaring words never loads with one, so the line only ever says it for a wordless component.
const fn command_export_name(export: &CommandExport) -> &'static str {
    match export {
        CommandExport::RunCommand => RUN_COMMAND_EXPORT,
        CommandExport::ResolveCommand => RESOLVE_COMMAND_EXPORT,
        CommandExport::Absent | CommandExport::Mismatched { .. } => "none",
    }
}

async fn describe_component(
    runtime: &Runtime,
    pre: &bindings::ProviderPre<StoreState>,
    source: &Path,
) -> Result<String, BrokerHostError> {
    let operation_timeout = runtime.limits.max_timeout;
    let http = HttpState::describe(runtime.http_ceilings(), operation_timeout)
        .map_err(|source| BrokerHostError::HttpConfiguration { source })?;
    let mut store = runtime.store(http, storage::StorageState::disabled())?;
    let operation = async {
        let bindings = pre.instantiate_async(&mut store).await.map_err(|error| {
            BrokerHostError::Instantiate {
                path: source.to_path_buf(),
                source: error,
            }
        })?;
        runtime.metrics.record_instantiation();
        bindings
            .call_describe(&mut store)
            .await
            .map_err(|error| BrokerHostError::Describe {
                path: source.to_path_buf(),
                source: error,
            })
    };
    #[allow(
        clippy::map_err_ignore,
        reason = "`tokio::time::error::Elapsed` carries only \"deadline has elapsed\"; the Timeout \
                  variant already names the operation and the budget it exceeded"
    )]
    let manifest =
        timeout(operation_timeout, operation)
            .await
            .map_err(|_| BrokerHostError::Timeout {
                operation: format!("describe {}", source.display()),
                timeout_ms: operation_timeout.as_millis() as u64,
            });
    runtime.record_fuel(&mut store);
    let manifest = manifest??;
    runtime.metrics.record_description();
    if store.data().http.attempted() || store.data().storage.attempted() {
        return Err(BrokerHostError::DescribeUsedHostImport {
            path: source.to_path_buf(),
        });
    }
    Ok(manifest)
}

fn validate_limits(limits: &BrokerHostLimits) -> Result<(), BrokerHostError> {
    host::validate_limits(
        &limits.store_bounds(),
        &[
            ("max_input_bytes", limits.max_input_bytes as u128),
            ("max_output_bytes", limits.max_output_bytes as u128),
            ("max_http_requests", u128::from(limits.max_http_requests)),
            (
                "max_http_request_bytes",
                u128::from(limits.max_http_request_bytes),
            ),
            (
                "max_http_response_bytes",
                u128::from(limits.max_http_response_bytes),
            ),
            ("max_http_headers", limits.max_http_headers as u128),
            (
                "max_http_header_bytes",
                limits.max_http_header_bytes as u128,
            ),
            ("fuel", u128::from(limits.fuel)),
            ("max_timeout", limits.max_timeout.as_nanos()),
        ],
    )
    .map_err(|zero| BrokerHostError::InvalidLimit { name: zero.name })
}

fn validate_authorized_constraints(
    constraints: &ExecutionConstraints,
    limits: &BrokerHostLimits,
) -> Result<(), BrokerHostError> {
    if constraints.timeout_ms == 0
        || Duration::from_millis(constraints.timeout_ms) > limits.max_timeout
    {
        return Err(BrokerHostError::AuthorizationExceedsHostLimit {
            field: "timeout_ms",
        });
    }
    if constraints.max_output_bytes == 0
        || constraints.max_output_bytes > limits.max_output_bytes as u64
    {
        return Err(BrokerHostError::AuthorizationExceedsHostLimit {
            field: "max_output_bytes",
        });
    }
    if constraints.http.is_some() && constraints.storage.is_some() {
        return Err(BrokerHostError::MixedHostAuthorization);
    }
    if let Some(secret) = &constraints.secret_use {
        secret
            .validate()
            .map_err(|source| BrokerHostError::InvalidSecretAuthorization { source })?;
        let Some(http) = &constraints.http else {
            return Err(BrokerHostError::SecretAuthorizationExceedsHttp);
        };
        if secret.max_injections > http.max_requests
            || secret
                .allowed_hosts
                .iter()
                .any(|host| !http.allowed_hosts.contains(host))
            || secret
                .allowed_methods
                .iter()
                .any(|method| !http.allowed_methods.contains(method))
        {
            return Err(BrokerHostError::SecretAuthorizationExceedsHttp);
        }
    }
    if let Some(http) = &constraints.http {
        if http.allowed_hosts.is_empty()
            || http.allowed_methods.is_empty()
            || http.max_requests == 0
            || http.max_request_bytes == 0
            || http.max_response_bytes == 0
        {
            return Err(BrokerHostError::InvalidHttpAuthorization);
        }
        if http.max_requests > limits.max_http_requests
            || http.max_request_bytes > limits.max_http_request_bytes
            || http.max_response_bytes > limits.max_http_response_bytes
        {
            return Err(BrokerHostError::AuthorizationExceedsHostLimit { field: "http" });
        }
    }
    Ok(())
}

fn validate_manifest(manifest: &ProviderManifest, source: &Path) -> Result<(), BrokerHostError> {
    // No effect gate: the broker authorizes an effect per invocation, so a manifest declaring an
    // external write is loadable here and refused by policy, not by the loader.
    host::validate_manifest(manifest, None)
        .map_err(|rejection| invalid_manifest(source, rejection.to_string()))
}

fn invalid_manifest(source: &Path, message: impl Into<String>) -> BrokerHostError {
    BrokerHostError::Manifest {
        path: source.to_path_buf(),
        message: message.into(),
    }
}

/// Failure to load or execute a broker-owned provider component.
#[derive(Debug, Error)]
pub enum BrokerHostError {
    /// No components were configured.
    #[error("at least one broker provider component is required")]
    NoProviders,
    /// A host ceiling was zero.
    #[error("broker host limit {name} must be greater than zero")]
    InvalidLimit {
        /// Invalid field.
        name: &'static str,
    },
    /// An authorization attempted to exceed a broker host ceiling.
    #[error("authorization constraint {field} exceeds the broker host ceiling")]
    AuthorizationExceedsHostLimit {
        /// Constraint field.
        field: &'static str,
    },
    /// HTTP authorization was present but incomplete or unbounded.
    #[error("HTTP authorization must contain destinations, methods, and positive limits")]
    InvalidHttpAuthorization,
    /// HTTP and storage authority were combined in one v1 capability.
    #[error("HTTP and storage authority cannot coexist in one capability")]
    MixedHostAuthorization,
    /// Secret-use authorization was structurally invalid.
    #[error("secret-use authorization is invalid")]
    InvalidSecretAuthorization {
        #[source]
        source: dekopon_capability::SecretUseGrantError,
    },
    /// Secret-use scope exceeded the surrounding HTTP authorization.
    #[error("secret-use authorization exceeds HTTP authority")]
    SecretAuthorizationExceedsHttp,
    /// Resolved secret material did not match the DRN/sink/binding committed into authorization.
    #[error("resolved secret credential does not match authorized secret use")]
    SecretCredentialMismatch,
    /// Storage was constrained but no native storage engine is configured.
    #[error("provider storage is disabled")]
    StorageDisabled,
    /// Storage authority was required but no grant reached the invocation boundary.
    #[error("authorized storage invocation is missing its storage grant")]
    MissingStorageGrant,
    /// A storage grant accompanied an invocation with no storage constraint.
    #[error("storage grant accompanied an invocation with no storage authority")]
    UnexpectedStorageGrant,
    /// The single-use storage grant did not match the authorized invocation.
    #[error("storage grant does not match authorized invocation")]
    StorageGrantMismatch,
    /// Native storage setup, transaction, or finalization failed.
    #[error("broker provider storage failed")]
    Storage {
        #[source]
        source: dekopon_storage_host::StorageHostError,
    },
    /// The native HTTP execution context rejected its ceiling or grant configuration.
    #[error("could not initialize the bounded HTTP execution context")]
    HttpConfiguration {
        /// Native validation failure.
        #[source]
        source: dekopon_http_host::ConfigurationError,
    },
    /// Wasmtime engine initialization failed.
    #[error("could not initialize the broker Wasmtime engine")]
    Engine {
        /// Wasmtime error.
        #[source]
        source: wasmtime::Error,
    },
    /// Store initialization failed.
    #[error("could not initialize a bounded broker Wasmtime store")]
    Store {
        /// Wasmtime error.
        #[source]
        source: wasmtime::Error,
    },
    /// Generated host imports could not be registered.
    #[error("could not register broker host interfaces")]
    Linker {
        /// Wasmtime error.
        #[source]
        source: wasmtime::Error,
    },
    /// A generated lock supplied a zero or over-ceiling expected component length.
    #[error("locked provider artifact is {size} bytes; maximum is {maximum}")]
    InvalidArtifactSize {
        /// Locked byte length.
        size: u64,
        /// Hard maximum.
        maximum: u64,
    },
    /// A generated lock supplied a malformed expected component digest.
    #[error("locked provider artifact digest must be sixty-four lowercase hexadecimal characters")]
    InvalidArtifactDigest,
    /// An unlocked component exceeded the same hard source-byte ceiling.
    #[error(
        "broker provider component {} is {actual} bytes; maximum is {maximum}",
        path.display()
    )]
    ArtifactTooLarge {
        /// Component path.
        path: PathBuf,
        /// Actual descriptor or bounded-read length.
        actual: u64,
        /// Hard maximum.
        maximum: u64,
    },
    /// The exact component buffer did not have the locked byte length.
    #[error(
        "broker provider component {} is {actual} bytes; provider lock expects {expected}",
        path.display()
    )]
    ArtifactSizeMismatch {
        /// Component path.
        path: PathBuf,
        /// Locked byte length.
        expected: u64,
        /// Actual buffer length.
        actual: u64,
    },
    /// The exact component buffer did not have the locked digest.
    #[error(
        "broker provider component {} has SHA-256 {actual}; provider lock expects {expected}",
        path.display()
    )]
    ArtifactDigestMismatch {
        /// Component path.
        path: PathBuf,
        /// Locked lowercase SHA-256.
        expected: String,
        /// Actual lowercase SHA-256.
        actual: String,
    },
    /// The validated manifest did not describe the provider identity recorded in the lock.
    #[error(
        "broker provider component {} describes provider {actual}; provider lock expects {expected}",
        path.display()
    )]
    ProviderIdentityMismatch {
        /// Component path.
        path: PathBuf,
        /// Locked provider identity.
        expected: ProviderId,
        /// Manifest provider identity.
        actual: ProviderId,
    },
    /// Source artifact metadata could not be read for the informational provider view.
    #[error("could not inspect broker provider artifact {}", path.display())]
    ArtifactMetadata {
        /// Component path.
        path: PathBuf,
        /// File read failure.
        #[source]
        source: std::io::Error,
    },
    /// The persistent compilation cache directory could not be prepared.
    #[error("could not open the broker provider compilation cache at {}", path.display())]
    CompileCache {
        /// Configured cache directory.
        path: PathBuf,
        /// Wasmtime error.
        #[source]
        source: wasmtime::Error,
    },
    /// Concurrently live stores already reserve the whole aggregate guest-memory ceiling.
    #[error(
        "broker provider stores already reserve the {maximum}-byte aggregate guest memory ceiling; \
         another {requested} bytes cannot be admitted"
    )]
    MemoryBudgetExhausted {
        /// Bytes one more store would reserve.
        requested: usize,
        /// Configured aggregate ceiling.
        maximum: usize,
    },
    /// Component compilation failed.
    #[error("could not compile broker provider component {}", path.display())]
    Compile {
        /// Component path.
        path: PathBuf,
        /// Wasmtime error.
        #[source]
        source: wasmtime::Error,
    },
    /// Component imports or exports could not be linked.
    #[error("could not instantiate broker provider component {}", path.display())]
    Instantiate {
        /// Component path.
        path: PathBuf,
        /// Wasmtime error.
        #[source]
        source: wasmtime::Error,
    },
    /// Provider attempted a host call while describing itself.
    #[error("provider component {} attempted a host import during describe", path.display())]
    DescribeUsedHostImport {
        /// Component path.
        path: PathBuf,
    },
    /// One or more providers conflict with each other or with the shell's reserved vocabulary.
    #[error("{report}")]
    ConflictingProviders {
        /// Every conflict found, boxed to keep this enum small.
        report: Box<ProviderConflicts>,
    },
    /// A provider returned something that is not a command run outcome.
    #[error("provider {provider} returned an unreadable command run outcome")]
    InvalidCommandRun {
        /// Provider identity.
        provider: ProviderId,
        /// Decode failure.
        #[source]
        source: serde_json::Error,
    },
    /// No loaded provider declared the requested command word.
    #[error("no loaded provider declares the command word {word:?}")]
    UnknownCommandWord {
        /// The unclaimed word.
        word: String,
    },
    /// Provider declared command words but exports no way to run them.
    #[error(
        "provider {provider} declares command words but component {} exports neither run-command \
         nor resolve-command; rebuild it against the dekopon:provider/provider-cli world",
        path.display()
    )]
    MissingCommandExport {
        /// Provider identity.
        provider: ProviderId,
        /// Component path.
        path: PathBuf,
    },
    /// Provider exports a command export as something the host cannot call.
    #[error(
        "provider {provider} exports {name} from component {} as {found}, not the function the \
         dekopon:provider package declares",
        path.display()
    )]
    CommandExportSignature {
        /// Provider identity.
        provider: ProviderId,
        /// Component path.
        path: PathBuf,
        /// Which export name was found.
        name: &'static str,
        /// Bounded description of what the component actually exports.
        found: String,
    },
    /// A command word's argv plus its piped value exceeded the host input bound.
    #[error("command input for provider {provider} is {length} bytes; broker maximum is {maximum}")]
    CommandInputTooLarge {
        /// Provider identity.
        provider: ProviderId,
        /// Actual bytes: every argv word plus the piped value.
        length: usize,
        /// Maximum bytes.
        maximum: usize,
    },
    /// Running a command word failed inside the guest.
    #[error("provider {provider} failed while running a command word")]
    RunCommand {
        /// Provider identity.
        provider: ProviderId,
        /// Underlying trap or error.
        #[source]
        source: wasmtime::Error,
    },
    /// Provider attempted a host call while running a command word.
    ///
    /// The run happens before authorization, so a component reaching for host authority there is
    /// refused rather than trusted.
    #[error(
        "provider component {} attempted a host import during a command run",
        path.display()
    )]
    RunCommandUsedHostImport {
        /// Component path.
        path: PathBuf,
    },
    /// Provider description failed.
    #[error("broker provider component {} failed while describing itself", path.display())]
    Describe {
        /// Component path.
        path: PathBuf,
        /// Wasmtime error.
        #[source]
        source: wasmtime::Error,
    },
    /// Manifest JSON was malformed.
    #[error("broker provider component {} returned an invalid manifest", path.display())]
    InvalidManifest {
        /// Component path.
        path: PathBuf,
        /// JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// Manifest violated a semantic rule.
    #[error("broker provider component {} has an invalid manifest: {message}", path.display())]
    Manifest {
        /// Component path.
        path: PathBuf,
        /// Validation detail.
        message: String,
    },
    /// Authorized capability has no provider route.
    #[error("no broker provider implements authorized capability {capability}")]
    UnknownCapability {
        /// Capability ID.
        capability: CapabilityId,
    },
    /// Authorization selected a different provider than the trusted route.
    #[error(
        "authorization selected provider {authorized} for {capability}, but route selects {routed}"
    )]
    AuthorizedProviderMismatch {
        /// Capability.
        capability: CapabilityId,
        /// Provider bound into authorization.
        authorized: ProviderId,
        /// Provider selected by the loaded route.
        routed: ProviderId,
    },
    /// Selected provider did not implement the routed capability.
    #[error("broker provider {provider} does not implement capability {capability}")]
    ProviderDoesNotImplement {
        /// Provider ID.
        provider: ProviderId,
        /// Capability ID.
        capability: CapabilityId,
    },
    /// Input was not an object.
    #[error("input for broker capability {capability} must be a JSON object")]
    InputNotObject {
        /// Capability ID.
        capability: CapabilityId,
    },
    /// Input serialization failed.
    #[error("could not serialize broker provider input")]
    SerializeInput {
        /// JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// Input exceeded the host bound.
    #[error("input for {capability} is {length} bytes; broker maximum is {maximum}")]
    InputTooLarge {
        /// Capability ID.
        capability: CapabilityId,
        /// Actual bytes.
        length: usize,
        /// Maximum bytes.
        maximum: usize,
    },
    /// Provider output exceeded its bound.
    #[error("broker provider {provider} returned {length} bytes; maximum is {maximum}")]
    OutputTooLarge {
        /// Provider ID or source path.
        provider: String,
        /// Actual bytes.
        length: usize,
        /// Maximum bytes.
        maximum: usize,
    },
    /// Provider operation exceeded its deadline.
    #[error("broker provider operation {operation} exceeded {timeout_ms} ms")]
    Timeout {
        /// Operation.
        operation: String,
        /// Bound.
        timeout_ms: u64,
    },
    /// Provider attempted a host call outside its authorization.
    #[error("broker rejected host call {reason} from provider {provider} capability {capability}")]
    HostCallRejected {
        /// Provider ID.
        provider: ProviderId,
        /// Capability ID.
        capability: CapabilityId,
        /// Stable rejection class.
        reason: &'static str,
    },
    /// Provider attempted a storage call outside its exact interface/access grant.
    #[error(
        "broker rejected storage call {reason} from provider {provider} capability {capability}"
    )]
    StorageCallRejected {
        /// Provider ID.
        provider: ProviderId,
        /// Capability ID.
        capability: CapabilityId,
        /// Stable rejection class.
        reason: &'static str,
    },
    /// Provider export trapped or failed.
    #[error("broker provider {provider} failed while invoking {capability}")]
    Invoke {
        /// Provider ID.
        provider: ProviderId,
        /// Capability ID.
        capability: CapabilityId,
        /// Wasmtime error.
        #[source]
        source: wasmtime::Error,
    },
    /// Provider returned a typed failure.
    #[error("broker provider {provider} failed {capability} with {code}: {message}")]
    ProviderFailure {
        /// Provider ID.
        provider: ProviderId,
        /// Capability ID.
        capability: CapabilityId,
        /// Stable provider code.
        code: String,
        /// Bounded provider detail.
        message: String,
    },
    /// Provider response JSON was malformed.
    #[error("broker provider {provider} returned an invalid response for {capability}")]
    InvalidOutput {
        /// Provider ID.
        provider: ProviderId,
        /// Capability ID.
        capability: CapabilityId,
        /// JSON error.
        #[source]
        source: serde_json::Error,
    },
}
