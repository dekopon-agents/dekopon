#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
// The sole exception is cwasm::deserialize, after trusted-artifact verification.
#![deny(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used))]

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
    self, CommandExportProblem, ConflictScan, EngineError, RUN_COMMAND_EXPORT, StoreLimits,
    check_command_export, command_export, command_input_bytes,
};
pub use dekopon_provider_sdk::{
    CommandRunOutcome, ComponentFailure, ComponentResponse, ProviderApiVersion, ProviderCapability,
    ProviderManifest,
};
use dekopon_storage_host::{StorageEvidence, StorageGrant, StorageHost};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tokio::time::timeout;
use tracing::Instrument as _;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Engine, Store};

pub mod asset;
mod clock;
mod cwasm;
mod http;
mod memory;
mod metadata;
mod settings;
mod storage;
use clock::ClockState;
pub use http::{
    BoundCredential, HttpCallEvidence, HttpConfigurationError, NonPublicHttpsAuthority,
    PlaintextHostError, PlaintextHosts, destinations_cover,
};
use http::{HttpCeilings, HttpState};
pub use metadata::LoadedProviderMetadata;
use metadata::identify_bytes;
use settings::SettingsState;

pub(crate) mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "provider",
        imports: { default: async | trappable },
        exports: { default: async },
        with: {
            "dekopon:storage/durable-files.file": crate::storage::FileResource,
            "dekopon:asset/asset.handle": crate::asset::HandleResource,
            "dekopon:asset/asset.writer": crate::asset::WriterResource,
        },
    });
}

pub const PROVIDER_WIT: &str = include_str!("../wit/deps/provider.wit");
pub const HTTP_WIT: &str = include_str!("../wit/deps/http.wit");
pub const STORAGE_WIT: &str = include_str!("../wit/deps/storage.wit");

pub const HARD_MAX_PROVIDER_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;
pub const DEFAULT_MAX_HTTP_REQUESTS: u32 = 32;
pub const DEFAULT_MAX_HTTP_REQUEST_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_MAX_HTTP_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
pub const DEFAULT_MAX_HTTP_HEADERS: usize = 128;
pub const DEFAULT_MAX_HTTP_HEADER_BYTES: usize = 64 * 1024;
/// Fuel is set high enough that durable-memory compaction's multi-megabyte rewrite completes
/// without trapping before the wall-clock deadline does.
pub const DEFAULT_FUEL: u64 = 8_000_000_000;
pub const DEFAULT_MAX_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounds guest memory across all live stores, since max_memory_bytes only bounds one; set it to
/// null for the old unbounded behavior.
pub const DEFAULT_MAX_TOTAL_MEMORY_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrokerHostLimits {
    pub max_memory_bytes: usize,
    pub max_table_elements: usize,
    pub max_instances: usize,
    pub max_tables: usize,
    pub max_memories: usize,
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
    pub max_http_requests: u32,
    pub max_http_request_bytes: u64,
    pub max_http_response_bytes: u64,
    pub max_http_headers: usize,
    pub max_http_header_bytes: usize,
    pub fuel: u64,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrokerHostOptions {
    /// None compiles without a cache; the operator must not modify these mapped files while the
    /// registry is alive, since hashes are checked once at startup.
    pub cwasm_dir: Option<PathBuf>,
    pub max_total_memory_bytes: Option<usize>,
    pub plaintext_hosts: PlaintextHosts,
    pub extra_ca_bundles: Arc<Vec<Vec<u8>>>,
    /// Exact private HTTPS destinations; never populated from provider input.
    pub non_public_https: Arc<Vec<NonPublicHttpsAuthority>>,
    /// JSON settings keyed by provider ID; readable only by that provider during invoke.
    pub provider_settings: Arc<BTreeMap<ProviderId, String>>,
}

impl Default for BrokerHostOptions {
    fn default() -> Self {
        Self {
            cwasm_dir: None,
            max_total_memory_bytes: Some(DEFAULT_MAX_TOTAL_MEMORY_BYTES),
            plaintext_hosts: PlaintextHosts::default(),
            extra_ca_bundles: Arc::new(Vec::new()),
            non_public_https: Arc::new(Vec::new()),
            provider_settings: Arc::new(BTreeMap::new()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockedProviderSource {
    path: PathBuf,
    artifact_bytes: u64,
    artifact_sha256: String,
    provider_id: ProviderId,
}

impl LockedProviderSource {
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

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn artifact_bytes(&self) -> u64 {
        self.artifact_bytes
    }

    #[must_use]
    pub fn artifact_sha256(&self) -> &str {
        &self.artifact_sha256
    }

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

/// Capped independently of the fuel ceiling so a store holding huge fuel still yields to the
/// executor often enough for the wall-clock deadline to fire.
const MAX_FUEL_YIELD_INTERVAL: u64 = 10_000;

impl BrokerHostLimits {
    #[must_use]
    pub const fn fuel_yield_interval(&self) -> u64 {
        if self.fuel < MAX_FUEL_YIELD_INTERVAL {
            self.fuel
        } else {
            MAX_FUEL_YIELD_INTERVAL
        }
    }

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

#[derive(Debug)]
pub struct BrokerInvocationFailure {
    pub error: Box<BrokerHostError>,
    pub http_calls: Vec<HttpCallEvidence>,
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrokerInvocationOutput {
    #[serde(skip)]
    pub assets: asset::AssetOutputs,
    pub provider: ProviderId,
    pub capability: CapabilityId,
    pub output: Value,
    pub http_calls: Vec<HttpCallEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage: Option<StorageEvidence>,
}

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
    engine_key: String,
    cwasm: Option<cwasm::Cache>,
    linker: Linker<StoreState>,
    limits: BrokerHostLimits,
    memory_budget: Option<Arc<MemoryBudget>>,
    plaintext_hosts: PlaintextHosts,
    extra_ca_bundles: Arc<Vec<Vec<u8>>>,
    non_public_https: Arc<Vec<NonPublicHttpsAuthority>>,
    provider_settings: Arc<BTreeMap<ProviderId, String>>,
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
        let config = host::config();
        let engine = host::engine(config).map_err(|error| match error {
            EngineError::Engine { source } => BrokerHostError::Engine { source },
        })?;
        let cwasm = options
            .cwasm_dir
            .clone()
            .map(|root| cwasm::Cache::new(root, &engine));
        let mut linker = Linker::new(&engine);
        bindings::Provider::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(|source| BrokerHostError::Linker { source })?;
        asset::link_bounded_writer(&mut linker)
            .map_err(|source| BrokerHostError::Linker { source })?;
        Ok(Self {
            engine_key: cwasm::compatibility_key(&engine),
            engine,
            cwasm,
            linker,
            limits,
            memory_budget: options.max_total_memory_bytes.map(|maximum| {
                Arc::new(MemoryBudget {
                    maximum,
                    reserved: AtomicUsize::new(0),
                })
            }),
            plaintext_hosts: options.plaintext_hosts.clone(),
            extra_ca_bundles: Arc::clone(&options.extra_ca_bundles),
            non_public_https: Arc::clone(&options.non_public_https),
            provider_settings: Arc::clone(&options.provider_settings),
        })
    }

    fn store(
        &self,
        http: HttpState,
        storage: storage::StorageState,
        clock: ClockState,
        settings: SettingsState,
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
        let mut store = Store::new(
            &self.engine,
            StoreState {
                limits: memory::MemoryLimiter::new(self.limits.store_bounds()),
                http,
                storage,
                clock,
                settings,
                assets: asset::AssetState::disabled(),
                table: storage::new_table(),
                instantiations: 0,
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
        tracing::Span::current().record("stores", 1_u64);
        Ok(store)
    }

    fn http_ceilings(&self) -> HttpCeilings {
        HttpCeilings {
            max_requests: self.limits.max_http_requests,
            max_request_bytes: self.limits.max_http_request_bytes,
            max_response_bytes: self.limits.max_http_response_bytes,
            max_headers: self.limits.max_http_headers,
            max_header_bytes: self.limits.max_http_header_bytes,
            plaintext_hosts: self.plaintext_hosts.clone(),
            extra_ca_bundles: Arc::clone(&self.extra_ca_bundles),
            non_public_https: Arc::clone(&self.non_public_https),
        }
    }
}

struct StoreState {
    assets: asset::AssetState,
    limits: memory::MemoryLimiter,
    http: HttpState,
    storage: storage::StorageState,
    /// Granted only in an invocation's store; descriptions and command runs are pure.
    clock: ClockState,
    settings: SettingsState,
    table: wasmtime::component::ResourceTable,
    instantiations: u64,
    _reserved: Option<MemoryReservation>,
}

impl bindings::dekopon::http::client::Host for StoreState {
    async fn stream(
        &mut self,
        request: bindings::dekopon::http::client::StreamedRequest,
    ) -> wasmtime::Result<
        Result<
            bindings::dekopon::http::client::StreamedResponse,
            bindings::dekopon::http::client::HttpError,
        >,
    > {
        use bindings::dekopon::http::client as wit;
        let Some(directory) = self.assets.directory()? else {
            self.assets
                .reject(bindings::dekopon::asset::asset::ErrorCode::Unconfigured);
            return Ok(Err(wit::HttpError {
                code: wit::ErrorCode::Internal,
                message: "unconfigured: configure assets.rootPath before streaming HTTP".to_owned(),
            }));
        };
        let mut body = Vec::with_capacity(request.body.len());
        for part in request.body {
            body.push(match part {
                wit::Part::Literal(bytes) => dekopon_http_host::Part::Literal(bytes),
                wit::Part::Asset(part) => {
                    let part = self.table.get(&part.handle)?.http_part(part.encoding);
                    if let Err(error) = self.assets.charge_stream(part.decoded_bytes) {
                        return Ok(Err(wit::HttpError {
                            code: wit::ErrorCode::RequestTooLarge,
                            message: error.message,
                        }));
                    }
                    dekopon_http_host::Part::Asset(part)
                }
            });
        }
        let response = self
            .http
            .client
            .stream(
                dekopon_http_host::StreamedRequest {
                    method: request.method,
                    uri: request.uri,
                    headers: request
                        .headers
                        .into_iter()
                        .map(|header| dekopon_http_host::Header {
                            name: header.name,
                            value: header.value,
                        })
                        .collect(),
                    body,
                },
                &directory,
            )
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                self.assets.reject(if self.http.client.asset_over_budget() {
                    bindings::dekopon::asset::asset::ErrorCode::OverBudget
                } else {
                    bindings::dekopon::asset::asset::ErrorCode::Io
                });
                return Ok(Err(http::map_error(error)));
            }
        };
        let content_type = response
            .headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case("content-type"))
            .and_then(|header| std::str::from_utf8(&header.value).ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        let handle = match asset::HandleResource::from_output(
            response.body,
            content_type,
            bindings::dekopon::asset::asset::Encoding::Identity,
            "response".to_owned(),
        )
        .await
        {
            Ok(handle) => handle,
            Err(error) => {
                self.assets.reject(error.code);
                return Ok(Err(wit::HttpError {
                    code: wit::ErrorCode::Internal,
                    message: error.message,
                }));
            }
        };
        Ok(Ok(wit::StreamedResponse {
            status: response.status,
            headers: response
                .headers
                .into_iter()
                .map(|header| wit::Header {
                    name: header.name,
                    value: header.value,
                })
                .collect(),
            body: self.table.push(handle)?,
        }))
    }

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

struct CompiledComponent {
    source: PathBuf,
    expected_provider_id: Option<ProviderId>,
    artifact_bytes: u64,
    artifact_sha256: String,
    compile_ms: u64,
    pre: bindings::ProviderPre<StoreState>,
    command_export: CommandExport,
}

pub struct BrokerWasmProvider {
    runtime: Arc<Runtime>,
    pre: bindings::ProviderPre<StoreState>,
    source: PathBuf,
    artifact_bytes: u64,
    artifact_sha256: String,
    manifest: ProviderManifest,
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

fn compile_component(
    runtime: &Runtime,
    source: ProviderSource,
) -> Result<CompiledComponent, BrokerHostError> {
    let span = tracing::info_span!(
        "provider.compile",
        path = %source.path.display(),
        artifact_bytes = tracing::field::Empty,
        artifact_sha256 = tracing::field::Empty,
        cache = if runtime.cwasm.is_some() { "lookup" } else { "bypass" },
        engine_key = %runtime.engine_key,
        cwasm_bytes = tracing::field::Empty,
        cwasm_sha256 = tracing::field::Empty,
        source_verify_us = tracing::field::Empty,
        cache_wait_us = tracing::field::Empty,
        elapsed_us = tracing::field::Empty,
        elapsed_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
    );
    span.in_scope(|| {
        let started = Instant::now();
        let mut result = prepare_component(runtime, source);
        let elapsed_us = cwasm::micros(started);
        let elapsed_ms = elapsed_us / 1000;
        if let Ok(compiled) = &mut result {
            compiled.compile_ms = elapsed_ms;
        }
        let outcome = if result.is_ok() { "ok" } else { "error" };
        span.record("elapsed_us", elapsed_us);
        span.record("elapsed_ms", elapsed_ms);
        span.record("outcome", outcome);
        match &result {
            Ok(_) => tracing::info!(elapsed_us, outcome, "provider component load finished"),
            Err(error) => tracing::error!(elapsed_us, outcome, error = %dekopon_core::bounded_attribute(&dekopon_core::error_chain(error)), "provider component load failed"),
        }
        result
    })
}

fn prepare_component(
    runtime: &Runtime,
    source: ProviderSource,
) -> Result<CompiledComponent, BrokerHostError> {
    let started = Instant::now();
    // Reads once, capped one byte over the limit, since a second read can't prove it matches what
    // Cranelift consumed and concurrent growth could otherwise allocate unbounded memory.
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
    let span = tracing::Span::current();
    span.record("artifact_bytes", artifact.bytes);
    span.record("artifact_sha256", &artifact.sha256);
    let source_verify_us = cwasm::micros(started);
    span.record("source_verify_us", source_verify_us);
    tracing::info!(
        source_verify_us,
        artifact_bytes = artifact.bytes,
        "provider source verified"
    );
    let expected_provider_id = source.expected.map(|expected| expected.provider_id);
    let source = source.path;
    let component = match &runtime.cwasm {
        Some(cache) => cache
            .load(&runtime.engine, &bytes, &artifact.sha256)
            .map_err(|error| BrokerHostError::CompiledArtifact {
                path: source.clone(),
                source: error,
            })?,
        None => cwasm::stage("compile", artifact.bytes, || {
            Component::new(&runtime.engine, &bytes)
        })
        .map_err(|error| BrokerHostError::Compile {
            path: source.clone(),
            source: error,
        })?,
    };
    drop(bytes);
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
        compile_ms: 0,
        pre,
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
            command_export,
        } = compiled;
        let manifest_json = describe_component(&runtime, &pre, &source)
            .instrument(tracing::info_span!(
                "provider.describe",
                path = %source.display(),
                stores = tracing::field::Empty,
                instantiations = tracing::field::Empty,
                fuel.consumed = tracing::field::Empty,
            ))
            .await?;
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
        // Checked at load from the component's own type, so a manifest promising command words it
        // can't run fails immediately rather than mid-session on first use.
        if let Err(problem) = check_command_export(&manifest, &command_export) {
            return Err(match problem {
                CommandExportProblem::Missing => BrokerHostError::MissingCommandExport {
                    provider: manifest.id.clone(),
                    path: source.clone(),
                },
                CommandExportProblem::Mismatched { found } => {
                    BrokerHostError::CommandExportSignature {
                        provider: manifest.id.clone(),
                        path: source.clone(),
                        found,
                    }
                }
            });
        }
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
            manifest,
            command_export,
        })
    }

    /// Runs before authorization, so a host-import attempt here is refused rather than trusted; it
    /// is import-free, timed out, and input/output-capped like describe.
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
        match &self.command_export {
            CommandExport::Present => {}
            CommandExport::Absent => {
                return Err(BrokerHostError::MissingCommandExport {
                    provider: self.manifest.id.clone(),
                    path: self.source.clone(),
                });
            }
            CommandExport::Mismatched { found } => {
                return Err(BrokerHostError::CommandExportSignature {
                    provider: self.manifest.id.clone(),
                    path: self.source.clone(),
                    found: found.clone(),
                });
            }
        }
        let operation_timeout = self.runtime.limits.max_timeout;
        let http = HttpState::describe(self.runtime.http_ceilings(), operation_timeout)
            .map_err(|source| BrokerHostError::HttpConfiguration { source })?;
        let mut store = self.runtime.store(
            http,
            storage::StorageState::disabled(),
            ClockState::describe(),
            SettingsState::describe(),
        )?;
        let argv = argv.to_vec();
        let stdin = stdin.map(str::to_owned);
        let signature = |source: wasmtime::Error| BrokerHostError::CommandExportSignature {
            provider: self.manifest.id.clone(),
            path: self.source.clone(),
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
            store.data_mut().instantiations += 1;
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
                    operation: format!("{RUN_COMMAND_EXPORT} {}", self.manifest.id),
                    timeout_ms: operation_timeout.as_millis() as u64,
                });
        record_store_outcome(&mut store, self.runtime.limits.fuel);
        // Check for a refused clock or settings read before the trap surfaces, or the real cause is
        // lost.
        if store.data().clock.attempted() || store.data().settings.attempted() {
            return Err(BrokerHostError::RunCommandUsedHostImport {
                path: self.source.clone(),
            });
        }
        let output = output??;
        if store.data().http.attempted()
            || store.data().storage.attempted()
            || store.data().assets.attempted()
        {
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

    #[allow(
        clippy::too_many_arguments,
        reason = "the invocation keeps separate authority grants and native resource ownership"
    )]
    async fn invoke(
        &self,
        capability: &CapabilityId,
        input: &Value,
        constraints: &ExecutionConstraints,
        credential: Option<BoundCredential>,
        storage_transaction: Option<dekopon_storage_host::StorageHandle>,
        assets: asset::AssetInputs,
        directory: Option<dekopon_http_host::asset::AssetDirectory>,
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

        let operation_timeout = Duration::from_millis(constraints.timeout_ms);
        let http = match HttpState::invoke(
            constraints.http.clone(),
            constraints.secret_use.clone(),
            credential,
            self.runtime.http_ceilings(),
            operation_timeout,
        ) {
            Ok(http) => http,
            Err(source) => return Err(BrokerHostError::HttpConfiguration { source }.into()),
        };
        let storage_state = storage_transaction.map_or_else(
            storage::StorageState::disabled,
            storage::StorageState::active,
        );
        let mut store = self.runtime.store(
            http,
            storage_state,
            ClockState::invoke(),
            SettingsState::invoke(
                self.runtime
                    .provider_settings
                    .get(&self.manifest.id)
                    .cloned(),
            ),
        )?;
        store.data_mut().assets = asset::AssetState::invoke(
            assets,
            asset::references(input),
            constraints.asset.clone().unwrap_or_default(),
            directory,
            format!("provider:{capability}"),
        )
        .await
        .map_err(|source| BrokerHostError::AssetInput { source })?;
        // Reads the actual initial fuel balance rather than assuming the configured budget applies,
        // since an unavailable observation must not be treated as zero usage.
        let initial_fuel = store.get_fuel().ok();
        store.data_mut().limits.observe_invocation(
            self.manifest.id.as_str(),
            capability.as_str(),
            initial_fuel,
        );
        // The store outlives the guest on every path, including a timeout dropping the operation
        // future, so dispatched-call evidence is harvested exactly once regardless of outcome.
        let mut executed = self
            .execute_in_store(
                &mut store,
                capability,
                &input_json,
                constraints,
                operation_timeout,
            )
            .await;
        store.data().assets.drain().await;
        // A caught, typed disk failure stays terminal even if later guest work times out or is
        // refused; never infer exhaustion from cancellation itself.
        if matches!(
            store.data().assets.violation(),
            Some(bindings::dekopon::asset::asset::ErrorCode::OverBudget)
        ) {
            executed = Err(BrokerHostError::AssetOverBudget);
        }
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
        record_store_outcome(&mut store, self.runtime.limits.fuel);
        store.data_mut().limits.finish(match &executed {
            Ok(_) => "succeeded",
            Err(BrokerHostError::ProviderFailure { .. }) => "provider-error",
            Err(BrokerHostError::Timeout { .. }) => "timeout",
            Err(BrokerHostError::Invoke { source, .. })
                if source.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::OutOfFuel) =>
            {
                "fuel-exhausted"
            }
            Err(BrokerHostError::Invoke { .. }) => "trap",
            Err(BrokerHostError::Instantiate { .. }) => "instantiation-error",
            Err(_) => "host-error",
        });
        let mut data = store.into_data();
        let storage = data.storage.take_evidence();
        let http_calls = data.http.into_evidence();
        match executed {
            Ok(output) => Ok(BrokerInvocationOutput {
                provider: self.manifest.id.clone(),
                capability: capability.clone(),
                output,
                assets: data.assets.finish(),
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
            store.data_mut().instantiations += 1;
            store.data_mut().limits.instantiated();
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
        // Host policy violations win even if the guest catches the error or a failing destructor
        // turns it into a trap; check policy before trusting the guest's result.
        if let Some(reason) = store.data().http.policy_violation() {
            return Err(BrokerHostError::HostCallRejected {
                provider: self.manifest.id.clone(),
                capability: capability.clone(),
                reason,
            });
        }
        if store.data().assets.violation().is_some() {
            return Err(BrokerHostError::HostCallRejected {
                provider: self.manifest.id.clone(),
                capability: capability.clone(),
                reason: "asset-call-rejected",
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

#[derive(Debug)]
pub struct BrokerProviderRegistry {
    providers: Vec<BrokerWasmProvider>,
    routes: BTreeMap<CapabilityId, usize>,
    storage_host: Option<StorageHost>,
    assets: Option<dekopon_http_host::asset::AssetDirectory>,
}

impl BrokerProviderRegistry {
    pub async fn load<I, P>(sources: I, limits: BrokerHostLimits) -> Result<Self, BrokerHostError>
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        Self::load_with_storage(sources, limits, None).await
    }

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
        let span = tracing::info_span!(
            "provider.registry_load",
            providers = sources.len(),
            mmap = options.cwasm_dir.is_some(),
            elapsed_us = tracing::field::Empty,
            outcome = tracing::field::Empty
        );
        let started = Instant::now();
        let result = async {
            if sources.is_empty() {
                return Err(BrokerHostError::NoProviders);
            }
            let runtime = Arc::new(Runtime::new(limits, options)?);
            let mut providers = Vec::with_capacity(sources.len());
            let mut scan = ConflictScan::new();
            for source in sources {
                let task_runtime = Arc::clone(&runtime);
                let task_source = source.clone();
                let span = tracing::Span::current();
                let compiling = tokio::task::spawn_blocking(move || {
                    span.in_scope(|| compile_component(&task_runtime, task_source))
                });
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
                assets: None,
            })
        }
        .instrument(span.clone())
        .await;
        let elapsed_us = cwasm::micros(started);
        let outcome = if result.is_ok() { "ok" } else { "error" };
        span.record("elapsed_us", elapsed_us);
        span.record("outcome", outcome);
        span.in_scope(|| tracing::info!(elapsed_us, outcome, "provider registry load finished"));
        result
    }

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
        let span = tracing::info_span!(
            "provider.run_command",
            provider = %provider.manifest.id,
            word,
            command.export = command_export_name(&provider.command_export),
            command.arguments = tracing::field::Empty,
            command.arguments.bytes = tracing::field::Empty,
            command.stdin = tracing::field::Empty,
            command.stdin.bytes = tracing::field::Empty,
            command.output = tracing::field::Empty,
            command.output.bytes = tracing::field::Empty,
            stores = tracing::field::Empty,
            instantiations = tracing::field::Empty,
            fuel.consumed = tracing::field::Empty,
        );
        let arguments = Value::Array(argv.iter().cloned().map(Value::String).collect()).to_string();
        span.record(
            "command.arguments",
            &*dekopon_core::bounded_attribute(&arguments),
        );
        span.record("command.arguments.bytes", arguments.len());
        if let Some(stdin) = stdin {
            span.record("command.stdin", &*dekopon_core::bounded_attribute(stdin));
            span.record("command.stdin.bytes", stdin.len());
        }
        let json = provider
            .run_command(argv, stdin)
            .instrument(span.clone())
            .await?;
        span.record("command.output", &*dekopon_core::bounded_attribute(&json));
        span.record("command.output.bytes", json.len());
        serde_json::from_str::<CommandRunOutcome>(&json).map_err(|source| {
            BrokerHostError::InvalidCommandRun {
                provider: provider.manifest.id.clone(),
                source,
            }
        })
    }

    pub fn manifests(&self) -> impl ExactSizeIterator<Item = &ProviderManifest> {
        self.providers.iter().map(|provider| &provider.manifest)
    }

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
            })
    }

    #[must_use]
    pub fn host_limits(&self) -> &BrokerHostLimits {
        &self
            .providers
            .first()
            .expect("a registry is constructed with at least one provider")
            .runtime
            .limits
    }

    pub fn set_assets(&mut self, directory: dekopon_http_host::asset::AssetDirectory) {
        self.assets = Some(directory);
    }

    #[must_use]
    pub fn storage_host(&self) -> Option<StorageHost> {
        self.storage_host.clone()
    }

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

    /// The credential stays separate from the authorization, which must remain safely renderable as
    /// evidence; a secret can't share that container, and the guest never observes it.
    pub async fn invoke(
        &self,
        authorized: AuthorizedInvocation,
        credential: Option<BoundCredential>,
        assets: asset::AssetInputs,
    ) -> Result<BrokerInvocationOutput, BrokerInvocationFailure> {
        self.invoke_with_storage(authorized, credential, None, assets)
            .await
    }

    pub async fn invoke_with_storage(
        &self,
        authorized: AuthorizedInvocation,
        credential: Option<BoundCredential>,
        storage_grant: Option<StorageGrant>,
        assets: asset::AssetInputs,
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
        let span = tracing::info_span!(
            "provider.invoke",
            capability = %proposal.capability,
            provider = %provider.manifest.id,
            input = tracing::field::Empty,
            input.bytes = tracing::field::Empty,
            storage = tracing::field::Empty,
            stores = tracing::field::Empty,
            instantiations = tracing::field::Empty,
            fuel.consumed = tracing::field::Empty,
        );
        let input = dekopon_core::bounded_display(&proposal.input);
        span.record("input", tracing::field::display(input.text()));
        span.record("input.bytes", input.bytes());
        if storage_backed {
            span.record("storage", true);
        }
        provider
            .invoke(
                &proposal.capability,
                &proposal.input,
                authorized.constraints(),
                credential,
                storage_transaction,
                assets,
                self.assets.clone(),
            )
            .instrument(span)
            .await
    }
}

fn record_store_outcome(store: &mut Store<StoreState>, supplied: u64) {
    let span = tracing::Span::current();
    span.record("instantiations", store.data().instantiations);
    let remaining = store.get_fuel().ok();
    if let Some(remaining) = remaining {
        span.record("fuel.consumed", supplied.saturating_sub(remaining));
    }
    store.data_mut().limits.record_remaining_fuel(remaining);
}

const fn command_export_name(export: &CommandExport) -> &'static str {
    match export {
        CommandExport::Present => RUN_COMMAND_EXPORT,
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
    let mut store = runtime.store(
        http,
        storage::StorageState::disabled(),
        ClockState::describe(),
        SettingsState::describe(),
    )?;
    let operation = async {
        let bindings = pre.instantiate_async(&mut store).await.map_err(|error| {
            BrokerHostError::Instantiate {
                path: source.to_path_buf(),
                source: error,
            }
        })?;
        store.data_mut().instantiations += 1;
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
    record_store_outcome(&mut store, runtime.limits.fuel);
    // During describe, check for a refused clock or settings read before the trap surfaces, or the
    // cause is lost.
    if store.data().clock.attempted() || store.data().settings.attempted() {
        return Err(BrokerHostError::DescribeUsedHostImport {
            path: source.to_path_buf(),
        });
    }
    let manifest = manifest??;
    if store.data().http.attempted()
        || store.data().storage.attempted()
        || store.data().assets.attempted()
    {
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
    host::validate_manifest(manifest)
        .map_err(|rejection| invalid_manifest(source, rejection.to_string()))
}

fn invalid_manifest(source: &Path, message: impl Into<String>) -> BrokerHostError {
    BrokerHostError::Manifest {
        path: source.to_path_buf(),
        message: message.into(),
    }
}

#[derive(Debug, Error)]
pub enum BrokerHostError {
    #[error("over-budget: broker asset disk capacity exhausted")]
    AssetOverBudget,
    #[error("invalid invocation assets")]
    AssetInput {
        #[source]
        source: asset::AssetAdmissionError,
    },

    #[error("at least one broker provider component is required")]
    NoProviders,
    #[error("broker host limit {name} must be greater than zero")]
    InvalidLimit { name: &'static str },
    #[error("authorization constraint {field} exceeds the broker host ceiling")]
    AuthorizationExceedsHostLimit { field: &'static str },
    #[error("HTTP authorization must contain destinations, methods, and positive limits")]
    InvalidHttpAuthorization,
    #[error("HTTP and storage authority cannot coexist in one capability")]
    MixedHostAuthorization,
    #[error("secret-use authorization is invalid")]
    InvalidSecretAuthorization {
        #[source]
        source: dekopon_capability::SecretUseGrantError,
    },
    #[error("secret-use authorization exceeds HTTP authority")]
    SecretAuthorizationExceedsHttp,
    #[error("resolved secret credential does not match authorized secret use")]
    SecretCredentialMismatch,
    #[error("provider storage is disabled")]
    StorageDisabled,
    #[error("authorized storage invocation is missing its storage grant")]
    MissingStorageGrant,
    #[error("storage grant accompanied an invocation with no storage authority")]
    UnexpectedStorageGrant,
    #[error("storage grant does not match authorized invocation")]
    StorageGrantMismatch,
    #[error("broker provider storage failed")]
    Storage {
        #[source]
        source: dekopon_storage_host::StorageHostError,
    },
    #[error("could not initialize the bounded HTTP execution context")]
    HttpConfiguration {
        #[source]
        source: dekopon_http_host::ConfigurationError,
    },
    #[error("could not initialize the broker Wasmtime engine")]
    Engine {
        #[source]
        source: wasmtime::Error,
    },
    #[error("could not initialize a bounded broker Wasmtime store")]
    Store {
        #[source]
        source: wasmtime::Error,
    },
    #[error("could not register broker host interfaces")]
    Linker {
        #[source]
        source: wasmtime::Error,
    },
    #[error("locked provider artifact is {size} bytes; maximum is {maximum}")]
    InvalidArtifactSize { size: u64, maximum: u64 },
    #[error("locked provider artifact digest must be sixty-four lowercase hexadecimal characters")]
    InvalidArtifactDigest,
    #[error(
        "broker provider component {} is {actual} bytes; maximum is {maximum}",
        path.display()
    )]
    ArtifactTooLarge {
        path: PathBuf,
        actual: u64,
        maximum: u64,
    },
    #[error(
        "broker provider component {} is {actual} bytes; provider lock expects {expected}",
        path.display()
    )]
    ArtifactSizeMismatch {
        path: PathBuf,
        expected: u64,
        actual: u64,
    },
    #[error(
        "broker provider component {} has SHA-256 {actual}; provider lock expects {expected}",
        path.display()
    )]
    ArtifactDigestMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error(
        "broker provider component {} describes provider {actual}; provider lock expects {expected}",
        path.display()
    )]
    ProviderIdentityMismatch {
        path: PathBuf,
        expected: ProviderId,
        actual: ProviderId,
    },
    #[error("could not inspect broker provider artifact {}", path.display())]
    ArtifactMetadata {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("compiled artifact load failed for {}; set compileOnLoad: true to bypass the cwasm cache", path.display())]
    CompiledArtifact {
        path: PathBuf,
        #[source]
        source: wasmtime::Error,
    },
    #[error(
        "broker provider stores already reserve the {maximum}-byte aggregate guest memory ceiling; \
         another {requested} bytes cannot be admitted"
    )]
    MemoryBudgetExhausted { requested: usize, maximum: usize },
    #[error("could not compile broker provider component {}", path.display())]
    Compile {
        path: PathBuf,
        #[source]
        source: wasmtime::Error,
    },
    #[error("could not instantiate broker provider component {}", path.display())]
    Instantiate {
        path: PathBuf,
        #[source]
        source: wasmtime::Error,
    },
    #[error("provider component {} attempted a host import during describe", path.display())]
    DescribeUsedHostImport { path: PathBuf },
    #[error("{report}")]
    ConflictingProviders { report: Box<ProviderConflicts> },
    #[error("provider {provider} returned an unreadable command run outcome")]
    InvalidCommandRun {
        provider: ProviderId,
        #[source]
        source: serde_json::Error,
    },
    #[error("no loaded provider declares the command word {word:?}")]
    UnknownCommandWord { word: String },
    #[error(
        "provider {provider} declares command words but component {} exports no run-command; \
         rebuild it against the dekopon:provider/provider-cli world",
        path.display()
    )]
    MissingCommandExport { provider: ProviderId, path: PathBuf },
    #[error(
        "provider {provider} exports run-command from component {} as {found}, not the function \
         the dekopon:provider package declares",
        path.display()
    )]
    CommandExportSignature {
        provider: ProviderId,
        path: PathBuf,
        found: String,
    },
    #[error("command input for provider {provider} is {length} bytes; broker maximum is {maximum}")]
    CommandInputTooLarge {
        provider: ProviderId,
        length: usize,
        maximum: usize,
    },
    #[error("provider {provider} failed while running a command word")]
    RunCommand {
        provider: ProviderId,
        #[source]
        source: wasmtime::Error,
    },
    #[error(
        "provider component {} attempted a host import during a command run",
        path.display()
    )]
    RunCommandUsedHostImport { path: PathBuf },
    #[error("broker provider component {} failed while describing itself", path.display())]
    Describe {
        path: PathBuf,
        #[source]
        source: wasmtime::Error,
    },
    #[error("broker provider component {} returned an invalid manifest", path.display())]
    InvalidManifest {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("broker provider component {} has an invalid manifest: {message}", path.display())]
    Manifest { path: PathBuf, message: String },
    #[error("no broker provider implements authorized capability {capability}")]
    UnknownCapability { capability: CapabilityId },
    #[error(
        "authorization selected provider {authorized} for {capability}, but route selects {routed}"
    )]
    AuthorizedProviderMismatch {
        capability: CapabilityId,
        authorized: ProviderId,
        routed: ProviderId,
    },
    #[error("broker provider {provider} does not implement capability {capability}")]
    ProviderDoesNotImplement {
        provider: ProviderId,
        capability: CapabilityId,
    },
    #[error("input for broker capability {capability} must be a JSON object")]
    InputNotObject { capability: CapabilityId },
    #[error("could not serialize broker provider input")]
    SerializeInput {
        #[source]
        source: serde_json::Error,
    },
    #[error("input for {capability} is {length} bytes; broker maximum is {maximum}")]
    InputTooLarge {
        capability: CapabilityId,
        length: usize,
        maximum: usize,
    },
    #[error("broker provider {provider} returned {length} bytes; maximum is {maximum}")]
    OutputTooLarge {
        provider: String,
        length: usize,
        maximum: usize,
    },
    #[error("broker provider operation {operation} exceeded {timeout_ms} ms")]
    Timeout { operation: String, timeout_ms: u64 },
    #[error("broker rejected host call {reason} from provider {provider} capability {capability}")]
    HostCallRejected {
        provider: ProviderId,
        capability: CapabilityId,
        reason: &'static str,
    },
    #[error(
        "broker rejected storage call {reason} from provider {provider} capability {capability}"
    )]
    StorageCallRejected {
        provider: ProviderId,
        capability: CapabilityId,
        reason: &'static str,
    },
    #[error("broker provider {provider} failed while invoking {capability}")]
    Invoke {
        provider: ProviderId,
        capability: CapabilityId,
        #[source]
        source: wasmtime::Error,
    },
    #[error("broker provider {provider} failed {capability} with {code}: {message}")]
    ProviderFailure {
        provider: ProviderId,
        capability: CapabilityId,
        code: String,
        message: String,
    },
    #[error("broker provider {provider} returned an invalid response for {capability}")]
    InvalidOutput {
        provider: ProviderId,
        capability: CapabilityId,
        #[source]
        source: serde_json::Error,
    },
}
