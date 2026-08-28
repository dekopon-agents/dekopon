//! Host-side plumbing shared by Dekopon's two Wasmtime provider hosts.
//!
//! `dekopon-provider-host` runs import-free read-only components synchronously; `dekopon-broker-host`
//! runs authorized components asynchronously with the project-owned HTTP and storage interfaces
//! linked. Everything beneath that difference is the same contract: the rules a component manifest
//! must satisfy, the ambiguities a provider set may not contain, the bounds on one store, and the
//! engine configuration. They live here so a Wasmtime upgrade or a new manifest rule is reviewed
//! once instead of twice.
//!
//! The hosts' own machinery stays with each host: the immediate host serializes calls behind a
//! mutex and interrupts them from a deadline thread, while the broker host yields on fuel so a
//! Tokio deadline can cancel a call. Neither the linkers nor the timeout machinery is shared.
//!
//! None of this is guest code. The module sits behind the non-default `host` feature, which pulls
//! in Wasmtime; a `wasm32-unknown-unknown` provider build never enables it.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
};

use dekopon_capability::EffectKind;
use dekopon_core::{CapabilityId, CommandWordConflict, ProviderId};
use serde_json::Value;
use thiserror::Error;
use wasmtime::{Cache, CacheConfig, Config, Engine, StoreLimitsBuilder};

use crate::ProviderManifest;

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
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ManifestRejection {
    /// The manifest broke a semantic rule; the message is the operator-facing detail.
    #[error("{message}")]
    Invalid {
        /// What was wrong with the manifest.
        message: String,
    },
    /// A capability declared an effect this host does not permit.
    #[error("provider {provider} capability {capability} has unsupported effect {effect}")]
    UnsupportedEffect {
        /// Provider identity.
        provider: ProviderId,
        /// Capability identity.
        capability: CapabilityId,
        /// Rejected effect.
        effect: EffectKind,
    },
}

/// Checks the manifest rules every Dekopon host enforces.
///
/// `permitted_effect` is the effect gate. `Some(EffectKind::ReadOnly)` is the immediate host, which
/// links nothing and so cannot honestly host a capability claiming to change the world; `None` is
/// the broker host, where a declared effect is an input to authorization rather than a load-time
/// refusal.
///
/// The caller owns the component path and attaches it to whichever of its own errors this maps to;
/// nothing here reads the filesystem.
///
/// # Errors
///
/// Returns [`ManifestRejection`] for an empty description, no capabilities, a duplicated capability
/// identifier, a capability with an empty description or a non-object input schema, or a capability
/// outside the effect gate.
pub fn validate_manifest(
    manifest: &ProviderManifest,
    permitted_effect: Option<EffectKind>,
) -> Result<(), ManifestRejection> {
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
        if permitted_effect.is_some_and(|permitted| capability.effect != permitted) {
            return Err(ManifestRejection::UnsupportedEffect {
                provider: manifest.id.clone(),
                capability: capability.id.clone(),
                effect: capability.effect,
            });
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
    ManifestRejection::Invalid {
        message: message.into(),
    }
}

/// The two operator-facing strings that legitimately differ between the hosts' conflict reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConflictWording {
    /// What this host is refusing to do: `"load"` for the immediate host, `"start"` for the broker.
    pub refusing_to: &'static str,
    /// How an operator drops one of two components declaring the same provider identity.
    pub duplicate_provider_remedy: &'static str,
}

/// Everything wrong with one provider set, gathered so an operator sees it once.
///
/// Ambiguity is fatal in a way absence is not: a provider identity, capability, or command word two
/// components both claim has no meaning a host can pick without silently choosing for the operator.
/// This reports rather than resolves, and reports *all of it* — fixing a provider set should take
/// one run, not one run per mistake.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderConflicts {
    /// How this report addresses the operator.
    pub wording: ConflictWording,
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
            "refusing to {} \u{2014} {} provider conflict(s)",
            self.wording.refusing_to,
            self.len()
        )?;
        for provider in &self.providers {
            writeln!(formatter, "\n  provider {provider}")?;
            writeln!(formatter, "    declared by more than one component")?;
            writeln!(
                formatter,
                "    fix: {}",
                self.wording.duplicate_provider_remedy
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
#[derive(Clone, Debug)]
pub struct ConflictScan {
    wording: ConflictWording,
    provider_ids: BTreeSet<ProviderId>,
    duplicate_providers: BTreeSet<ProviderId>,
    duplicate_capabilities: BTreeSet<CapabilityId>,
    declared_words: Vec<(String, Vec<String>)>,
    routes: BTreeMap<CapabilityId, usize>,
}

impl ConflictScan {
    /// Starts a scan that reports in this host's wording.
    #[must_use]
    pub fn new(wording: ConflictWording) -> Self {
        Self {
            wording,
            provider_ids: BTreeSet::new(),
            duplicate_providers: BTreeSet::new(),
            duplicate_capabilities: BTreeSet::new(),
            declared_words: Vec::new(),
            routes: BTreeMap::new(),
        }
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
        // The broker refuses these words at startup; the immediate host has to agree, or an author
        // learns about a conflict only once the provider reaches `dekopon-brokerd`.
        let command_words = dekopon_core::command_word_conflicts(&self.declared_words);
        if !self.duplicate_providers.is_empty()
            || !self.duplicate_capabilities.is_empty()
            || !command_words.is_empty()
        {
            return Err(ProviderConflicts {
                wording: self.wording,
                providers: self.duplicate_providers.into_iter().collect(),
                capabilities: self.duplicate_capabilities.into_iter().collect(),
                command_words,
            });
        }
        Ok(self.routes)
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

/// The Wasmtime configuration both hosts start from.
///
/// The caller adds exactly one thing to it: how that host interrupts a guest running too long. The
/// immediate host enables epoch interruption and ticks the epoch from a deadline thread; the broker
/// host enables async support and yields on a fuel interval so a Tokio deadline can cancel the call.
/// That split is real, and it stays at the call sites rather than becoming a flag here — sharing one
/// more line would mean compiling Wasmtime's `async` feature into a synchronous host.
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
    use dekopon_capability::Idempotency;
    use dekopon_core::RiskLevel;
    use serde_json::json;

    use super::{
        ConflictScan, ConflictWording, EffectKind, ManifestRejection, StoreLimits, validate_limits,
        validate_manifest,
    };
    use crate::{ProviderApiVersion, ProviderCapability, ProviderManifest};

    const WORDING: ConflictWording = ConflictWording {
        refusing_to: "load",
        duplicate_provider_remedy: "remove one",
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
                idempotency: Idempotency::Idempotent,
                input_schema: json!({"type": "object"}),
            }],
        }
    }

    /// The gate is the whole difference between the two hosts' manifest rules.
    #[test]
    fn the_effect_gate_refuses_only_what_the_host_cannot_host() {
        let writer = manifest("writer", "writer.write", EffectKind::ExternalWrite);

        validate_manifest(&writer, None).expect("the broker authorizes effects separately");
        let error = validate_manifest(&writer, Some(EffectKind::ReadOnly))
            .expect_err("immediate mode links nothing and must refuse an external write");

        assert!(matches!(error, ManifestRejection::UnsupportedEffect { .. }));
    }

    #[test]
    fn a_non_object_input_schema_is_refused_under_either_gate() {
        let mut fixture = manifest("fixture", "fixture.run", EffectKind::ReadOnly);
        fixture.capabilities[0].input_schema = json!({"type": "string"});

        for gate in [None, Some(EffectKind::ReadOnly)] {
            let error = validate_manifest(&fixture, gate)
                .expect_err("prompt tools require object arguments");
            assert!(matches!(error, ManifestRejection::Invalid { .. }));
        }
    }

    /// Two simultaneous conflicts, both reported: an operator fixes a provider set in one run.
    #[test]
    fn a_scan_reports_every_conflict_rather_than_the_first() {
        let mut scan = ConflictScan::new(WORDING);
        scan.record(&manifest("shared", "one.run", EffectKind::ReadOnly), 0);
        scan.record(&manifest("shared", "one.run", EffectKind::ReadOnly), 1);

        let report = scan.finish().expect_err("a duplicated set must not route");

        assert_eq!(report.providers.len(), 1);
        assert_eq!(report.capabilities.len(), 1);
        assert_eq!(report.len(), 2);
        assert!(!report.is_empty());
        let rendered = report.to_string();
        assert!(rendered.contains("refusing to load"), "{rendered}");
        assert!(rendered.contains("provider shared"), "{rendered}");
        assert!(rendered.contains("capability one.run"), "{rendered}");
    }

    #[test]
    fn an_unambiguous_scan_routes_every_capability() {
        let mut scan = ConflictScan::new(WORDING);
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
