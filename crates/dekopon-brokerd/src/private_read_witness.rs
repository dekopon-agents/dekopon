use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use dekopon_core::FileHygieneError;

#[derive(Default)]
struct State {
    reads: usize,
    last_bytes: Vec<u8>,
}

fn registry() -> &'static Mutex<BTreeMap<PathBuf, Arc<Mutex<State>>>> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<PathBuf, Arc<Mutex<State>>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub(crate) struct TrackedRead {
    path: PathBuf,
    state: Arc<Mutex<State>>,
}

impl TrackedRead {
    pub(crate) fn new(path: &Path) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let path = path.canonicalize().unwrap();
        registry()
            .lock()
            .unwrap()
            .insert(path.clone(), Arc::clone(&state));
        Self { path, state }
    }

    pub(crate) fn count(&self) -> usize {
        self.state.lock().unwrap().reads
    }

    pub(crate) fn last_bytes(&self) -> Vec<u8> {
        self.state.lock().unwrap().last_bytes.clone()
    }

    pub(crate) fn reset(&self) {
        *self.state.lock().unwrap() = State::default();
    }
}

impl Drop for TrackedRead {
    fn drop(&mut self) {
        registry().lock().unwrap().remove(&self.path);
    }
}

pub(crate) fn record(path: &Path, result: &Result<Vec<u8>, FileHygieneError>) {
    let state = registry().lock().unwrap().get(path).cloned();
    if let Some(state) = state {
        let mut state = state.lock().unwrap();
        state.reads += 1;
        if let Ok(bytes) = result {
            state.last_bytes.clone_from(bytes);
        }
    }
}
