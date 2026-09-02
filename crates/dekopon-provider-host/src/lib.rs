//! Bounded WebAssembly component hosting for immediate-mode Dekopon providers.
//!
//! This crate intentionally supports only read-only components with no host imports. It is a
//! useful execution laboratory, not an authorization broker: loading a component does not grant
//! external authority, credentials, filesystem access, network access, clocks, or environment
//! variables.

#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    fmt, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use dekopon_capability::EffectKind;
use dekopon_core::{CapabilityId, ProviderId};
use dekopon_provider_sdk::host::CommandExport;
pub use dekopon_provider_sdk::host::ProviderConflicts;
use dekopon_provider_sdk::host::{
    self, CommandExportProblem, ConflictScan, ConflictWording, EngineError, ManifestRejection,
    RESOLVE_COMMAND_EXPORT, RUN_COMMAND_EXPORT, StoreLimits,
};
pub use dekopon_provider_sdk::{
    CommandRunOutcome, ComponentFailure, ComponentResponse, ProviderApiVersion, ProviderCapability,
    ProviderManifest,
};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store};

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "provider",
    });
}

/// The WIT contract implemented by provider components.
pub const PROVIDER_WIT: &str = include_str!("../wit/provider.wit");

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
/// Default Wasm instruction fuel supplied to each store.
pub const DEFAULT_FUEL: u64 = 10_000_000;
/// Default wall-clock timeout for instantiation and invocation.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Limits applied independently to every provider description or invocation call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostLimits {
    /// Maximum size of each linear memory in one store.
    ///
    /// Wasmtime applies this per memory, not per store, which is why the store also bounds how many
    /// memories, tables, table elements, and instances a component may create.
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
    ///
    /// This bounds what the host will parse, not peak host allocation: the buffered-string WIT
    /// contract lifts the whole guest string into host memory before it can be measured, so the
    /// transient allocation is bounded by the store's memory limits instead.
    pub max_output_bytes: usize,
    /// Wasm instruction fuel supplied to one store.
    pub fuel: u64,
    /// Wall-clock bound for instantiation plus the exported call.
    pub timeout: Duration,
}

impl Default for HostLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: host::DEFAULT_MAX_MEMORY_BYTES,
            max_table_elements: host::DEFAULT_MAX_TABLE_ELEMENTS,
            max_instances: host::DEFAULT_MAX_INSTANCES,
            max_tables: host::DEFAULT_MAX_TABLES,
            max_memories: host::DEFAULT_MAX_MEMORIES,
            max_input_bytes: host::DEFAULT_MAX_INPUT_BYTES,
            max_output_bytes: host::DEFAULT_MAX_OUTPUT_BYTES,
            fuel: DEFAULT_FUEL,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl HostLimits {
    /// The subset of these bounds Wasmtime enforces on one fresh store.
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

/// Operational host settings that bound nothing a component may do.
///
/// These are separate from [`HostLimits`] for the same reason the broker host keeps them apart:
/// a limit is part of what a call is allowed to consume, while a cache directory only decides
/// where already-compiled machine code is kept between processes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HostOptions {
    /// Directory for Wasmtime's content-addressed compilation cache.
    ///
    /// `None` recompiles every component with Cranelift in every process, which is the dominant
    /// cost of a short `inspect` or `invoke`. A hit is keyed by the artifact bytes and the engine
    /// configuration, so a rebuilt component compiles again rather than being served stale.
    ///
    /// The cache holds compiled code this process executes: point it only at a directory the
    /// invoking user controls.
    pub compile_cache_dir: Option<PathBuf>,
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
    deadline: DeadlineWorker,
}

impl Runtime {
    fn new(limits: HostLimits, options: &HostOptions) -> Result<Self, ProviderHostError> {
        validate_limits(&limits)?;

        let mut config = host::config();
        // Synchronous execution: a deadline thread interrupts a running guest by ticking the epoch.
        config.epoch_interruption(true);
        let engine = host::engine(config, options.compile_cache_dir.as_deref()).map_err(
            |error| match error {
                EngineError::CompileCache { path, source } => {
                    ProviderHostError::CompileCache { path, source }
                }
                EngineError::Engine { source } => ProviderHostError::Engine { source },
            },
        )?;
        let deadline = DeadlineWorker::start(engine.clone())?;

        Ok(Self {
            engine,
            limits,
            execution: Mutex::new(()),
            deadline,
        })
    }

    fn store(&self) -> Result<Store<StoreState>, ProviderHostError> {
        let mut store = Store::new(
            &self.engine,
            StoreState {
                limits: self.limits.store_bounds().store_limits(),
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
    limits: wasmtime::StoreLimits,
}

/// A provider component compiled by Wasmtime.
pub struct WasmProvider {
    runtime: Arc<Runtime>,
    component: Component,
    source: PathBuf,
    manifest: ProviderManifest,
    /// Which command export the component offers, read once from its type at load.
    command_export: CommandExport,
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
        // A manifest that promises command words the component cannot run would fail at the first
        // `gh …` a model typed. Prove it at load instead, from the component's own type, which
        // distinguishes "no such export" from "wrong signature" without instantiating again.
        let command_export = host::command_export(&runtime.engine, &component);
        if let Err(problem) = host::check_command_export(&manifest, &command_export) {
            return Err(match problem {
                CommandExportProblem::Missing => ProviderHostError::MissingCommandExport {
                    provider: manifest.id,
                    path: source,
                },
                CommandExportProblem::Mismatched { name, found } => {
                    ProviderHostError::CommandExportSignature {
                        provider: manifest.id,
                        path: source,
                        name,
                        found,
                    }
                }
            });
        }

        Ok(Self {
            runtime,
            component,
            source,
            manifest,
            command_export,
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

        let timeout_ms = self.runtime.limits.timeout.as_millis();
        let span = tracing::info_span!(
            "provider.invoke",
            provider.id = %self.manifest.id,
            capability.id = %capability,
            provider.path = %self.source.display(),
            input.bytes = input_json.len(),
            output.bytes = tracing::field::Empty,
            fuel.remaining = tracing::field::Empty
        );
        let _entered = span.enter();
        let _execution = self
            .runtime
            .execution
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let mut store = self.runtime.store()?;
        let linker = Linker::new(&self.runtime.engine);
        let mut deadline = self.runtime.deadline.arm(self.runtime.limits.timeout)?;
        let bindings = match bindings::Provider::instantiate(&mut store, &self.component, &linker) {
            Ok(bindings) => bindings,
            Err(source) => {
                if deadline.stop() {
                    tracing::warn!(
                        provider.id = %self.manifest.id,
                        capability.id = %capability,
                        timeout_ms = %timeout_ms,
                        "instantiating a provider component exceeded its wall-clock deadline"
                    );
                    return Err(ProviderHostError::Timeout {
                        operation: format!("instantiate {}", self.manifest.id),
                        timeout_ms,
                        source: Some(source),
                    });
                }
                return Err(ProviderHostError::Instantiate {
                    path: self.source.clone(),
                    source,
                });
            }
        };
        let result = bindings.call_invoke(&mut store, capability.as_str(), &input_json);
        let expired = deadline.stop();
        if let Ok(fuel) = store.get_fuel() {
            span.record("fuel.remaining", fuel);
        }
        let output_json = match settle(result, expired) {
            CallOutcome::Completed(output_json) => output_json,
            CallOutcome::TimedOut(source) => {
                tracing::warn!(
                    provider.id = %self.manifest.id,
                    capability.id = %capability,
                    timeout_ms = %timeout_ms,
                    "provider invocation exceeded its wall-clock deadline"
                );
                return Err(ProviderHostError::Timeout {
                    operation: format!("invoke {capability}"),
                    timeout_ms,
                    source: Some(source),
                });
            }
            CallOutcome::Failed(source) => {
                tracing::warn!(
                    provider.id = %self.manifest.id,
                    capability.id = %capability,
                    "provider invocation trapped or failed inside the component"
                );
                return Err(ProviderHostError::Invoke {
                    provider: self.manifest.id.clone(),
                    capability: capability.clone(),
                    source,
                });
            }
        };

        span.record("output.bytes", output_json.len());
        if output_json.len() > self.runtime.limits.max_output_bytes {
            tracing::warn!(
                provider.id = %self.manifest.id,
                capability.id = %capability,
                output.bytes = output_json.len(),
                maximum = self.runtime.limits.max_output_bytes,
                "provider returned more output than the configured maximum"
            );
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

    /// Runs one command word's argv inside the component, in a fresh bounded execution context.
    ///
    /// The outcome is a proposal, rendered text, or a decline; none of it carries authorization,
    /// and the run is as import-free as `invoke`: the linker is empty. Which export is called was
    /// decided at load from the component's type. `run-command` receives `argv` and `stdin`; a
    /// legacy `resolve-command` guest receives `argv` alone, because its contract has no piped
    /// value, and its answer is adapted into the same [`CommandRunOutcome`].
    ///
    /// # Errors
    ///
    /// Returns [`ProviderHostError::CommandInputTooLarge`] before instantiation when `argv` plus
    /// `stdin` exceed the input bound, and [`ProviderHostError`] when instantiation or the call
    /// fails, exceeds a bound, or answers with something that is not its wire type.
    pub fn run_command(
        &self,
        argv: &[String],
        stdin: Option<&str>,
    ) -> Result<CommandRunOutcome, ProviderHostError> {
        let length = host::command_input_bytes(argv, stdin);
        if length > self.runtime.limits.max_input_bytes {
            return Err(ProviderHostError::CommandInputTooLarge {
                provider: self.manifest.id.clone(),
                length,
                maximum: self.runtime.limits.max_input_bytes,
            });
        }
        let export_name = match &self.command_export {
            CommandExport::RunCommand => RUN_COMMAND_EXPORT,
            CommandExport::ResolveCommand => RESOLVE_COMMAND_EXPORT,
            CommandExport::Absent => {
                return Err(ProviderHostError::MissingCommandExport {
                    provider: self.manifest.id.clone(),
                    path: self.source.clone(),
                });
            }
            CommandExport::Mismatched { name, found } => {
                return Err(ProviderHostError::CommandExportSignature {
                    provider: self.manifest.id.clone(),
                    path: self.source.clone(),
                    name,
                    found: found.clone(),
                });
            }
        };

        let timeout_ms = self.runtime.limits.timeout.as_millis();
        // DEBUG rather than INFO: a command run is not budget-bounded the way a capability call
        // is, and the enclosing shell span carries the outcome once per word.
        let span = tracing::debug_span!(
            "provider.run_command",
            provider.id = %self.manifest.id,
            provider.path = %self.source.display(),
            command.export = export_name,
            input.bytes = length,
            output.bytes = tracing::field::Empty,
            fuel.remaining = tracing::field::Empty
        );
        let _entered = span.enter();
        let _execution = self
            .runtime
            .execution
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let mut store = self.runtime.store()?;
        // The generated bindings describe the base world, which has no command export; the raw
        // instance is looked up by name instead, still through an empty linker.
        let linker = Linker::<StoreState>::new(&self.runtime.engine);
        let mut deadline = self.runtime.deadline.arm(self.runtime.limits.timeout)?;
        let instance = match linker.instantiate(&mut store, &self.component) {
            Ok(instance) => instance,
            Err(source) => {
                if deadline.stop() {
                    tracing::warn!(
                        provider.id = %self.manifest.id,
                        command.export = export_name,
                        timeout_ms = %timeout_ms,
                        "instantiating a provider component exceeded its wall-clock deadline"
                    );
                    return Err(ProviderHostError::Timeout {
                        operation: format!("instantiate {}", self.manifest.id),
                        timeout_ms,
                        source: Some(source),
                    });
                }
                return Err(ProviderHostError::Instantiate {
                    path: self.source.clone(),
                    source,
                });
            }
        };
        let signature = |source: wasmtime::Error| ProviderHostError::CommandExportSignature {
            provider: self.manifest.id.clone(),
            path: self.source.clone(),
            name: export_name,
            found: source.to_string(),
        };
        let result = if export_name == RUN_COMMAND_EXPORT {
            let function = instance
                .get_typed_func::<(Vec<String>, Option<String>), (String,)>(
                    &mut store,
                    RUN_COMMAND_EXPORT,
                )
                .map_err(signature)?;
            function
                .call(&mut store, (argv.to_vec(), stdin.map(str::to_owned)))
                .and_then(|(output,)| function.post_return(&mut store).map(|()| output))
        } else {
            let function = instance
                .get_typed_func::<(Vec<String>,), (String,)>(&mut store, RESOLVE_COMMAND_EXPORT)
                .map_err(signature)?;
            function
                .call(&mut store, (argv.to_vec(),))
                .and_then(|(output,)| function.post_return(&mut store).map(|()| output))
        };
        let expired = deadline.stop();
        if let Ok(fuel) = store.get_fuel() {
            span.record("fuel.remaining", fuel);
        }
        let output_json = match settle(result, expired) {
            CallOutcome::Completed(output_json) => output_json,
            CallOutcome::TimedOut(source) => {
                tracing::warn!(
                    provider.id = %self.manifest.id,
                    command.export = export_name,
                    timeout_ms = %timeout_ms,
                    "provider command run exceeded its wall-clock deadline"
                );
                return Err(ProviderHostError::Timeout {
                    operation: format!("{export_name} {}", self.manifest.id),
                    timeout_ms,
                    source: Some(source),
                });
            }
            CallOutcome::Failed(source) => {
                tracing::warn!(
                    provider.id = %self.manifest.id,
                    command.export = export_name,
                    "provider command run trapped or failed inside the component"
                );
                return Err(ProviderHostError::RunCommand {
                    provider: self.manifest.id.clone(),
                    source,
                });
            }
        };

        span.record("output.bytes", output_json.len());
        if output_json.len() > self.runtime.limits.max_output_bytes {
            tracing::warn!(
                provider.id = %self.manifest.id,
                command.export = export_name,
                output.bytes = output_json.len(),
                maximum = self.runtime.limits.max_output_bytes,
                "provider returned more command output than the configured maximum"
            );
            return Err(ProviderHostError::OutputTooLarge {
                provider: self.manifest.id.to_string(),
                length: output_json.len(),
                maximum: self.runtime.limits.max_output_bytes,
            });
        }
        host::parse_command_run(&self.command_export, &output_json).map_err(|source| {
            ProviderHostError::InvalidCommandRun {
                provider: self.manifest.id.clone(),
                source,
            }
        })
    }
}

/// How this host addresses an operator in a conflict report.
///
/// The immediate host loads a component set named on the command line; the broker starts from a
/// configured directory. Everything else in the report is shared.
const CONFLICT_WORDING: ConflictWording = ConflictWording {
    refusing_to: "load",
    duplicate_provider_remedy: "remove one, or drop its --provider argument",
};

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
        Self::load_with_options(sources, limits, &HostOptions::default())
    }

    /// Loads and validates all components, optionally reusing a persistent compilation cache.
    pub fn load_with_options<I, P>(
        sources: I,
        limits: HostLimits,
        options: &HostOptions,
    ) -> Result<Self, ProviderHostError>
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        let sources = sources.into_iter().map(Into::into).collect::<Vec<_>>();
        if sources.is_empty() {
            return Err(ProviderHostError::NoProviders);
        }

        let runtime = Arc::new(Runtime::new(limits, options)?);
        let mut providers = Vec::with_capacity(sources.len());
        // Every conflict, then one failure. Returning on the first would make fixing a provider set
        // take one run per mistake; an operator should see the whole picture once.
        let mut scan = ConflictScan::new(CONFLICT_WORDING);

        for source in sources {
            let provider = WasmProvider::load(Arc::clone(&runtime), source)?;
            scan.record(&provider.manifest, providers.len());
            providers.push(provider);
        }

        let routes = scan
            .finish()
            .map_err(|report| ProviderHostError::ConflictingProviders {
                report: Box::new(report),
            })?;

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

    /// Returns each provider's command words, in load order, skipping providers declaring none.
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

    /// Runs one command word's argv through the provider that declared it.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderHostError::UnknownCommandWord`] when no loaded provider declared the
    /// word, without instantiating anything, and otherwise whatever
    /// [`WasmProvider::run_command`] returns.
    pub fn run_command(
        &self,
        word: &str,
        argv: &[String],
        stdin: Option<&str>,
    ) -> Result<CommandRunOutcome, ProviderHostError> {
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
            .ok_or_else(|| ProviderHostError::UnknownCommandWord {
                word: word.to_owned(),
            })?;
        provider.run_command(argv, stdin)
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

    let timeout_ms = runtime.limits.timeout.as_millis();
    let mut store = runtime.store()?;
    let linker = Linker::new(&runtime.engine);
    let mut deadline = runtime.deadline.arm(runtime.limits.timeout)?;
    let bindings = match bindings::Provider::instantiate(&mut store, component, &linker) {
        Ok(bindings) => bindings,
        Err(error) => {
            if deadline.stop() {
                tracing::warn!(
                    provider.path = %source.display(),
                    timeout_ms = %timeout_ms,
                    "instantiating a provider component exceeded its wall-clock deadline"
                );
                return Err(ProviderHostError::Timeout {
                    operation: format!("instantiate {}", source.display()),
                    timeout_ms,
                    source: Some(error),
                });
            }
            return Err(ProviderHostError::Instantiate {
                path: source.to_path_buf(),
                source: error,
            });
        }
    };
    let result = bindings.call_describe(&mut store);
    let expired = deadline.stop();

    match settle(result, expired) {
        CallOutcome::Completed(manifest_json) => Ok(manifest_json),
        CallOutcome::TimedOut(error) => {
            tracing::warn!(
                provider.path = %source.display(),
                timeout_ms = %timeout_ms,
                "describing a provider component exceeded its wall-clock deadline"
            );
            Err(ProviderHostError::Timeout {
                operation: format!("describe {}", source.display()),
                timeout_ms,
                source: Some(error),
            })
        }
        CallOutcome::Failed(error) => Err(ProviderHostError::Describe {
            path: source.to_path_buf(),
            source: error,
        }),
    }
}

/// Outcome of one guest call once its wall-clock deadline has been disarmed.
enum CallOutcome<T> {
    /// The call returned before anything interrupted it.
    Completed(T),
    /// The call failed and the deadline had already fired.
    TimedOut(wasmtime::Error),
    /// The call failed on its own.
    Failed(wasmtime::Error),
}

/// Settles the race between a completed call and a deadline that fired around the same moment.
///
/// The epoch can tick in the window between the guest returning and the deadline being disarmed,
/// where it interrupted nothing — an interruption that landed in time produces `Err` instead. So a
/// completed `Ok` wins, and only a failed call may be reported as a timeout.
fn settle<T>(result: Result<T, wasmtime::Error>, expired: bool) -> CallOutcome<T> {
    match result {
        Ok(value) => CallOutcome::Completed(value),
        Err(error) if expired => CallOutcome::TimedOut(error),
        Err(error) => CallOutcome::Failed(error),
    }
}

fn validate_limits(limits: &HostLimits) -> Result<(), ProviderHostError> {
    host::validate_limits(
        &limits.store_bounds(),
        &[
            ("max_input_bytes", limits.max_input_bytes as u128),
            ("max_output_bytes", limits.max_output_bytes as u128),
            ("fuel", u128::from(limits.fuel)),
            ("timeout", limits.timeout.as_nanos()),
        ],
    )
    .map_err(|zero| ProviderHostError::InvalidLimit { name: zero.name })
}

fn validate_manifest(manifest: &ProviderManifest, source: &Path) -> Result<(), ProviderHostError> {
    // Immediate mode links nothing, so a capability that claims to change the world cannot load
    // here at all; the broker host passes no gate and authorizes effects instead.
    host::validate_manifest(manifest, Some(EffectKind::ReadOnly)).map_err(|rejection| {
        match rejection {
            ManifestRejection::Invalid { message } => invalid_manifest(source, message),
            ManifestRejection::UnsupportedEffect {
                provider,
                capability,
                effect,
            } => ProviderHostError::UnsupportedEffect {
                provider,
                capability,
                effect,
            },
        }
    })
}

fn invalid_manifest(source: &Path, message: impl Into<String>) -> ProviderHostError {
    ProviderHostError::Manifest {
        path: source.to_path_buf(),
        message: message.into(),
    }
}

/// One deadline armed on the runtime's worker.
struct Armed {
    timeout: Duration,
    expired: Arc<AtomicBool>,
    cancel: Receiver<()>,
    done: SyncSender<()>,
}

/// The single wall-clock deadline thread a [`Runtime`] owns.
///
/// The execution mutex admits one call at a time, so a thread spawned and joined per describe and
/// invoke was pure overhead on the hot path — and thread creation is the first thing a loaded host
/// refuses. This one parks on its command channel between calls and outlives all of them.
struct DeadlineWorker {
    deadlines: Option<Sender<Armed>>,
    handle: Option<JoinHandle<()>>,
}

impl DeadlineWorker {
    fn start(engine: Engine) -> Result<Self, ProviderHostError> {
        let (deadlines, armed) = mpsc::channel::<Armed>();
        let handle = thread::Builder::new()
            .name("dekopon-provider-deadline".to_owned())
            .spawn(move || {
                while let Ok(armed) = armed.recv() {
                    if matches!(
                        armed.cancel.recv_timeout(armed.timeout),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        armed.expired.store(true, Ordering::Release);
                        engine.increment_epoch();
                    }
                    // Only now is `expired` final, so a caller waiting on this signal cannot read
                    // the flag while the deadline is still firing.
                    let _ignored = armed.done.send(());
                }
            })
            .map_err(|source| ProviderHostError::Timer { source })?;

        Ok(Self {
            deadlines: Some(deadlines),
            handle: Some(handle),
        })
    }

    fn arm(&self, timeout: Duration) -> Result<Deadline, ProviderHostError> {
        let (cancel, cancelled) = mpsc::channel();
        // Bounded at one so the worker's completion signal never blocks on a caller that is gone.
        let (done, finished) = mpsc::sync_channel(1);
        let expired = Arc::new(AtomicBool::new(false));
        self.deadlines
            .as_ref()
            .ok_or_else(deadline_worker_stopped)?
            .send(Armed {
                timeout,
                expired: Arc::clone(&expired),
                cancel: cancelled,
                done,
            })
            .map_err(|_ignored| deadline_worker_stopped())?;

        Ok(Deadline {
            cancel: Some(cancel),
            finished,
            expired,
        })
    }
}

impl Drop for DeadlineWorker {
    fn drop(&mut self) {
        // Closing the command channel ends the loop; join so the worker cannot outlive the engine
        // whose epoch it increments.
        drop(self.deadlines.take());
        if let Some(handle) = self.handle.take() {
            let _ignored = handle.join();
        }
    }
}

fn deadline_worker_stopped() -> ProviderHostError {
    ProviderHostError::Timer {
        source: io::Error::other("the provider deadline worker is no longer running"),
    }
}

/// One call's armed deadline, disarmed exactly once.
struct Deadline {
    cancel: Option<Sender<()>>,
    finished: Receiver<()>,
    expired: Arc<AtomicBool>,
}

impl Deadline {
    fn stop(&mut self) -> bool {
        if let Some(cancel) = self.cancel.take() {
            let _ignored = cancel.send(());
            let _ignored = self.finished.recv();
        }
        self.expired.load(Ordering::Acquire)
    }
}

impl Drop for Deadline {
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
    /// The persistent compilation cache directory could not be prepared.
    #[error("could not open the provider compilation cache at {}", path.display())]
    CompileCache {
        /// Configured cache directory.
        path: PathBuf,
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
    /// The loaded component set contained duplicate providers, capabilities, or command words.
    #[error("{report}")]
    ConflictingProviders {
        /// Every conflict found across the whole load.
        report: Box<ProviderConflicts>,
    },
    /// No loaded component declared the requested capability.
    #[error("no loaded provider implements capability {capability}")]
    UnknownCapability {
        /// Requested capability.
        capability: CapabilityId,
    },
    /// No loaded component declared the requested command word.
    #[error("no loaded provider declares the command word {word:?}")]
    UnknownCommandWord {
        /// The unclaimed word.
        word: String,
    },
    /// A component declared command words but exports no way to run them.
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
    /// A component exports a command export as something the host cannot call.
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
    /// A command word's argv plus its piped value exceeded the input bound.
    #[error("command input for provider {provider} is {length} bytes; the maximum is {maximum}")]
    CommandInputTooLarge {
        /// Provider identity.
        provider: ProviderId,
        /// Actual bytes: every argv word plus the piped value.
        length: usize,
        /// Configured bound.
        maximum: usize,
    },
    /// Running a command word trapped or otherwise failed inside the component.
    #[error("provider {provider} failed while running a command word")]
    RunCommand {
        /// Provider identity.
        provider: ProviderId,
        /// Wasmtime error.
        #[source]
        source: wasmtime::Error,
    },
    /// A component answered a command run with something that is not its wire type.
    #[error("provider {provider} returned an unreadable command run outcome")]
    InvalidCommandRun {
        /// Provider identity.
        provider: ProviderId,
        /// JSON error.
        #[source]
        source: serde_json::Error,
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
        /// The interruption Wasmtime reported, when the call reported one.
        #[source]
        source: Option<wasmtime::Error>,
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
        CallOutcome, ProviderApiVersion, ProviderCapability, ProviderHostError, ProviderManifest,
        settle, validate_manifest,
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

    #[test]
    fn a_completed_call_outlives_a_deadline_that_fired_too_late() {
        // The epoch can tick between the guest returning and the deadline being disarmed. It
        // interrupted nothing, so the output stands rather than being reported as a timeout.
        let outcome = settle(Ok::<_, wasmtime::Error>("{\"kind\":\"succeeded\"}"), true);

        assert!(matches!(
            outcome,
            CallOutcome::Completed("{\"kind\":\"succeeded\"}")
        ));
    }

    #[test]
    fn a_failed_call_separates_a_fired_deadline_from_a_guest_trap() {
        let timed_out = settle(
            Err::<(), _>(wasmtime::Error::msg("epoch deadline reached")),
            true,
        );
        let failed = settle(Err::<(), _>(wasmtime::Error::msg("unreachable")), false);

        assert!(matches!(timed_out, CallOutcome::TimedOut(_)));
        assert!(matches!(failed, CallOutcome::Failed(_)));
    }
}
