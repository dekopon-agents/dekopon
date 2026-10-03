use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, BTreeSet, hash_map::DefaultHasher},
    fs::{self, File},
    hash::{Hash as _, Hasher as _},
    io::{self, Read as _, Write as _},
    path::{Path, PathBuf},
    time::Instant,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use wasmtime::{Engine, component::Component};

use crate::metadata::{hex_digest, identify_bytes};

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

#[derive(Debug)]
pub(crate) enum Lookup {
    Mapped(Component),
    Missing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Publication {
    Hit,
    Compiled,
    Repaired,
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
        fs::remove_file(path)?;
        self.files += 1;
        self.bytes = self.bytes.saturating_add(metadata.len());
        let span = tracing::Span::current();
        span.record("removed_files", self.files);
        span.record("removed_bytes", self.bytes);
        Ok(())
    }
}

pub(crate) struct Cache {
    root: PathBuf,
    engine_key: String,
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

    pub(crate) fn precompile_component(
        &self,
        engine: &Engine,
        wasm: &[u8],
        source_sha256: &str,
    ) -> wasmtime::Result<Publication> {
        match self.read_entry(source_sha256) {
            Ok(Some(entry)) => match stage("verify", entry.bytes, || {
                verify(&self.object(&entry), &entry)
            }) {
                Ok(()) => return Ok(Publication::Hit),
                Err(error) => refuse_unreadable(&error)?,
            },
            Ok(None) => {}
            Err(error) => refuse_unreadable(&error)?,
        }
        let (_, publication) = self.publish_with_outcome(engine, wasm, source_sha256)?;
        Ok(publication)
    }

    pub(crate) fn prune<'a>(
        &self,
        live_sources: impl IntoIterator<Item = &'a str>,
    ) -> wasmtime::Result<Removed> {
        let live_sources = live_sources.into_iter().collect::<BTreeSet<_>>();
        let current = self.root.join(&self.engine_key);
        let objects = self.root.join("sha256");
        let mut removed = Removed::default();
        for path in entries(&self.root)? {
            if path != current && path != objects {
                removed.tree(&path)?;
            }
        }
        let mut referenced = BTreeSet::new();
        for path in entries(&current)? {
            let live = path.extension().is_some_and(|ext| ext == "json")
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| live_sources.contains(stem));
            if live {
                let source = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .ok_or_else(|| wasmtime::Error::msg("compiled index has no source digest"))?;
                let entry = self.read_entry(source)?.ok_or_else(|| {
                    wasmtime::Error::msg(format!("missing live index {}", path.display()))
                })?;
                referenced.insert(self.object(&entry));
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

    pub(crate) fn publish_component(
        &self,
        engine: &Engine,
        wasm: &[u8],
        source_sha256: &str,
    ) -> wasmtime::Result<Component> {
        self.publish_with_outcome(engine, wasm, source_sha256)
            .map(|(component, _)| component)
    }

    fn publish_with_outcome(
        &self,
        engine: &Engine,
        wasm: &[u8],
        source_sha256: &str,
    ) -> wasmtime::Result<(Component, Publication)> {
        let mut repaired = false;
        match self.load(engine, source_sha256) {
            Ok(Lookup::Mapped(component)) => return Ok((component, Publication::Hit)),
            Ok(Lookup::Missing) => {}
            Err(error) => {
                let index = self.index(source_sha256);
                let bytes = read_index(&index)?;
                let mut fault_path = index.clone();
                if let Some(bytes) = bytes
                    && let Ok(entry) = parse_entry(&bytes, &index)
                {
                    let object = self.object(&entry);
                    match File::open(&object) {
                        Ok(_) => {
                            if let Err(fault) = verify(&object, &entry) {
                                refuse_unreadable(&fault)?;
                                remove(&object, "compiled artifact")?;
                                fault_path = object.clone();
                            }
                        }
                        Err(io_error) if io_error.kind() == std::io::ErrorKind::NotFound => {
                            fault_path = object;
                        }
                        Err(io_error) => {
                            return Err(wasmtime::Error::new(io_error)
                                .context(format!("open compiled artifact {}", object.display())));
                        }
                    }
                }
                remove(&index, "compiled index")?;
                tracing::warn!(path = %fault_path.display(), reason = %error, "repairing compiled artifact");
                repaired = true;
            }
        }
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
        let existing = object.try_exists().map_err(|error| {
            wasmtime::Error::msg(format!(
                "stat compiled artifact {}: {error}",
                object.display()
            ))
        })?;
        let mut reusable = existing;
        if existing && let Err(error) = stage("verify", entry.bytes, || verify(&object, &entry)) {
            refuse_unreadable(&error)?;
            remove(&object, "compiled artifact")?;
            tracing::warn!(path = %object.display(), reason = %error, "repairing compiled artifact");
            reusable = false;
            repaired = true;
        }
        let index = self.index(source_sha256);
        stage("publish", entry.bytes, || {
            fs::create_dir_all(self.root.join(&self.engine_key))?;
            if !reusable {
                fs::create_dir_all(self.root.join("sha256"))?;
                ensure_capacity(&self.root.join("sha256"), entry.bytes)?;
                publish(&object, &compiled)?;
            }
            publish(&index, &serde_json::to_vec(&entry)?)
        })?;
        drop(compiled);
        let mut loaded = self.loaded.lock();
        self.map(engine, entry, &mut loaded).map(|component| {
            (
                component,
                if repaired {
                    Publication::Repaired
                } else {
                    Publication::Compiled
                },
            )
        })
    }

    pub(crate) fn load(&self, engine: &Engine, source_sha256: &str) -> wasmtime::Result<Lookup> {
        self.load_inner(engine, source_sha256)
            .map_err(|error| error.context(format!("cwasm cache {}", self.root.display())))
    }

    fn load_inner(&self, engine: &Engine, source_sha256: &str) -> wasmtime::Result<Lookup> {
        let started = Instant::now();
        let mut loaded = self.loaded.lock();
        tracing::Span::current().record("cache_wait_us", micros(started));
        let Some(entry) = self.read_entry(source_sha256)? else {
            tracing::Span::current().record("cache", "miss");
            return Ok(Lookup::Missing);
        };
        tracing::Span::current().record("cache", "hit");
        if let Some(component) = loaded.get(&entry.sha256) {
            tracing::Span::current().record("cache", "reuse");
            record_artifact(&entry);
            return Ok(Lookup::Mapped(component.clone()));
        }
        let object = self.object(&entry);
        stage("verify", entry.bytes, || verify(&object, &entry))?;
        self.map(engine, entry, &mut loaded).map(Lookup::Mapped)
    }

    fn read_entry(&self, source_sha256: &str) -> wasmtime::Result<Option<Entry>> {
        let index = self.index(source_sha256);
        read_index(&index)?
            .map(|bytes| parse_entry(&bytes, &index))
            .transpose()
    }

    fn index(&self, source_sha256: &str) -> PathBuf {
        self.root
            .join(&self.engine_key)
            .join(format!("{source_sha256}.json"))
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

fn read_index(index: &Path) -> wasmtime::Result<Option<Vec<u8>>> {
    let file = match File::open(index) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(wasmtime::Error::new(error)
                .context(format!("open compiled index {}", index.display())));
        }
    };
    let mut bytes = Vec::new();
    file.take(MAX_INDEX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            wasmtime::Error::new(error).context(format!("read compiled index {}", index.display()))
        })?;
    Ok(Some(bytes))
}

fn parse_entry(bytes: &[u8], index: &Path) -> wasmtime::Result<Entry> {
    wasmtime::ensure!(
        bytes.len() as u64 <= MAX_INDEX_BYTES,
        "index {} exceeds {MAX_INDEX_BYTES} bytes",
        index.display()
    );
    let entry: Entry = serde_json::from_slice(bytes).map_err(|error| {
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

fn remove(path: &Path, what: &str) -> wasmtime::Result<()> {
    fs::remove_file(path).map_err(|error| {
        wasmtime::Error::new(error).context(format!("remove {what} {}", path.display()))
    })
}

fn refuse_unreadable(error: &wasmtime::Error) -> wasmtime::Result<()> {
    for cause in error.chain() {
        if let Some(io_error) = cause.downcast_ref::<std::io::Error>()
            && io_error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(wasmtime::Error::msg(format!(
                "cannot repair unreadable compiled artifact: {error:#}"
            )));
        }
    }
    Ok(())
}

fn entries(path: &Path) -> io::Result<Vec<PathBuf>> {
    let listing = match fs::read_dir(path) {
        Ok(listing) => listing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    listing
        .map(|entry| entry.map(|entry| entry.path()))
        .collect()
}

fn record_artifact(entry: &Entry) {
    let span = tracing::Span::current();
    span.record("cwasm_sha256", &entry.sha256);
    span.record("cwasm_bytes", entry.bytes);
}

fn verify(path: &Path, entry: &Entry) -> wasmtime::Result<()> {
    let mut file = File::open(path).map_err(|error| {
        wasmtime::Error::new(error).context(format!("open compiled artifact {}", path.display()))
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
            "compiled cache {} is full (maximum {MAX_CACHE_OBJECTS} objects / {MAX_CACHE_BYTES} bytes); delete the pod; `provider precompile` repairs it, or set compileOnLoad: true",
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
    // Never overwrite a mapped inode; publishers hold the store lock.
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
    // SAFETY: publishers produce cache objects with Engine::precompile_component. Hits have a
    // verified hash and a trusted local index binding them to source/engine identity. The
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
        let cache = Cache::new(root.to_owned(), engine);
        let source = identify_bytes(EMPTY_COMPONENT);
        cache
            .publish_component(engine, EMPTY_COMPONENT, &source.sha256)
            .expect("cold publish");
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
        let cache = Cache::new(directory.path().to_owned(), &engine);
        let source = identify_bytes(EMPTY_COMPONENT);
        cache
            .publish_component(&engine, b"not wasm", &source.sha256)
            .expect("publisher does not recompile a warm entry");
        cache
            .load(&engine, &source.sha256)
            .expect("warm mapped load");
        #[cfg(target_os = "linux")]
        assert!(
            fs::read_to_string("/proc/self/maps")
                .expect("maps")
                .contains(object.to_str().expect("path"))
        );
        fs::remove_file(&object).expect("unlink is safe; no mapped inode mutation");
        cache
            .load(&engine, &source.sha256)
            .expect("same boot reuses mapping without reopening or hashing");
        drop(cache);
        let error = Cache::new(directory.path().to_owned(), &engine)
            .load(&engine, &source.sha256)
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
            .load(&engine, &source.sha256)
            .expect_err("hash mismatch");
        assert!(
            format!("{error:#}").contains("SHA-256 mismatch"),
            "{error:#}"
        );
        assert_eq!(fs::read(&object).expect("unchanged"), damaged);
        fs::write(&object, &original[..original.len() - 1]).expect("truncate unmapped object");
        let error = cache
            .load(&engine, &source.sha256)
            .expect_err("length mismatch");
        assert!(
            format!("{error:#}").contains("length mismatch"),
            "{error:#}"
        );
    }

    #[test]
    fn missing_index_reuses_verified_object_and_republishes_index() {
        let directory = tempfile::tempdir().expect("directory");
        let engine = Engine::default();
        let (index, object) = populate(directory.path(), &engine);
        fs::remove_file(&index).expect("remove index");
        let source = identify_bytes(EMPTY_COMPONENT);
        Cache::new(directory.path().to_owned(), &engine)
            .publish_component(&engine, EMPTY_COMPONENT, &source.sha256)
            .expect("publish index for existing object");
        assert!(index.is_file());
        let entry: Entry =
            serde_json::from_slice(&fs::read(&index).expect("index")).expect("entry");
        assert_eq!(
            object,
            Cache::new(directory.path().to_owned(), &engine).object(&entry)
        );
    }

    #[test]
    fn missing_index_repairs_mismatched_object() {
        let directory = tempfile::tempdir().expect("directory");
        let engine = Engine::default();
        let (index, object) = populate(directory.path(), &engine);
        fs::remove_file(&index).expect("remove index");
        let mut damaged = fs::read(&object).expect("compiled bytes");
        damaged[0] ^= 1;
        fs::write(&object, &damaged).expect("corrupt unmapped object");
        let source = identify_bytes(EMPTY_COMPONENT);
        Cache::new(directory.path().to_owned(), &engine)
            .publish_component(&engine, EMPTY_COMPONENT, &source.sha256)
            .expect("replace mismatched object");
        assert!(index.exists());
        assert_ne!(fs::read(&object).expect("replaced object"), damaged);
    }

    #[test]
    fn publisher_repairs_a_faulty_index_and_dangling_object() {
        let directory = tempfile::tempdir().expect("directory");
        let engine = Engine::default();
        let (index, object) = populate(directory.path(), &engine);
        fs::remove_file(&object).expect("simulate missing object");
        let source = identify_bytes(EMPTY_COMPONENT);
        Cache::new(directory.path().to_owned(), &engine)
            .publish_component(&engine, EMPTY_COMPONENT, &source.sha256)
            .expect("repair missing object");
        assert!(object.exists());
        fs::write(&index, b"not json").expect("damage index");
        Cache::new(directory.path().to_owned(), &engine)
            .publish_component(&engine, EMPTY_COMPONENT, &source.sha256)
            .expect("repair malformed index");
        assert!(index.exists());
        assert!(object.exists());
    }

    #[test]
    fn malformed_index_and_non_directory_cache_are_fatal() {
        let directory = tempfile::tempdir().expect("directory");
        let engine = Engine::default();
        let (index, _) = populate(directory.path(), &engine);
        let source = identify_bytes(EMPTY_COMPONENT);
        fs::write(index, b"not json").expect("damage index");
        let error = Cache::new(directory.path().to_owned(), &engine)
            .load(&engine, &source.sha256)
            .expect_err("bad index");
        assert!(
            format!("{error:#}").contains("invalid compiled index"),
            "{error:#}"
        );
        let file = directory.path().join("not-a-directory");
        fs::write(&file, b"x").expect("file");
        let error = Cache::new(file, &engine)
            .load(&engine, &source.sha256)
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
            .load(&changed, &source.sha256)
            .expect_err("Wasmtime compatibility refusal");
        assert!(format!("{error:#}").contains("fuel"), "{error:#}");
        assert_eq!(
            fs::read(index).expect("original"),
            fs::read(other_index).expect("not repaired")
        );
    }

    #[test]
    fn precompile_after_an_engine_change_keeps_the_shared_object_and_prunes_the_old_generation() {
        let directory = tempfile::tempdir().expect("directory");
        let engine = Engine::default();
        let (index, object) = populate(directory.path(), &engine);
        let root = directory.path().join("v1");
        let old = root.join("old-engine");
        fs::create_dir(&old).expect("old generation");
        fs::copy(&index, old.join(index.file_name().expect("index name")))
            .expect("shared object index");
        let orphan = root.join("sha256/orphan.cwasm");
        fs::write(&orphan, b"orphan").expect("orphan object");
        let temporary = root.join("sha256/.interrupted");
        fs::write(&temporary, b"temporary").expect("interrupted publish");
        let source = identify_bytes(EMPTY_COMPONENT);
        let cache = Cache::new(directory.path().to_owned(), &engine);
        let removed = cache
            .prune([source.sha256.as_str()])
            .expect("prune old generation");
        assert_eq!(removed.files, 3);
        assert_eq!(
            removed.bytes,
            fs::metadata(&index).expect("current index").len() + 6 + 9
        );
        assert!(!old.exists());
        assert!(!orphan.exists());
        assert!(!temporary.exists());
        assert!(object.exists());
        assert!(matches!(
            cache.load(&engine, &source.sha256),
            Ok(Lookup::Mapped(_))
        ));
    }

    #[test]
    fn precompile_replaces_a_short_object_left_at_its_own_address() {
        let directory = tempfile::tempdir().expect("directory");
        let engine = Engine::default();
        let (index, object) = populate(directory.path(), &engine);
        let bytes = fs::read(&object).expect("compiled bytes");
        fs::write(&object, &bytes[..bytes.len() / 2]).expect("power cut short object");
        fs::remove_file(&index).expect("index not published");
        let source = identify_bytes(EMPTY_COMPONENT);
        let cache = Cache::new(directory.path().to_owned(), &engine);
        assert_eq!(
            cache
                .precompile_component(&engine, EMPTY_COMPONENT, &source.sha256)
                .expect("repair"),
            Publication::Repaired
        );
        assert!(matches!(
            Cache::new(directory.path().to_owned(), &engine).load(&engine, &source.sha256),
            Ok(Lookup::Mapped(_))
        ));
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
