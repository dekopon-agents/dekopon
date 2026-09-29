//! Only a missing index counts as a cache miss; every other failure stops startup, and the operator
//! must never modify or truncate a mapped artifact in place.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    hash::{Hash as _, Hasher},
    io::{self, Read as _, Write as _},
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Instant,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use wasmtime::{Config, Engine, component::Component};

use crate::metadata::{hex_digest, identify_bytes};

const LAYOUT: &str = "v2";
const PREVIOUS_LAYOUT: &str = "v1";

const MAX_INDEX_BYTES: u64 = 4096;
// Compiled artifacts can exceed the 64 MiB source ceiling; this bounds refusal only, never
// allocation from an unchecked on-disk length, and promises nothing about resident memory.
const MAX_CWASM_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_CACHE_OBJECTS: usize = 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    sha256: String,
    bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CacheMiss {
    #[default]
    Fail,
    Compile,
}

pub(crate) enum Lookup {
    Mapped(Component),
    Missing,
}

pub(crate) enum Compiler {
    Serial,
    Pool(rayon::ThreadPool),
}

impl Compiler {
    pub(crate) fn new(threads: NonZeroU32) -> Result<Self, rayon::ThreadPoolBuildError> {
        if threads.get() == 1 {
            return Ok(Self::Serial);
        }
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads.get() as usize)
            .thread_name(|index| format!("dekopon-compile-{index}"))
            .build()
            .map(Self::Pool)
    }

    pub(crate) fn configure(&self, config: &mut Config) {
        config.parallel_compilation(matches!(self, Self::Pool(_)));
    }

    pub(crate) fn run<T: Send>(&self, work: impl FnOnce() -> T + Send) -> T {
        match self {
            Self::Serial => work(),
            // Wasmtime's parallel compilation runs on the rayon pool of the calling thread.
            Self::Pool(pool) => pool.install(work),
        }
    }
}

#[derive(Default)]
pub(crate) struct Removed {
    pub(crate) files: u64,
    pub(crate) bytes: u64,
}

impl Removed {
    fn tree(&mut self, path: &Path) -> io::Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if metadata.is_dir() {
            for entry in entries(path)? {
                self.tree(&entry)?;
            }
            return fs::remove_dir(path);
        }
        // Unlink only, never truncate: a mapped object stays valid until its mapper exits.
        fs::remove_file(path)?;
        self.files += 1;
        self.bytes = self.bytes.saturating_add(metadata.len());
        Ok(())
    }
}

pub(crate) struct Cache {
    root: PathBuf,
    engine_key: String,
    miss: CacheMiss,
    loaded: Mutex<BTreeMap<String, Component>>,
}

impl Cache {
    pub(crate) fn new(root: PathBuf, engine: &Engine, miss: CacheMiss) -> Self {
        Self {
            root: root.join(LAYOUT),
            engine_key: compatibility_key(engine),
            miss,
            loaded: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) fn load(
        &self,
        engine: &Engine,
        compiler: &Compiler,
        wasm: &[u8],
        source_sha256: &str,
    ) -> wasmtime::Result<Lookup> {
        self.load_inner(engine, compiler, wasm, source_sha256)
            .map_err(|error| error.context(format!("cwasm cache {}", self.root.display())))
    }

    pub(crate) fn publish_missing(
        &self,
        engine: &Engine,
        compiler: &Compiler,
        wasm: &[u8],
        source_sha256: &str,
    ) -> wasmtime::Result<bool> {
        let index = self.index(source_sha256);
        let published = match fs::symlink_metadata(&index) {
            Ok(_) => false,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.compile_and_publish(engine, compiler, wasm, &index)?;
                true
            }
            Err(error) => {
                return Err(wasmtime::Error::msg(format!(
                    "open compiled index {}: {error}",
                    index.display()
                )));
            }
        };
        Ok(published)
    }

    pub(crate) fn prune<'a>(
        &self,
        live_sources: impl IntoIterator<Item = &'a str>,
    ) -> wasmtime::Result<Removed> {
        let live_sources = live_sources.into_iter().collect::<BTreeSet<_>>();
        let mut removed = Removed::default();
        removed.tree(&self.root.with_file_name(PREVIOUS_LAYOUT))?;
        let current = self.root.join(&self.engine_key);
        let objects = self.root.join("sha256");
        for path in entries(&self.root)? {
            if path != current && path != objects {
                removed.tree(&path)?;
            }
        }
        let mut referenced = BTreeSet::new();
        for path in entries(&current)? {
            let live = path
                .extension()
                .is_some_and(|extension| extension == "json")
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| live_sources.contains(stem));
            if live {
                let file = File::open(&path)?;
                referenced.insert(self.object(&read_entry(&path, file)?));
            } else {
                removed.tree(&path)?;
            }
        }
        for path in entries(&objects)? {
            if !referenced.contains(&path) {
                removed.tree(&path)?;
            }
        }
        Ok(removed)
    }

    fn index(&self, source_sha256: &str) -> PathBuf {
        self.root
            .join(&self.engine_key)
            .join(format!("{source_sha256}.json"))
    }

    fn compile_and_publish(
        &self,
        engine: &Engine,
        compiler: &Compiler,
        wasm: &[u8],
        index: &Path,
    ) -> wasmtime::Result<Entry> {
        let compiled = stage("compile", wasm.len() as u64, || {
            compiler.run(|| engine.precompile_component(wasm))
        })?;
        wasmtime::ensure!(
            compiled.len() as u64 <= MAX_CWASM_BYTES,
            "compiled component exceeds {MAX_CWASM_BYTES} bytes"
        );
        let artifact = stage("artifact_hash", compiled.len() as u64, || {
            Ok(identify_bytes(&compiled))
        })?;
        let entry = Entry {
            sha256: artifact.sha256,
            bytes: artifact.bytes,
        };
        let object = self.object(&entry);
        stage("publish", entry.bytes, || {
            fs::create_dir_all(self.root.join("sha256"))?;
            fs::create_dir_all(self.root.join(&self.engine_key))?;
            ensure_capacity(&self.root.join("sha256"), entry.bytes)?;
            publish(&object, &compiled)?;
            publish(index, &serde_json::to_vec(&entry)?)
        })?;
        Ok(entry)
    }

    fn load_inner(
        &self,
        engine: &Engine,
        compiler: &Compiler,
        wasm: &[u8],
        source_sha256: &str,
    ) -> wasmtime::Result<Lookup> {
        let started = Instant::now();
        let mut loaded = self.loaded.lock().map_err(|error| {
            wasmtime::Error::msg(format!("compiled component loader lock poisoned: {error}"))
        })?;
        tracing::Span::current().record("cache_wait_us", micros(started));
        let index = self.index(source_sha256);
        let entry = match File::open(&index) {
            Ok(file) => {
                tracing::Span::current().record("cache", "hit");
                read_entry(&index, file)?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                tracing::Span::current().record("cache", "miss");
                if self.miss == CacheMiss::Fail {
                    return Ok(Lookup::Missing);
                }
                let entry = self.compile_and_publish(engine, compiler, wasm, &index)?;
                // No second verification pass runs on a cold miss, since these exact bytes were
                // just hashed and published locally; warm boots stream-verify instead.
                return self.map(engine, entry, &mut loaded).map(Lookup::Mapped);
            }
            Err(error) => {
                return Err(wasmtime::Error::msg(format!(
                    "open compiled index {}: {error}",
                    index.display()
                )));
            }
        };
        self.load_entry(engine, entry, &mut loaded)
            .map(Lookup::Mapped)
    }

    fn load_entry(
        &self,
        engine: &Engine,
        entry: Entry,
        loaded: &mut BTreeMap<String, Component>,
    ) -> wasmtime::Result<Component> {
        if let Some(component) = loaded.get(&entry.sha256) {
            tracing::Span::current().record("cache", "reuse");
            record_artifact(&entry);
            return Ok(component.clone());
        }
        let object = self.object(&entry);
        stage("verify", entry.bytes, || verify(&object, &entry))?;
        self.map(engine, entry, loaded)
    }

    fn object(&self, entry: &Entry) -> PathBuf {
        self.root
            .join("sha256")
            .join(format!("{}.cwasm", entry.sha256))
    }

    fn map(
        &self,
        engine: &Engine,
        entry: Entry,
        loaded: &mut BTreeMap<String, Component>,
    ) -> wasmtime::Result<Component> {
        record_artifact(&entry);
        let path = self.object(&entry);
        let component = stage("deserialize", entry.bytes, || deserialize(engine, &path))?;
        loaded.insert(entry.sha256, component.clone());
        Ok(component)
    }
}

fn record_artifact(entry: &Entry) {
    let span = tracing::Span::current();
    span.record("cwasm_sha256", &entry.sha256);
    span.record("cwasm_bytes", entry.bytes);
}

fn entries(directory: &Path) -> io::Result<Vec<PathBuf>> {
    match fs::read_dir(directory) {
        Ok(entries) => entries
            .map(|entry| entry.map(|entry| entry.path()))
            .collect(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn read_entry(index: &Path, file: File) -> wasmtime::Result<Entry> {
    let mut bytes = Vec::new();
    file.take(MAX_INDEX_BYTES + 1).read_to_end(&mut bytes)?;
    wasmtime::ensure!(
        bytes.len() as u64 <= MAX_INDEX_BYTES,
        "index {} exceeds {MAX_INDEX_BYTES} bytes",
        index.display()
    );
    let entry: Entry = serde_json::from_slice(&bytes).map_err(|error| {
        wasmtime::Error::msg(format!(
            "invalid compiled index {}: {error}",
            index.display()
        ))
    })?;
    wasmtime::ensure!(
        entry.sha256.len() == 64
            && entry
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid compiled SHA-256 in {}",
        index.display()
    );
    wasmtime::ensure!(
        entry.bytes > 0 && entry.bytes <= MAX_CWASM_BYTES,
        "invalid compiled length {} in {} (maximum {MAX_CWASM_BYTES})",
        entry.bytes,
        index.display()
    );
    Ok(entry)
}

fn verify(path: &Path, entry: &Entry) -> wasmtime::Result<()> {
    let mut file = File::open(path).map_err(|error| {
        wasmtime::Error::msg(format!(
            "open compiled artifact {}: {error}",
            path.display()
        ))
    })?;
    let actual = file.metadata()?.len();
    wasmtime::ensure!(
        actual == entry.bytes,
        "compiled length mismatch for {}: expected {}, got {actual}",
        path.display(),
        entry.bytes
    );
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut remaining = entry.bytes;
    while remaining != 0 {
        let size = usize::try_from(remaining.min(buffer.len() as u64))?;
        file.read_exact(&mut buffer[..size])?;
        hash.update(&buffer[..size]);
        remaining -= size as u64;
    }
    let actual = hex_digest(&hash.finalize());
    wasmtime::ensure!(
        actual == entry.sha256,
        "compiled SHA-256 mismatch for {}: expected {}, got {actual}",
        path.display(),
        entry.sha256
    );
    Ok(())
}

fn ensure_capacity(objects: &Path, requested: u64) -> wasmtime::Result<()> {
    let mut bytes = requested;
    for (count, entry) in fs::read_dir(objects)?.enumerate() {
        let entry = entry?;
        bytes = bytes.saturating_add(entry.metadata()?.len());
        wasmtime::ensure!(
            count + 1 < MAX_CACHE_OBJECTS && bytes <= MAX_CACHE_BYTES,
            "compiled cache {} is full (maximum {MAX_CACHE_OBJECTS} objects / {MAX_CACHE_BYTES} bytes); stop its users and remove the cwasm directory, or set compileOnLoad: true",
            objects.display()
        );
    }
    Ok(())
}

fn publish(path: &Path, bytes: &[u8]) -> wasmtime::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| wasmtime::Error::msg("compiled artifact has no parent"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    // Never overwrites an inode another broker may already have mapped; concurrent publishers fail
    // visibly, with no locking, retry, or recovery transaction.
    temporary.persist_noclobber(path).map_err(|error| {
        wasmtime::Error::msg(format!(
            "publish compiled artifact {}: {error}",
            path.display()
        ))
    })?;
    Ok(())
}

#[allow(
    unsafe_code,
    reason = "Wasmtime's trusted-file deserializer; the only unsafe call in the broker host"
)]
fn deserialize(engine: &Engine, path: &Path) -> wasmtime::Result<Component> {
    // SAFETY: cache misses originate in Engine::precompile_component. Hits have a verified
    // content hash and a trusted local index binding them to source/engine identity. The
    // operator owns the cache and keeps mapped files immutable until all users exit.
    // Adversarial filesystem mutation is explicitly outside this feature's trust model.
    unsafe { Component::deserialize_file(engine, path) }
}

struct Fingerprint(Sha256);

impl Hasher for Fingerprint {
    fn finish(&self) -> u64 {
        let digest = self.0.clone().finalize();
        let mut prefix = [0; 8];
        prefix.copy_from_slice(&digest[..8]);
        u64::from_be_bytes(prefix)
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
}

// The std Hash byte feed is stable in practice but not promised; if it moves, the key changes and
// every entry misses, never matches wrongly, because deserialization re-checks compatibility.
pub(crate) fn compatibility_key(engine: &Engine) -> String {
    let mut fingerprint = Fingerprint(Sha256::new());
    engine
        .precompile_compatibility_hash()
        .hash(&mut fingerprint);
    hex_digest(&fingerprint.0.finalize())
}

pub(crate) fn micros(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

pub(crate) fn stage<T>(
    stage: &'static str,
    bytes: u64,
    work: impl FnOnce() -> wasmtime::Result<T>,
) -> wasmtime::Result<T> {
    let span = tracing::info_span!(
        "provider.load_stage",
        stage,
        bytes,
        elapsed_us = tracing::field::Empty,
        outcome = tracing::field::Empty
    );
    span.in_scope(|| {
        let started = Instant::now();
        let result = work();
        let elapsed_us = micros(started);
        let outcome = if result.is_ok() { "ok" } else { "error" };
        span.record("elapsed_us", elapsed_us);
        span.record("outcome", outcome);
        match &result {
            Ok(_) => tracing::info!(
                stage,
                bytes,
                elapsed_us,
                outcome,
                "provider load stage finished"
            ),
            Err(_) => tracing::error!(
                stage,
                bytes,
                elapsed_us,
                outcome,
                "provider load stage failed"
            ),
        }
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY_COMPONENT: &[u8] = b"\0asm\x0d\0\x01\0";

    fn populate(root: &Path, engine: &Engine) -> (PathBuf, PathBuf) {
        let cache = Cache::new(root.to_owned(), engine, CacheMiss::Compile);
        let source = identify_bytes(EMPTY_COMPONENT);
        cache
            .load(engine, &Compiler::Serial, EMPTY_COMPONENT, &source.sha256)
            .expect("cold load");
        let index = cache
            .root
            .join(&cache.engine_key)
            .join(format!("{}.json", source.sha256));
        let entry: Entry =
            serde_json::from_slice(&fs::read(&index).expect("index")).expect("entry");
        (index, cache.object(&entry))
        // Drop the cache and its mmap'd handles before a test mutates the artifact file, avoiding a
        // mapping race.
    }

    #[test]
    fn warm_load_is_content_addressed_and_does_not_recompile() {
        let directory = tempfile::tempdir().expect("directory");
        let engine = Engine::default();
        let (_, object) = populate(directory.path(), &engine);
        let bytes = fs::read(&object).expect("compiled bytes");
        assert_eq!(
            object.file_stem().expect("stem").to_str(),
            Some(identify_bytes(&bytes).sha256.as_str())
        );
        let cache = Cache::new(directory.path().to_owned(), &engine, CacheMiss::Compile);
        let source = identify_bytes(EMPTY_COMPONENT);
        cache
            .load(&engine, &Compiler::Serial, b"not wasm", &source.sha256)
            .expect("warm mapped load");
        #[cfg(target_os = "linux")]
        assert!(
            fs::read_to_string("/proc/self/maps")
                .expect("maps")
                .contains(object.to_str().expect("path"))
        );
        fs::remove_file(&object).expect("unlink is safe; no mapped inode mutation");
        cache
            .load(&engine, &Compiler::Serial, b"not wasm", &source.sha256)
            .expect("same boot reuses mapping without reopening or hashing");
        drop(cache);
        let error = Cache::new(directory.path().to_owned(), &engine, CacheMiss::Compile)
            .load(&engine, &Compiler::Serial, EMPTY_COMPONENT, &source.sha256)
            .err()
            .expect("next boot verifies again");
        assert!(
            format!("{error:#}").contains("open compiled artifact"),
            "{error:#}"
        );
    }

    #[test]
    fn corrupt_bytes_and_lengths_fail_without_repair() {
        let directory = tempfile::tempdir().expect("directory");
        let engine = Engine::default();
        let (_, object) = populate(directory.path(), &engine);
        let original = fs::read(&object).expect("bytes");
        let mut damaged = original.clone();
        damaged[0] ^= 1;
        fs::write(&object, &damaged).expect("corrupt unmapped object");
        let source = identify_bytes(EMPTY_COMPONENT);
        let cache = Cache::new(directory.path().to_owned(), &engine, CacheMiss::Compile);
        let error = cache
            .load(&engine, &Compiler::Serial, EMPTY_COMPONENT, &source.sha256)
            .err()
            .expect("hash mismatch");
        assert!(
            format!("{error:#}").contains("SHA-256 mismatch"),
            "{error:#}"
        );
        assert_eq!(fs::read(&object).expect("unchanged"), damaged);
        fs::write(&object, &original[..original.len() - 1]).expect("truncate unmapped object");
        let error = cache
            .load(&engine, &Compiler::Serial, EMPTY_COMPONENT, &source.sha256)
            .err()
            .expect("length mismatch");
        assert!(
            format!("{error:#}").contains("length mismatch"),
            "{error:#}"
        );
    }

    #[test]
    fn malformed_index_and_non_directory_cache_are_fatal() {
        let directory = tempfile::tempdir().expect("directory");
        let engine = Engine::default();
        let (index, _) = populate(directory.path(), &engine);
        let source = identify_bytes(EMPTY_COMPONENT);
        fs::write(index, b"not json").expect("damage index");
        let error = Cache::new(directory.path().to_owned(), &engine, CacheMiss::Compile)
            .load(&engine, &Compiler::Serial, EMPTY_COMPONENT, &source.sha256)
            .err()
            .expect("bad index");
        assert!(
            format!("{error:#}").contains("invalid compiled index"),
            "{error:#}"
        );
        let file = directory.path().join("not-a-directory");
        fs::write(&file, b"x").expect("file");
        let error = Cache::new(file, &engine, CacheMiss::Compile)
            .load(&engine, &Compiler::Serial, EMPTY_COMPONENT, &source.sha256)
            .err()
            .expect("I/O failure is not a miss");
        assert!(
            format!("{error:#}").contains("open compiled index"),
            "{error:#}"
        );
    }

    #[test]
    fn engine_changes_get_distinct_indexes_and_incompatible_artifacts_fail() {
        let directory = tempfile::tempdir().expect("directory");
        let engine = Engine::default();
        let (index, _) = populate(directory.path(), &engine);
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let changed = Engine::new(&config).expect("different engine");
        let cache = Cache::new(directory.path().to_owned(), &changed, CacheMiss::Compile);
        assert_ne!(
            cache.engine_key,
            Cache::new(directory.path().to_owned(), &engine, CacheMiss::Compile).engine_key
        );
        let source = identify_bytes(EMPTY_COMPONENT);
        let other_index = cache
            .root
            .join(&cache.engine_key)
            .join(format!("{}.json", source.sha256));
        fs::create_dir_all(other_index.parent().expect("parent")).expect("index directory");
        fs::copy(&index, &other_index).expect("deliberately misfile incompatible artifact");
        let error = cache
            .load(&changed, &Compiler::Serial, EMPTY_COMPONENT, &source.sha256)
            .err()
            .expect("Wasmtime compatibility refusal");
        assert!(format!("{error:#}").contains("fuel"), "{error:#}");
        assert_eq!(
            fs::read(index).expect("original"),
            fs::read(other_index).expect("not repaired")
        );
    }

    #[test]
    fn the_engine_key_is_a_digest_independent_of_the_compile_thread_count() {
        let key = |compiler: &Compiler| {
            let mut config = dekopon_provider_sdk::host::config();
            compiler.configure(&mut config);
            compatibility_key(&Engine::new(&config).expect("engine"))
        };
        let serial = key(&Compiler::Serial);
        let pooled = key(&Compiler::new(NonZeroU32::new(2).expect("two")).expect("pool"));
        assert_eq!(serial, pooled);
        assert_eq!(serial, key(&Compiler::Serial));
        assert_eq!(serial.len(), 64);
    }

    #[test]
    fn prune_keeps_exactly_the_live_set() {
        let directory = tempfile::tempdir().expect("directory");
        let engine = Engine::default();
        let (index, object) = populate(directory.path(), &engine);
        let root = directory.path().join(LAYOUT);
        let cache = Cache::new(directory.path().to_owned(), &engine, CacheMiss::Compile);
        let stale = [
            directory.path().join("v1/abc/source.json"),
            directory.path().join("v1/sha256/old.cwasm"),
            root.join("0000/source.json"),
            root.join(".tmpstray"),
            root.join(&cache.engine_key)
                .join(format!("{}.json", "f".repeat(64))),
            root.join("sha256")
                .join(format!("{}.cwasm", "e".repeat(64))),
        ];
        for path in &stale {
            fs::create_dir_all(path.parent().expect("parent")).expect("directory");
            fs::write(path, b"stale").expect("stale file");
        }
        let source = identify_bytes(EMPTY_COMPONENT);
        let removed = cache.prune([source.sha256.as_str()]).expect("prune");
        assert_eq!(removed.files, stale.len() as u64);
        assert_eq!(removed.bytes, 5 * stale.len() as u64);
        let mut remaining = Vec::new();
        let mut pending = vec![directory.path().to_owned()];
        while let Some(path) = pending.pop() {
            for entry in entries(&path).expect("entries") {
                if entry.is_dir() {
                    pending.push(entry);
                } else {
                    remaining.push(entry);
                }
            }
        }
        remaining.sort();
        let mut expected = vec![index, object];
        expected.sort();
        assert_eq!(remaining, expected);
    }

    #[test]
    fn full_cache_refuses_publication_instead_of_evicting() {
        let directory = tempfile::tempdir().expect("directory");
        let object = directory.path().join("large");
        File::create(&object)
            .expect("sparse artifact")
            .set_len(MAX_CACHE_BYTES)
            .expect("sparse length");
        let error = ensure_capacity(directory.path(), 1).expect_err("full cache");
        assert!(error.to_string().contains("is full"), "{error:#}");
        assert_eq!(
            fs::metadata(object).expect("retained").len(),
            MAX_CACHE_BYTES
        );
    }

    #[test]
    fn publication_never_overwrites_an_existing_inode() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("artifact");
        publish(&path, b"original").expect("publish");
        let error = publish(&path, b"replacement").expect_err("no overwrite or retry");
        assert!(
            error.to_string().contains("publish compiled artifact"),
            "{error:#}"
        );
        assert_eq!(fs::read(path).expect("original retained"), b"original");
    }
}
