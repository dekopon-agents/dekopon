//! Broker-owned bounded asynchronous WebAssembly provider hosting.
//!
//! The current immediate host intentionally has an empty linker. This crate is the privileged
//! counterpart intended only for a separately deployed broker: it accepts an
//! [`AuthorizedInvocation`], links the project-owned buffered HTTP interface, and applies the
//! invocation's exact host-call constraints in a fresh store.

#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use dekopon_capability::{AuthorizedInvocation, ExecutionConstraints};
use dekopon_core::{CapabilityId, ProviderId};
pub use dekopon_provider_sdk::{
    ComponentFailure, ComponentResponse, ProviderApiVersion, ProviderCapability, ProviderManifest,
};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tokio::time::timeout;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};

mod http;
pub use http::HttpCallEvidence;
use http::{HttpCeilings, HttpState};

pub(crate) mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "provider",
        imports: { default: async | trappable },
        exports: { default: async },
    });
}

/// Provider export package mirrored into the broker host bindings.
pub const PROVIDER_WIT: &str = include_str!("../wit/deps/provider.wit");
/// Buffered HTTP package mirrored into the broker host bindings.
pub const HTTP_WIT: &str = include_str!("../wit/deps/http.wit");

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
pub const DEFAULT_FUEL: u64 = 10_000_000;
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
}

struct Runtime {
    engine: Engine,
    limits: BrokerHostLimits,
}

impl Runtime {
    fn new(limits: BrokerHostLimits) -> Result<Self, BrokerHostError> {
        validate_limits(&limits)?;
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.async_support(true);
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(|source| BrokerHostError::Engine { source })?;
        Ok(Self { engine, limits })
    }

    fn store(&self, http: HttpState) -> Result<Store<StoreState>, BrokerHostError> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(self.limits.max_memory_bytes)
            .table_elements(self.limits.max_table_elements)
            .instances(self.limits.max_instances)
            .tables(self.limits.max_tables)
            .memories(self.limits.max_memories)
            .build();
        let mut store = Store::new(&self.engine, StoreState { limits, http });
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(self.limits.fuel)
            .map_err(|source| BrokerHostError::Store { source })?;
        store
            .fuel_async_yield_interval(Some(self.limits.fuel.min(10_000)))
            .map_err(|source| BrokerHostError::Store { source })?;
        Ok(store)
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
    limits: StoreLimits,
    http: HttpState,
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
        let component = Component::from_file(&runtime.engine, &source).map_err(|error| {
            BrokerHostError::Compile {
                path: source.clone(),
                source: error,
            }
        })?;
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
        Ok(Self {
            runtime,
            component,
            source,
            manifest,
        })
    }

    async fn invoke(
        &self,
        capability: &CapabilityId,
        input: &Value,
        constraints: &ExecutionConstraints,
    ) -> Result<BrokerInvocationOutput, BrokerHostError> {
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
            });
        }
        if !input.is_object() {
            return Err(BrokerHostError::InputNotObject {
                capability: capability.clone(),
            });
        }
        let input_json = serde_json::to_string(input)
            .map_err(|source| BrokerHostError::SerializeInput { source })?;
        if input_json.len() > self.runtime.limits.max_input_bytes {
            return Err(BrokerHostError::InputTooLarge {
                capability: capability.clone(),
                length: input_json.len(),
                maximum: self.runtime.limits.max_input_bytes,
            });
        }

        let operation_timeout = Duration::from_millis(constraints.timeout_ms);
        let http = HttpState::invoke(
            constraints.http.clone(),
            self.runtime.http_ceilings(),
            operation_timeout,
        )
        .map_err(|source| BrokerHostError::HttpConfiguration { source })?;
        let mut store = self.runtime.store(http)?;
        let linker = self.runtime.linker()?;
        let operation = async {
            let bindings =
                bindings::Provider::instantiate_async(&mut store, &self.component, &linker)
                    .await
                    .map_err(|source| BrokerHostError::Instantiate {
                        path: self.source.clone(),
                        source,
                    })?;
            bindings
                .call_invoke(&mut store, capability.as_str(), &input_json)
                .await
                .map_err(|source| BrokerHostError::Invoke {
                    provider: self.manifest.id.clone(),
                    capability: capability.clone(),
                    source,
                })
        };
        let output_json = timeout(operation_timeout, operation).await.map_err(|_| {
            BrokerHostError::Timeout {
                operation: format!("invoke {capability}"),
                timeout_ms: constraints.timeout_ms,
            }
        })??;
        if let Some(reason) = store.data().http.policy_violation() {
            return Err(BrokerHostError::HostCallRejected {
                provider: self.manifest.id.clone(),
                capability: capability.clone(),
                reason,
            });
        }

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
        let output = match response {
            ComponentResponse::Succeeded { output } => output,
            ComponentResponse::Failed { error } => {
                return Err(BrokerHostError::ProviderFailure {
                    provider: self.manifest.id.clone(),
                    capability: capability.clone(),
                    code: error.code,
                    message: error.message,
                });
            }
        };
        let http_calls = store.into_data().http.into_evidence();
        Ok(BrokerInvocationOutput {
            provider: self.manifest.id.clone(),
            capability: capability.clone(),
            output,
            http_calls,
        })
    }
}

/// Deterministic capability registry owned by a privileged broker.
#[derive(Debug)]
pub struct BrokerProviderRegistry {
    providers: Vec<BrokerWasmProvider>,
    routes: BTreeMap<CapabilityId, usize>,
}

impl BrokerProviderRegistry {
    /// Compiles and validates provider components using one shared asynchronous engine.
    pub async fn load<I, P>(sources: I, limits: BrokerHostLimits) -> Result<Self, BrokerHostError>
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
        let mut provider_ids = BTreeSet::new();
        let mut routes = BTreeMap::new();
        for source in sources {
            let provider = BrokerWasmProvider::load(Arc::clone(&runtime), source).await?;
            if !provider_ids.insert(provider.manifest.id.clone()) {
                return Err(BrokerHostError::DuplicateProvider {
                    provider: provider.manifest.id.clone(),
                });
            }
            let provider_index = providers.len();
            for capability in &provider.manifest.capabilities {
                if routes
                    .insert(capability.id.clone(), provider_index)
                    .is_some()
                {
                    return Err(BrokerHostError::DuplicateCapability {
                        capability: capability.id.clone(),
                    });
                }
            }
            providers.push(provider);
        }
        Ok(Self { providers, routes })
    }

    /// Returns validated manifests in component load order.
    pub fn manifests(&self) -> impl ExactSizeIterator<Item = &ProviderManifest> {
        self.providers.iter().map(|provider| &provider.manifest)
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
        let runtime = &self
            .providers
            .first()
            .expect("a registry is constructed with at least one provider")
            .runtime;
        validate_authorized_constraints(constraints, &runtime.limits)
    }

    /// Consumes one broker-authorized proposal through its trusted capability route.
    pub async fn invoke(
        &self,
        authorized: AuthorizedInvocation,
    ) -> Result<BrokerInvocationOutput, BrokerHostError> {
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
            });
        }
        provider
            .invoke(
                &proposal.capability,
                &proposal.input,
                authorized.constraints(),
            )
            .await
    }
}

async fn describe_component(
    runtime: &Runtime,
    component: &Component,
    source: &Path,
) -> Result<String, BrokerHostError> {
    let operation_timeout = runtime.limits.max_timeout;
    let http = HttpState::describe(runtime.http_ceilings(), operation_timeout)
        .map_err(|source| BrokerHostError::HttpConfiguration { source })?;
    let mut store = runtime.store(http)?;
    let linker = runtime.linker()?;
    let operation = async {
        let bindings = bindings::Provider::instantiate_async(&mut store, component, &linker)
            .await
            .map_err(|error| BrokerHostError::Instantiate {
                path: source.to_path_buf(),
                source: error,
            })?;
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
            })??;
    if store.data().http.attempted() {
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
