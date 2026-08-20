//! Broker-owned bounded asynchronous WebAssembly provider hosting.
//!
//! The current immediate host intentionally has an empty linker. This crate is the privileged
//! counterpart intended only for a separately deployed broker: it accepts an
//! [`AuthorizedInvocation`], links only the project-owned buffered HTTP and namespace-bound
//! storage interfaces, and applies the invocation's exact host-call constraints in a fresh store.

#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use dekopon_capability::{AuthorizedInvocation, ExecutionConstraints};
use dekopon_core::{CapabilityId, CommandWordConflict, ProviderId};
pub use dekopon_provider_sdk::{
    CommandResolution, ComponentFailure, ComponentResponse, ProviderApiVersion, ProviderCapability,
    ProviderManifest,
};
use dekopon_storage_host::{StorageEvidence, StorageGrant, StorageHost};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tokio::time::timeout;
use tracing::Instrument as _;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store, StoreLimitsBuilder};

mod http;
mod metadata;
mod metrics;
mod storage;
pub use http::{BoundCredential, HttpCallEvidence};
use http::{HttpCeilings, HttpState};
pub use metadata::{ComponentInterfaceItem, LoadedProviderMetadata};
use metadata::{component_interface, identify_artifact};
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

/// Default maximum linear memory per provider call (64 MiB).
pub const DEFAULT_MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;
/// Default maximum elements in each Wasm table.
pub const DEFAULT_MAX_TABLE_ELEMENTS: usize = 100_000;
/// Default maximum core instances in one store.
pub const DEFAULT_MAX_INSTANCES: usize = 64;
/// Default maximum tables in one store.
pub const DEFAULT_MAX_TABLES: usize = 16;
/// Default maximum linear memories in one store.
pub const DEFAULT_MAX_MEMORIES: usize = 4;
/// Default maximum serialized provider input (1 MiB).
pub const DEFAULT_MAX_INPUT_BYTES: usize = 1024 * 1024;
/// Default maximum serialized manifest or provider output (1 MiB).
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
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
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
            max_table_elements: DEFAULT_MAX_TABLE_ELEMENTS,
            max_instances: DEFAULT_MAX_INSTANCES,
            max_tables: DEFAULT_MAX_TABLES,
            max_memories: DEFAULT_MAX_MEMORIES,
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
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

/// Failed broker-provider invocation and the evidence for calls that already executed.
///
/// An invocation can fail after the guest has already dispatched authorized HTTP requests, so
/// the terminal failure carries the same sanitized metadata a success would have carried. The
/// evidence is empty only when the failure preceded any dispatch.
#[derive(Debug)]
pub struct BrokerInvocationFailure {
    /// Host failure that ended the invocation.
    pub error: BrokerHostError,
    /// Sanitized metadata for every HTTP call dispatched before the failure.
    pub http_calls: Vec<HttpCallEvidence>,
    /// Content-free storage evidence when a storage transaction began.
    pub storage: Option<StorageEvidence>,
}

impl From<BrokerHostError> for BrokerInvocationFailure {
    fn from(error: BrokerHostError) -> Self {
        Self {
            error,
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
        Some(&self.error)
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

struct Runtime {
    engine: Engine,
    limits: BrokerHostLimits,
    metrics: BrokerHostMetrics,
}

impl Runtime {
    fn new(limits: BrokerHostLimits) -> Result<Self, BrokerHostError> {
        validate_limits(&limits)?;
        let metrics = BrokerHostMetrics::new(limits.clone());
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.async_support(true);
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(|source| BrokerHostError::Engine { source })?;
        Ok(Self {
            engine,
            limits,
            metrics,
        })
    }

    fn store(
        &self,
        http: HttpState,
        storage: storage::StorageState,
    ) -> Result<Store<StoreState>, BrokerHostError> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(self.limits.max_memory_bytes)
            .table_elements(self.limits.max_table_elements)
            .instances(self.limits.max_instances)
            .tables(self.limits.max_tables)
            .memories(self.limits.max_memories)
            .build();
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
            },
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(self.limits.fuel)
            .map_err(|source| BrokerHostError::Store { source })?;
        store
            .fuel_async_yield_interval(Some(self.limits.fuel.min(10_000)))
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

    fn linker(&self) -> Result<Linker<StoreState>, BrokerHostError> {
        let mut linker = Linker::new(&self.engine);
        bindings::Provider::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(|source| BrokerHostError::Linker { source })?;
        Ok(linker)
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

/// One provider component compiled by the broker host.
pub struct BrokerWasmProvider {
    runtime: Arc<Runtime>,
    component: Component,
    source: PathBuf,
    artifact_bytes: u64,
    artifact_sha256: String,
    imports: Vec<ComponentInterfaceItem>,
    exports: Vec<ComponentInterfaceItem>,
    interface_truncated: bool,
    manifest: ProviderManifest,
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

impl BrokerWasmProvider {
    async fn load(runtime: Arc<Runtime>, source: PathBuf) -> Result<Self, BrokerHostError> {
        let artifact =
            identify_artifact(&source).map_err(|error| BrokerHostError::ArtifactMetadata {
                path: source.clone(),
                source: error,
            })?;
        // Compilation happens once per provider at startup rather than per invocation, so this
        // span answers "why was the broker slow to become ready", not "why was that call slow".
        let compile = tracing::info_span!("provider.compile");
        let started = Instant::now();
        let component = compile
            .in_scope(|| Component::from_file(&runtime.engine, &source))
            .map_err(|error| BrokerHostError::Compile {
                path: source.clone(),
                source: error,
            })?;
        runtime
            .metrics
            .record_compilation(started.elapsed(), artifact.bytes);
        let after_compile =
            identify_artifact(&source).map_err(|error| BrokerHostError::ArtifactMetadata {
                path: source.clone(),
                source: error,
            })?;
        if after_compile != artifact {
            return Err(BrokerHostError::ArtifactChanged { path: source });
        }
        let (imports, exports, interface_truncated) =
            component_interface(&runtime.engine, &component);
        let manifest_json = describe_component(&runtime, &component, &source).await?;
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
        // A manifest that promises command words the component cannot rewrite would fail at the
        // first `gh …` a model typed, in a session, hours later. Prove it at load instead.
        if !manifest.command_words.is_empty()
            && !exports_resolve_command(&runtime, &component, &source).await?
        {
            return Err(BrokerHostError::MissingResolveCommand {
                provider: manifest.id.clone(),
                path: source.clone(),
            });
        }
        runtime.metrics.record_provider_loaded();
        Ok(Self {
            runtime,
            component,
            source,
            artifact_bytes: artifact.bytes,
            artifact_sha256: artifact.sha256,
            imports,
            exports,
            interface_truncated,
            manifest,
        })
    }

    /// Rewrites one command word's argv into a capability proposal, inside the guest.
    ///
    /// Bounded exactly as `describe` is: import-free, timed out, and output-capped. The rewrite
    /// runs *before* authorization, so a component that reaches for a host import here is refused
    /// rather than trusted.
    pub async fn resolve_command(&self, argv: &[String]) -> Result<String, BrokerHostError> {
        self.runtime.metrics.record_command_resolution();
        let operation_timeout = self.runtime.limits.max_timeout;
        let http = HttpState::describe(self.runtime.http_ceilings(), operation_timeout)
            .map_err(|source| BrokerHostError::HttpConfiguration { source })?;
        let mut store = self
            .runtime
            .store(http, storage::StorageState::disabled())?;
        let linker = self.runtime.linker()?;
        let argv = argv.to_vec();
        let operation = async {
            let instance = linker
                .instantiate_async(&mut store, &self.component)
                .await
                .map_err(|source| BrokerHostError::Instantiate {
                    path: self.source.clone(),
                    source,
                })?;
            self.runtime.metrics.record_instantiation();
            let function = resolve_command_export(&mut store, &instance).ok_or_else(|| {
                BrokerHostError::MissingResolveCommand {
                    provider: self.manifest.id.clone(),
                    path: self.source.clone(),
                }
            })?;
            let (output,) = function
                .call_async(&mut store, (argv,))
                .await
                .map_err(|source| BrokerHostError::ResolveCommand {
                    provider: self.manifest.id.clone(),
                    source,
                })?;
            function
                .post_return_async(&mut store)
                .await
                .map_err(|source| BrokerHostError::ResolveCommand {
                    provider: self.manifest.id.clone(),
                    source,
                })?;
            Ok::<_, BrokerHostError>(output)
        };
        let output =
            timeout(operation_timeout, operation)
                .await
                .map_err(|_| BrokerHostError::Timeout {
                    operation: format!("resolve-command {}", self.manifest.id),
                    timeout_ms: operation_timeout.as_millis() as u64,
                });
        self.runtime.record_fuel(&mut store);
        let output = output??;
        if store.data().http.attempted() || store.data().storage.attempted() {
            return Err(BrokerHostError::ResolveCommandUsedHostImport {
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
                error,
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
        let linker = self.runtime.linker()?;
        let operation = async {
            let bindings =
                bindings::Provider::instantiate_async(&mut *store, &self.component, &linker)
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

/// Everything wrong with one provider set, gathered so an operator sees it once.
///
/// Ambiguity is fatal in a way absence is not: a word or capability two providers both claim has no
/// meaning the broker can pick without silently choosing for the operator. This reports rather than
/// resolves, and reports *all of it* — fixing a provider directory should take one restart.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderConflicts {
    /// Provider identities declared by more than one component.
    pub providers: Vec<ProviderId>,
    /// Capability identifiers declared by more than one component.
    pub capabilities: Vec<CapabilityId>,
    /// Command words that cannot be granted to the providers claiming them.
    pub command_words: Vec<CommandWordConflict>,
}

impl ProviderConflicts {
    /// Reports how many distinct conflicts this covers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.len() + self.capabilities.len() + self.command_words.len()
    }

    /// Reports whether there is nothing to complain about.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl fmt::Display for ProviderConflicts {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "refusing to start \u{2014} {} provider conflict(s)",
            self.len()
        )?;
        for provider in &self.providers {
            writeln!(formatter, "\n  provider {provider}")?;
            writeln!(formatter, "    declared by more than one component")?;
            writeln!(
                formatter,
                "    fix: remove one, or drop it from the provider search path"
            )?;
        }
        for capability in &self.capabilities {
            writeln!(formatter, "\n  capability {capability}")?;
            writeln!(formatter, "    declared by more than one component")?;
            writeln!(
                formatter,
                "    fix: rename it in one provider, or drop that provider"
            )?;
        }
        for conflict in &self.command_words {
            writeln!(formatter, "\n  command word `{}`", conflict.word)?;
            for claimant in &conflict.claimants {
                writeln!(formatter, "    claimed by  {claimant}")?;
            }
            writeln!(formatter, "    {}", conflict.kind.explanation())?;
            writeln!(formatter, "    fix: {}", conflict.kind.remedy())?;
        }
        if !self.command_words.is_empty() {
            write!(
                formatter,
                "\nReserved words: {}",
                dekopon_core::RESERVED_COMMAND_WORDS.join(" ")
            )?;
        }
        Ok(())
    }
}

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
        let sources = sources.into_iter().map(Into::into).collect::<Vec<_>>();
        if sources.is_empty() {
            return Err(BrokerHostError::NoProviders);
        }
        let runtime = Arc::new(Runtime::new(limits)?);
        let mut providers = Vec::with_capacity(sources.len());
        let mut routes = BTreeMap::new();
        // Every conflict, then one failure. Returning on the first would make fixing a provider
        // directory take one restart per mistake; an operator should see the whole picture once.
        let mut duplicate_providers = BTreeSet::new();
        let mut duplicate_capabilities = BTreeSet::new();
        let mut provider_ids = BTreeSet::new();
        let mut declared_words = Vec::new();
        for source in sources {
            let provider = BrokerWasmProvider::load(Arc::clone(&runtime), source).await?;
            if !provider_ids.insert(provider.manifest.id.clone()) {
                duplicate_providers.insert(provider.manifest.id.clone());
            }
            declared_words.push((
                provider.manifest.id.to_string(),
                provider.manifest.command_words.clone(),
            ));
            let provider_index = providers.len();
            for capability in &provider.manifest.capabilities {
                if routes
                    .insert(capability.id.clone(), provider_index)
                    .is_some()
                {
                    duplicate_capabilities.insert(capability.id.clone());
                }
            }
            providers.push(provider);
        }

        let command_words = dekopon_core::command_word_conflicts(&declared_words);
        if !duplicate_providers.is_empty()
            || !duplicate_capabilities.is_empty()
            || !command_words.is_empty()
        {
            return Err(BrokerHostError::ConflictingProviders {
                report: Box::new(ProviderConflicts {
                    providers: duplicate_providers.into_iter().collect(),
                    capabilities: duplicate_capabilities.into_iter().collect(),
                    command_words,
                }),
            });
        }
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

    /// Rewrites one command word's argv through the provider that declared it.
    ///
    /// # Errors
    ///
    /// Returns [`BrokerHostError::UnknownCommandWord`] when no loaded provider declared it, and any
    /// guest failure from the rewrite itself.
    pub async fn resolve_command(
        &self,
        word: &str,
        argv: &[String],
    ) -> Result<CommandResolution, BrokerHostError> {
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
        let json = provider.resolve_command(argv).await?;
        serde_json::from_str::<CommandResolution>(&json).map_err(|source| {
            BrokerHostError::InvalidCommandResolution {
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
        let storage_transaction = match (&authorized.constraints().storage, storage_grant) {
            (None, None) => None,
            (None, Some(_)) => return Err(BrokerHostError::UnexpectedStorageGrant.into()),
            (Some(_), None) => return Err(BrokerHostError::MissingStorageGrant.into()),
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

/// Looks up the optional `resolve-command` export on an instantiated component.
///
/// Optional on purpose: `dekopon:provider@0.2.0` defines it in a separate `provider-commands`
/// world so a component built against an earlier package version keeps loading, contributing no
/// command words. Independent provider release cadence is the whole point of moving providers out
/// of this repository, and it does not survive a contract that forces lockstep rebuilds.
fn resolve_command_export(
    store: &mut Store<StoreState>,
    instance: &wasmtime::component::Instance,
) -> Option<wasmtime::component::TypedFunc<(Vec<String>,), (String,)>> {
    instance
        .get_typed_func::<(Vec<String>,), (String,)>(&mut *store, "resolve-command")
        .ok()
}

/// Reports whether a component exports `resolve-command`, by instantiating it once.
async fn exports_resolve_command(
    runtime: &Runtime,
    component: &Component,
    source: &Path,
) -> Result<bool, BrokerHostError> {
    let http = HttpState::describe(runtime.http_ceilings(), runtime.limits.max_timeout)
        .map_err(|source| BrokerHostError::HttpConfiguration { source })?;
    let mut store = runtime.store(http, storage::StorageState::disabled())?;
    let linker = runtime.linker()?;
    let instantiated = linker
        .instantiate_async(&mut store, component)
        .await
        .map_err(|error| BrokerHostError::Instantiate {
            path: source.to_path_buf(),
            source: error,
        });
    runtime.record_fuel(&mut store);
    let instance = instantiated?;
    runtime.metrics.record_instantiation();
    Ok(resolve_command_export(&mut store, &instance).is_some())
}

async fn describe_component(
    runtime: &Runtime,
    component: &Component,
    source: &Path,
) -> Result<String, BrokerHostError> {
    let operation_timeout = runtime.limits.max_timeout;
    let http = HttpState::describe(runtime.http_ceilings(), operation_timeout)
        .map_err(|source| BrokerHostError::HttpConfiguration { source })?;
    let mut store = runtime.store(http, storage::StorageState::disabled())?;
    let linker = runtime.linker()?;
    let operation = async {
        let bindings = bindings::Provider::instantiate_async(&mut store, component, &linker)
            .await
            .map_err(|error| BrokerHostError::Instantiate {
                path: source.to_path_buf(),
                source: error,
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
    for (name, value) in [
        ("max_memory_bytes", limits.max_memory_bytes as u128),
        ("max_table_elements", limits.max_table_elements as u128),
        ("max_instances", limits.max_instances as u128),
        ("max_tables", limits.max_tables as u128),
        ("max_memories", limits.max_memories as u128),
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
    ] {
        if value == 0 {
            return Err(BrokerHostError::InvalidLimit { name });
        }
    }
    Ok(())
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
    if manifest.description.trim().is_empty() {
        return Err(invalid_manifest(source, "description must not be empty"));
    }
    if manifest.capabilities.is_empty() {
        return Err(invalid_manifest(
            source,
            "at least one capability is required",
        ));
    }
    let mut capabilities = BTreeSet::new();
    for capability in &manifest.capabilities {
        if !capabilities.insert(capability.id.clone()) {
            return Err(invalid_manifest(
                source,
                format!("capability {} is declared more than once", capability.id),
            ));
        }
        if capability.description.trim().is_empty() {
            return Err(invalid_manifest(
                source,
                format!("capability {} has an empty description", capability.id),
            ));
        }
        let Some(schema) = capability.input_schema.as_object() else {
            return Err(invalid_manifest(
                source,
                format!("capability {} inputSchema must be an object", capability.id),
            ));
        };
        if schema.get("type").and_then(Value::as_str) != Some("object") {
            return Err(invalid_manifest(
                source,
                format!(
                    "capability {} inputSchema must declare type object",
                    capability.id
                ),
            ));
        }
    }
    Ok(())
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
    /// Source artifact metadata could not be read for the informational provider view.
    #[error("could not inspect broker provider artifact {}", path.display())]
    ArtifactMetadata {
        /// Component path.
        path: PathBuf,
        /// File read failure.
        #[source]
        source: std::io::Error,
    },
    /// Provider source changed while startup was compiling it, so retained metadata would lie.
    #[error("broker provider artifact changed while it was being compiled: {}", path.display())]
    ArtifactChanged {
        /// Component path.
        path: PathBuf,
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
    /// A provider returned something that is not a command resolution.
    #[error("provider {provider} returned an unreadable command resolution")]
    InvalidCommandResolution {
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
    /// Provider declared command words but exports no way to rewrite them.
    #[error(
        "provider {provider} declares command words but component {} exports no resolve-command; \
         rebuild it against the dekopon:provider/provider-commands world",
        path.display()
    )]
    MissingResolveCommand {
        /// Provider identity.
        provider: ProviderId,
        /// Component path.
        path: PathBuf,
    },
    /// Rewriting a command word failed inside the guest.
    #[error("provider {provider} failed while rewriting a command word")]
    ResolveCommand {
        /// Provider identity.
        provider: ProviderId,
        /// Underlying trap or error.
        #[source]
        source: wasmtime::Error,
    },
    /// Provider attempted a host call while rewriting a command word.
    ///
    /// The rewrite runs before authorization, so a component reaching for host authority there is
    /// refused rather than trusted.
    #[error(
        "provider component {} attempted a host import during resolve-command",
        path.display()
    )]
    ResolveCommandUsedHostImport {
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
    /// Provider identity was duplicated.
    #[error("broker provider {provider} is declared by more than one component")]
    DuplicateProvider {
        /// Provider ID.
        provider: ProviderId,
    },
    /// Capability route was duplicated.
    #[error("broker capability {capability} is declared by more than one provider")]
    DuplicateCapability {
        /// Capability ID.
        capability: CapabilityId,
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
