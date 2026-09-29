use std::{
    collections::BTreeMap,
    fs,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt as _,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use dekopon_broker::{
    Attestation, AttestorGrant, AuthenticatedContext, Broker, BrokerLimits, CapabilityRoute,
    ConstraintCatalog, ConstraintSet, CredentialStore, IdentityDirectory, InMemoryAuditLog,
    InvocationRequest, PolicyEngine, PolicyWorld, SecretResolver as _,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_capability::{
    EffectKind, ExecutionConstraints, HttpConstraints, HttpPathRule, InvocationOutcome,
};
use dekopon_core::{Actor, AgentId, PrincipalId, RiskLevel, SecretDrn};
use dekopon_test_support::{
    CaptureLayer, LoopbackServer, Record, content_length, provider_fixture,
};
use serde_json::{Value, json};
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::layer::SubscriberExt as _;

use super::{
    MapResolver, Projection, ResolvedEntry, SecretMapError, SecretMapFile, SecretSource,
    validate_map,
};

const OLD_REFRESH: &str = "old-refresh-private-sentinel";
const NEW_REFRESH: &str = "new-refresh-private-sentinel";
const NEW_ACCESS: &str = "new-access-private-sentinel";

struct TokenApi {
    endpoint: String,
    requests: Arc<Mutex<Vec<String>>>,
    done: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
}

impl TokenApi {
    fn serving(reply: impl Fn(usize) -> (&'static str, String) + Send + 'static) -> Self {
        Self::serving_with_request(move |count, _| reply(count))
    }

    fn serving_with_request(
        mut reply: impl FnMut(usize, &str) -> (&'static str, String) + Send + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("API listener");
        let endpoint = format!("http://{}", listener.local_addr().expect("address"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let recorded = Arc::clone(&requests);
        let stopping = Arc::clone(&done);
        let task = thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                if stopping.load(Ordering::SeqCst) {
                    break;
                }
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("read deadline");
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .expect("write deadline");
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let read = stream.read(&mut chunk).expect("read request");
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    assert!(request.len() <= 32 * 1024);
                    if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n")
                        && request.len() >= end + 4 + content_length(&request[..end])
                    {
                        break;
                    }
                }
                let mut all = recorded.lock().expect("requests");
                all.push(String::from_utf8(request).expect("HTTP request"));
                let (status, body) = reply(all.len(), all.last().expect("recorded request"));
                drop(all);
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).expect("response");
            }
        });
        Self {
            endpoint,
            requests,
            done,
            task: Some(task),
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests").clone()
    }
}

impl Drop for TokenApi {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        drop(TcpStream::connect(
            self.endpoint.trim_start_matches("http://"),
        ));
        self.task
            .take()
            .expect("API thread")
            .join()
            .expect("API join");
    }
}

fn drn(suffix: &str) -> SecretDrn {
    format!("drn:com.xrl:secret:test:oauth/{suffix}")
        .parse()
        .expect("DRN")
}

fn source(path: &Path, endpoint: &str) -> SecretSource {
    serde_json::from_value(source_value(path, endpoint)).expect("source")
}

fn source_value(path: &Path, endpoint: &str) -> Value {
    json!({"kind":"oauth2Refresh", "recordPath":path, "tokenEndpoint":endpoint, "clientId":"public-client-id"})
}

fn write_record(path: &Path, expires_at: u64) {
    fs::write(path, json!({"version":1, "access":"old-access-private-sentinel", "refresh":OLD_REFRESH, "expiresAt":expires_at}).to_string()).expect("record");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("private record");
}

fn expired(path: &Path) {
    write_record(path, 1);
}

fn resolver(path: &Path, endpoint: &str, suffixes: &[&str]) -> MapResolver {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .expect("client");
    MapResolver {
        entries: suffixes
            .iter()
            .map(|name| {
                (
                    drn(name),
                    ResolvedEntry {
                        source: source(path, endpoint),
                        projection: Projection::default(),
                        client: client.clone(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
        expected_uid: rustix::process::geteuid().as_raw(),
    }
}

fn success() -> (&'static str, String) {
    (
        "200 OK",
        json!({"access_token":NEW_ACCESS,"refresh_token":NEW_REFRESH,"expires_in":3600})
            .to_string(),
    )
}

fn content(request: &str) -> &str {
    request.split_once("\r\n\r\n").expect("request body").1
}

#[tokio::test]
async fn a_refresh_replaces_the_record_and_the_next_resolution_reuses_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    expired(&path);
    let stale = dir.path().join("record.tmp-123");
    fs::write(&stale, b"abandoned-private-record").unwrap();
    let api = TokenApi::serving(|_| success());
    let resolver = resolver(&path, &api.endpoint, &["one"]);
    for _ in 0..2 {
        let entry = resolver.entries.get(&drn("one")).unwrap();
        assert_eq!(
            resolver
                .resolve_source(&entry.source, &entry.client)
                .await
                .unwrap(),
            NEW_ACCESS.as_bytes()
        );
    }
    let stored: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(stored["access"], NEW_ACCESS);
    assert_eq!(stored["refresh"], NEW_REFRESH);
    assert!(!stale.exists());
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let requests = api.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        content(&requests[0]),
        "grant_type=refresh_token&refresh_token=old-refresh-private-sentinel&client_id=public-client-id"
    );
}

#[tokio::test]
async fn an_omitted_replacement_refresh_token_keeps_the_predecessor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    expired(&path);
    let api = TokenApi::serving(|_| {
        (
            "200 OK",
            json!({"access_token":NEW_ACCESS,"expires_in":3600}).to_string(),
        )
    });
    resolver(&path, &api.endpoint, &["one"])
        .resolve(&drn("one"))
        .await
        .unwrap();
    let stored: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(stored["refresh"], OLD_REFRESH);
    assert_eq!(api.requests().len(), 1);
}

#[tokio::test]
async fn an_empty_replacement_refresh_token_is_malformed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    expired(&path);
    let initial = fs::read(&path).unwrap();
    let api = TokenApi::serving(|_| {
        (
            "200 OK",
            json!({"access_token":NEW_ACCESS,"refresh_token":"","expires_in":3600}).to_string(),
        )
    });
    assert_eq!(
        resolver(&path, &api.endpoint, &["one"])
            .resolve(&drn("one"))
            .await
            .unwrap_err()
            .category,
        "malformed"
    );
    assert_eq!(fs::read(&path).unwrap(), initial);
    assert_eq!(api.requests().len(), 1);
}

#[tokio::test]
async fn invalid_grant_needs_reauthorization_and_never_replays_the_old_token() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    expired(&path);
    let initial = fs::read(&path).unwrap();
    let api = TokenApi::serving(|_| {
        (
            "400 Bad Request",
            json!({"error":"invalid_grant"}).to_string(),
        )
    });
    let resolver = resolver(&path, &api.endpoint, &["one"]);
    for _ in 0..2 {
        assert_eq!(
            resolver.resolve(&drn("one")).await.unwrap_err().category,
            "reauthorization-required"
        );
    }
    assert_eq!(fs::read(&path).unwrap(), initial);
    let requests = api.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| content(request).contains(OLD_REFRESH))
    );
}

#[tokio::test]
async fn concurrent_resolutions_of_one_record_spend_one_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    expired(&path);
    let mut used = std::collections::BTreeSet::new();
    let api = TokenApi::serving_with_request(move |_, request| {
        let token = url::form_urlencoded::parse(content(request).as_bytes())
            .find(|(key, _)| key == "refresh_token")
            .map(|(_, value)| value.into_owned())
            .expect("refresh_token form field");
        if used.insert(token) {
            success()
        } else {
            (
                "400 Bad Request",
                json!({"error":"invalid_grant"}).to_string(),
            )
        }
    });
    let resolver = Arc::new(resolver(&path, &api.endpoint, &["one"]));
    let tasks = (0..8)
        .map(|_| {
            let resolver = Arc::clone(&resolver);
            tokio::spawn(async move {
                let entry = resolver.entries.get(&drn("one")).unwrap();
                resolver.resolve_source(&entry.source, &entry.client).await
            })
        })
        .collect::<Vec<_>>();
    for task in tasks {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), task)
                .await
                .expect("resolution deadline")
                .expect("join")
                .expect("resolve"),
            NEW_ACCESS.as_bytes()
        );
    }
    assert_eq!(api.requests().len(), 1);
}

fn map(path: &Path, endpoint: &str, bootstrap: Option<&Path>) -> SecretMapFile {
    let mut secrets = vec![
        json!({"drn":drn("one"), "source":source_value(path,endpoint),"bindings":[{
            "id":"first","capability":"vm.session.create","sink":"httpBearer","allowedHosts":["runner.example"],"allowedMethods":["POST"],"allowedPaths":[{"match":"exact","path":"/v1/sessions"}]
        }]}),
    ];
    if let Some(bootstrap) = bootstrap {
        secrets.push(json!({"drn":drn("bootstrap"),"source":{"kind":"onePasswordConnect","endpoint":"https://example.org", "tokenFile":bootstrap,"vault":"a","item":"b","field":"c"},"bindings":[]}));
    } else {
        secrets.push(json!({"drn":drn("two"), "source":source_value(path,endpoint),"bindings":[{
            "id":"second","capability":"vm.session.create","sink":"httpBearer","allowedHosts":["runner.example"],"allowedMethods":["POST"],"allowedPaths":[{"match":"exact","path":"/v1/sessions"}]
        }]}));
    }
    serde_json::from_value(json!({"apiVersion":"dekopon.dev/secret-map/v1alpha1","mapRevision":"test","secrets":secrets})).unwrap()
}

fn oauth_policy() -> PolicyEngine {
    let policy = format!(
        r#"@id("allow-fetch")
        permit(principal == Dekopon::Principal::"caller",
               action == Dekopon::Action::"http-probe.fetch",
               resource == Dekopon::Provider::"http-probe")
        when {{ context.via == "gateway" && context.agent == "agent" }};
        @id("allow-prompt")
        permit(principal == Dekopon::Principal::"caller",
               action == Dekopon::Action::"agent.prompt",
               resource == Dekopon::Agent::"agent")
        when {{ context.via == "gateway" }};
        @id("use-one")
        permit(principal == Dekopon::Principal::"caller",
               action == Dekopon::Action::"secret.use",
               resource == Dekopon::Secret::"{}")
        when {{ context.capability == "http-probe.fetch"
             && context.provider == "http-probe" && context.sink == "httpBearer" }};
        @id("use-two")
        permit(principal == Dekopon::Principal::"caller",
               action == Dekopon::Action::"secret.use",
               resource == Dekopon::Secret::"{}")
        when {{ context.capability == "http-probe.fetch"
             && context.provider == "http-probe" && context.sink == "httpBearer" }};"#,
        drn("one"),
        drn("two")
    );
    let world = PolicyWorld::new(
        ["caller".parse::<PrincipalId>().unwrap()],
        [(
            "http-probe.fetch".parse().unwrap(),
            "http-probe".parse().unwrap(),
        )],
    )
    .unwrap()
    .with_secrets([drn("one"), drn("two")]);
    PolicyEngine::new(&policy, &world).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn two_drns_over_one_record_share_one_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    expired(&path);
    let api = TokenApi::serving(|n| {
        if n == 1 {
            success()
        } else {
            (
                "400 Bad Request",
                json!({"error":"invalid_grant"}).to_string(),
            )
        }
    });
    let target = LoopbackServer::serving(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
        2,
    );
    let authority = target.authority().to_owned();
    let mut file = map(&path, &api.endpoint, None);
    for entry in &mut file.secrets {
        for binding in &mut entry.bindings {
            binding.capability = "http-probe.fetch".parse().unwrap();
            binding.allowed_hosts = vec![authority.clone()];
            binding.allowed_methods = vec!["GET".to_owned()];
            binding.allowed_paths = vec![HttpPathRule::Exact {
                path: "/api/v1/thing".to_owned(),
            }];
        }
    }
    let catalog = validate_map(
        file,
        &dir.path().join("map.json"),
        rustix::process::geteuid().as_raw(),
    )
    .await
    .unwrap();
    assert_eq!(catalog.drns().count(), 2);
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .unwrap();
    let constraints = ConstraintCatalog::new([(
        "http-probe.fetch".parse().unwrap(),
        ConstraintSet {
            route: CapabilityRoute::Generic,
            provider: "http-probe".parse().unwrap(),
            effect: EffectKind::ReadOnly,
            risk: RiskLevel::Low,
            credential: None,
            constraints: ExecutionConstraints {
                asset: None,
                timeout_ms: 5_000,
                max_output_bytes: 1024 * 1024,
                http: Some(HttpConstraints {
                    allowed_hosts: vec![authority.clone()],
                    propagate_trace: false,
                    allowed_methods: vec!["GET".to_owned()],
                    max_requests: 1,
                    max_request_bytes: 64 * 1024,
                    max_response_bytes: 64 * 1024,
                    allow_plaintext_loopback: true,
                }),
                storage: None,
                secret_use: None,
            },
        },
    )])
    .unwrap();
    let broker = Arc::new(
        Broker::new(
            registry,
            "broker-test".parse().unwrap(),
            "policy-test".to_owned(),
            oauth_policy(),
            constraints,
            CredentialStore::empty(),
            IdentityDirectory::new([(
                "slack.t0123abc.u9xyz".parse().unwrap(),
                "caller".parse().unwrap(),
            )])
            .unwrap(),
            Arc::new(InMemoryAuditLog::new(16).unwrap()),
            BrokerLimits::default(),
        )
        .unwrap()
        .with_secret_catalog(catalog)
        .unwrap(),
    );
    let invoke = |id: &'static str, secret: SecretDrn| {
        let broker = Arc::clone(&broker);
        let authority = authority.clone();
        tokio::spawn(async move {
            let id: dekopon_core::InvocationId = id.parse().unwrap();
            let request = InvocationRequest {
                id: id.clone(),
                capability: "http-probe.fetch".parse().unwrap(),
                trace_parent: "00-0000000000000000000000000000f1c7-00000000000000f1-00"
                    .parse()
                    .unwrap(),
                input: json!({"uri":format!("http://{authority}/api/v1/thing"),"method":"GET"}),
                secret_use: Some(dekopon_core::SecretUseProposal::HttpBearer { secret }),
            };
            let context = AuthenticatedContext::new(
                "gateway".parse().unwrap(),
                Actor::Service {
                    principal: "gateway".parse().unwrap(),
                },
            )
            .unwrap();
            let attestation = Attestation::for_subject(
                "slack.t0123abc.u9xyz".parse().unwrap(),
                "agent".parse::<AgentId>().unwrap(),
            )
            .bound_to(id);
            broker
                .invoke(
                    &context,
                    Some(&AttestorGrant { namespaces: None }),
                    Some(&attestation),
                    request,
                    Default::default(),
                )
                .await
        })
    };
    let a = invoke("invoke-oauth-one", drn("one"));
    let b = invoke("invoke-oauth-two", drn("two"));
    for task in [a, b] {
        let result = tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("resolution deadline")
            .expect("join")
            .expect("invocation");
        assert_eq!(
            result.result.outcome,
            InvocationOutcome::Succeeded,
            "{:?}",
            result.result.error
        );
    }
    assert_eq!(api.requests().len(), 1);
    for _ in 0..2 {
        let request = target.request_text();
        assert!(request.contains(&format!("authorization: Bearer {NEW_ACCESS}")));
    }
    target.join();
}

#[tokio::test]
async fn a_rotation_whose_save_fails_still_returns_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    expired(&path);
    let guard = dekopon_core::private_file::PrivateFileLock::acquire(&path).unwrap();
    drop(guard);
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
    let api = TokenApi::serving(|_| success());
    let resolver = resolver(&path, &api.endpoint, &["one"]);
    let capture = CaptureLayer::new();
    let entry = resolver.entries.get(&drn("one")).unwrap();
    let result = resolver
        .resolve_source(&entry.source, &entry.client)
        .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
        .await;
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(result.unwrap(), NEW_ACCESS.as_bytes());
    let errors = capture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::Event {
                level: "ERROR",
                fields,
                ..
            } => Some(fields),
            Record::Event { .. } | Record::Span { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains(&format!(" record_path={}", path.display())));
    assert!(errors[0].contains(" error=Permission denied"), "{errors:?}");
    for token in [OLD_REFRESH, NEW_REFRESH, NEW_ACCESS] {
        assert!(!errors[0].contains(token), "error event disclosed {token}");
    }
}

#[tokio::test]
async fn a_group_readable_or_missing_record_fails_without_a_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    expired(&path);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    let api = TokenApi::serving(|_| success());
    let resolver = resolver(&path, &api.endpoint, &["one"]);
    let capture = CaptureLayer::new();
    assert_eq!(
        resolver
            .resolve(&drn("one"))
            .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
            .await
            .unwrap_err()
            .category,
        "insecure-file"
    );
    assert!(capture.records().iter().any(|record| matches!(
        record,
        Record::Event { parent: Some(parent), fields, .. }
            if parent == "broker.credential.refresh" && fields.contains("file-hygiene")
    )));
    fs::remove_file(&path).unwrap();
    assert_eq!(
        resolver.resolve(&drn("one")).await.unwrap_err().category,
        "io"
    );
    assert!(api.requests().is_empty());
}

#[tokio::test]
async fn no_token_or_oauth_error_body_reaches_the_trace() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    expired(&path);
    let api = TokenApi::serving(|n| {
        match n {
        1 => success(),
        2 => (
            "400 Bad Request",
            json!({"error":"invalid_grant","error_description":"private-error-description-sentinel"}).to_string(),
        ),
        3 => (
            "200 OK",
            json!({"access_token":OLD_REFRESH,"refresh_token":NEW_REFRESH,"expires_in":3600}).to_string(),
        ),
        _ => (
            "200 OK",
            json!({"access_token":NEW_REFRESH,"refresh_token":NEW_REFRESH,"expires_in":3600}).to_string(),
        ),
    }
    });
    let resolver = resolver(&path, &api.endpoint, &["one"]);
    let capture = CaptureLayer::new();
    async {
        resolver.resolve(&drn("one")).await.unwrap();
        expired(&path);
        assert_eq!(
            resolver.resolve(&drn("one")).await.unwrap_err().category,
            "reauthorization-required"
        );
        assert_eq!(
            resolver.resolve(&drn("one")).await.unwrap_err().category,
            "bootstrap-reflected"
        );
        let stored: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored["access"], OLD_REFRESH);
        assert_eq!(stored["refresh"], NEW_REFRESH);
        assert_eq!(stored["expiresAt"], 0);
        assert_eq!(api.requests().len(), 3);
        assert_eq!(
            resolver.resolve(&drn("one")).await.unwrap_err().category,
            "bootstrap-reflected"
        );
        assert_eq!(api.requests().len(), 4);
        assert_eq!(
            resolver.resolve(&drn("one")).await.unwrap_err().category,
            "bootstrap-reflected"
        );
        let stored: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored["access"], NEW_REFRESH);
        assert_eq!(stored["expiresAt"], 0);
    }
    .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
    .await;
    let recorded = format!("{} {}", capture.spans_text(), capture.events_text());
    for secret in [
        OLD_REFRESH,
        NEW_REFRESH,
        NEW_ACCESS,
        "old-access-private-sentinel",
        "private-error-description-sentinel",
        "public-client-id",
    ] {
        assert!(!recorded.contains(secret), "trace disclosed {secret}");
    }
    assert!(recorded.contains("reauth-required"), "{recorded}");
    assert_eq!(api.requests().len(), 5);
}

#[tokio::test]
async fn a_shared_record_with_a_different_endpoint_or_client_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    let mut file = map(&path, "https://one.example/token", None);
    file.secrets[1].source = source(&path, "https://two.example/token");
    let mut third = file.secrets[0].clone();
    third.drn = drn("three");
    third.source = serde_json::from_value(json!({
        "kind": "oauth2Refresh",
        "recordPath": path,
        "tokenEndpoint": "https://one.example/token",
        "clientId": "other-client-id"
    }))
    .unwrap();
    third.bindings[0].id = "third".to_owned();
    file.secrets.push(third);
    let Err(SecretMapError::Validation { problems }) = validate_map(
        file,
        &dir.path().join("map.json"),
        rustix::process::geteuid().as_raw(),
    )
    .await
    else {
        panic!("mismatched refresh sources cannot share a record");
    };
    assert_eq!(problems.len(), 3, "{problems:?}");
    for (first, second) in [("one", "two"), ("one", "three"), ("two", "three")] {
        assert!(
            problems.iter().any(|problem| {
                problem.contains(&path.display().to_string())
                    && problem.contains(drn(first).as_str())
                    && problem.contains(drn(second).as_str())
            }),
            "{problems:?}"
        );
    }
}

#[tokio::test]
async fn the_record_path_cannot_be_the_map_or_a_bootstrap_file() {
    let dir = tempfile::tempdir().unwrap();
    let map_path = dir.path().join("map.json");
    let endpoint = "https://example.org/token";
    for (map_file, needle) in [
        (map(&map_path, endpoint, None), "secret material"),
        (
            map(
                &dir.path().join("bootstrap.json"),
                endpoint,
                Some(&dir.path().join("bootstrap.json")),
            ),
            "bootstrap credential",
        ),
    ] {
        let Err(SecretMapError::Validation { problems }) =
            validate_map(map_file, &map_path, rustix::process::geteuid().as_raw()).await
        else {
            panic!("collision must fail")
        };
        assert!(
            problems.iter().any(|problem| problem.contains(needle)),
            "{problems:?}"
        );
    }
}
