//! Host-side plumbing for Dekopon's Wasmtime embeddings.
//!
//! `dekopon-broker-host` runs authorized components asynchronously with the project-owned HTTP
//! and storage interfaces linked. This module owns manifest validation, provider-set conflict
//! reports, store bounds, engine configuration, and optional command-export inspection.
//! These shared SDK APIs remain available to external hosts as well.
//!
//! The broker host owns its linker and yields on fuel so a Tokio deadline can cancel a call.
//! None of this is guest code. The module sits behind the non-default `host` feature, which pulls
//! in Wasmtime; a `wasm32-unknown-unknown` provider build never enables it.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
};

use dekopon_core::{CapabilityId, CommandWordConflict, ProviderId};
use serde_json::Value;
use thiserror::Error;
use wasmtime::component::types::{ComponentFunc, ComponentItem};
use wasmtime::component::{Component, Type};
use wasmtime::{Cache, CacheConfig, Config, Engine, StoreLimitsBuilder};

use crate::{CommandResolution, CommandRunOutcome, ProviderManifest};

/// Default maximum size of each linear memory in one store (64 MiB).
pub const DEFAULT_MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;
/// Default maximum elements in each Wasm table.
pub const DEFAULT_MAX_TABLE_ELEMENTS: usize = 100_000;
/// Default maximum core instances in one store.
pub const DEFAULT_MAX_INSTANCES: usize = 64;
/// Default maximum tables in one store.
pub const DEFAULT_MAX_TABLES: usize = 16;
/// Default maximum linear memories in one store.
pub const DEFAULT_MAX_MEMORIES: usize = 4;
/// Default maximum serialized provider input size (1 MiB).
pub const DEFAULT_MAX_INPUT_BYTES: usize = 1024 * 1024;
/// Default maximum serialized provider output or manifest size (1 MiB).
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Export a `provider-cli` component runs one command word through.
pub const RUN_COMMAND_EXPORT: &str = "run-command";
/// Export a legacy `provider-commands` component rewrites one command word through.
pub const RESOLVE_COMMAND_EXPORT: &str = "resolve-command";

/// Maximum bytes of one rendered component signature.
///
/// Signatures come from a component's own type, which its author controls, and end up in the load
/// error naming what a component exports under a command export instead of a callable function.
const MAX_SIGNATURE_BYTES: usize = 4 * 1024;

/// The bounds Wasmtime itself enforces on one fresh store.
///
/// This is the subset of a host's limits that shapes a store; fuel, wall-clock, and serialized
/// input/output bounds stay with the host that owns the machinery enforcing them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreLimits {
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
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
            max_table_elements: DEFAULT_MAX_TABLE_ELEMENTS,
            max_instances: DEFAULT_MAX_INSTANCES,
            max_tables: DEFAULT_MAX_TABLES,
            max_memories: DEFAULT_MAX_MEMORIES,
        }
    }
}

impl StoreLimits {
    /// Builds the Wasmtime resource limiter a host installs on one fresh store.
    #[must_use]
    pub fn store_limits(&self) -> wasmtime::StoreLimits {
        StoreLimitsBuilder::new()
            .memory_size(self.max_memory_bytes)
            .table_elements(self.max_table_elements)
            .instances(self.max_instances)
            .tables(self.max_tables)
            .memories(self.max_memories)
            .build()
    }

    /// These bounds as named values, in the order [`validate_limits`] reports them.
    fn named(&self) -> [(&'static str, u128); 5] {
        [
            ("max_memory_bytes", self.max_memory_bytes as u128),
            ("max_table_elements", self.max_table_elements as u128),
            ("max_instances", self.max_instances as u128),
            ("max_tables", self.max_tables as u128),
            ("max_memories", self.max_memories as u128),
        ]
    }
}

/// A host limit configured as zero.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("host limit {name} must be greater than zero")]
pub struct ZeroLimit {
    /// Name of the field that was zero.
    pub name: &'static str,
}

/// Refuses a zero in any store bound or in the caller's `additional` named limits.
///
/// Zero is not a permissive setting: a zero store bound makes every instantiation fail, and zero
/// fuel or a zero deadline traps before guest code runs. Both hosts check before compiling anything,
/// so an operator gets the field name rather than an instantiation failure from inside Wasmtime.
///
/// # Errors
///
/// Returns [`ZeroLimit`] naming the first zero-valued field, store bounds first.
pub fn validate_limits(
    limits: &StoreLimits,
    additional: &[(&'static str, u128)],
) -> Result<(), ZeroLimit> {
    for (name, value) in limits.named().into_iter().chain(additional.iter().copied()) {
        if value == 0 {
            return Err(ZeroLimit { name });
        }
    }
    Ok(())
}

/// Why a component's manifest cannot be loaded.
///
/// The manifest broke a semantic rule; the message is the operator-facing detail.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct ManifestRejection {
    /// What was wrong with the manifest.
    pub message: String,
}

/// Checks the manifest rules every Dekopon host enforces.
///
/// A declared effect is an input to authorization rather than a load-time refusal: policy
/// authorizes an effect per invocation, so nothing here gates on one.
///
/// The caller owns the component path and attaches it to whichever of its own errors this maps to;
/// nothing here reads the filesystem.
///
/// # Errors
///
/// Returns [`ManifestRejection`] for an empty description, no capabilities, a duplicated capability
/// identifier, or a capability with an empty description or a non-object input schema.
pub fn validate_manifest(manifest: &ProviderManifest) -> Result<(), ManifestRejection> {
    if manifest.description.trim().is_empty() {
        return Err(invalid("description must not be empty"));
    }
    if manifest.capabilities.is_empty() {
        return Err(invalid("at least one capability is required"));
    }

    let mut capabilities = BTreeSet::new();
    for capability in &manifest.capabilities {
        if !capabilities.insert(capability.id.clone()) {
            return Err(invalid(format!(
                "capability {} is declared more than once",
                capability.id
            )));
        }
        if capability.description.trim().is_empty() {
            return Err(invalid(format!(
                "capability {} has an empty description",
                capability.id
            )));
        }
        let Some(schema) = capability.input_schema.as_object() else {
            return Err(invalid(format!(
                "capability {} inputSchema must be an object",
                capability.id
            )));
        };
        if schema.get("type").and_then(Value::as_str) != Some("object") {
            return Err(invalid(format!(
                "capability {} inputSchema must declare type object",
                capability.id
            )));
        }
    }

    Ok(())
}

fn invalid(message: impl Into<String>) -> ManifestRejection {
    ManifestRejection {
        message: message.into(),
    }
}

/// Everything wrong with one provider set, gathered so an operator sees it once.
///
/// Ambiguity is fatal in a way absence is not: a provider identity, capability, or command word two
/// components both claim has no meaning a host can pick without silently choosing for the operator.
/// This reports rather than resolves, and reports *all of it* — fixing a provider set should take
/// one run, not one run per mistake.
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

/// Accumulates every ambiguity in one provider set, then fails once with all of them.
///
/// A host records each manifest as it loads, in load order, and finishes with either the
/// deterministic capability routes or the whole report.
#[derive(Clone, Debug, Default)]
pub struct ConflictScan {
    provider_ids: BTreeSet<ProviderId>,
    duplicate_providers: BTreeSet<ProviderId>,
    duplicate_capabilities: BTreeSet<CapabilityId>,
    declared_words: Vec<(String, Vec<String>)>,
    routes: BTreeMap<CapabilityId, usize>,
}

impl ConflictScan {
    /// Starts an empty scan.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one loaded manifest and the index of its component in load order.
    pub fn record(&mut self, manifest: &ProviderManifest, provider_index: usize) {
        if !self.provider_ids.insert(manifest.id.clone()) {
            self.duplicate_providers.insert(manifest.id.clone());
        }
        self.declared_words
            .push((manifest.id.to_string(), manifest.command_words.clone()));
        for capability in &manifest.capabilities {
            if self
                .routes
                .insert(capability.id.clone(), provider_index)
                .is_some()
            {
                self.duplicate_capabilities.insert(capability.id.clone());
            }
        }
    }

    /// Returns the deterministic capability routes, or every conflict the set contains.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderConflicts`] when a provider identity or capability is claimed twice, or
    /// when a command word collides with another provider's, a shell builtin, or a reserved word.
    pub fn finish(self) -> Result<BTreeMap<CapabilityId, usize>, ProviderConflicts> {
        // Reserved words and shell builtins are collected here beside the duplicates so one
        // report carries every reason a provider set cannot start.
        let command_words = dekopon_core::command_word_conflicts(&self.declared_words);
        if !self.duplicate_providers.is_empty()
            || !self.duplicate_capabilities.is_empty()
            || !command_words.is_empty()
        {
            return Err(ProviderConflicts {
                providers: self.duplicate_providers.into_iter().collect(),
                capabilities: self.duplicate_capabilities.into_iter().collect(),
                command_words,
            });
        }
        Ok(self.routes)
    }
}

/// Which optional command export a compiled component offers, read from its own type.
///
/// Absent and wrong-typed are different operator problems with different fixes, and neither is
/// worth an instantiation to discover. `run-command` takes precedence: a component exporting both
/// is called through the newer one.
#[derive(Clone, Debug, PartialEq)]
pub enum CommandExport {
    /// Exports `run-command: func(argv: list<string>, stdin: option<string>) -> string`.
    RunCommand,
    /// Exports only the legacy `resolve-command: func(argv: list<string>) -> string`.
    ResolveCommand,
    /// Exports neither: the component was built against the base `dekopon:provider` world.
    Absent,
    /// Exports the first name found as something the host cannot call.
    Mismatched {
        /// Which export name was found.
        name: &'static str,
        /// Bounded description of what the component actually exports under it.
        found: String,
    },
}

/// Reads which command export `component` offers from its type.
#[must_use]
pub fn command_export(engine: &Engine, component: &Component) -> CommandExport {
    let component_type = component.component_type();
    let find = |wanted: &str| {
        component_type
            .exports(engine)
            .find(|(name, _)| *name == wanted)
            .map(|(_, item)| item.ty)
    };
    if let Some(item) = find(RUN_COMMAND_EXPORT) {
        return classify_export(
            RUN_COMMAND_EXPORT,
            &item,
            runs_commands,
            CommandExport::RunCommand,
        );
    }
    if let Some(item) = find(RESOLVE_COMMAND_EXPORT) {
        return classify_export(
            RESOLVE_COMMAND_EXPORT,
            &item,
            resolves_commands,
            CommandExport::ResolveCommand,
        );
    }
    CommandExport::Absent
}

fn classify_export(
    name: &'static str,
    item: &ComponentItem,
    has_expected_type: fn(&ComponentFunc) -> bool,
    present: CommandExport,
) -> CommandExport {
    let ComponentItem::ComponentFunc(function) = item else {
        return CommandExport::Mismatched {
            name,
            found: item_kind(item).to_owned(),
        };
    };
    if has_expected_type(function) {
        present
    } else {
        CommandExport::Mismatched {
            name,
            found: function_signature(function),
        }
    }
}

/// `func(argv: list<string>, stdin: option<string>) -> string`.
fn runs_commands(function: &ComponentFunc) -> bool {
    let mut params = function.params();
    let argv_is_strings = params.len() == 2
        && matches!(params.next(), Some((_, Type::List(list))) if list.ty() == Type::String);
    let stdin_is_optional_string =
        matches!(params.next(), Some((_, Type::Option(option))) if option.ty() == Type::String);
    argv_is_strings && stdin_is_optional_string && returns_one_string(function)
}

/// `func(argv: list<string>) -> string`.
fn resolves_commands(function: &ComponentFunc) -> bool {
    let mut params = function.params();
    let argv_is_strings = params.len() == 1
        && matches!(params.next(), Some((_, Type::List(list))) if list.ty() == Type::String);
    argv_is_strings && returns_one_string(function)
}

fn returns_one_string(function: &ComponentFunc) -> bool {
    let mut results = function.results();
    results.len() == 1 && results.next() == Some(Type::String)
}

/// The broad kind of one item in a component type, as a stable word.
#[must_use]
pub const fn item_kind(item: &ComponentItem) -> &'static str {
    match item {
        ComponentItem::ComponentFunc(_) => "function",
        ComponentItem::CoreFunc(_) => "core-function",
        ComponentItem::Module(_) => "module",
        ComponentItem::Component(_) => "component",
        ComponentItem::ComponentInstance(_) => "instance",
        ComponentItem::Type(_) => "type",
        ComponentItem::Resource(_) => "resource",
    }
}

/// Renders one component function's type as `fn(name: Type, …) -> (Type, …)`, bounded.
#[must_use]
pub fn function_signature(function: &ComponentFunc) -> String {
    let params = function
        .params()
        .map(|(name, value)| format!("{name}: {value:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let results = function
        .results()
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    bounded_signature(format!("fn({params}) -> ({results})"))
}

/// Truncates a rendered component type to the signature bound, marking the cut.
#[must_use]
pub fn bounded_signature(mut value: String) -> String {
    if value.len() <= MAX_SIGNATURE_BYTES {
        return value;
    }
    let mut end = MAX_SIGNATURE_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value.push('\u{2026}');
    value
}

/// Why a manifest's command words cannot be served by the component that declared them.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CommandExportProblem {
    /// The component exports neither `run-command` nor `resolve-command`.
    #[error("exports neither {RUN_COMMAND_EXPORT} nor {RESOLVE_COMMAND_EXPORT}")]
    Missing,
    /// The component exports the name as something the host cannot call.
    #[error("exports {name} as {found}")]
    Mismatched {
        /// Which export name was found.
        name: &'static str,
        /// Bounded description of what the component actually exports under it.
        found: String,
    },
}

/// The load gate for command words: a manifest that declares any needs one callable export.
///
/// A manifest promising words the component cannot run would fail at the first `gh …` a model
/// typed, hours into a session. Both hosts prove it at load instead, from the component's own
/// type. A manifest declaring no words passes whatever the component exports: nothing will ever
/// call it.
///
/// # Errors
///
/// Returns [`CommandExportProblem::Missing`] when words are declared and neither export exists,
/// and [`CommandExportProblem::Mismatched`] when the export found has a type the host cannot call.
pub fn check_command_export(
    manifest: &ProviderManifest,
    export: &CommandExport,
) -> Result<(), CommandExportProblem> {
    if manifest.command_words.is_empty() {
        return Ok(());
    }
    match export {
        CommandExport::RunCommand | CommandExport::ResolveCommand => Ok(()),
        CommandExport::Absent => Err(CommandExportProblem::Missing),
        CommandExport::Mismatched { name, found } => Err(CommandExportProblem::Mismatched {
            name,
            found: found.clone(),
        }),
    }
}

/// Bytes a host counts against its input bound for one command run: every argv word plus the
/// piped value.
#[must_use]
pub fn command_input_bytes(argv: &[String], stdin: Option<&str>) -> usize {
    argv.iter().fold(stdin.map_or(0, str::len), |total, word| {
        total.saturating_add(word.len())
    })
}

/// Decodes what a command export returned, into the one outcome type a host handles.
///
/// A legacy `resolve-command` guest answers with a [`CommandResolution`], which converts
/// losslessly. Anything else is parsed as a [`CommandRunOutcome`] directly: for
/// [`CommandExport::Absent`] and [`CommandExport::Mismatched`] the caller's gate refused the
/// component at load, so there is no legacy shape to expect.
///
/// # Errors
///
/// Returns the JSON error when the text is not the wire type the export produces, so a host can
/// report it with the provider it came from.
pub fn parse_command_run(
    export: &CommandExport,
    json: &str,
) -> Result<CommandRunOutcome, serde_json::Error> {
    match export {
        CommandExport::ResolveCommand => {
            serde_json::from_str::<CommandResolution>(json).map(CommandRunOutcome::from)
        }
        CommandExport::RunCommand | CommandExport::Absent | CommandExport::Mismatched { .. } => {
            serde_json::from_str(json)
        }
    }
}

/// Failure to build the shared Wasmtime engine.
#[derive(Debug, Error)]
pub enum EngineError {
    /// The persistent compilation cache directory could not be prepared.
    #[error("could not open the provider compilation cache at {}", path.display())]
    CompileCache {
        /// Configured cache directory.
        path: PathBuf,
        /// Wasmtime error.
        #[source]
        source: wasmtime::Error,
    },
    /// Wasmtime engine initialization failed.
    #[error("could not initialize the Wasmtime engine")]
    Engine {
        /// Wasmtime error.
        #[source]
        source: wasmtime::Error,
    },
}

/// Base Wasmtime configuration for the broker host and external embeddings.
///
/// The caller configures how it interrupts a guest running too long. The broker host calls exports
/// asynchronously and yields on a fuel interval so a Tokio deadline can cancel the call. External
/// embeddings must choose their own interruption policy; this shared configuration does not
/// require Wasmtime's `async` feature.
#[must_use]
pub fn config() -> Config {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    config
}

/// Builds the one engine a host compiles and runs every component on.
///
/// A cache directory holds compiled machine code this process will execute, keyed by the artifact
/// bytes and the engine configuration, so a rebuilt component compiles again rather than being
/// served stale: point it only at a directory the host's own user controls. `None` runs Cranelift
/// again in every process.
///
/// # Errors
///
/// Returns [`EngineError::CompileCache`] when the cache directory cannot be prepared, and
/// [`EngineError::Engine`] when Wasmtime refuses the configuration.
pub fn engine(mut config: Config, compile_cache_dir: Option<&Path>) -> Result<Engine, EngineError> {
    if let Some(directory) = compile_cache_dir {
        let mut cache = CacheConfig::new();
        cache.with_directory(directory);
        config.cache(Some(Cache::new(cache).map_err(|source| {
            EngineError::CompileCache {
                path: directory.to_path_buf(),
                source,
            }
        })?));
    }
    Engine::new(&config).map_err(|source| EngineError::Engine { source })
}

#[cfg(test)]
mod tests {
    use dekopon_capability::EffectKind;
    use dekopon_core::RiskLevel;
    use serde_json::json;

    use super::{
        CommandExport, CommandExportProblem, ConflictScan, MAX_SIGNATURE_BYTES,
        RESOLVE_COMMAND_EXPORT, RUN_COMMAND_EXPORT, StoreLimits, bounded_signature,
        check_command_export, command_input_bytes, parse_command_run, validate_limits,
        validate_manifest,
    };
    use crate::{
        CommandRunOutcome, ComponentFailure, ProviderApiVersion, ProviderCapability,
        ProviderManifest,
    };

    fn manifest(id: &str, capability: &str, effect: EffectKind) -> ProviderManifest {
        ProviderManifest {
            api_version: ProviderApiVersion::V1Alpha1,
            id: id.parse().expect("valid provider fixture"),
            description: "Fixture provider".to_owned(),
            command_words: Vec::new(),
            capabilities: vec![ProviderCapability {
                id: capability.parse().expect("valid capability fixture"),
                description: "Runs a fixture".to_owned(),
                effect,
                risk: RiskLevel::Low,
                input_schema: json!({"type": "object"}),
            }],
        }
    }

    /// A declared effect is authorization's input, never the loader's: an external write loads.
    #[test]
    fn a_declared_external_write_is_loadable() {
        let writer = manifest("writer", "writer.write", EffectKind::ExternalWrite);

        validate_manifest(&writer).expect("the broker authorizes effects per invocation");
    }

    #[test]
    fn a_non_object_input_schema_is_refused() {
        let mut fixture = manifest("fixture", "fixture.run", EffectKind::ReadOnly);
        fixture.capabilities[0].input_schema = json!({"type": "string"});

        let error = validate_manifest(&fixture).expect_err("prompt tools require object arguments");

        assert!(
            error.message.contains("must declare type object"),
            "{error}"
        );
    }

    /// Two simultaneous conflicts, both reported: an operator fixes a provider set in one run.
    #[test]
    fn a_scan_reports_every_conflict_rather_than_the_first() {
        let mut scan = ConflictScan::new();
        scan.record(&manifest("shared", "one.run", EffectKind::ReadOnly), 0);
        scan.record(&manifest("shared", "one.run", EffectKind::ReadOnly), 1);

        let report = scan.finish().expect_err("a duplicated set must not route");

        assert_eq!(report.providers.len(), 1);
        assert_eq!(report.capabilities.len(), 1);
        assert_eq!(report.len(), 2);
        assert!(!report.is_empty());
        let rendered = report.to_string();
        assert!(rendered.contains("refusing to start"), "{rendered}");
        assert!(rendered.contains("provider shared"), "{rendered}");
        assert!(rendered.contains("capability one.run"), "{rendered}");
    }

    #[test]
    fn an_unambiguous_scan_routes_every_capability() {
        let mut scan = ConflictScan::new();
        scan.record(&manifest("first", "first.run", EffectKind::ReadOnly), 0);
        scan.record(&manifest("second", "second.run", EffectKind::ReadOnly), 1);

        let routes = scan.finish().expect("distinct providers do not conflict");

        assert_eq!(
            routes
                .get(&"second.run".parse().expect("valid capability fixture"))
                .copied(),
            Some(1)
        );
    }

    fn mismatched(name: &'static str) -> CommandExport {
        CommandExport::Mismatched {
            name,
            found: "fn(argv: String) -> (String)".to_owned(),
        }
    }

    /// Either callable export satisfies a manifest that declares words.
    #[test]
    fn the_command_gate_accepts_both_callable_exports() {
        let mut fixture = manifest("fixture", "fixture.run", EffectKind::ReadOnly);
        fixture.command_words = vec!["fixture".to_owned()];

        for export in [CommandExport::RunCommand, CommandExport::ResolveCommand] {
            check_command_export(&fixture, &export).expect("a callable export serves the words");
        }
    }

    #[test]
    fn the_command_gate_names_what_is_missing_or_mistyped() {
        let mut fixture = manifest("fixture", "fixture.run", EffectKind::ReadOnly);
        fixture.command_words = vec!["fixture".to_owned()];

        assert_eq!(
            check_command_export(&fixture, &CommandExport::Absent),
            Err(CommandExportProblem::Missing)
        );
        let problem = check_command_export(&fixture, &mismatched(RUN_COMMAND_EXPORT))
            .expect_err("a wrong type is refused");
        assert_eq!(
            problem,
            CommandExportProblem::Mismatched {
                name: RUN_COMMAND_EXPORT,
                found: "fn(argv: String) -> (String)".to_owned(),
            }
        );
        assert!(
            problem.to_string().contains("run-command as fn(argv"),
            "{problem}"
        );
    }

    /// Nothing calls an export no word routes to, so a wordless manifest is not held to it.
    #[test]
    fn a_manifest_without_words_passes_the_command_gate_whatever_is_exported() {
        let fixture = manifest("fixture", "fixture.run", EffectKind::ReadOnly);

        for export in [
            CommandExport::RunCommand,
            CommandExport::ResolveCommand,
            CommandExport::Absent,
            mismatched(RESOLVE_COMMAND_EXPORT),
        ] {
            check_command_export(&fixture, &export).expect("no word will ever reach the export");
        }
    }

    #[test]
    fn a_legacy_export_answer_parses_into_the_shared_outcome() {
        let outcome = parse_command_run(
            &CommandExport::ResolveCommand,
            r#"{"outcome":"resolved","capability":"fixture.run","input":{"last":5}}"#,
        )
        .expect("a legacy resolution parses");
        assert_eq!(
            outcome,
            CommandRunOutcome::Proposed {
                capability: "fixture.run".parse().expect("valid capability fixture"),
                input: json!({"last": 5}),
            }
        );

        let outcome = parse_command_run(
            &CommandExport::ResolveCommand,
            r#"{"outcome":"failed","error":{"code":"usage","message":"fixture --last N"}}"#,
        )
        .expect("a legacy decline parses");
        assert_eq!(
            outcome,
            CommandRunOutcome::Failed {
                error: ComponentFailure {
                    code: "usage".to_owned(),
                    message: "fixture --last N".to_owned(),
                },
            }
        );
    }

    #[test]
    fn a_run_export_answer_parses_only_as_the_run_wire_type() {
        let outcome = parse_command_run(
            &CommandExport::RunCommand,
            r#"{"outcome":"rendered","stdout":"Usage: fixture\n","stderr":"","status":0}"#,
        )
        .expect("a rendered page parses");
        assert_eq!(
            outcome,
            CommandRunOutcome::Rendered {
                stdout: "Usage: fixture\n".to_owned(),
                stderr: String::new(),
                status: 0,
            }
        );

        let error = parse_command_run(
            &CommandExport::RunCommand,
            r#"{"outcome":"resolved","capability":"fixture.run","input":{}}"#,
        )
        .expect_err("the legacy tag is not a run outcome");
        assert!(error.to_string().contains("resolved"), "{error}");

        let error = parse_command_run(
            &CommandExport::RunCommand,
            r#"{"outcome":"rendered","stdout":"","stderr":"","status":0,"extra":1}"#,
        )
        .expect_err("unknown fields are refused");
        assert!(error.to_string().contains("extra"), "{error}");
    }

    #[test]
    fn command_input_counts_every_argv_word_and_the_piped_value() {
        let argv = vec!["say".to_owned(), "-".to_owned()];

        assert_eq!(command_input_bytes(&argv, None), 4);
        assert_eq!(command_input_bytes(&argv, Some("hello")), 9);
        assert_eq!(command_input_bytes(&[], Some("hello")), 5);
    }

    #[test]
    fn an_oversized_signature_is_cut_at_a_character_boundary() {
        let short = "fn() -> (String)".to_owned();
        assert_eq!(bounded_signature(short.clone()), short);

        let long = "\u{e9}".repeat(MAX_SIGNATURE_BYTES);
        let cut = bounded_signature(long);
        assert!(cut.ends_with('\u{2026}'), "{cut}");
        assert!(cut.len() <= MAX_SIGNATURE_BYTES + '\u{2026}'.len_utf8());
    }

    #[test]
    fn a_zero_store_bound_is_named_before_the_hosts_own_limits() {
        let limits = StoreLimits {
            max_tables: 0,
            ..StoreLimits::default()
        };

        let error = validate_limits(&limits, &[("fuel", 0)]).expect_err("zero must be refused");

        assert_eq!(error.name, "max_tables");
        assert_eq!(
            validate_limits(&StoreLimits::default(), &[("fuel", 0)])
                .expect_err("a zero additional limit is refused too")
                .name,
            "fuel"
        );
    }
}
