use std::{io::Read as _, os::unix::fs::PermissionsExt as _, path::Path, sync::Arc};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use dekopon_broker::{
    Attestation, AttestorGrant, AuthenticatedContext, Broker, BrokerLimits, CapabilityRoute,
    ConstraintCatalog, ConstraintSet, IdentityDirectory, InMemoryAuditLog, PolicyEngine,
    PolicyWorld,
};
use dekopon_broker_host::{
    BrokerHostLimits, BrokerProviderRegistry, CommandRunOutcome, Streams, asset::AssetInputs,
};
use dekopon_broker_protocol::{
    BrokerResponse, InvocationRequest, ProtocolVersion, ResponseEnvelope, RunOutcome,
};
use dekopon_capability::{EffectKind, ExecutionConstraints, HttpConstraints, InvocationOutcome};
use dekopon_core::{
    Actor, AttestedSubject, FileHygieneError, FileTier, RiskLevel, SecretUseProposal,
};
use dekopon_http_host::{Response, test_transport::Transport};
use dekopon_test_support::{CaptureLayer, Record};
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
use serde_json::{Value, json};
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::layer::SubscriberExt as _;

use super::{SecretMapError, SignedJwtConfigError, load, tests::write};
use crate::github_app::GithubAppKey;

const KEY: &[u8] = include_bytes!("../../tests/fixture/github-app-test-only.pem");
const RUNNER: &str = "runner.example.test";
const ISSUER: &str = "https://issuer.example.test";
const SECOND_ISSUER: &str = "https://issuer.example.test/alternate";
const RUNNER_BEARER: &str = "fixture-runner-bearer";
const PERSONA: &str = "drn:secret:login/alpha";
const SECOND_PERSONA: &str = "drn:secret:login/beta";
const RUNNER_DRN: &str = "drn:secret:login/runner";
const SESSION: &str = "fixture-session";
const ARGV: &[&str] = &["login-fixture"];
const OK: &str =
    r#"{"outcome":"executed","stdout":"ok","stderr":"","exitCode":0,"truncated":false}"#;

async fn secret_map(directory: &Path, ttl: u32, aud: &str, mode: u32) -> std::path::PathBuf {
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
      kid: receipt-key
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

fn assert_jwt(token: &str, aud: &str, iss: &str) -> String {
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
    assert_eq!(
        decode(header),
        json!({"alg":"RS256","typ":"JWT","kid":"receipt-key"})
    );
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
    jti.to_owned()
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one broker-to-fake-exec round trip exercises two audiences and the trace"
)]
async fn the_broker_delivers_a_verified_persona_jwt_only_in_exec_stdin() {
    let captured = CaptureLayer::workspace();
    let subscriber = tracing_subscriber::registry().with(captured.clone());
    async {
        let directory = tempfile::tempdir().unwrap();
        let map = secret_map(directory.path(), 300, "https://alpha.example.test", 0o600).await;
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
            (
                RUNNER.to_owned(),
                replies.iter().cycle().take(4).cloned().collect(),
            ),
            ("unused.example.test".to_owned(), replies[0].clone()),
            4,
        );
        let component = directory.path().join("vm.wasm");
        std::fs::write(&component, vm_component()).unwrap();
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
            Arc::new(InMemoryAuditLog::new(32).unwrap()),
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
        let argv = ARGV.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        let propose = async |broker: &Broker<InMemoryAuditLog>| {
            broker
                .run_command(
                    &actor,
                    Some(&attestor),
                    Some(&attestation),
                    "ssh",
                    &argv,
                    false,
                )
                .await
                .unwrap()
        };
        let unmapped = propose(&broker).await;
        let broker = broker.with_secret_catalog(catalog).unwrap();
        let outcome = propose(&broker).await;
        assert_eq!(unmapped, outcome);
        let wire = serde_json::to_vec(&ResponseEnvelope::command_run(outcome)).unwrap();
        let CommandRunOutcome::Proposed {
            capability,
            input,
            secret_use: Some(secret_use),
        } = decode_command_proposal(&wire)
        else {
            panic!("expected the vm proposal")
        };
        assert_eq!(secret_use.secret().as_str(), PERSONA);
        assert_eq!(input["argv"], json!(ARGV));

        let mut execs = Vec::new();
        for (id, secret) in [
            ("jwt-alpha", secret_use),
            (
                "jwt-beta",
                SecretUseProposal::VmExecStdin {
                    secret: SECOND_PERSONA.parse().unwrap(),
                },
            ),
            (
                "jwt-unmapped",
                SecretUseProposal::VmExecStdin {
                    secret: "drn:secret:login/missing".parse().unwrap(),
                },
            ),
            (
                "jwt-reserved-runner",
                SecretUseProposal::HttpBearer {
                    secret: RUNNER_DRN.parse().unwrap(),
                },
            ),
        ] {
            let (host, mut reader) = std::os::unix::net::UnixStream::pair().unwrap();
            let invocation = id.parse::<dekopon_core::InvocationId>().unwrap();
            let claim = attestation.clone().bound_to(invocation.clone());
            let request = InvocationRequest {
                id: invocation,
                capability: capability.clone(),
                trace_parent: "00-0000000000000000000000000000f1c7-00000000000000f1-00"
                    .parse()
                    .unwrap(),
                input: input.clone(),
                secret_use: Some(secret.clone()),
            };
            let request = serde_json::from_value(serde_json::to_value(request).unwrap()).unwrap();
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
            if matches!(id, "jwt-unmapped" | "jwt-reserved-runner") {
                assert_eq!(outcome.result.outcome, InvocationOutcome::Denied);
                continue;
            }
            assert_eq!(outcome.result.outcome, InvocationOutcome::Succeeded);
            assert_eq!(stdout, OK.as_bytes());
            let ran = ResponseEnvelope {
                api_version: ProtocolVersion::V1Alpha2,
                response: BrokerResponse::Ran {
                    outcome: RunOutcome::Invoked {
                        capability: capability.clone(),
                        input: input.clone(),
                        secret_use: Some(secret),
                        result: outcome.result,
                        attached: Vec::new(),
                        removed: Vec::new(),
                        sent: Vec::new(),
                    },
                },
            };
            let frame = serde_json::to_string(&ran).unwrap();
            let decoded: ResponseEnvelope = serde_json::from_str(&frame).unwrap();
            assert_eq!(decoded, ran);
            execs.push((frame, stdout));
        }
        let observed = transport.requests();
        assert_eq!(observed.len(), 4);
        let mut jtis = Vec::new();
        for ((calls, (frame, stdout)), (aud, iss)) in
            observed.as_chunks::<2>().0.iter().zip(execs).zip([
                ("https://alpha.example.test", ISSUER),
                ("https://beta.example.test", SECOND_ISSUER),
            ])
        {
            let [create, exec] = calls;
            assert_eq!(create.sent.body, create.guest.body);
            let bearer = format!("Bearer {RUNNER_BEARER}");
            for request in [&create.sent, &exec.sent] {
                let authorization = request
                    .headers
                    .iter()
                    .find(|header| header.name == "authorization")
                    .unwrap();
                assert_eq!(authorization.value, bearer.as_bytes());
            }
            assert_eq!(
                exec.sent.uri,
                format!("https://{RUNNER}/prefix/v1/sessions/{SESSION}/exec")
            );
            assert_eq!(exec.sent.method, "POST");
            let guest: Value = serde_json::from_slice(&exec.guest.body).unwrap();
            let sent: Value = serde_json::from_slice(&exec.sent.body).unwrap();
            assert_eq!(guest, json!({"argv": ARGV, "deadlineMs": 25000}));
            assert_eq!(sent["argv"], guest["argv"]);
            assert_eq!(sent["deadlineMs"], guest["deadlineMs"]);
            let jwt = sent["stdin"].as_str().unwrap();
            assert!(!frame.contains(jwt) && !String::from_utf8(stdout).unwrap().contains(jwt));
            let jti = assert_jwt(jwt, aud, iss);
            for record in captured.records() {
                let (Record::Event { fields, .. } | Record::Span { fields, .. }) = &record;
                for attribute in fields.values() {
                    for hidden in jwt.split('.').chain([
                        aud,
                        "receipt-key",
                        "user-fixture",
                        "org-fixture",
                        "reviewer-fixture",
                        "email-fixture@example.test",
                        iss,
                    ]) {
                        assert!(
                            !attribute.contains(hidden),
                            "JWT data exposed in a trace attribute"
                        );
                    }
                }
            }
            jtis.push(jti);
        }
        assert_ne!(jtis[0], jtis[1]);
        let records = captured.records();
        let observed_jtis = records
            .iter()
            .filter_map(|record| record.field("secret.jti"))
            .collect::<Vec<_>>();
        assert_eq!(
            observed_jtis,
            jtis.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }
    .with_subscriber(subscriber)
    .await;
}

fn decode_command_proposal(bytes: &[u8]) -> CommandRunOutcome {
    let wire: ResponseEnvelope = serde_json::from_slice(bytes).unwrap();
    let BrokerResponse::CommandRun { result } = wire.response else {
        panic!("expected a command response")
    };
    result
}

fn vm_component() -> Vec<u8> {
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
        "input": {"profile":"fixture","name":"default","argv":ARGV,"deadlineMs":25000},
        "secretUse": {"kind":"vmExecStdin","secret": PERSONA}
    })
    .to_string();
    let create_uri = format!("https://{RUNNER}/prefix/v1/sessions");
    let exec_uri = format!("{create_uri}/{SESSION}/exec");
    let create = r#"{"profile":"fixture","name":"default"}"#;
    let exec = json!({"argv":ARGV,"deadlineMs":25000}).to_string();
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
