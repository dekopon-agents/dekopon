//! Trusted, immutable, file-backed compiled components. No retry, repair, or fallback.
//!
//! Only a missing index is a cache miss. Every other failure stops startup. The operator
//! owns this directory and must never modify/truncate a mapped artifact in place.

use std::{
    collections::{BTreeMap, hash_map::DefaultHasher},
    fs::{self, File},
    hash::{Hash as _, Hasher as _},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    sync::Mutex,
    time::Instant,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use wasmtime::{Engine, component::Component};

use crate::metadata::{hex_digest, identify_bytes};

const MAX_INDEX_BYTES: u64 = 4096;
// Compiled code can be larger than the 64 MiB source ceiling. Refuse, never allocate from
// an unchecked on-disk length. This is an artifact limit, not a resident-memory promise.
const MAX_CWASM_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_CACHE_OBJECTS: usize = 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    sha256: String,
    bytes: u64,
}

pub(crate) struct Cache {
    root: PathBuf,
    engine_key: String,
    // Serializes cold compilation/publication and deduplicates verification within one registry
    // boot. Component clones retain the same mapping, not another copy of the native code.
    loaded: Mutex<BTreeMap<String, Component>>,
}

impl Cache {
    pub(crate) fn new(root: PathBuf, engine: &Engine) -> Self {
        Self {
            root: root.join("v1"),
            engine_key: compatibility_key(engine),
            loaded: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) fn load(
        &self,
        engine: &Engine,
        wasm: &[u8],
        source_sha256: &str,
    ) -> wasmtime::Result<Component> {
        self.load_inner(engine, wasm, source_sha256)
            .map_err(|error| error.context(format!("cwasm cache {}", self.root.display())))
    }

    fn load_inner(
        &self,
        engine: &Engine,
        wasm: &[u8],
        source_sha256: &str,
    ) -> wasmtime::Result<Component> {
        let started = Instant::now();
        let mut loaded = self.loaded.lock().map_err(|error| {
            wasmtime::Error::msg(format!("compiled component loader lock poisoned: {error}"))
        })?;
        tracing::Span::current().record("cache_wait_us", micros(started));
        let index = self
            .root
            .join(&self.engine_key)
            .join(format!("{source_sha256}.json"));
        let entry = match File::open(&index) {
            Ok(file) => {
                tracing::Span::current().record("cache", "hit");
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
                entry
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::Span::current().record("cache", "miss");
                let compiled = stage("compile", wasm.len() as u64, || {
                    engine.precompile_component(wasm)
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
                    publish(&index, &serde_json::to_vec(&entry)?)
                })?;
                drop(compiled);
                // These exact bytes were just hashed and published by us. There is no second
                // verification pass on a cold miss. Warm boots stream-verify below.
                return self.map(engine, entry, &mut loaded);
            }
            Err(error) => {
                return Err(wasmtime::Error::msg(format!(
                    "open compiled index {}: {error}",
                    index.display()
                )));
            }
        };
        if let Some(component) = loaded.get(&entry.sha256) {
            tracing::Span::current().record("cache", "reuse");
            record_artifact(&entry);
            return Ok(component.clone());
        }
        let object = self.object(&entry);
        stage("verify", entry.bytes, || verify(&object, &entry))?;
        self.map(engine, entry, &mut loaded)
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
    // Never overwrite an inode another broker may have mapped. Concurrent publishers fail
    // visibly; there is no lock protocol, retry loop, or recovery transaction.
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

pub(crate) fn compatibility_key(engine: &Engine) -> String {
    let mut fingerprint = DefaultHasher::new();
    engine
        .precompile_compatibility_hash()
        .hash(&mut fingerprint);
    format!("{:016x}", fingerprint.finish())
}

pub(crate) fn micros(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// Each stage keeps its own span and a completion event: both OTLP traces and JSON logs can
/// answer which part was expensive, including a failed attempt. No per-function compiler hook
/// is available here; the compilation unit is one provider component.
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
            // The caller reports the cause on the component load, not at every stage boundary.
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
        let cache = Cache::new(root.to_owned(), engine);
        let source = identify_bytes(EMPTY_COMPONENT);
        cache
            .load(engine, EMPTY_COMPONENT, &source.sha256)
            .expect("cold load");
        let index = cache
            .root
            .join(&cache.engine_key)
            .join(format!("{}.json", source.sha256));
        let entry: Entry =
            serde_json::from_slice(&fs::read(&index).expect("index")).expect("entry");
        (index, cache.object(&entry))
        // Cache and all mappings drop before a test mutates any artifact.
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
        let cache = Cache::new(directory.path().to_owned(), &engine);
        let source = identify_bytes(EMPTY_COMPONENT);
        // The caller normally supplies verified Wasm. An invalid buffer here proves the hit
        // does not call the compiler, independently of wall-clock timing.
        cache
            .load(&engine, b"not wasm", &source.sha256)
            .expect("warm mapped load");
        #[cfg(target_os = "linux")]
        assert!(
            fs::read_to_string("/proc/self/maps")
                .expect("maps")
                .contains(object.to_str().expect("path"))
        );
        fs::remove_file(&object).expect("unlink is safe; no mapped inode mutation");
        cache
            .load(&engine, b"not wasm", &source.sha256)
            .expect("same boot reuses mapping without reopening or hashing");
        drop(cache);
        let error = Cache::new(directory.path().to_owned(), &engine)
            .load(&engine, EMPTY_COMPONENT, &source.sha256)
            .expect_err("next boot verifies again");
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
        let cache = Cache::new(directory.path().to_owned(), &engine);
        let error = cache
            .load(&engine, EMPTY_COMPONENT, &source.sha256)
            .expect_err("hash mismatch");
        assert!(
            format!("{error:#}").contains("SHA-256 mismatch"),
            "{error:#}"
        );
        assert_eq!(fs::read(&object).expect("unchanged"), damaged);
        fs::write(&object, &original[..original.len() - 1]).expect("truncate unmapped object");
        let error = cache
            .load(&engine, EMPTY_COMPONENT, &source.sha256)
            .expect_err("length mismatch");
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
        let error = Cache::new(directory.path().to_owned(), &engine)
            .load(&engine, EMPTY_COMPONENT, &source.sha256)
            .expect_err("bad index");
        assert!(
            format!("{error:#}").contains("invalid compiled index"),
            "{error:#}"
        );
        let file = directory.path().join("not-a-directory");
        fs::write(&file, b"x").expect("file");
        let error = Cache::new(file, &engine)
            .load(&engine, EMPTY_COMPONENT, &source.sha256)
            .expect_err("I/O failure is not a miss");
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
        let cache = Cache::new(directory.path().to_owned(), &changed);
        assert_ne!(
            cache.engine_key,
            Cache::new(directory.path().to_owned(), &engine).engine_key
        );
        let source = identify_bytes(EMPTY_COMPONENT);
        let other_index = cache
            .root
            .join(&cache.engine_key)
            .join(format!("{}.json", source.sha256));
        fs::create_dir_all(other_index.parent().expect("parent")).expect("index directory");
        fs::copy(&index, &other_index).expect("deliberately misfile incompatible artifact");
        let error = cache
            .load(&changed, EMPTY_COMPONENT, &source.sha256)
            .expect_err("Wasmtime compatibility refusal");
        assert!(format!("{error:#}").contains("fuel"), "{error:#}");
        assert_eq!(
            fs::read(index).expect("original"),
            fs::read(other_index).expect("not repaired")
        );
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
