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
