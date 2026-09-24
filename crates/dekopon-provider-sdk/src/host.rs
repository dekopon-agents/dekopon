use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use dekopon_core::{CapabilityId, CommandWordConflict, ProviderId};
use serde_json::Value;
use thiserror::Error;
use wasmtime::component::types::{ComponentFunc, ComponentItem};
use wasmtime::component::{Component, Type};
use wasmtime::{Config, Engine, StoreLimitsBuilder};

use crate::ProviderManifest;

pub const DEFAULT_MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_MAX_TABLE_ELEMENTS: usize = 100_000;
pub const DEFAULT_MAX_INSTANCES: usize = 64;
pub const DEFAULT_MAX_TABLES: usize = 16;
pub const DEFAULT_MAX_MEMORIES: usize = 4;
pub const DEFAULT_MAX_INPUT_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;

pub const RUN_COMMAND_EXPORT: &str = "run-command";

/// The signature comes from the component's own type, which its author controls, and appears in a
/// load error, so its length is bounded.
const MAX_SIGNATURE_BYTES: usize = 4 * 1024;

/// This is the subset of a host's limits that shapes a store; fuel, wall-clock, and serialized
/// input/output bounds stay with the host enforcing them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreLimits {
    /// Wasmtime applies this per memory, not per store, which is why the store also bounds
    /// memories, tables, table elements, and instances.
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

/// A zero store bound fails every instantiation, and zero fuel or a zero deadline traps before
/// guest code runs, so this checks first and names the field.
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

/// The manifest broke a semantic rule; the message is the operator-facing detail.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct ManifestRejection {
    /// What was wrong with the manifest.
    pub message: String,
}

/// A declared effect is authorization's input at each invocation, not something this load-time
/// check gates on.
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

/// Ambiguity is fatal because a host cannot silently pick among conflicting claims, so this reports
/// every conflict in one run instead of one per mistake.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderConflicts {
    /// Provider identities declared by more than one component.
    pub providers: Vec<ProviderId>,
    /// Capability identifiers declared by more than one component.
    pub capabilities: Vec<CapabilityId>,
    /// Providers that declare capabilities and no command word to reach them through.
    pub wordless: Vec<ProviderId>,
    /// Command words that cannot be granted to the providers claiming them.
    pub command_words: Vec<CommandWordConflict>,
}

impl ProviderConflicts {
    /// Reports how many distinct conflicts this covers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.len()
            + self.capabilities.len()
            + self.wordless.len()
            + self.command_words.len()
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
        for provider in &self.wordless {
            writeln!(formatter, "\n  provider {provider}")?;
            writeln!(
                formatter,
                "    declares capabilities but no command words, so no model can reach them"
            )?;
            writeln!(
                formatter,
                "    fix: declare commandWords and export run-command, or drop it from the provider \
                 search path"
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

/// Accumulates every ambiguity in one provider set; a host records each manifest in load order and
/// finishes with the deterministic capability routes or the whole conflict report.
#[derive(Clone, Debug, Default)]
pub struct ConflictScan {
    provider_ids: BTreeSet<ProviderId>,
    duplicate_providers: BTreeSet<ProviderId>,
    duplicate_capabilities: BTreeSet<CapabilityId>,
    wordless: BTreeSet<ProviderId>,
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
        if !manifest.capabilities.is_empty() && manifest.command_words.is_empty() {
            self.wordless.insert(manifest.id.clone());
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
    pub fn finish(self) -> Result<BTreeMap<CapabilityId, usize>, ProviderConflicts> {
        let command_words = dekopon_core::command_word_conflicts(&self.declared_words);
        if !self.duplicate_providers.is_empty()
            || !self.duplicate_capabilities.is_empty()
            || !self.wordless.is_empty()
            || !command_words.is_empty()
        {
            return Err(ProviderConflicts {
                providers: self.duplicate_providers.into_iter().collect(),
                capabilities: self.duplicate_capabilities.into_iter().collect(),
                wordless: self.wordless.into_iter().collect(),
                command_words,
            });
        }
        Ok(self.routes)
    }
}

/// Absent and wrong-typed are different operator problems with different fixes, and neither is
/// worth instantiating the component to discover.
#[derive(Clone, Debug, PartialEq)]
pub enum CommandExport {
    /// Exports `run-command: func(argv: list<string>, stdin: option<string>) -> string`.
    Present,
    /// Exports nothing under the name: the component was built against the base
    /// `dekopon:provider` world.
    Absent,
    /// Exports `run-command` as something the host cannot call.
    Mismatched {
        /// Bounded description of what the component actually exports under the name.
        found: String,
    },
}

/// Reads whether `component` offers the `run-command` export from its type.
#[must_use]
pub fn command_export(engine: &Engine, component: &Component) -> CommandExport {
    let Some(item) = component
        .component_type()
        .exports(engine)
        .find(|(name, _)| *name == RUN_COMMAND_EXPORT)
        .map(|(_, item)| item.ty)
    else {
        return CommandExport::Absent;
    };
    let ComponentItem::ComponentFunc(function) = &item else {
        return CommandExport::Mismatched {
            found: item_kind(&item).to_owned(),
        };
    };
    if runs_commands(function) {
        CommandExport::Present
    } else {
        CommandExport::Mismatched {
            found: function_signature(function),
        }
    }
}

fn runs_commands(function: &ComponentFunc) -> bool {
    let mut params = function.params();
    let argv_is_strings = params.len() == 2
        && matches!(params.next(), Some((_, Type::List(list))) if list.ty() == Type::String);
    let stdin_is_optional_string =
        matches!(params.next(), Some((_, Type::Option(option))) if option.ty() == Type::String);
    argv_is_strings && stdin_is_optional_string && returns_one_string(function)
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
    /// The component does not export `run-command`.
    #[error("exports no {RUN_COMMAND_EXPORT}")]
    Missing,
    /// The component exports the name as something the host cannot call.
    #[error("exports {RUN_COMMAND_EXPORT} as {found}")]
    Mismatched {
        /// Bounded description of what the component actually exports under it.
        found: String,
    },
}

/// Checking at load catches a manifest promising words its component cannot run, instead of failing
/// hours into a session at the first call.
pub fn check_command_export(
    manifest: &ProviderManifest,
    export: &CommandExport,
) -> Result<(), CommandExportProblem> {
    if manifest.command_words.is_empty() {
        return Ok(());
    }
    match export {
        CommandExport::Present => Ok(()),
        CommandExport::Absent => Err(CommandExportProblem::Missing),
        CommandExport::Mismatched { found } => Err(CommandExportProblem::Mismatched {
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

/// Failure to build the shared Wasmtime engine.
#[derive(Debug, Error)]
pub enum EngineError {
    /// Wasmtime engine initialization failed.
    #[error("could not initialize the Wasmtime engine")]
    Engine {
        /// Wasmtime error.
        #[source]
        source: wasmtime::Error,
    },
}

/// The broker host calls exports asynchronously and yields on a fuel interval so a Tokio deadline
/// can cancel the call; external embeddings choose their own interruption policy.
#[must_use]
pub fn config() -> Config {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    config
}

/// Persistent compiled artifacts are owned by the broker host, not Wasmtime's own compressed cache.
pub fn engine(config: Config) -> Result<Engine, EngineError> {
    Engine::new(&config).map_err(|source| EngineError::Engine { source })
}

#[cfg(test)]
mod tests {
    use dekopon_capability::EffectKind;
    use dekopon_core::RiskLevel;
    use serde_json::json;

    use super::{
        CommandExport, CommandExportProblem, ConflictScan, MAX_SIGNATURE_BYTES, StoreLimits,
        bounded_signature, check_command_export, command_input_bytes, validate_limits,
        validate_manifest,
    };
    use crate::{ProviderApiVersion, ProviderCapability, ProviderManifest};

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

    fn worded(id: &str, capability: &str, word: &str) -> ProviderManifest {
        let mut fixture = manifest(id, capability, EffectKind::ReadOnly);
        fixture.command_words = vec![word.to_owned()];
        fixture
    }

    #[test]
    fn a_scan_reports_every_conflict_rather_than_the_first() {
        let mut scan = ConflictScan::new();
        scan.record(&worded("shared", "one.run", "one"), 0);
        scan.record(&worded("shared", "one.run", "two"), 1);

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
    fn every_wordless_provider_is_named_in_the_one_conflict_report() {
        let mut scan = ConflictScan::new();
        scan.record(&manifest("first", "first.run", EffectKind::ReadOnly), 0);
        scan.record(&manifest("second", "second.run", EffectKind::ReadOnly), 1);
        scan.record(&worded("third", "third.run", "jq"), 2);

        let report = scan
            .finish()
            .expect_err("a provider no word reaches must not route");

        assert_eq!(
            report
                .wordless
                .iter()
                .map(|provider| provider.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert_eq!(report.command_words.len(), 1, "{report:?}");
        assert_eq!(report.len(), 3);
        let rendered = report.to_string();
        assert_eq!(
            rendered.matches("refusing to start").count(),
            1,
            "{rendered}"
        );
        assert!(
            rendered.starts_with("refusing to start \u{2014} 3 provider conflict(s)"),
            "{rendered}"
        );
        for provider in ["first", "second"] {
            assert!(
                rendered.contains(&format!(
                    "\n  provider {provider}\n    declares capabilities but no command words"
                )),
                "{provider} must be named: {rendered}"
            );
        }
        assert!(rendered.contains("command word `jq`"), "{rendered}");
    }

    #[test]
    fn an_unambiguous_scan_routes_every_capability() {
        let mut scan = ConflictScan::new();
        scan.record(&worded("first", "first.run", "first"), 0);
        scan.record(&worded("second", "second.run", "second"), 1);

        let routes = scan.finish().expect("distinct providers do not conflict");

        assert_eq!(
            routes
                .get(&"second.run".parse().expect("valid capability fixture"))
                .copied(),
            Some(1)
        );
    }

    fn mismatched() -> CommandExport {
        CommandExport::Mismatched {
            found: "fn(argv: String) -> (String)".to_owned(),
        }
    }

    #[test]
    fn the_command_gate_accepts_the_callable_export() {
        let mut fixture = manifest("fixture", "fixture.run", EffectKind::ReadOnly);
        fixture.command_words = vec!["fixture".to_owned()];

        check_command_export(&fixture, &CommandExport::Present)
            .expect("a callable export serves the words");
    }

    #[test]
    fn the_command_gate_names_what_is_missing_or_mistyped() {
        let mut fixture = manifest("fixture", "fixture.run", EffectKind::ReadOnly);
        fixture.command_words = vec!["fixture".to_owned()];

        let problem = check_command_export(&fixture, &CommandExport::Absent)
            .expect_err("a missing export is refused");
        assert_eq!(problem, CommandExportProblem::Missing);
        assert!(problem.to_string().contains("no run-command"), "{problem}");
        let problem =
            check_command_export(&fixture, &mismatched()).expect_err("a wrong type is refused");
        assert_eq!(
            problem,
            CommandExportProblem::Mismatched {
                found: "fn(argv: String) -> (String)".to_owned(),
            }
        );
        assert!(
            problem.to_string().contains("run-command as fn(argv"),
            "{problem}"
        );
    }

    #[test]
    fn a_manifest_without_words_passes_the_command_gate_whatever_is_exported() {
        let fixture = manifest("fixture", "fixture.run", EffectKind::ReadOnly);

        for export in [CommandExport::Present, CommandExport::Absent, mismatched()] {
            check_command_export(&fixture, &export).expect("no word will ever reach the export");
        }
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
