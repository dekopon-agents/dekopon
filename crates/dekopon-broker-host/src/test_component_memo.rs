use std::{
    collections::BTreeMap,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use parking_lot::Mutex;
use wasmtime::{Engine, component::Component};

static MEMO: OnceLock<Memo> = OnceLock::new();
static BYPASS_COMPILES: AtomicUsize = AtomicUsize::new(0);

pub struct TestComponentMemoization {
    memo: &'static Memo,
}

impl TestComponentMemoization {
    #[must_use]
    pub fn compile_count(&self) -> usize {
        self.memo.compiles.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn hit_count(&self) -> usize {
        self.memo.hits.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn bypass_compile_count(&self) -> usize {
        BYPASS_COMPILES.load(Ordering::Relaxed)
    }
}

pub fn enable_test_component_memoization() -> TestComponentMemoization {
    TestComponentMemoization {
        memo: MEMO.get_or_init(Memo::default),
    }
}

#[derive(Default)]
struct Memo {
    slots: Mutex<BTreeMap<ComponentKey, Arc<ComponentSlot>>>,
    compiles: AtomicUsize,
    hits: AtomicUsize,
}

#[derive(Default)]
struct ComponentSlot {
    serialized: Mutex<Option<Vec<u8>>>,
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
struct ComponentKey {
    engine: String,
    sha256: String,
}

pub(crate) fn compile(
    engine: &Engine,
    engine_key: &str,
    sha256: &str,
    bytes: &[u8],
) -> wasmtime::Result<Component> {
    match MEMO.get() {
        Some(memo) => memo.compile(engine, engine_key, sha256, bytes),
        None => {
            BYPASS_COMPILES.fetch_add(1, Ordering::Relaxed);
            Component::new(engine, bytes)
        }
    }
}

impl Memo {
    fn compile(
        &self,
        engine: &Engine,
        engine_key: &str,
        sha256: &str,
        bytes: &[u8],
    ) -> wasmtime::Result<Component> {
        let key = ComponentKey {
            engine: engine_key.to_owned(),
            sha256: sha256.to_owned(),
        };
        let slot = {
            let mut slots = self.slots.lock();
            Arc::clone(slots.entry(key).or_default())
        };
        let mut serialized = slot.serialized.lock();
        if let Some(bytes) = serialized.as_ref() {
            let component = deserialize(engine, bytes)?;
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(component);
        }
        self.compiles.fetch_add(1, Ordering::Relaxed);
        let component = Component::new(engine, bytes)?;
        *serialized = Some(component.serialize()?);
        Ok(component)
    }
}

#[allow(
    unsafe_code,
    reason = "only immutable bytes serialized by this process under the matching engine key reach this deserializer"
)]
fn deserialize(engine: &Engine, bytes: &[u8]) -> wasmtime::Result<Component> {
    // SAFETY: slots contain only successful Component::serialize output from this process, keyed by engine compatibility and source hash, and kept immutable while deserializing.
    unsafe { Component::deserialize(engine, bytes) }
}
