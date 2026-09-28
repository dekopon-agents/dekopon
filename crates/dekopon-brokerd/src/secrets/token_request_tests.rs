use std::{
    collections::BTreeMap,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    os::unix::fs::symlink,
    path::Path,
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

use dekopon_broker::SecretResolver as _;
use dekopon_core::SecretDrn;
use dekopon_test_support::{CaptureLayer, content_length};
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::pki_types::PrivatePkcs8KeyDer;
use serde_json::{Value, json};
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::layer::SubscriberExt as _;

use super::{
    MapResolver, Projection, ResolvedEntry, SecretMapError, SecretMapFile, SecretSource,
    SourceError, kubernetes_client, validate_map,
};

const BOOTSTRAP: &str = "bootstrap-private-fixture-one";
const MINTED: &str = "minted-private-fixture-one";

struct TestTls {
    ca_pem: String,
    config: Arc<rustls::ServerConfig>,
}

impl TestTls {
    fn new() -> Self {
        let now = super::OffsetDateTime::now_utc();
        let mut ca_params = CertificateParams::new(Vec::new()).expect("CA parameters");
        ca_params.not_before = now - time::Duration::days(1);
        ca_params.not_after = now + time::Duration::days(1);
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let ca_key = KeyPair::generate().expect("generate CA key");
        let ca = ca_params.self_signed(&ca_key).expect("CA certificate");

        let mut server_params =
            CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
                .expect("server names");
        server_params.not_before = ca_params.not_before;
        server_params.not_after = ca_params.not_after;
        server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_key = KeyPair::generate().expect("generate server key");
        let server_cert = server_params
            .signed_by(&server_key, &Issuer::from_params(&ca_params, &ca_key))
            .expect("server certificate");
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![server_cert.into()],
            PrivatePkcs8KeyDer::from(server_key.serialize_der()).into(),
        )
        .expect("server configuration");
        Self {
            ca_pem: ca.pem(),
            config: Arc::new(config),
        }
    }
}

struct TokenApi {
    endpoint: String,
    requests: mpsc::Receiver<Vec<u8>>,
    task: Option<thread::JoinHandle<()>>,
}

impl TokenApi {
    fn serving(tls: &TestTls, responses: Vec<Vec<u8>>) -> Self {
        let config = Arc::clone(&tls.config);
        let listener = TcpListener::bind("127.0.0.1:0").expect("API listener");
        let endpoint = format!("https://{}", listener.local_addr().expect("address"));
        let (sender, requests) = mpsc::channel();
        let task = thread::spawn(move || {
            for response in responses {
                let (stream, _) = listener.accept().expect("accept");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("read timeout");
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .expect("write timeout");
                let connection = rustls::ServerConnection::new(Arc::clone(&config)).expect("TLS");
                let mut stream = rustls::StreamOwned::new(connection, stream);
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let Ok(count) = stream.read(&mut chunk) else {
                        return;
                    };
                    if count == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..count]);
                    assert!(request.len() <= 32 * 1024, "bounded test request");
                    if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                        && request.len() >= end + 4 + content_length(&request[..end])
                    {
                        break;
                    }
                }
                sender.send(request).expect("record request");
                stream.write_all(&response).expect("API response");
                stream.flush().expect("flush response");
            }
        });
        Self {
            endpoint,
            requests,
            task: Some(task),
        }
    }
}

impl Drop for TokenApi {
    fn drop(&mut self) {
        drop(TcpStream::connect(
            self.endpoint.trim_start_matches("https://"),
        ));
        self.task
            .take()
            .expect("test task")
            .join()
            .expect("API task");
    }
}

fn response(status: &str, body: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes()
}

fn issued(token: &str, lifetime: i64) -> Value {
    json!({"status": {
        "token": token,
        "expirationTimestamp": (super::OffsetDateTime::now_utc() + time::Duration::seconds(lifetime))
            .format(&super::Rfc3339).expect("expiry"),
    }})
}

fn projection(root: &Path, generation: &str, token: &str, ca: &[u8]) {
    let path = root.join(generation);
    std::fs::create_dir(&path).expect("generation");
    std::fs::write(path.join("token"), token).expect("bootstrap fixture");
    std::fs::write(path.join("ca.crt"), ca).expect("CA fixture");
    symlink(generation, root.join("..data-next")).expect("generation link");
    std::fs::rename(root.join("..data-next"), root.join("..data")).expect("atomic rotation");
}

fn source(root: &Path, endpoint: &str) -> SecretSource {
    serde_json::from_value(source_value(root, endpoint)).expect("TokenRequest source")
}

fn source_value(root: &Path, endpoint: &str) -> Value {
    json!({
        "kind": "kubernetesTokenRequest", "endpoint": endpoint,
        "bootstrapRoot": root, "tokenKey": "token", "caKey": "ca.crt",
        "namespace": "dekopon", "serviceAccount": "gylmar-vm", "audience": "vm-runner",
        "expirationSeconds": 600,
    })
}

fn drn() -> SecretDrn {
    "drn:com.xrl:secret:test:vm/gylmar".parse().expect("DRN")
}

fn map(root: &Path, endpoint: &str) -> SecretMapFile {
    serde_json::from_value(json!({
        "apiVersion": "dekopon.dev/secret-map/v1alpha1", "mapRevision": "test",
        "secrets": [{
            "drn": drn(), "source": source_value(root, endpoint),
            "bindings": [{
                "id": "runner-token", "capability": "vm.session.create", "sink": "httpBearer",
                "allowedHosts": ["runner.example"], "allowedMethods": ["POST"],
                "allowedPaths": [{"match": "exact", "path": "/v1/sessions"}]
            }]
        }]
    }))
    .expect("private map")
}

async fn resolver(root: &Path, endpoint: &str) -> MapResolver {
    let source = source(root, endpoint);
    source.validate().expect("valid fixed configuration");
    MapResolver {
        entries: BTreeMap::from([(
            drn(),
            ResolvedEntry {
                source,
                projection: Projection::default(),
                client: kubernetes_client(root, "ca.crt").await.expect("cluster CA"),
            },
        )]),
        expected_uid: rustix::process::geteuid().as_raw(),
    }
}

#[tokio::test]
async fn each_resolution_mints_the_fixed_subject_and_audience_with_the_current_bootstrap() {
    let root = tempfile::tempdir().expect("projection root");
    let tls = TestTls::new();
    projection(root.path(), "..one", BOOTSTRAP, tls.ca_pem.as_bytes());
    let api = TokenApi::serving(
        &tls,
        vec![
            response("201 Created", &issued(MINTED, 30).to_string()),
            response(
                "201 Created",
                &issued("minted-private-fixture-two", 30).to_string(),
            ),
            response("403 Forbidden", MINTED),
        ],
    );
    let resolver = resolver(root.path(), &api.endpoint).await;
    let capture = CaptureLayer::workspace();
    async {
        let first = resolver
            .resolve(&drn())
            .await
            .expect("shorter returned lifetime is valid");
        assert!(!format!("{first:?} {resolver:?}").contains(MINTED));
        projection(
            root.path(),
            "..two",
            "bootstrap-private-fixture-two",
            tls.ca_pem.as_bytes(),
        );
        let entry = resolver.entries.get(&drn()).expect("configured entry");
        let second = resolver
            .resolve_source(&entry.source, &entry.client)
            .await
            .expect("mint again rather than reuse");
        assert_eq!(second, b"minted-private-fixture-two");
        let error = resolver
            .resolve(&drn())
            .await
            .expect_err("no stale fallback after failure");
        assert_eq!(error.category, "rejected");
        assert!(!error.to_string().contains(MINTED));
    }
    .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
    .await;
    for bootstrap in [
        BOOTSTRAP,
        "bootstrap-private-fixture-two",
        "bootstrap-private-fixture-two",
    ] {
        let request = api
            .requests
            .recv_timeout(Duration::from_secs(5))
            .expect("request");
        let request = String::from_utf8(request).expect("HTTP");
        let (headers, body) = request.split_once("\r\n\r\n").expect("HTTP body");
        assert!(headers.starts_with(
            "POST /api/v1/namespaces/dekopon/serviceaccounts/gylmar-vm/token HTTP/1.1\r\n"
        ));
        assert!(headers.contains(&format!("authorization: Bearer {bootstrap}\r\n")));
        assert_eq!(
            serde_json::from_str::<Value>(body).expect("TokenRequest"),
            json!({
                "apiVersion": "authentication.k8s.io/v1", "kind": "TokenRequest",
                "spec": { "audiences": ["vm-runner"], "expirationSeconds": 600 },
            })
        );
    }
    let events = capture.events_text();
    assert!(events.contains("rejected"), "{events}");
    for secret in [
        BOOTSTRAP,
        MINTED,
        "bootstrap-private-fixture-two",
        "minted-private-fixture-two",
    ] {
        assert!(!events.contains(secret));
    }
}

#[tokio::test]
async fn malformed_expired_or_reflected_issuance_fails_without_disclosing_response_bytes() {
    let root = tempfile::tempdir().expect("projection root");
    let tls = TestTls::new();
    projection(root.path(), "..one", BOOTSTRAP, tls.ca_pem.as_bytes());
    let cases = [
        ("201 Created", "not-json".to_owned(), "malformed"),
        (
            "201 Created",
            json!({"status":{"token":MINTED,"expirationTimestamp":MINTED}}).to_string(),
            "malformed",
        ),
        ("201 Created", issued(MINTED, -1).to_string(), "expired"),
        (
            "201 Created",
            json!({"status":{"token":MINTED}}).to_string(),
            "malformed",
        ),
        ("201 Created", issued("", 30).to_string(), "malformed"),
        (
            "201 Created",
            issued(BOOTSTRAP, 30).to_string(),
            "bootstrap-reflected",
        ),
        ("401 Unauthorized", BOOTSTRAP.to_owned(), "rejected"),
    ];
    let api = TokenApi::serving(
        &tls,
        cases
            .iter()
            .map(|(status, body, _)| response(status, body))
            .collect(),
    );
    let resolver = resolver(root.path(), &api.endpoint).await;
    let capture = CaptureLayer::workspace();
    async {
        for (_, _, category) in cases {
            let error = resolver
                .resolve(&drn())
                .await
                .expect_err("issuance refused");
            assert_eq!(error.category, category);
        }
    }
    .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
    .await;
    let events = capture.events_text();
    assert!(!events.contains(MINTED));
    assert!(!events.contains(BOOTSTRAP));
}

#[tokio::test]
async fn the_configured_ca_is_required_and_redirects_cannot_forward_bootstrap_authorization() {
    let root = tempfile::tempdir().expect("projection root");
    let tls = TestTls::new();
    projection(
        root.path(),
        "..one",
        BOOTSTRAP,
        TestTls::new().ca_pem.as_bytes(),
    );
    let api = TokenApi::serving(
        &tls,
        vec![response("201 Created", &issued(MINTED, 30).to_string())],
    );
    let resolver = resolver(root.path(), &api.endpoint).await;
    assert_eq!(
        resolver
            .resolve(&drn())
            .await
            .expect_err("wrong CA")
            .category,
        "transport"
    );
    assert!(
        api.requests.try_recv().is_err(),
        "no authenticated request on untrusted TLS"
    );
    drop(api);

    projection(root.path(), "..two", BOOTSTRAP, tls.ca_pem.as_bytes());
    let destination = TokenApi::serving(&tls, vec![response("200 OK", "{}")]);
    let api = TokenApi::serving(&tls, vec![format!(
        "HTTP/1.1 307 Temporary Redirect\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", destination.endpoint
    ).into_bytes()]);
    let resolver = self::resolver(root.path(), &api.endpoint).await;
    assert_eq!(
        resolver
            .resolve(&drn())
            .await
            .expect_err("redirect refused")
            .category,
        "rejected"
    );
    assert!(
        destination.requests.try_recv().is_err(),
        "bootstrap stayed at configured API"
    );
}

#[tokio::test]
async fn missing_bootstrap_and_invalid_ca_fail_before_issuance() {
    let root = tempfile::tempdir().expect("projection root");
    let tls = TestTls::new();
    projection(root.path(), "..one", BOOTSTRAP, tls.ca_pem.as_bytes());
    let resolver = resolver(root.path(), "https://127.0.0.1:9").await;
    std::fs::remove_file(root.path().join("..one/token")).expect("remove bootstrap");
    let map_path = Path::new("/run/map.yaml");
    validate_map(
        map(root.path(), "https://127.0.0.1:9"),
        map_path,
        resolver.expected_uid,
    )
    .await
    .expect("startup reads CA but neither contacts API nor reads bootstrap");
    assert_eq!(
        resolver
            .resolve(&drn())
            .await
            .expect_err("missing bootstrap")
            .category,
        "io"
    );
    projection(root.path(), "..two", BOOTSTRAP, b"not a CA");
    assert!(matches!(
        validate_map(
            map(root.path(), "https://127.0.0.1:9"),
            map_path,
            resolver.expected_uid
        )
        .await,
        Err(SecretMapError::Source {
            source: SourceError::Config
        })
    ));
}

#[tokio::test]
async fn token_request_config_is_strict_and_its_bootstrap_cannot_be_application_material() {
    let root = tempfile::tempdir().expect("projection root");
    for (key, value) in [
        ("endpoint", json!("http://127.0.0.1:443")),
        ("namespace", json!("../other")),
        ("serviceAccount", json!("other/token")),
        ("audience", json!("")),
        ("tokenKey", json!("../token")),
        ("bootstrapRoot", json!("relative")),
        ("expirationSeconds", json!(599)),
        ("timeoutMs", json!(120001)),
    ] {
        let mut value_source = source_value(root.path(), "https://kubernetes.default.svc");
        value_source[key] = value;
        let source: SecretSource = serde_json::from_value(value_source).expect("source shape");
        assert!(source.validate().is_err(), "invalid {key}");
    }
    let mut value = source_value(root.path(), "https://kubernetes.default.svc");
    value["audiences"] = json!(["other"]);
    assert!(serde_json::from_value::<SecretSource>(value).is_err());
    let file: SecretMapFile = serde_json::from_value(json!({
        "apiVersion":"dekopon.dev/secret-map/v1alpha1", "mapRevision":"test",
        "secrets":[
            {"drn":drn(), "source":source_value(root.path(), "https://kubernetes.default.svc"), "bindings":[]},
            {"drn":"drn:com.xrl:secret:test:bootstrap", "source": {
                "kind":"kubernetesProjection", "root":root.path(), "key":"token", "declaredOrigin":"serviceAccountToken"
            }, "bindings":[]}
        ]
    })).expect("map");
    let Err(SecretMapError::Validation { problems }) = validate_map(
        file,
        Path::new("/run/map.yaml"),
        rustix::process::geteuid().as_raw(),
    )
    .await
    else {
        panic!("bootstrap collision must fail before loading CA");
    };
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("bootstrap credential"))
    );
    assert!(problems.len() >= 3, "aggregate validation: {problems:?}");
}
