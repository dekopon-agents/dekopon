#![cfg(unix)]

#[test]
fn check_provider_set_refuses_an_unusable_registry_ca_bundle() {
    use std::{fs, os::unix::fs::PermissionsExt as _, process::Command};
    let root = tempfile::tempdir().expect("fixtures");
    let config = root.path().join("broker.yaml");
    fs::write(&config, r#"{"apiVersion":"dekopon.dev/brokerd/v1alpha1","socketPath":"broker.sock","providerSet":{"lockPath":"providers.lock.yaml","storePath":"store"}}"#).expect("config");
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).expect("permissions");
    let bundle = root.path().join("ca.pem");
    fs::write(&bundle, "not PEM").expect("bundle");
    let output = Command::new(env!("CARGO_BIN_EXE_dekopon-brokerd"))
        .arg("check")
        .arg(&config)
        .arg("--provider-set")
        .arg(root.path().join("providers.yaml"))
        .arg("--store")
        .arg(root.path().join("check-store"))
        .env("DEKOPON_REGISTRY_CA_BUNDLE", &bundle)
        .output()
        .expect("run check");
    assert_eq!(output.status.code(), Some(1));
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.contains("DEKOPON_REGISTRY_CA_BUNDLE"), "{output}");
    assert!(output.contains(bundle.to_str().expect("path")), "{output}");
}

#[path = "../../src/test_tls.rs"]
mod test_tls;

#[test]
fn materialized_relaxed_check_reports_skipped_stages_as_problems() {
    use std::{fs, os::unix::fs::PermissionsExt as _, process::Command, time::Duration};

    use dekopon_brokerd::CONFIG_API_VERSION;
    use serde_json::{Value, json};

    let tls = test_tls::TestTls::new();
    let (registry, digest) = registry_for_relaxed_checks(&tls);

    let root = tempfile::tempdir().expect("fixtures");
    let config = root.path().join("broker.yaml");
    let provider_set = root.path().join("providers.yaml");
    let bundle = root.path().join("ca.pem");
    let store = root.path().join("store");
    let workflows = root.path().join("absent-workflows");
    let authority = registry.endpoint.trim_start_matches("https://");
    fs::write(
        &config,
        json!({
            "apiVersion": CONFIG_API_VERSION,
            "socketPath": "broker.sock",
            "providerSet": {"lockPath": "providers.lock.yaml", "storePath": "store"},
            "identities": [{
                "uid": dekopon_brokerd::current_uid(),
                "principal": "gateway",
                "actor": {"type": "service", "principal": "gateway"}
            }]
        })
        .to_string(),
    )
    .expect("broker config");
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).expect("config permissions");
    fs::write(
        &provider_set,
        format!("apiVersion: dekopon.dev/provider-set/v1alpha1\nproviders:\n  - source: {authority}/test/provider:1.0.0\n"),
    )
    .expect("provider set");
    fs::write(&bundle, tls.ca_pem).expect("CA bundle");

    let skipped = json!([
        "providers, policy and capabilities skipped under relaxed verification",
        "essential workflows skipped under relaxed verification"
    ]);
    for (flag, status, skipped_key, empty_key) in [
        (Some("--materialized"), 1, "problems", "warnings"),
        (None, 0, "warnings", "problems"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_dekopon-brokerd"))
            .arg("check")
            .arg(&config)
            .arg("--provider-verification=relaxed")
            .arg("--provider-set")
            .arg(&provider_set)
            .arg("--store")
            .arg(&store)
            .arg("--workflows")
            .arg(&workflows)
            .arg("--output=json")
            .args(flag)
            .env("DEKOPON_REGISTRY_CA_BUNDLE", &bundle)
            .output()
            .expect("run check");
        let report: Value = serde_json::from_slice(&output.stdout).expect("JSON report");
        assert_eq!(report["timings"]["probe"]["succeeded"], true, "{report}");
        assert_eq!(output.status.code(), Some(status), "{report}");
        assert_eq!(report[skipped_key], skipped);
        assert_eq!(report[empty_key], json!([]));
    }

    let manifest_request = "GET /v2/test/provider/manifests/1.0.0 HTTP/1.1\r\n";
    let blob_request = format!("HEAD /v2/test/provider/blobs/{digest} HTTP/1.1\r\n");
    for expected in [
        manifest_request,
        &blob_request,
        manifest_request,
        &blob_request,
    ] {
        let request = registry
            .requests
            .recv_timeout(Duration::from_secs(5))
            .expect("registry request");
        assert!(request.starts_with(expected.as_bytes()));
    }
}

fn registry_for_relaxed_checks(tls: &test_tls::TestTls) -> (test_tls::TokenApi, String) {
    use std::fs;

    use dekopon_brokerd::{PROVIDER_ARTIFACT_TYPE, PROVIDER_LAYER_MEDIA_TYPE};
    use serde_json::json;
    use sha2::{Digest as _, Sha256};
    use test_tls::response;

    let component = fs::read(dekopon_test_support::provider_fixture(
        "cli-probe-provider.wasm",
    ))
    .expect("checked cli-probe component");
    let sha256 = |bytes: &[u8]| {
        use std::fmt::Write as _;
        let mut digest = String::from("sha256:");
        for byte in Sha256::digest(bytes) {
            write!(&mut digest, "{byte:02x}").expect("digest encodes");
        }
        digest
    };
    let digest = sha256(&component);
    let size = component.len();
    let manifest_type = "application/vnd.oci.image.manifest.v1+json";
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": manifest_type,
        "artifactType": PROVIDER_ARTIFACT_TYPE,
        "config": {
            "mediaType": "application/vnd.oci.empty.v1+json",
            "digest": sha256(b"{}"),
            "size": 2
        },
        "layers": [{"mediaType": PROVIDER_LAYER_MEDIA_TYPE, "digest": digest, "size": size}]
    });
    let manifest = String::from_utf8(response("200 OK", &manifest.to_string()))
        .expect("UTF-8 fixture response")
        .replacen(
            "Content-Type: application/json",
            &format!("Content-Type: {manifest_type}"),
            1,
        )
        .into_bytes();
    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n")
        .into_bytes();
    let registry =
        test_tls::TokenApi::serving(tls, vec![manifest.clone(), head.clone(), manifest, head]);
    (registry, digest)
}
