use std::{io::Read as _, os::unix::fs::PermissionsExt as _, path::Path, sync::Arc};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use dekopon_broker::{
    Attestation, AttestorGrant, AuditSink, AuthenticatedContext, Broker, BrokerLimits,
    CapabilityRoute, ConstraintCatalog, ConstraintSet, IdentityDirectory, InMemoryAuditLog,
    PolicyEngine, PolicyWorld,
};
use dekopon_broker_host::{
    BrokerHostLimits, BrokerProviderRegistry, CommandRunOutcome, Streams, asset::AssetInputs,
};
use dekopon_broker_protocol::InvocationRequest;
use dekopon_capability::{EffectKind, ExecutionConstraints, HttpConstraints, InvocationOutcome};
use dekopon_core::{Actor, AttestedSubject, FileHygieneError, FileTier, RiskLevel};
use dekopon_http_host::{Response, test_transport::Transport};
use dekopon_test_support::{CaptureLayer, LoopbackServer};
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::layer::SubscriberExt as _;

use super::{SecretMapError, SignedJwtConfigError, load, tests::write};
use crate::{
    github_app::GithubAppKey,
    token_issuer::{IssueError, SelfSignedIssuer, SignedJwtConfig, thumbprint},
};

const KEY: &[u8] = include_bytes!("../../tests/fixture/github-app-test-only.pem");
const HELPER_BYTES: &[u8] = b"login helper fixture";
const RUNNER: &str = "runner.example.test";
const ISSUER: &str = "https://issuer.example.test";
const SECOND_ISSUER: &str = "https://issuer.example.test/alternate";
const RUNNER_BEARER: &str = "fixture-runner-bearer";
const PERSONA: &str = "drn:secret:login/alpha";
const SECOND_PERSONA: &str = "drn:secret:login/beta";
const RUNNER_DRN: &str = "drn:secret:login/runner";
const SESSION: &str = "fixture-session";
const UNREACHABLE_JWKS: &str = "http://127.0.0.1:1/jwks";
const OK: &str =
    r#"{"outcome":"executed","stdout":"ok","stderr":"","exitCode":0,"truncated":false}"#;

fn helper_path(directory: &Path) -> String {
    directory.join("login-fixture").display().to_string()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn published() -> (String, Value) {
    let key = GithubAppKey::from_pem(KEY).expect("fixture key");
    let components = key.components();
    let n = URL_SAFE_NO_PAD.encode(&components.n);
    let e = URL_SAFE_NO_PAD.encode(&components.e);
    let kid = thumbprint(&n, &e);
    let jwks = json!({"keys": [{"kty": "RSA", "kid": kid, "n": n, "e": e, "use": "sig"}]});
    (kid, jwks)
}

fn http_json(body: &Value) -> Vec<u8> {
    let body = body.to_string();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

async fn secret_map(directory: &Path, ttl: u32, aud: &str, mode: u32) -> std::path::PathBuf {
    secret_map_with(directory, ttl, aud, mode, UNREACHABLE_JWKS).await
}

async fn secret_map_with(
    directory: &Path,
    ttl: u32,
    aud: &str,
    mode: u32,
    jwks_uri: &str,
) -> std::path::PathBuf {
    let helper = directory.join("login-fixture");
    write(&helper, HELPER_BYTES).await;
    let key = directory.join("signing.pem");
    write(&key, KEY).await;
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(mode)).unwrap();
    let runner = directory.join("runner");
    write(&runner, RUNNER_BEARER.as_bytes()).await;
    let persona = |drn, id, iss, aud| {
        format!(
            r#"  - drn: {drn}
    destinations: [{RUNNER}]
    source:
      kind: signedJwt
      privateKey: {key}
      jwksUri: "{jwks_uri}"
      helper: {{ path: {helper}, sha256: {helper_sha} }}
      iss: "{iss}"
      aud: "{aud}"
      ttl: {ttl}
      runnerSecret: {RUNNER_DRN}
      claims: {{ sub: user-fixture, org: org-fixture, role: reviewer-fixture, email: email-fixture@example.test }}
    bindings:
      - id: {id}
        capabilities: [vm.exec]
        sink: vmExecStdin
        allowedHosts: [{RUNNER}]
        allowedMethods: [POST]
        allowedPaths: [{{ match: exact, path: /prefix/v1/sessions/{SESSION}/exec }}]
"#,
            key = key.display(),
            helper = helper.display(),
            helper_sha = sha256_hex(HELPER_BYTES),
        )
    };
    let document = format!(
        r#"apiVersion: dekopon.dev/secret-map/v1alpha1
mapRevision: signed-jwt-fixture
secrets:
  - drn: {RUNNER_DRN}
    destinations: [{RUNNER}]
    source: {{ kind: secureFile, path: {runner} }}
    projection: {{ format: utf8 }}
    bindings:
      - id: runner
        capabilities: [vm.exec]
        sink: httpBearer
        allowedHosts: [{RUNNER}]
        allowedMethods: [POST]
        allowedPaths: [{{ match: segmentPrefix, path: /prefix/v1/sessions }}]
        maxInjections: 2
{alpha}{beta}"#,
        runner = runner.display(),
        alpha = persona(PERSONA, "alpha", ISSUER, aud),
        beta = persona(
            SECOND_PERSONA,
            "beta",
            SECOND_ISSUER,
            "https://beta.example.test"
        ),
    );
    let path = directory.join("map.yaml");
    write(&path, document.as_bytes()).await;
    path
}

#[tokio::test]
async fn signed_jwt_boot_reports_invalid_ttl_audience_and_private_key_together() {
    let directory = tempfile::tempdir().unwrap();
    let path = secret_map(directory.path(), 301, "", 0o640).await;
    let error = load(&path, rustix::process::geteuid().as_raw())
        .await
        .unwrap_err();
    let SecretMapError::SignedJwtValidation { jwt_problems, .. } = error else {
        panic!("expected typed signedJwt conflicts: {error}");
    };
    let mut problems = jwt_problems
        .iter()
        .filter(|problem| problem.secret.as_str() == PERSONA);
    assert!(matches!(
        problems.next().map(|problem| &problem.source),
        Some(SignedJwtConfigError::Ttl)
    ));
    assert!(matches!(
        problems.next().map(|problem| &problem.source),
        Some(SignedJwtConfigError::Audience)
    ));
    assert!(matches!(
        problems.next().map(|problem| &problem.source),
        Some(SignedJwtConfigError::KeyRead(
            FileHygieneError::InsecureMode {
                tier: FileTier::Private,
                mode: 0o640,
                forbidden: 0o077,
                ..
            }
        ))
    ));
    assert!(problems.next().is_none());
}

#[tokio::test]
async fn materialized_check_validates_persona_keys() {
    let directory = tempfile::tempdir().unwrap();
    let path = secret_map(directory.path(), 300, "https://alpha.example.test", 0o640).await;
    let uid = rustix::process::geteuid().as_raw();

    super::load_shapes(&path, uid, None).await.unwrap();
    let mut material_problems = Vec::new();
    super::load_shapes(&path, uid, Some(&mut material_problems))
        .await
        .unwrap();

    assert_eq!(material_problems.len(), 2);
    for (problem, persona) in material_problems.iter().zip([PERSONA, SECOND_PERSONA]) {
        let SecretMapError::SignedJwtValidation { jwt_problems, .. } = problem else {
            panic!("expected typed signedJwt conflicts: {problem}");
        };
        assert!(matches!(
            jwt_problems.as_slice(),
            [super::SignedJwtProblem {
                secret,
                source: SignedJwtConfigError::KeyRead(FileHygieneError::InsecureMode {
                    tier: FileTier::Private,
                    ..
                }),
            }] if secret.as_str() == persona
        ));
    }
}

#[tokio::test]
async fn duplicate_personas_report_every_signed_jwt_conflict() {
    let directory = tempfile::tempdir().unwrap();
    let path = secret_map(directory.path(), 301, "", 0o600).await;
    let document = tokio::fs::read_to_string(&path).await.unwrap();
    let document = document.replace(SECOND_PERSONA, PERSONA).replacen(
        &format!("runnerSecret: {RUNNER_DRN}"),
        "runnerSecret: drn:secret:login/missing",
        1,
    );
    write(&path, document.as_bytes()).await;

    let error = load(&path, rustix::process::geteuid().as_raw())
        .await
        .unwrap_err();
    let SecretMapError::SignedJwtValidation {
        problems,
        jwt_problems,
    } = error
    else {
        panic!("expected typed signedJwt conflicts: {error}");
    };
    assert!(problems.contains(&format!("duplicate DRN {PERSONA}")));
    assert_eq!(
        jwt_problems
            .iter()
            .filter(|problem| matches!(problem.source, SignedJwtConfigError::Ttl))
            .count(),
        2
    );
    assert!(jwt_problems.iter().any(|problem| {
        problem.secret.as_str() == PERSONA
            && matches!(problem.source, SignedJwtConfigError::Audience)
    }));
    assert!(jwt_problems.iter().any(|problem| {
        problem.secret.as_str() == PERSONA
            && matches!(problem.source, SignedJwtConfigError::RunnerBinding)
    }));
}

#[tokio::test]
async fn signed_jwt_boot_requires_an_absolute_https_issuer() {
    let directory = tempfile::tempdir().unwrap();
    let path = secret_map(directory.path(), 300, "https://alpha.example.test", 0o600).await;
    let document = tokio::fs::read_to_string(&path).await.unwrap();
    let uid = rustix::process::geteuid().as_raw();
    let issuer_line = format!("      iss: \"{ISSUER}\"\n");
    let missing = document.replacen(&issuer_line, "", 1);
    write(&path, missing.as_bytes()).await;
    let error = load(&path, uid).await.unwrap_err();
    assert!(matches!(
        error,
        SecretMapError::Decode { diagnostic }
            if diagnostic.starts_with("missing field") && diagnostic.ends_with("(iss)")
    ));

    for invalid in ["http://issuer.example.test", "../issuer"] {
        let invalid = document.replacen(ISSUER, invalid, 1);
        write(&path, invalid.as_bytes()).await;
        let error = load(&path, uid).await.unwrap_err();
        let SecretMapError::SignedJwtValidation { jwt_problems, .. } = error else {
            panic!("expected a typed issuer error: {error}");
        };
        assert!(matches!(
            jwt_problems.as_slice(),
            [problem] if problem.secret.as_str() == PERSONA
                && matches!(problem.source, SignedJwtConfigError::Issuer)
        ));
    }
}

#[tokio::test]
async fn signed_jwt_requires_runner_coverage_for_each_configured_capability() {
    let directory = tempfile::tempdir().unwrap();
    let path = secret_map(directory.path(), 300, "https://alpha.example.test", 0o600).await;
    let document = tokio::fs::read_to_string(&path).await.unwrap();
    let uid = rustix::process::geteuid().as_raw();
    let covered = document.replace("[vm.exec]", "[vm.exec, vm.login]");
    write(&path, covered.as_bytes()).await;
    load(&path, uid).await.unwrap();

    let uncovered = covered.replacen("[vm.exec, vm.login]", "[vm.exec]", 1);
    write(&path, uncovered.as_bytes()).await;
    let error = load(&path, uid).await.unwrap_err();
    let SecretMapError::SignedJwtValidation { jwt_problems, .. } = error else {
        panic!("expected a typed runner binding error: {error}");
    };
    assert_eq!(
        jwt_problems
            .iter()
            .map(|problem| problem.secret.as_str())
            .collect::<Vec<_>>(),
        [PERSONA, SECOND_PERSONA],
    );
    assert!(
        jwt_problems
            .iter()
            .all(|problem| matches!(problem.source, SignedJwtConfigError::RunnerBinding))
    );
}

async fn issuer(directory: &Path, jwks_uri: &str) -> SelfSignedIssuer {
    let key = directory.join("signing.pem");
    write(&key, KEY).await;
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let helper = directory.join("login-fixture");
    write(&helper, HELPER_BYTES).await;
    let config: SignedJwtConfig = serde_json::from_value(json!({
        "privateKey": key, "runnerSecret": RUNNER_DRN, "jwksUri": jwks_uri,
        "helper": {"path": helper, "sha256": sha256_hex(HELPER_BYTES)},
        "iss": ISSUER, "aud": "https://alpha.example.test", "ttl": 300,
        "claims": {"sub": "user-fixture", "org": "org-fixture", "role": "reviewer-fixture", "email": "email-fixture@example.test"},
    }))
    .unwrap();
    SelfSignedIssuer::open(
        config,
        rustix::process::geteuid().as_raw(),
        &mut Sha256::new(),
    )
    .unwrap()
}

#[test]
fn the_kid_is_the_rfc_7638_thumbprint() {
    let n = "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgw";
    assert_eq!(
        thumbprint(n, "AQAB"),
        "NzbLsXh8uDCcd-6MNwXF4W_7noWXFZAfHkxZsRGC9Xs"
    );
}

#[tokio::test]
async fn removed_signed_jwt_fields_are_refused_before_new_required_fields() {
    let directory = tempfile::tempdir().unwrap();
    let path = secret_map(directory.path(), 300, "https://alpha.example.test", 0o600).await;
    let document = tokio::fs::read_to_string(&path).await.unwrap();
    let document: serde_yaml::Value = serde_yaml::from_str(&document).unwrap();
    let config = directory.path().join("broker.yaml");
    let provider = directory.path().join("cli-probe.wasm");
    write(
        &provider,
        include_bytes!("../../../../examples/providers/cli-probe-provider.wasm"),
    )
    .await;
    crate::tests::write_config(
        &config,
        &json!({
            "apiVersion": crate::config::CONFIG_API_VERSION,
            "socketPath": "broker.sock",
            "secretMapPath": path,
            "providers": [provider],
        }),
    );
    let uid = rustix::process::geteuid().as_raw();
    let reports_removal = |error: &SecretMapError| {
        matches!(
            error,
            SecretMapError::SignedJwtValidation { jwt_problems, .. }
                if matches!(jwt_problems.as_slice(),
                    [
                        super::SignedJwtProblem { secret: first, source: SignedJwtConfigError::KidRemoved },
                        super::SignedJwtProblem { secret: second, source: SignedJwtConfigError::KidRemoved },
                    ] if first.as_str() == PERSONA && second.as_str() == SECOND_PERSONA
                )
        )
    };

    for kid in [
        serde_yaml::Value::from("receipt-key"),
        serde_yaml::Value::Null,
    ] {
        for missing in [&[][..], &["jwksUri"], &["helper"], &["jwksUri", "helper"]] {
            let mut candidate = document.clone();
            for persona in &mut candidate["secrets"].as_sequence_mut().unwrap()[1..] {
                let source = persona["source"].as_mapping_mut().unwrap();
                source.insert("kid".into(), kid.clone());
                for field in missing {
                    source.remove(*field);
                }
            }
            let yaml = serde_yaml::to_string(&candidate).unwrap();
            write(&path, yaml.as_bytes()).await;
            let error = load(&path, uid).await.unwrap_err();
            let report = crate::check(&config, None, None).await;
            assert!(
                reports_removal(&error),
                "boot with kid {kid:?} and missing {missing:?}: {error}"
            );
            assert!(
                report.problems.iter().any(|error| matches!(
                    error, crate::BrokerdError::Secrets(source) if reports_removal(source)
                )),
                "check with kid {kid:?} and missing {missing:?}: {report:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_signer_without_jwks_uri_or_helper_pin_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let path = secret_map(directory.path(), 300, "https://alpha.example.test", 0o600).await;
    let document = tokio::fs::read_to_string(&path).await.unwrap();
    let uid = rustix::process::geteuid().as_raw();
    for (prefix, field) in [("      jwksUri:", "jwksUri"), ("      helper:", "helper")] {
        let line = document
            .lines()
            .find(|line| line.starts_with(prefix))
            .unwrap();
        let missing = document.replacen(&format!("{line}\n"), "", 2);
        write(&path, missing.as_bytes()).await;
        let error = load(&path, uid).await.unwrap_err();
        assert!(matches!(
            error,
            SecretMapError::Decode { diagnostic }
                if diagnostic.starts_with("missing field") && diagnostic.ends_with(&format!("({field})"))
        ));
    }
}

#[tokio::test]
async fn an_unusable_jwks_uri_or_helper_pin_is_a_typed_config_error() {
    let directory = tempfile::tempdir().unwrap();
    let path = secret_map(directory.path(), 300, "https://alpha.example.test", 0o600).await;
    let document = tokio::fs::read_to_string(&path).await.unwrap();
    let uid = rustix::process::geteuid().as_raw();
    let helper_sha = sha256_hex(HELPER_BYTES);
    for (from, to, expected) in [
        (UNREACHABLE_JWKS, "http://jwks.example.test/jwks", 0),
        (UNREACHABLE_JWKS, "/jwks", 0),
        (helper_sha.as_str(), "not-a-digest", 1),
        (&helper_path(directory.path()), "login-fixture", 1),
    ] {
        let invalid = document.replace(from, to);
        write(&path, invalid.as_bytes()).await;
        let error = load(&path, uid).await.unwrap_err();
        let SecretMapError::SignedJwtValidation { jwt_problems, .. } = error else {
            panic!("expected a typed config error: {error}");
        };
        assert!(jwt_problems.iter().any(|problem| match expected {
            0 => matches!(problem.source, SignedJwtConfigError::JwksUri),
            _ => matches!(problem.source, SignedJwtConfigError::Helper),
        }));
    }
}

#[tokio::test]
async fn signing_is_refused_while_the_jwks_is_unreachable() {
    let directory = tempfile::tempdir().unwrap();
    let capture = CaptureLayer::new();
    let unreachable = issuer(directory.path(), UNREACHABLE_JWKS).await;
    let error = unreachable
        .issue_with_verified_helper_for_test()
        .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
        .await;
    assert!(matches!(error, Err(IssueError::JwksUnreachable)));
    assert_eq!(
        capture
            .events_where(
                "dekopon_brokerd::secrets",
                &[
                    ("category", "jwks-unreachable"),
                    ("error.connect", "true"),
                    ("error.timeout", "false"),
                ],
            )
            .len(),
        1
    );
    let missing = LoopbackServer::once(
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    let not_found = issuer(directory.path(), &format!("{}/jwks", missing.url())).await;
    let error = not_found
        .issue_with_verified_helper_for_test()
        .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
        .await;
    assert!(matches!(error, Err(IssueError::JwksUnreachable)));
    assert_eq!(
        capture
            .events_where(
                "dekopon_brokerd::token_issuer",
                &[
                    ("category", "jwks-unreachable"),
                    ("http.response.status_code", "404"),
                ],
            )
            .len(),
        1
    );
}

#[tokio::test]
async fn signing_is_refused_while_the_jwks_serves_another_kid() {
    let directory = tempfile::tempdir().unwrap();
    let (_, ours) = published();
    let mut jwks = ours.clone();
    jwks["keys"][0]["kid"] = json!("stale-kid");
    let stale = LoopbackServer::once(&http_json(&jwks));
    let issuer_stale = issuer(directory.path(), &format!("{}/jwks", stale.url())).await;
    assert!(matches!(
        issuer_stale.issue_with_verified_helper_for_test().await,
        Err(IssueError::JwksKidNotServed)
    ));

    let mut forged = ours;
    forged["keys"][0]["n"] = json!("AQAB");
    let forged = LoopbackServer::once(&http_json(&forged));
    let issuer_forged = issuer(directory.path(), &format!("{}/jwks", forged.url())).await;
    assert!(matches!(
        issuer_forged.issue_with_verified_helper_for_test().await,
        Err(IssueError::JwksKidNotServed)
    ));

    let empty = LoopbackServer::once(&http_json(&json!({"keys": []})));
    let issuer_empty = issuer(directory.path(), &format!("{}/jwks", empty.url())).await;
    assert!(matches!(
        issuer_empty.issue_with_verified_helper_for_test().await,
        Err(IssueError::JwksKidNotServed)
    ));
}

#[tokio::test]
async fn signing_is_refused_on_a_malformed_jwks() {
    let directory = tempfile::tempdir().unwrap();
    let server = LoopbackServer::once(&http_json(&json!({"not": "a key set"})));
    let issuer = issuer(directory.path(), &format!("{}/jwks", server.url())).await;
    let capture = CaptureLayer::new();
    let error = issuer
        .issue_with_verified_helper_for_test()
        .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
        .await;
    assert!(matches!(error, Err(IssueError::JwksMalformed)));
    assert_eq!(
        capture
            .events_where(
                "dekopon_brokerd::secrets",
                &[("category", "jwks-malformed"), ("error.class", "Data")],
            )
            .len(),
        1
    );
}

#[tokio::test]
async fn a_rotation_signs_only_after_the_new_kid_is_published() {
    let directory = tempfile::tempdir().unwrap();
    let (kid, jwks) = published();
    let mut previous = jwks.clone();
    previous["keys"][0]["kid"] = json!("previous-kid");
    let server = LoopbackServer::sequence([http_json(&previous), http_json(&jwks)]);
    let issuer = issuer(directory.path(), &format!("{}/jwks", server.url())).await;
    assert!(matches!(
        issuer.issue_with_verified_helper_for_test().await,
        Err(IssueError::JwksKidNotServed)
    ));
    let issued = issuer
        .issue_with_verified_helper_for_test()
        .await
        .ok()
        .unwrap();
    assert_jwt(
        std::str::from_utf8(issued.token.expose()).unwrap(),
        "https://alpha.example.test",
        ISSUER,
        &kid,
    );
}

fn assert_jwt(token: &str, aud: &str, iss: &str, kid: &str) {
    let (input, signature) = token.rsplit_once('.').expect("JWT signature");
    let key = GithubAppKey::from_pem(KEY).expect("fixture key");
    UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.public_key())
        .verify(
            input.as_bytes(),
            &URL_SAFE_NO_PAD.decode(signature).unwrap(),
        )
        .expect("the public key verifies RS256");
    let (header, claims) = input.split_once('.').expect("JWT header and claims");
    let decode =
        |part| serde_json::from_slice::<Value>(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap();
    assert_eq!(decode(header), json!({"alg":"RS256","typ":"JWT","kid":kid}));
    let claims = decode(claims);
    let iat = claims["iat"].as_u64().unwrap();
    let exp = claims["exp"].as_u64().unwrap();
    let jti = claims["jti"].as_str().unwrap();
    assert!(!jti.is_empty());
    assert!((1..=300).contains(&(exp - iat)));
    assert_eq!(
        claims,
        json!({
            "iss": iss,
            "iat": iat, "exp": exp, "aud": aud, "jti": jti,
            "sub": "user-fixture", "org": "org-fixture", "role": "reviewer-fixture", "email": "email-fixture@example.test"
        })
    );
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one family test follows the real broker and HTTP host to the runner transport"
)]
async fn signed_jwt_refuses_unattested_runner_before_jwks_or_exec() {
    let captured = CaptureLayer::workspace();
    let subscriber = tracing_subscriber::registry().with(captured.clone());
    async {
        let directory = tempfile::tempdir().unwrap();
        let (_, jwks) = published();
        let server = LoopbackServer::pooled(&http_json(&jwks), 2);
        let jwks_uri = format!("{}/jwks", server.url());
        let issuer = issuer(directory.path(), &jwks_uri).await;
        let issuance = issuer.issue().await;
        let helper = helper_path(directory.path());
        let map = secret_map_with(
            directory.path(),
            300,
            "https://alpha.example.test",
            0o600,
            &jwks_uri,
        )
        .await;
        let catalog = load(&map, rustix::process::geteuid().as_raw())
            .await
            .unwrap();
        let replies = [
            Response {
                status: 201,
                headers: Vec::new(),
                body: json!({"sessionId": SESSION}).to_string().into_bytes(),
            },
            Response {
                status: 200,
                headers: Vec::new(),
                body: OK.as_bytes().to_vec(),
            },
        ];
        let transport = Transport::new(
            (RUNNER.to_owned(), replies.to_vec()),
            ("unused.example.test".to_owned(), replies[0].clone()),
            2,
        );
        let component = directory.path().join("vm.wasm");
        std::fs::write(&component, vm_component(&helper)).unwrap();
        let registry = BrokerProviderRegistry::load([component], BrokerHostLimits::default())
            .await
            .unwrap()
            .with_test_http_transport(transport.clone());
        let capability = "vm.exec".parse().unwrap();
        let provider = "vm".parse().unwrap();
        let caller: dekopon_core::PrincipalId = "caller".parse().unwrap();
        let subject: dekopon_core::Subject = "drn:slack:user:t0123abc:u9xyz".parse().unwrap();
        let world = PolicyWorld::new([caller.clone()], [(capability, provider)])
            .unwrap()
            .with_secrets(catalog.drns().cloned());
        let policy = PolicyEngine::new(
            r#"
            @id("allow-agent")
            permit(principal == Dekopon::Principal::"caller",
                   action == Dekopon::Action::"agent.prompt",
                   resource == Dekopon::Agent::"agent");
            @id("allow-exec")
            permit(principal == Dekopon::Principal::"caller",
                   action == Dekopon::Action::"vm.exec",
                   resource == Dekopon::Provider::"vm");
            @id("allow-secret")
            permit(principal == Dekopon::Principal::"caller",
                   action == Dekopon::Action::"secret.use", resource)
            when { context.sink == "vmExecStdin" || context.sink == "httpBearer" };
        "#,
            &world,
        )
        .unwrap();
        let constraints = ConstraintCatalog::new([(
            "vm.exec".parse().unwrap(),
            ConstraintSet {
                egress_profile: None,
                default_secret_use: None,
                route: CapabilityRoute::Generic,
                provider: "vm".parse().unwrap(),
                effect: EffectKind::ExternalWrite,
                risk: RiskLevel::High,
                constraints: ExecutionConstraints {
                    http: Some(HttpConstraints {
                        allowed_hosts: vec![RUNNER.to_owned()],
                        allowed_methods: vec!["POST".to_owned()],
                        max_requests: 2,
                        max_request_bytes: 64 * 1024,
                        max_response_bytes: 4096,
                        propagate_trace: false,
                        allow_plaintext_loopback: false,
                        request_templates: Vec::new(),
                        signed_download: None,
                    }),
                    ..ExecutionConstraints::default()
                },
            },
        )])
        .unwrap();
        let broker = Broker::new(
            registry,
            "broker-test".parse().unwrap(),
            policy,
            constraints,
            IdentityDirectory::new([(subject.clone(), caller)]).unwrap(),
            Arc::new(AuditSink::InMemory(Arc::new(
                InMemoryAuditLog::new(32).unwrap(),
            ))),
            BrokerLimits::default(),
        )
        .unwrap();
        let actor = AuthenticatedContext::new(
            "gateway".parse().unwrap(),
            Actor::Service {
                principal: "gateway".parse().unwrap(),
            },
        )
        .unwrap();
        let attestor = AttestorGrant { namespaces: None };
        let attestation = Attestation::for_subject(
            AttestedSubject::one(subject).unwrap(),
            "agent".parse().unwrap(),
        );
        let broker = broker.with_secret_catalog(catalog).unwrap();
        let CommandRunOutcome::Proposed {
            capability,
            input,
            secret_use: Some(secret_use),
        } = broker
            .run_command(
                &actor,
                Some(&attestor),
                Some(&attestation),
                "ssh",
                &[helper],
                false,
            )
            .await
            .unwrap()
        else {
            panic!("expected the vm proposal")
        };
        let (host, mut reader) = std::os::unix::net::UnixStream::pair().unwrap();
        let invocation = "unattested-helper"
            .parse::<dekopon_core::InvocationId>()
            .unwrap();
        let claim = attestation.bound_to(invocation.clone());
        let request = InvocationRequest {
            id: invocation,
            capability,
            trace_parent: "00-0000000000000000000000000000f1c7-00000000000000f1-00"
                .parse()
                .unwrap(),
            input,
            secret_use: Some(secret_use),
        };
        let outcome = broker
            .invoke(
                &actor,
                Some(&attestor),
                Some(&claim),
                request,
                AssetInputs {
                    streams: Some(Streams {
                        stdin: None,
                        stdout: host.into(),
                    }),
                    ..AssetInputs::default()
                },
            )
            .await
            .unwrap();
        let mut stdout = Vec::new();
        reader.read_to_end(&mut stdout).unwrap();

        assert_eq!(
            (
                issuance.err().map(|cause| cause.category()),
                outcome.result.outcome,
                outcome.result.error.as_deref(),
                server.recorded().len(),
                transport.requests().len(),
                stdout.len(),
            ),
            (
                Some("runner-helper-unattested"),
                InvocationOutcome::Failed,
                Some("secret-resolution"),
                0,
                0,
                0,
            )
        );
        assert_eq!(
            captured
                .events_where(
                    "dekopon_broker",
                    &[
                        ("event", "broker_secret_resolution_failed"),
                        ("category", "runner-helper-unattested"),
                    ],
                )
                .len(),
            1
        );
    }
    .with_subscriber(subscriber)
    .await;
}

fn vm_component(helper: &str) -> Vec<u8> {
    let manifest = json!({
        "apiVersion": "dekopon.dev/provider/v1alpha1", "id": "vm",
        "description": "VM exec fixture", "commandWords": ["ssh"],
        "capabilities": [{
            "id": "vm.exec", "description": "Send an exec to the fixture", "effect": "external-write",
            "risk": "High", "inputSchema": { "type": "object" }
        }]
    }).to_string();
    let proposal = json!({
        "outcome": "proposed", "capability": "vm.exec",
        "input": {"profile":"fixture","name":"default","argv":[helper],"deadlineMs":25000},
        "secretUse": {"kind":"vmExecStdin","secret": PERSONA}
    })
    .to_string();
    let create_uri = format!("https://{RUNNER}/prefix/v1/sessions");
    let exec_uri = format!("{create_uri}/{SESSION}/exec");
    let create = r#"{"profile":"fixture","name":"default"}"#;
    let exec = json!({"argv":[helper],"deadlineMs":25000}).to_string();
    let hex = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|byte| format!("\\{byte:02x}"))
            .collect::<String>()
    };
    let manifest_ptr =
        hex(&[256_u32.to_le_bytes(), (manifest.len() as u32).to_le_bytes()].concat());
    let proposal_ptr = hex(&[
        2048_u32.to_le_bytes(),
        (proposal.len() as u32).to_le_bytes(),
    ]
    .concat());
    let header = hex(&[
        5332_u32.to_le_bytes(),
        12_u32.to_le_bytes(),
        5350_u32.to_le_bytes(),
        16_u32.to_le_bytes(),
    ]
    .concat());
    wat::parse_str(format!(r#"(component
    (import "dekopon:http/client@1.1.0" (instance $http
        (type $header-record (record (field "name" string) (field "value" (list u8))))
        (export "header" (type $header (eq $header-record)))
        (type $request-record (record (field "method" string) (field "uri" string) (field "headers" (list $header)) (field "body" (list u8))))
        (export "request" (type $request (eq $request-record)))
        (type $response-record (record (field "status" u16) (field "headers" (list $header)) (field "body" (list u8))))
        (export "response" (type $response (eq $response-record)))
        (type $code-enum (enum "invalid-method" "invalid-uri" "invalid-header" "request-too-large" "denied" "host-call-limit" "dns" "connect" "tls" "timeout" "protocol" "response-too-large" "internal"))
        (export "error-code" (type $code (eq $code-enum)))
        (type $error-record (record (field "code" $code) (field "message" string)))
        (export "http-error" (type $error (eq $error-record)))
        (export "send" (func (param "request" $request) (result (result $response (error $error)))))
    ))
    (import "dekopon:stdio/streams@0.1.0" (instance $stdio
        (type $error (enum "closed"))
        (export "write-error" (type $write-error (eq $error)))
        (export "writer" (type $writer (sub resource)))
        (export "stdout" (func (result (own $writer))))
        (export "[method]writer.write" (func (param "self" (borrow $writer)) (param "bytes" (list u8)) (result (result (error $write-error)))))
    ))
    (core module $mem (memory (export "memory") 2)
        (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 65536))
    (core instance $mem (instantiate $mem))
    (alias core export $mem "memory" (core memory $memory))
    (alias core export $mem "realloc" (core func $realloc))
    (core func $send (canon lower (func $http "send") (memory $memory) (realloc $realloc)))
    (core func $stdout (canon lower (func $stdio "stdout")))
    (core func $write (canon lower (func $stdio "[method]writer.write") (memory $memory)))
    (core module $guest
        (import "mem" "memory" (memory 2))
        (import "host" "send" (func $send (param i32 i32 i32 i32 i32 i32 i32 i32 i32)))
        (import "host" "stdout" (func $stdout (result i32)))
        (import "host" "write" (func $write (param i32 i32 i32 i32)))
        (data (i32.const 0) "{manifest_ptr}{proposal_ptr}")
        (data (i32.const 256) "{manifest}")
        (data (i32.const 2048) "{proposal}")
        (data (i32.const 4096) "{create_uri}")
        (data (i32.const 4352) "{exec_uri}")
        (data (i32.const 4608) "{create}")
        (data (i32.const 4864) "{exec}")
        (data (i32.const 5120) "POST")
        (data (i32.const 5312) "{header}")
        (data (i32.const 5332) "content-type")
        (data (i32.const 5350) "application/json")
        (func (export "describe") (result i32) i32.const 0)
        (func (export "run-command") (param i32 i32 i32) (result i32) i32.const 8)
        (func (export "invoke") (param i32 i32 i32 i32) (result i32)
            i32.const 5120 i32.const 4 i32.const 4096 i32.const {create_uri_len}
            i32.const 5312 i32.const 1 i32.const 4608 i32.const {create_len} i32.const 5504 call $send
            i32.const 5504 i32.load8_u if unreachable end
            i32.const 5120 i32.const 4 i32.const 4352 i32.const {exec_uri_len}
            i32.const 5312 i32.const 1 i32.const 4864 i32.const {exec_len} i32.const 5504 call $send
            i32.const 5504 i32.load8_u if unreachable end
            call $stdout i32.const 5520 i32.load i32.const 5524 i32.load i32.const 5552 call $write
            i32.const 16 i32.const 0 i32.store16
            i32.const 16))
    (core instance $guest (instantiate $guest (with "mem" (instance $mem))
        (with "host" (instance (export "send" (func $send)) (export "stdout" (func $stdout)) (export "write" (func $write))))))
    (func (export "describe") (result string) (canon lift (core func $guest "describe") (memory $memory)))
    (func (export "run-command") (param "argv" (list string)) (param "stdin-piped" bool) (result string)
        (canon lift (core func $guest "run-command") (memory $memory) (realloc $realloc)))
    (func (export "invoke") (param "capability" string) (param "input-json" string) (result (result (error u8)))
        (canon lift (core func $guest "invoke") (memory $memory) (realloc $realloc)))
)"#,
        manifest = hex(manifest.as_bytes()), proposal = hex(proposal.as_bytes()),
        create_uri = hex(create_uri.as_bytes()), exec_uri = hex(exec_uri.as_bytes()),
        create = hex(create.as_bytes()), exec = hex(exec.as_bytes()),
        create_uri_len = create_uri.len(), exec_uri_len = exec_uri.len(),
        create_len = create.len(), exec_len = exec.len(),
    )).unwrap()
}
