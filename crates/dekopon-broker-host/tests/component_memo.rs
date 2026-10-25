#![allow(clippy::unwrap_used, clippy::disallowed_methods)]

#[path = "../src/test_component_memo.rs"]
mod implementation;

use std::{
    collections::hash_map::DefaultHasher,
    fmt::Write as _,
    hash::{Hash as _, Hasher as _},
    sync::{Arc, Barrier},
};

use dekopon_broker_host::{
    BrokerHostError, BrokerHostLimits, BrokerProviderRegistry, enable_test_component_memoization,
    host,
};
use dekopon_test_support::{CaptureLayer, Record, provider_fixture};
use sha2::{Digest as _, Sha256};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};
use wasmtime::Engine;

fn engine_key(engine: &Engine) -> String {
    let mut hash = DefaultHasher::new();
    engine.precompile_compatibility_hash().hash(&mut hash);
    format!("{:016x}", hash.finish())
}

fn compile(engine: &Engine, bytes: &[u8]) -> wasmtime::Result<wasmtime::component::Component> {
    let mut sha256 = String::new();
    for byte in Sha256::digest(bytes) {
        write!(&mut sha256, "{byte:02x}").unwrap();
    }
    implementation::compile(engine, &engine_key(engine), &sha256, bytes)
}

fn assert_key_separation_and_concurrent_counts() {
    let handle = implementation::enable_test_component_memoization();
    let bytes = Arc::new(std::fs::read(provider_fixture("cli-probe-provider.wasm")).unwrap());
    let barrier = Arc::new(Barrier::new(4));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let bytes = Arc::clone(&bytes);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let engine = host::engine(host::config()).unwrap();
                barrier.wait();
                compile(&engine, &bytes).unwrap();
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(handle.compile_count(), 1);
    assert_eq!(handle.hit_count(), 3);
    assert_eq!(handle.bypass_compile_count(), 0);

    let engine = host::engine(host::config()).unwrap();
    let different_bytes = std::fs::read(provider_fixture("http-probe-provider.wasm")).unwrap();
    compile(&engine, &different_bytes).unwrap();
    assert_eq!(handle.compile_count(), 2);
    let mut changed_config = host::config();
    changed_config.cranelift_opt_level(wasmtime::OptLevel::None);
    let changed_engine = host::engine(changed_config).unwrap();
    assert_ne!(engine_key(&engine), engine_key(&changed_engine));
    compile(&changed_engine, &bytes).unwrap();
    assert_eq!(handle.compile_count(), 3);
    assert_eq!(handle.hit_count(), 3);
    for _ in 0..2 {
        assert!(compile(&engine, b"invalid component").is_err());
    }
    assert_eq!(handle.compile_count(), 5);
    assert_eq!(handle.hit_count(), 3);
}

#[tokio::test]
async fn opted_in_loads_reuse_only_matching_components() {
    let capture = CaptureLayer::workspace();
    tracing_subscriber::registry().with(capture.clone()).init();
    let handle = enable_test_component_memoization();
    let path = provider_fixture("cli-probe-provider.wasm");
    BrokerProviderRegistry::load([path.clone()], BrokerHostLimits::default())
        .await
        .unwrap();
    capture.clear();
    BrokerProviderRegistry::load([path], BrokerHostLimits::default())
        .await
        .unwrap();
    assert_eq!(handle.compile_count(), 1);
    assert_eq!(handle.hit_count(), 1);
    let records = capture.records();
    let compiles: Vec<_> = records
        .iter()
        .filter(|record| {
            matches!(
                record,
                Record::Span {
                    name: "provider.compile",
                    ..
                }
            )
        })
        .filter(|record| record.field("cache").is_some())
        .collect();
    assert_eq!(compiles.len(), 1);
    assert_eq!(compiles[0].field("cache"), Some("bypass"));
    let stages: Vec<_> = records
        .iter()
        .filter(|record| {
            matches!(
                record,
                Record::Span {
                    name: "provider.load_stage",
                    ..
                }
            )
        })
        .filter(|record| record.field("stage").is_some())
        .collect();
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0].field("stage"), Some("compile"));
    let invalid = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(invalid.path(), b"invalid component").unwrap();
    for _ in 0..2 {
        assert!(matches!(
            BrokerProviderRegistry::load([invalid.path()], BrokerHostLimits::default()).await,
            Err(BrokerHostError::Compile { .. })
        ));
    }
    assert_eq!(handle.compile_count(), 3);
    assert_eq!(handle.hit_count(), 1);
    assert_key_separation_and_concurrent_counts();
}
