use std::{fmt::Write as _, path::PathBuf};

use dekopon_provider_sdk::host::{bounded_signature, function_signature, item_kind};
use sha2::{Digest as _, Sha256};
use wasmtime::Engine;
use wasmtime::component::Component;
use wasmtime::component::types::ComponentItem;

use crate::ProviderManifest;

/// Maximum component type entries retained for one provider's informational view.
///
/// Provider metadata is owner-supplied but still must not make an unauthenticated status page grow
/// without bound. The manifest and the component itself remain authoritative when this summary is
/// truncated. Each rendered signature is bounded by the SDK's shared signature limit.
const MAX_INTERFACE_ITEMS: usize = 1_024;
const MAX_INTERFACE_DEPTH: usize = 8;

/// Metadata retained for one component that was actually compiled into the broker registry.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedProviderMetadata {
    /// Local source file compiled by Wasmtime.
    pub source: PathBuf,
    /// Length of the buffer that was compiled.
    pub artifact_bytes: u64,
    /// Lowercase SHA-256 of the exact bytes that were compiled.
    pub artifact_sha256: String,
    /// Validated manifest returned by the component.
    pub manifest: ProviderManifest,
    /// Top-level component imports and their nested interface members.
    pub imports: Vec<ComponentInterfaceItem>,
    /// Top-level component exports and their nested interface members.
    pub exports: Vec<ComponentInterfaceItem>,
    /// Whether interface retention hit its defensive item or depth bound.
    pub interface_truncated: bool,
}

/// One import, export, function, or nested interface visible in Wasmtime's component type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComponentInterfaceItem {
    /// Component-model name.
    pub name: String,
    /// Stable broad kind (`instance`, `function`, `module`, and so on).
    pub kind: &'static str,
    /// Human-readable function or core-item type when Wasmtime exposes one.
    pub signature: Option<String>,
    /// Nested exports for component instances or nested components.
    pub members: Vec<ComponentInterfaceItem>,
}

#[derive(Eq, PartialEq)]
pub(crate) struct ArtifactIdentity {
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
}

/// Identifies the exact buffer a caller is about to hand to Wasmtime.
///
/// Taking bytes rather than a path is the point: a digest computed from a second read cannot prove
/// it describes what Cranelift compiled, and the recorded `artifact_sha256` is published metadata.
pub(crate) fn identify_bytes(bytes: &[u8]) -> ArtifactIdentity {
    let digest = Sha256::digest(bytes);
    let mut sha256 = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut sha256, "{byte:02x}").expect("writing to a String cannot fail");
    }
    ArtifactIdentity {
        bytes: bytes.len() as u64,
        sha256,
    }
}

pub(crate) fn component_interface(
    engine: &Engine,
    component: &Component,
) -> (
    Vec<ComponentInterfaceItem>,
    Vec<ComponentInterfaceItem>,
    bool,
) {
    let component_type = component.component_type();
    let mut budget = InterfaceBudget {
        remaining: MAX_INTERFACE_ITEMS,
        truncated: false,
    };
    let imports = component_type
        .imports(engine)
        .filter_map(|(name, item)| summarize(name, item, engine, &mut budget, 0))
        .collect();
    let exports = component_type
        .exports(engine)
        .filter_map(|(name, item)| summarize(name, item, engine, &mut budget, 0))
        .collect();
    (imports, exports, budget.truncated)
}

struct InterfaceBudget {
    remaining: usize,
    truncated: bool,
}

fn summarize(
    name: &str,
    item: ComponentItem,
    engine: &Engine,
    budget: &mut InterfaceBudget,
    depth: usize,
) -> Option<ComponentInterfaceItem> {
    if budget.remaining == 0 {
        budget.truncated = true;
        return None;
    }
    budget.remaining -= 1;
    if depth >= MAX_INTERFACE_DEPTH {
        budget.truncated = true;
        return Some(ComponentInterfaceItem {
            name: name.to_owned(),
            kind: item_kind(&item),
            signature: item_signature(&item),
            members: Vec::new(),
        });
    }

    let kind = item_kind(&item);
    let signature = item_signature(&item);
    let members = match item {
        ComponentItem::Component(component) => component
            .exports(engine)
            .filter_map(|(child, item)| summarize(child, item, engine, budget, depth + 1))
            .collect(),
        ComponentItem::ComponentInstance(instance) => instance
            .exports(engine)
            .filter_map(|(child, item)| summarize(child, item, engine, budget, depth + 1))
            .collect(),
        ComponentItem::Module(module) => module
            .exports(engine)
            .filter_map(|(child, item)| {
                if budget.remaining == 0 {
                    budget.truncated = true;
                    return None;
                }
                budget.remaining -= 1;
                Some(ComponentInterfaceItem {
                    name: child.to_owned(),
                    kind: "core-export",
                    signature: Some(bounded_debug(&item)),
                    members: Vec::new(),
                })
            })
            .collect(),
        ComponentItem::ComponentFunc(_)
        | ComponentItem::CoreFunc(_)
        | ComponentItem::Type(_)
        | ComponentItem::Resource(_) => Vec::new(),
    };
    Some(ComponentInterfaceItem {
        name: name.to_owned(),
        kind,
        signature,
        members,
    })
}

fn item_signature(item: &ComponentItem) -> Option<String> {
    match item {
        ComponentItem::ComponentFunc(function) => Some(function_signature(function)),
        ComponentItem::CoreFunc(function) => Some(bounded_debug(function)),
        ComponentItem::Type(value) => Some(bounded_debug(value)),
        ComponentItem::Resource(value) => Some(bounded_debug(value)),
        ComponentItem::Module(_)
        | ComponentItem::Component(_)
        | ComponentItem::ComponentInstance(_) => None,
    }
}

fn bounded_debug(value: &impl std::fmt::Debug) -> String {
    bounded_signature(format!("{value:?}"))
}
