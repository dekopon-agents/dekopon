//! Real-binary startup, diagnostics, and JSON tracing for the operator's mmap toggle.

#![allow(clippy::unwrap_used)]

use dekopon_test_support::provider_fixture;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    fs::{self, File},
    io::Read as _,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().expect("inspect child").is_none() {
            self.0.kill().expect("stop test broker");
        }
        self.0.wait().expect("reap test broker");
    }
}

fn private_file(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).expect("write fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("private file");
}

fn start(config: &Path, directory: &Path, label: &str) -> (Process, PathBuf, PathBuf) {
    let stdout = directory.join(format!("{label}.stdout"));
    let stderr = directory.join(format!("{label}.stderr"));
    let child = Command::new(env!("CARGO_BIN_EXE_dekopon-brokerd"))
        .arg("--config")
        .arg(config)
        .env("RUST_LOG", "info")
        .stdin(Stdio::null())
        .stdout(File::create(&stdout).expect("stdout"))
        .stderr(File::create(&stderr).expect("stderr"))
        .spawn()
        .expect("spawn broker");
    (Process(child), stdout, stderr)
}

fn ready(process: &mut Process, socket: &Path, stderr: &Path) {
    let until = Instant::now() + Duration::from_secs(30);
    while !socket.exists() {
        assert!(
            process.0.try_wait().expect("status").is_none(),
            "startup failed: {}",
            fs::read_to_string(stderr).expect("stderr")
        );
        assert!(Instant::now() < until, "startup timeout");
        thread::sleep(Duration::from_millis(20));
    }
}

fn failed(process: &mut Process, stdout: &Path) -> String {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = process.0.try_wait().expect("status") {
            assert!(!status.success(), "a broken cache must fail startup");
            return fs::read_to_string(stdout).expect("JSON startup diagnostics");
        }
        assert!(Instant::now() < until, "failure must not hang or serve");
        thread::sleep(Duration::from_millis(20));
    }
}

fn stages(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .expect("stdout")
        .lines()
        .filter_map(|line| {
            let value: Value = serde_json::from_str(line).expect("JSON log");
            (value["message"] == "provider load stage finished").then_some(value)
        })
        .collect()
}

#[test]
fn broker_reports_cold_warm_corrupt_and_bypassed_compiled_artifacts() {
    let directory = tempfile::tempdir().expect("directory");
    let root = directory.path();
    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).expect("private root");
    let wasm = fs::read(provider_fixture("cli-probe-provider.wasm")).expect("fixture");
    let digest = Sha256::digest(&wasm)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let store = root.join("store");
    for path in [&store, &store.join("blobs"), &store.join("blobs/sha256")] {
        fs::create_dir(path).expect("directory");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("private store");
    }
    private_file(&store.join(format!("blobs/sha256/{digest}.wasm")), &wasm);
    let lock = root.join("providers.lock.json");
    private_file(
        &lock,
        &serde_json::to_vec(&json!({
            "apiVersion": "dekopon.dev/provider-lock/v1alpha1",
            "providers": [{
                "source": "ghcr.io/example/cli-probe:1.0.0", "resolvedVersion": "1.0.0",
                "manifestDigest": format!("sha256:{}", "1".repeat(64)),
                "componentDigest": format!("sha256:{digest}"), "componentBytes": wasm.len(),
                "providerId": "cli-probe"
            }]
        }))
        .expect("lock"),
    );
    let config = root.join("broker.json");
    let socket = root.join("broker.sock");
    let mut document = json!({
        "apiVersion": dekopon_brokerd::CONFIG_API_VERSION,
        "socketPath": socket, "brokerPrincipal": "broker-test", "policyRevision": "test",
        "providerSet": { "lockPath": lock, "storePath": store },
        "identities": [{ "uid": dekopon_brokerd::current_uid(), "principal": "caller",
            "actor": { "type": "agent", "agent": "test" } }]
    });
    private_file(&config, &serde_json::to_vec(&document).expect("config"));
    for (label, expected) in [
        (
            "cold",
            vec!["compile", "artifact_hash", "publish", "deserialize"],
        ),
        ("warm", vec!["verify", "deserialize"]),
    ] {
        let (mut process, stdout, stderr) = start(&config, root, label);
        ready(&mut process, &socket, &stderr);
        drop(process); // All mappings are gone before any corruption test.
        fs::remove_file(&socket).expect("remove killed broker's socket");
        let records = stages(&stdout);
        assert_eq!(
            records
                .iter()
                .map(|v| v["stage"].as_str().expect("stage"))
                .collect::<Vec<_>>(),
            expected,
            "stdout: {}",
            fs::read_to_string(&stdout).expect("stdout")
        );
        for record in records {
            assert!(record["elapsed_us"].as_u64().is_some(), "{record}");
            let spans = record["spans"]
                .as_array()
                .expect("JSON includes span ancestry");
            assert!(
                spans.iter().any(|s| s["name"] == "provider.registry_load"),
                "{record}"
            );
            assert!(
                spans.iter().any(|s| s["name"] == "provider.compile"),
                "{record}"
            );
        }
    }
    let object = fs::read_dir(store.join("cwasm/v1/sha256"))
        .expect("objects")
        .next()
        .expect("object")
        .expect("entry")
        .path();
    let mut bytes = Vec::new();
    File::open(&object)
        .expect("artifact")
        .read_to_end(&mut bytes)
        .expect("read artifact");
    bytes[0] ^= 1;
    fs::write(object, bytes).expect("corrupt only after processes exit");
    let (mut process, stdout, _) = start(&config, root, "corrupt");
    let error = failed(&mut process, &stdout);
    assert!(error.contains("SHA-256 mismatch"), "{error}");
    assert!(error.contains("compileOnLoad: true"), "{error}");
    assert!(!socket.exists());
    drop(process);

    document["compileOnLoad"] = json!(true);
    private_file(&config, &serde_json::to_vec(&document).expect("config"));
    let (mut process, stdout, stderr) = start(&config, root, "bypass");
    ready(&mut process, &socket, &stderr);
    drop(process);
    assert_eq!(
        stages(&stdout)
            .iter()
            .map(|v| v["stage"].as_str().expect("stage"))
            .collect::<Vec<_>>(),
        ["compile"]
    );
}
