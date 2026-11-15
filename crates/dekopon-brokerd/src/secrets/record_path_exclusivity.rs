use std::os::unix::fs::PermissionsExt as _;

use super::{SecretMapError, load_with_bytes};

const REFRESH: &str = "secrev-t2-FIXTURE-refresh-token-0001";

fn uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn write(path: &std::path::Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write fixture");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod fixture");
}

#[tokio::test]
async fn an_oauth2_record_path_cannot_supply_another_entry() {
    let directory = tempfile::tempdir().expect("temporary directory");
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
        .expect("private directory");
    let record = directory.path().join("record.json");
    write(
        &record,
        serde_json::json!({
            "version": 2,
            "state": "live",
            "access": "secrev-t2-FIXTURE-access-token-0001",
            "refresh": REFRESH,
            "expiresAt": 4_102_444_800_u64,
            "lastRefresh": 1,
        })
        .to_string()
        .as_bytes(),
    );
    let map = directory.path().join("map.yaml");
    let document = format!(
        "apiVersion: dekopon.dev/secret-map/v1alpha1\n\
         mapRevision: secrev-t2\n\
         secrets:\n\
         \x20 - drn: drn:secret:idp/access\n\
         \x20   destinations: [api.example.test]\n\
         \x20   source: {{ kind: oauth2Refresh, recordPath: {record}, tokenEndpoint: \"https://idp.example.test/oauth/token\", clientId: client }}\n\
         \x20   projection: {{ format: utf8 }}\n\
         \x20   bindings:\n\
         \x20     - id: access\n\
         \x20       capabilities: [http-probe.fetch]\n\
         \x20       sink: httpBearer\n\
         \x20       allowedHosts: [api.example.test]\n\
         \x20       allowedMethods: [GET]\n\
         \x20       allowedPaths: [{{ match: exact, path: /v1/me }}]\n\
         \x20 - drn: drn:secret:idp/refresh\n\
         \x20   destinations: [api.example.test]\n\
         \x20   source: {{ kind: secureFile, path: {record} }}\n\
         \x20   projection: {{ format: json, pointer: /refresh }}\n\
         \x20   bindings:\n\
         \x20     - id: refresh\n\
         \x20       capabilities: [http-probe.post]\n\
         \x20       sink: httpBearer\n\
         \x20       allowedHosts: [api.example.test]\n\
         \x20       allowedMethods: [POST]\n\
         \x20       allowedPaths: [{{ match: exact, path: /v1/echo }}]\n",
        record = record.display()
    );
    write(&map, document.as_bytes());

    let error = match load_with_bytes(&map, uid()).await {
        Err(error) => error,
        Ok(_) => panic!("the record supplied another entry"),
    };
    let SecretMapError::Validation { problems } = error else {
        panic!("unexpected error: {error:?}");
    };
    assert!(
        problems.iter().any(|problem| {
            problem.contains(&record.display().to_string())
                && problem.contains("drn:secret:idp/access")
                && problem.contains("drn:secret:idp/refresh")
        }),
        "{problems:?}"
    );
}

#[tokio::test]
async fn all_record_path_conflicts_name_their_entries_and_paths() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let map = directory.path().join("map.yaml");
    let first = directory.path().join("first.json");
    let second = directory.path().join("second.json");
    let third = directory.path().join("third.json");
    let entry = |name: &str, source: &str| {
        format!(
            "  - drn: drn:secret:test/{name}\n    destinations: [api.example.test]\n    source: {source}\n    bindings:\n      - id: {name}\n        capabilities: [http-probe.fetch]\n        sink: httpBearer\n        allowedHosts: [api.example.test]\n        allowedMethods: [GET]\n        allowedPaths: [{{ match: exact, path: /v1/me }}]\n"
        )
    };
    let oauth2 = |path: &std::path::Path| {
        format!(
            "{{ kind: oauth2Refresh, recordPath: {}, tokenEndpoint: https://idp.example.test/token, clientId: client }}",
            path.display()
        )
    };
    let document = format!(
        "apiVersion: dekopon.dev/secret-map/v1alpha1\nmapRevision: test\nsecrets:\n{}{}{}{}{}{}",
        entry("first-owner", &oauth2(&first)),
        entry(
            "first-reader",
            &format!("{{ kind: secureFile, path: {} }}", first.display())
        ),
        entry("second-owner", &oauth2(&second)),
        entry("second-owner-too", &oauth2(&second)),
        entry("third-owner", &oauth2(&third)),
        entry(
            "third-bootstrap",
            &format!(
                "{{ kind: onePasswordConnect, endpoint: https://connect.example.test, tokenFile: {}, vault: vault, item: item, field: password }}",
                third.display()
            )
        ),
    );
    write(&map, document.as_bytes());

    let error = match load_with_bytes(&map, uid()).await {
        Err(error) => error,
        Ok(_) => panic!("both shared record paths loaded"),
    };
    let SecretMapError::Validation { problems } = error else {
        panic!("unexpected error: {error:?}");
    };
    for (path, left, right) in [
        (&first, "first-owner", "first-reader"),
        (&second, "second-owner", "second-owner-too"),
        (&third, "third-owner", "third-bootstrap"),
    ] {
        let mut entries = [
            format!("drn:secret:test/{left}"),
            format!("drn:secret:test/{right}"),
        ];
        entries.sort();
        let expected = format!(
            "secret path {} is used by secret-map entries {}",
            path.display(),
            entries.join(", ")
        );
        assert!(
            problems.iter().any(|problem| problem == &expected),
            "{problems:?}"
        );
    }
}
