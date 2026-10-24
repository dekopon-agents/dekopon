use dekopon_test_support::content_length;
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::pki_types::PrivatePkcs8KeyDer;
use std::{
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

pub(crate) struct TestTls {
    pub(crate) ca_pem: String,
    config: Arc<rustls::ServerConfig>,
}

impl TestTls {
    pub(crate) fn new() -> Self {
        let now = time::OffsetDateTime::now_utc();
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

pub(crate) struct TokenApi {
    pub(crate) endpoint: String,
    pub(crate) requests: mpsc::Receiver<Vec<u8>>,
    task: Option<thread::JoinHandle<()>>,
}

impl TokenApi {
    pub(crate) fn serving(tls: &TestTls, responses: Vec<Vec<u8>>) -> Self {
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

pub(crate) fn response(status: &str, body: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes()
}
