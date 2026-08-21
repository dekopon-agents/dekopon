//! Bounded WebAssembly component hosting for immediate-mode Dekopon providers.
//!
//! This crate intentionally supports only read-only components with no host imports. It is a
//! useful execution laboratory, not an authorization broker: loading a component does not grant
//! external authority, credentials, filesystem access, network access, clocks, or environment
//! variables.

#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use dekopon_capability::EffectKind;
use dekopon_core::{CapabilityId, ProviderId};
pub use dekopon_provider_sdk::{
    ComponentFailure, ComponentResponse, ProviderApiVersion, ProviderCapability, ProviderManifest,
};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "provider",
    });
}

/// The WIT contract implemented by provider components.
pub const PROVIDER_WIT: &str = include_str!("../wit/provider.wit");

/// Default maximum linear-memory allocation per provider invocation (64 MiB).
pub const DEFAULT_MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;
/// Default maximum serialized provider input size (1 MiB).
pub const DEFAULT_MAX_INPUT_BYTES: usize = 1024 * 1024;
/// Default maximum serialized provider output or manifest size (1 MiB).
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// Default Wasm instruction fuel supplied to each store.
pub const DEFAULT_FUEL: u64 = 10_000_000;
/// Default wall-clock timeout for instantiation and invocation.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Limits applied independently to every provider description or invocation call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostLimits {
    /// Maximum linear-memory allocation in one store.
    pub max_memory_bytes: usize,
    /// Maximum serialized invocation input.
    pub max_input_bytes: usize,
    /// Maximum serialized manifest or invocation output.
    pub max_output_bytes: usize,
    /// Wasm instruction fuel supplied to one store.
    pub fuel: u64,
    /// Wall-clock bound for instantiation plus the exported call.
    pub timeout: Duration,
}

impl Default for HostLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            fuel: DEFAULT_FUEL,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// Successful output from a routed provider invocation.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InvocationOutput {
    /// Provider selected for the capability.
    pub provider: ProviderId,
    /// Invoked capability.
    pub capability: CapabilityId,
    /// Valid JSON returned by the component.
    pub output: Value,
}

struct Runtime {
    engine: Engine,
    limits: HostLimits,
    execution: Mutex<()>,
}

impl Runtime {
    fn new(limits: HostLimits) -> Result<Self, ProviderHostError> {
        validate_limits(&limits)?;

        let mut config = Config::new();
        config.wasm_component_model(true);
        config.consume_fuel(true);
        config.epoch_interruption(true);
        let engine = Engine::new(&config).map_err(|source| ProviderHostError::Engine { source })?;

        Ok(Self {
            engine,
            limits,
            execution: Mutex::new(()),
        })
    }

    fn store(&self) -> Result<Store<StoreState>, ProviderHostError> {
        let store_limits = StoreLimitsBuilder::new()
            .memory_size(self.limits.max_memory_bytes)
            .build();
        let mut store = Store::new(
            &self.engine,
            StoreState {
                limits: store_limits,
            },
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(self.limits.fuel)
            .map_err(|source| ProviderHostError::Store { source })?;
        store.set_epoch_deadline(1);
        Ok(store)
    }
}

#[derive(Debug)]
struct StoreState {
    limits: StoreLimits,
}

/// A provider component compiled by Wasmtime.
pub struct WasmProvider {
    runtime: Arc<Runtime>,
    component: Component,
    source: PathBuf,
    manifest: ProviderManifest,
}

impl fmt::Debug for WasmProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WasmProvider")
            .field("source", &self.source)
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

impl WasmProvider {
    fn load(runtime: Arc<Runtime>, source: PathBuf) -> Result<Self, ProviderHostError> {
        let compile_span = tracing::info_span!(
            "provider.compile",
            provider.path = %source.display()
        );
        let component = {
            let _entered = compile_span.enter();
            Component::from_file(&runtime.engine, &source).map_err(|error| {
                ProviderHostError::Compile {
                    path: source.clone(),
                    source: error,
                }
            })?
        };

        let manifest_json = describe_component(&runtime, &component, &source)?;
        if manifest_json.len() > runtime.limits.max_output_bytes {
            return Err(ProviderHostError::OutputTooLarge {
                provider: source.display().to_string(),
                length: manifest_json.len(),
                maximum: runtime.limits.max_output_bytes,
            });
        }
        let manifest =
            serde_json::from_str::<ProviderManifest>(&manifest_json).map_err(|error| {
                ProviderHostError::InvalidManifest {
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
}

impl WasmProvider {
    /// Returns the validated component manifest.
    #[must_use]
    pub const fn manifest(&self) -> &ProviderManifest {
        &self.manifest
    }

    /// Invokes one capability in a fresh bounded execution context.
    ///
    /// This carries no authorization semantics: immediate mode is read-only and unprivileged.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderHostError`] when the component does not implement the capability, the
    /// input is not object-shaped, execution exceeds a bound, or the response does not decode.
    pub fn invoke(
        &self,
        capability: &CapabilityId,
        input: &Value,
    ) -> Result<Value, ProviderHostError> {
        if !self
            .manifest
            .capabilities
            .iter()
            .any(|candidate| &candidate.id == capability)
        {
            return Err(ProviderHostError::ProviderDoesNotImplement {
                provider: self.manifest.id.clone(),
                capability: capability.clone(),
            });
        }

        if !input.is_object() {
            return Err(ProviderHostError::InputNotObject {
                capability: capability.clone(),
            });
        }
        let input_json = serde_json::to_string(input)
            .map_err(|source| ProviderHostError::SerializeInput { source })?;
        if input_json.len() > self.runtime.limits.max_input_bytes {
            return Err(ProviderHostError::InputTooLarge {
                capability: capability.clone(),
                length: input_json.len(),
                maximum: self.runtime.limits.max_input_bytes,
            });
        }

        let span = tracing::info_span!(
            "provider.invoke",
            provider.id = %self.manifest.id,
            capability.id = %capability,
            provider.path = %self.source.display()
        );
        let _entered = span.enter();
        let _execution = self
            .runtime
            .execution
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let mut store = self.runtime.store()?;
        let linker = Linker::new(&self.runtime.engine);
        let mut timer =
            EpochTimer::start(self.runtime.engine.clone(), self.runtime.limits.timeout)?;
        let bindings = match bindings::Provider::instantiate(&mut store, &self.component, &linker) {
            Ok(bindings) => bindings,
            Err(source) => {
                if timer.stop() {
                    return Err(ProviderHostError::Timeout {
                        operation: format!("instantiate {}", self.manifest.id),
                        timeout_ms: self.runtime.limits.timeout.as_millis(),
                    });
                }
                return Err(ProviderHostError::Instantiate {
                    path: self.source.clone(),
                    source,
                });
            }
        };
        let result = bindings.call_invoke(&mut store, capability.as_str(), &input_json);
        let expired = timer.stop();
        if expired {
            return Err(ProviderHostError::Timeout {
                operation: format!("invoke {capability}"),
                timeout_ms: self.runtime.limits.timeout.as_millis(),
            });
        }
        let output_json = result.map_err(|source| ProviderHostError::Invoke {
            provider: self.manifest.id.clone(),
            capability: capability.clone(),
            source,
        })?;

        if output_json.len() > self.runtime.limits.max_output_bytes {
            return Err(ProviderHostError::OutputTooLarge {
                provider: self.manifest.id.to_string(),
                length: output_json.len(),
                maximum: self.runtime.limits.max_output_bytes,
            });
        }
        let response =
            serde_json::from_str::<ComponentResponse>(&output_json).map_err(|source| {
                ProviderHostError::InvalidOutput {
                    provider: self.manifest.id.clone(),
                    capability: capability.clone(),
                    source,
                }
            })?;
        match response {
            ComponentResponse::Succeeded { output } => Ok(output),
            ComponentResponse::Failed { error } => Err(ProviderHostError::ProviderFailure {
                provider: self.manifest.id.clone(),
                capability: capability.clone(),
                code: error.code,
                message: error.message,
            }),
        }
    }
}

/// A deterministic capability-to-component registry.
#[derive(Debug)]
pub struct ProviderRegistry {
    providers: Vec<WasmProvider>,
    routes: BTreeMap<CapabilityId, usize>,
}

impl ProviderRegistry {
    /// Loads and validates all components with one shared Wasmtime engine.
    pub fn load<I, P>(sources: I, limits: HostLimits) -> Result<Self, ProviderHostError>
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        let sources = sources.into_iter().map(Into::into).collect::<Vec<_>>();
        if sources.is_empty() {
            return Err(ProviderHostError::NoProviders);
        }

        let runtime = Arc::new(Runtime::new(limits)?);
        let mut providers = Vec::with_capacity(sources.len());
        let mut provider_ids = BTreeSet::new();
        let mut routes = BTreeMap::new();

        for source in sources {
            let provider = WasmProvider::load(Arc::clone(&runtime), source)?;
            if !provider_ids.insert(provider.manifest.id.clone()) {
                return Err(ProviderHostError::DuplicateProvider {
                    provider: provider.manifest.id.clone(),
                });
            }

            let provider_index = providers.len();
            for capability in &provider.manifest.capabilities {
                if routes
                    .insert(capability.id.clone(), provider_index)
                    .is_some()
                {
                    return Err(ProviderHostError::DuplicateCapability {
                        capability: capability.id.clone(),
                    });
                }
            }
            providers.push(provider);
        }

        Ok(Self { providers, routes })
    }

    /// Returns manifests in command-line load order.
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
                .expect("registry routes are built from provider manifests");
            (&provider.manifest.id, capability)
        })
    }

    /// Routes and invokes one capability.
    pub fn invoke(
        &self,
        capability: &CapabilityId,
        input: &Value,
    ) -> Result<InvocationOutput, ProviderHostError> {
        let provider_index = self.routes.get(capability).copied().ok_or_else(|| {
            ProviderHostError::UnknownCapability {
                capability: capability.clone(),
            }
        })?;
        let provider = &self.providers[provider_index];
        let output = provider.invoke(capability, input)?;

        Ok(InvocationOutput {
            provider: provider.manifest.id.clone(),
            capability: capability.clone(),
            output,
        })
    }
}

fn describe_component(
    runtime: &Runtime,
    component: &Component,
    source: &Path,
) -> Result<String, ProviderHostError> {
    let span = tracing::info_span!("provider.describe", provider.path = %source.display());
    let _entered = span.enter();
    let _execution = runtime
        .execution
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let mut store = runtime.store()?;
    let linker = Linker::new(&runtime.engine);
    let mut timer = EpochTimer::start(runtime.engine.clone(), runtime.limits.timeout)?;
    let bindings = match bindings::Provider::instantiate(&mut store, component, &linker) {
        Ok(bindings) => bindings,
        Err(error) => {
            if timer.stop() {
                return Err(ProviderHostError::Timeout {
                    operation: format!("instantiate {}", source.display()),
                    timeout_ms: runtime.limits.timeout.as_millis(),
                });
            }
            return Err(ProviderHostError::Instantiate {
                path: source.to_path_buf(),
                source: error,
            });
        }
    };
    let result = bindings.call_describe(&mut store);
    let expired = timer.stop();

    if expired {
        return Err(ProviderHostError::Timeout {
            operation: format!("describe {}", source.display()),
            timeout_ms: runtime.limits.timeout.as_millis(),
        });
    }
    result.map_err(|error| ProviderHostError::Describe {
        path: source.to_path_buf(),
        source: error,
    })
}

fn validate_limits(limits: &HostLimits) -> Result<(), ProviderHostError> {
    for (name, value) in [
        ("max_memory_bytes", limits.max_memory_bytes as u128),
        ("max_input_bytes", limits.max_input_bytes as u128),
        ("max_output_bytes", limits.max_output_bytes as u128),
        ("fuel", u128::from(limits.fuel)),
        ("timeout", limits.timeout.as_nanos()),
    ] {
        if value == 0 {
            return Err(ProviderHostError::InvalidLimit { name });
        }
    }
    Ok(())
}

fn validate_manifest(manifest: &ProviderManifest, source: &Path) -> Result<(), ProviderHostError> {
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
        if capability.effect != EffectKind::ReadOnly {
            return Err(ProviderHostError::UnsupportedEffect {
                provider: manifest.id.clone(),
                capability: capability.id.clone(),
                effect: capability.effect,
            });
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

fn invalid_manifest(source: &Path, message: impl Into<String>) -> ProviderHostError {
    ProviderHostError::Manifest {
        path: source.to_path_buf(),
        message: message.into(),
    }
}

#[derive(Debug)]
struct EpochTimer {
    cancel: Option<Sender<()>>,
    handle: Option<JoinHandle<()>>,
    expired: Arc<AtomicBool>,
}

impl EpochTimer {
    fn start(engine: Engine, timeout: Duration) -> Result<Self, ProviderHostError> {
        let (cancel, receiver) = mpsc::channel();
        let expired = Arc::new(AtomicBool::new(false));
        let thread_expired = Arc::clone(&expired);
        let handle = thread::Builder::new()
            .name("dekopon-provider-deadline".to_owned())
            .spawn(move || {
                if matches!(
                    receiver.recv_timeout(timeout),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    thread_expired.store(true, Ordering::Release);
                    engine.increment_epoch();
                }
            })
            .map_err(|source| ProviderHostError::Timer { source })?;

        Ok(Self {
            cancel: Some(cancel),
            handle: Some(handle),
            expired,
        })
    }

    fn stop(&mut self) -> bool {
        if let Some(cancel) = self.cancel.take() {
            let _ignored = cancel.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ignored = handle.join();
        }
        self.expired.load(Ordering::Acquire)
    }
}

impl Drop for EpochTimer {
    fn drop(&mut self) {
        let _expired = self.stop();
    }
}

/// Failure to load, validate, route, or invoke a provider component.
#[derive(Debug, Error)]
pub enum ProviderHostError {
    /// No component paths were supplied.
    #[error("at least one provider component is required")]
    NoProviders,
    /// A configured execution limit was zero.
    #[error("provider host limit {name} must be greater than zero")]
    InvalidLimit {
        /// Invalid limit name.
        name: &'static str,
    },
    /// Wasmtime engine initialization failed.
    #[error("could not initialize the Wasmtime engine")]
    Engine {
        /// Wasmtime error.
        #[source]
        source: wasmtime::Error,
    },
    /// Store initialization failed.
    #[error("could not initialize a bounded Wasmtime store")]
    Store {
        /// Wasmtime error.
        #[source]
        source: wasmtime::Error,
    },
    /// Starting the wall-clock deadline worker failed.
    #[error("could not start provider deadline worker")]
    Timer {
        /// Thread creation error.
        #[source]
        source: io::Error,
    },
    /// Component compilation or decoding failed.
    #[error("could not compile provider component {}", path.display())]
    Compile {
        /// Component path.
        path: PathBuf,
        /// Wasmtime error.
        #[source]
        source: wasmtime::Error,
    },
    /// Component imports could not be linked or instantiated.
    #[error("could not instantiate provider component {}", path.display())]
    Instantiate {
        /// Component path.
        path: PathBuf,
        /// Wasmtime error.
        #[source]
        source: wasmtime::Error,
    },
    /// Calling the provider description export failed.
    #[error("provider component {} failed while describing itself", path.display())]
    Describe {
        /// Component path.
        path: PathBuf,
        /// Wasmtime error.
        #[source]
        source: wasmtime::Error,
    },
    /// A component returned malformed manifest JSON.
    #[error("provider component {} returned an invalid manifest", path.display())]
    InvalidManifest {
        /// Component path.
        path: PathBuf,
        /// JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// A component manifest violated a semantic constraint.
    #[error("provider component {} has an invalid manifest: {message}", path.display())]
    Manifest {
        /// Component path.
        path: PathBuf,
        /// Validation detail.
        message: String,
    },
    /// Immediate mode was asked to load a mutating capability.
    #[error(
        "provider {provider} capability {capability} has unsupported effect {effect}; immediate mode permits read-only capabilities only"
    )]
    UnsupportedEffect {
        /// Provider identity.
        provider: ProviderId,
        /// Capability identity.
        capability: CapabilityId,
        /// Rejected effect.
        effect: EffectKind,
    },
    /// Two components declared the same provider ID.
    #[error("provider {provider} is declared by more than one component")]
    DuplicateProvider {
        /// Duplicate provider identity.
        provider: ProviderId,
    },
    /// Two loaded components declared the same capability ID.
    #[error("capability {capability} is declared by more than one provider component")]
    DuplicateCapability {
        /// Duplicate capability identity.
        capability: CapabilityId,
    },
    /// No loaded component declared the requested capability.
    #[error("no loaded provider implements capability {capability}")]
    UnknownCapability {
        /// Requested capability.
        capability: CapabilityId,
    },
    /// A provider was called with a capability absent from its own manifest.
    #[error("provider {provider} does not implement capability {capability}")]
    ProviderDoesNotImplement {
        /// Provider identity.
        provider: ProviderId,
        /// Requested capability.
        capability: CapabilityId,
    },
    /// Invocation input did not match the object-shaped provider contract.
    #[error("input for capability {capability} must be a JSON object")]
    InputNotObject {
        /// Capability identity.
        capability: CapabilityId,
    },
    /// Invocation input could not be serialized.
    #[error("could not serialize provider invocation input")]
    SerializeInput {
        /// JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// Invocation input exceeded its bound.
    #[error("input for capability {capability} is {length} bytes; the maximum is {maximum}")]
    InputTooLarge {
        /// Capability identity.
        capability: CapabilityId,
        /// Actual serialized size.
        length: usize,
        /// Configured bound.
        maximum: usize,
    },
    /// Invocation or manifest output exceeded its bound.
    #[error("provider {provider} returned {length} bytes; the maximum is {maximum}")]
    OutputTooLarge {
        /// Provider identity or source path.
        provider: String,
        /// Actual serialized size.
        length: usize,
        /// Configured bound.
        maximum: usize,
    },
    /// Component execution exceeded its wall-clock timeout.
    #[error("provider operation {operation} exceeded its {timeout_ms} ms timeout")]
    Timeout {
        /// Operation description.
        operation: String,
        /// Configured timeout.
        timeout_ms: u128,
    },
    /// Calling a provider export trapped or otherwise failed.
    #[error("provider {provider} failed while invoking capability {capability}")]
    Invoke {
        /// Provider identity.
        provider: ProviderId,
        /// Capability identity.
        capability: CapabilityId,
        /// Wasmtime error.
        #[source]
        source: wasmtime::Error,
    },
    /// A provider returned an explicit typed failure.
    #[error("provider {provider} failed capability {capability} with {code}: {message}")]
    ProviderFailure {
        /// Provider identity.
        provider: ProviderId,
        /// Capability identity.
        capability: CapabilityId,
        /// Stable provider error code.
        code: String,
        /// Bounded provider error detail.
        message: String,
    },
    /// A provider returned data that was not a valid component response.
    #[error("provider {provider} returned an invalid response for capability {capability}")]
    InvalidOutput {
        /// Provider identity.
        provider: ProviderId,
        /// Capability identity.
        capability: CapabilityId,
        /// JSON error.
        #[source]
        source: serde_json::Error,
    },
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use dekopon_capability::{EffectKind, Idempotency};
    use dekopon_core::RiskLevel;
    use serde_json::json;

    use super::{
        ProviderApiVersion, ProviderCapability, ProviderHostError, ProviderManifest,
        validate_manifest,
    };

    fn manifest(effect: EffectKind, input_schema: serde_json::Value) -> ProviderManifest {
        ProviderManifest {
            api_version: ProviderApiVersion::V1Alpha1,
            id: "fixture".parse().expect("valid provider fixture"),
            description: "Fixture provider".to_owned(),
            command_words: Vec::new(),
            capabilities: vec![ProviderCapability {
                id: "fixture.run".parse().expect("valid capability fixture"),
                description: "Runs a fixture".to_owned(),
                effect,
                risk: RiskLevel::Low,
                idempotency: Idempotency::Idempotent,
                input_schema,
            }],
        }
    }

    #[test]
    fn rejects_mutating_provider_manifests() {
        let error = validate_manifest(
            &manifest(EffectKind::ExternalWrite, json!({"type": "object"})),
            Path::new("writer.wasm"),
        )
        .expect_err("external writes must not load in immediate mode");

        assert!(matches!(error, ProviderHostError::UnsupportedEffect { .. }));
    }

    #[test]
    fn requires_object_shaped_prompt_schemas() {
        let error = validate_manifest(
            &manifest(EffectKind::ReadOnly, json!({"type": "string"})),
            Path::new("invalid-schema.wasm"),
        )
        .expect_err("prompt tools require object arguments");

        assert!(matches!(error, ProviderHostError::Manifest { .. }));
    }
}
