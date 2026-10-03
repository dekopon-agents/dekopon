use std::{
    any::TypeId,
    collections::HashMap,
    marker::PhantomData,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry, TestImports};
use dekopon_capability::{
    ExecutionConstraints, HttpConstraints, ProposedInvocation, broker::AuthorizationGate,
};
use dekopon_core::{Actor, AgentId, InvocationId, PrincipalId, TraceId};
use dekopon_http_host::LoopbackHttpsPin;
use dekopon_provider_sdk::provider::{
    self, Body, Header, HttpError, HttpErrorCode, NativeExit, NativeStdio, OpenedResponse, Port,
    Provider, Request, Response, StreamedRequest, StreamedResponse,
};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
type CacheKey = (TypeId, PathBuf);
type ComponentCache =
    Mutex<HashMap<CacheKey, Vec<(BrokerHostLimits, Arc<BrokerProviderRegistry>)>>>;
static COMPONENTS: OnceLock<ComponentCache> = OnceLock::new();

pub(crate) fn cached_registry<P: Provider>(
    component: PathBuf,
    limits: BrokerHostLimits,
) -> Result<Arc<BrokerProviderRegistry>, dekopon_broker_host::BrokerHostError> {
    let key = (TypeId::of::<P>(), component.clone());
    let cache = COMPONENTS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut entries = cache.lock();
    let configurations = entries.entry(key).or_default();
    if let Some((_, cached)) = configurations.iter().find(|(cached, _)| *cached == limits) {
        return Ok(Arc::clone(cached));
    }
    let loaded =
        Arc::new(runtime().block_on(BrokerProviderRegistry::load([component], limits.clone()))?);
    configurations.push((limits, Arc::clone(&loaded)));
    Ok(loaded)
}

pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("testkit runtime")
    })
}

/// One scripted HTTPS origin, with a single response and no credential or policy override.
#[derive(Clone, Debug)]
pub struct HttpScript {
    pub hostname: String,
    pub method: String,
    pub response: Response,
}

impl HttpScript {
    #[must_use]
    pub fn new(hostname: impl Into<String>, method: impl Into<String>, response: Response) -> Self {
        Self {
            hostname: hostname.into(),
            method: method.into(),
            response,
        }
    }
}

/// A test invocation builder for a real checked provider component.
pub struct Run<P: Provider> {
    component: PathBuf,
    limits: BrokerHostLimits,
    clock: Option<SystemTime>,
    http: Option<Result<ScriptServer, HarnessError>>,
    stdin: Option<Vec<u8>>,
    close_stdout_after: Option<usize>,
    _provider: PhantomData<P>,
}

/// A typed provider's real-component test entry point.
pub struct Harness<P: Provider>(PhantomData<P>);

impl<P: Provider> Harness<P> {
    /// Binds a caller-supplied artifact; the compiled component is shared per binary and provider type.
    #[must_use]
    pub fn get(component: impl AsRef<Path>) -> Run<P> {
        Run {
            component: component.as_ref().to_path_buf(),
            limits: BrokerHostLimits::default(),
            clock: None,
            http: None,
            stdin: None,
            close_stdout_after: None,
            _provider: PhantomData,
        }
    }

    /// Number of compiled registry identities for this provider in the current test binary.
    #[must_use]
    pub fn compiled_identities() -> usize {
        COMPONENTS.get().map_or(0, |cache| {
            cache
                .lock()
                .keys()
                .filter(|key| key.0 == TypeId::of::<P>())
                .count()
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("component host failed: {0}")]
    Host(#[from] dekopon_broker_host::BrokerHostError),
    #[error("invocation failed: {0}")]
    Invocation(#[from] Box<dekopon_broker_host::BrokerInvocationFailure>),
    #[error("invalid fixture: {0}")]
    Fixture(&'static str),
    #[error("authorization failed: {0}")]
    Authorization(#[from] dekopon_capability::AuthorizationError),
    #[error("invalid identifier: {0}")]
    Identifier(#[from] dekopon_core::IdentifierError),
    #[error("fixture I/O: {0}")]
    Io(#[from] std::io::Error),
}

impl<P: Provider> Run<P> {
    #[must_use]
    pub fn host_limits(mut self, limits: BrokerHostLimits) -> Self {
        self.limits = limits;
        self
    }

    #[must_use]
    pub fn clock(mut self, instant: SystemTime) -> Self {
        self.clock = Some(instant);
        self
    }

    #[must_use]
    pub fn http(mut self, script: HttpScript) -> Self {
        self.http = Some(serve_https(script));
        self
    }

    /// Supplies piped bytes; an empty slice is distinct from no pipe.
    #[must_use]
    pub fn stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(bytes.into());
        self
    }

    /// Closes the real stdout reader after at most `bytes` bytes; zero closes it before invocation.
    #[must_use]
    pub fn close_stdout_after(mut self, bytes: usize) -> Self {
        self.close_stdout_after = Some(bytes);
        self
    }

    /// The exact scripted origin to pass to the provider input.
    #[must_use]
    pub fn origin(&self) -> Option<&str> {
        self.http
            .as_ref()
            .and_then(|server| server.as_ref().ok())
            .map(|server| server.origin.as_str())
    }

    /// Calls a checked component and captures its exit status, stdout and stderr.
    ///
    /// # Errors
    /// Fails on missing artifacts, host refusals, TLS fixture errors or capture failures.
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "only a guest exit is a captured result; every present and future host error remains a refusal"
    )]
    pub fn call(self, capability: &str, input: Value) -> Result<ComponentOutput, HarnessError> {
        let component = self.component.canonicalize()?;
        let registry = cached_registry::<P>(component, self.limits.clone())?;
        let mut imports = TestImports {
            clock: self.clock,
            ..TestImports::default()
        };
        let mut http = None;
        let mut server = None;
        if let Some(script) = self.http {
            let started = script?;
            imports.loopback_https_pin = Some(started.pin.clone());
            http = Some(HttpConstraints {
                allowed_hosts: vec![started.authority.clone()],
                allowed_methods: vec![started.method.clone()],
                max_requests: 1,
                max_request_bytes: 64 * 1024,
                max_response_bytes: 64 * 1024,
                allow_plaintext_loopback: false,
                propagate_trace: false,
            });
            server = Some(started);
        }
        let capability = capability.parse()?;
        let provider = P::ID.parse()?;
        let proposal = ProposedInvocation::new(
            "typed-testkit-call".parse::<InvocationId>()?,
            capability,
            Actor::Agent {
                agent: "typed-testkit".parse::<AgentId>()?,
            },
            TraceId::new(*b"dekopon-testkit!")
                .map_err(|_error| HarnessError::Fixture("trace ID"))?,
            input,
        );
        let authorized = AuthorizationGate::new().authorize(
            proposal,
            provider,
            "testkit-decision".to_owned(),
            "testkit-broker".parse::<PrincipalId>()?,
            "testkit-policy".to_owned(),
            ExecutionConstraints {
                asset: None,
                timeout_ms: self
                    .limits
                    .max_timeout
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX),
                http,
                storage: None,
                secret_use: None,
            },
        )?;
        let (host_end, stdout) = std::os::unix::net::UnixStream::pair()?;
        let stdin = self
            .stdin
            .map(|bytes| {
                let (host, mut feeder) = std::os::unix::net::UnixStream::pair()?;
                Ok::<_, std::io::Error>((host, move || {
                    use std::io::Write;
                    drop(feeder.write_all(&bytes));
                }))
            })
            .transpose()?;
        let (stdin_end, feeder) = match stdin {
            Some((end, feeder)) => (Some(end.into()), Some(feeder)),
            None => (None, None),
        };
        let assets = dekopon_broker_host::asset::AssetInputs {
            streams: Some(dekopon_broker_host::Streams {
                stdin: stdin_end,
                stdout: host_end.into(),
            }),
            ..Default::default()
        };
        let (result, captured, fed) = runtime().block_on(async {
            let close_after = self.close_stdout_after;
            let (closed, ready) = tokio::sync::oneshot::channel();
            let capture = tokio::task::spawn_blocking(move || {
                use std::io::Read;
                if close_after == Some(0) {
                    drop(stdout);
                    closed.send(()).map_err(|_cancelled| {
                        std::io::Error::other("stdout reader startup cancelled")
                    })?;
                    return Ok::<_, std::io::Error>(Vec::new());
                }
                closed.send(()).map_err(|_cancelled| {
                    std::io::Error::other("stdout reader startup cancelled")
                })?;
                let mut bytes = Vec::new();
                let limit = close_after
                    .unwrap_or(16 * 1024 * 1024 + 1)
                    .min(16 * 1024 * 1024 + 1);
                stdout.take(limit as u64).read_to_end(&mut bytes)?;
                Ok::<_, std::io::Error>(bytes)
            });
            ready
                .await
                .map_err(|_closed| HarnessError::Fixture("stdout reader startup"))?;
            let invoke =
                registry.invoke_with_test_imports(authorized, None, None, assets, Some(&imports));
            let feed = feeder.map(|f| tokio::task::spawn_blocking(f));
            let (result, captured) = tokio::join!(invoke, capture);
            let fed = match feed {
                Some(task) => Some(task.await),
                None => None,
            };
            Ok::<_, HarnessError>((result, captured, fed))
        })?;
        drop(server);
        if fed.is_some_and(|result| result.is_err()) {
            return Err(HarnessError::Fixture("stdin feeder"));
        }
        let stdout = captured.map_err(|_error| HarnessError::Fixture("stdout capture"))??;
        if stdout.len() > 16 * 1024 * 1024 {
            return Err(HarnessError::Fixture("stdout exceeds capture limit"));
        }
        let (status, stderr, http_calls) = match result {
            Ok(output) => (0, output.stderr, output.http_calls),
            Err(failure) => match *failure.error {
                dekopon_broker_host::BrokerHostError::ProviderFailure {
                    status, stderr, ..
                } => (status, stderr, failure.http_calls),
                error => {
                    return Err(HarnessError::Invocation(Box::new(
                        dekopon_broker_host::BrokerInvocationFailure {
                            error: Box::new(error),
                            http_calls: failure.http_calls,
                            storage: failure.storage,
                        },
                    )));
                }
            },
        };
        Ok(ComponentOutput {
            status,
            stdout,
            stderr,
            http_calls,
        })
    }
}

/// Captured terminal result and HTTP evidence from one real component invocation.
#[derive(Debug)]
pub struct ComponentOutput {
    pub status: u8,
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub http_calls: Vec<dekopon_http_host::HttpCallEvidence>,
}

struct ScriptServer {
    authority: String,
    origin: String,
    method: String,
    pin: LoopbackHttpsPin,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ScriptServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn response_head(response: &Response) -> Result<Vec<u8>, HarnessError> {
    let mut bytes = format!("HTTP/1.1 {} OK\r\n", response.status).into_bytes();
    let expected_length = response.body.len().to_string();
    let mut has_length = false;
    for header in &response.headers {
        Header::new(header.name.as_str(), header.value.as_slice())
            .map_err(|_error| HarnessError::Fixture("invalid scripted response header"))?;
        let name = header.name.to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "connection"
                | "transfer-encoding"
                | "trailer"
                | "upgrade"
                | "proxy-connection"
                | "keep-alive"
        ) {
            return Err(HarnessError::Fixture(
                "scripted response cannot set hop-by-hop headers",
            ));
        }
        if name == "content-length" {
            if has_length || header.value != expected_length.as_bytes() {
                return Err(HarnessError::Fixture(
                    "scripted content length must match the body",
                ));
            }
            has_length = true;
        }
        bytes.extend_from_slice(header.name.as_bytes());
        bytes.extend_from_slice(b": ");
        bytes.extend_from_slice(&header.value);
        bytes.extend_from_slice(b"\r\n");
    }
    if !has_length {
        bytes.extend_from_slice(format!("Content-Length: {expected_length}\r\n").as_bytes());
    }
    bytes.extend_from_slice(b"Connection: close\r\n\r\n");
    Ok(bytes)
}

fn serve_https(script: HttpScript) -> Result<ScriptServer, HarnessError> {
    let response_head = response_head(&script.response)?;
    if script.hostname.is_empty()
        || !script
            .hostname
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    {
        return Err(HarnessError::Fixture("HTTPS hostname must be a DNS name"));
    }
    let certified = rcgen::generate_simple_self_signed(vec![script.hostname.clone()])
        .map_err(|_error| HarnessError::Fixture("certificate generation"))?;
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_error| HarnessError::Fixture("TLS versions"))?
    .with_no_client_auth()
    .with_single_cert(
        vec![certified.cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()).into(),
    )
    .map_err(|_error| HarnessError::Fixture("TLS certificate"))?;
    let listener = runtime().block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))?;
    let address: SocketAddr = listener.local_addr()?;
    let authority = format!("{}:{}", script.hostname, address.port());
    let pin = LoopbackHttpsPin::new(&authority, address, certified.cert.pem().into_bytes())
        .map_err(HarnessError::Fixture)?;
    let origin = format!("https://{authority}");
    let method = script.method.clone();
    let task = runtime().spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
            if let Ok(mut tls) = acceptor.accept(stream).await {
                let mut request = [0; 4096];
                if tls.read(&mut request).await.is_ok() {
                    let body = &script.response.body;
                    if tls.write_all(&response_head).await.is_ok()
                        && tls.write_all(body).await.is_ok()
                    {
                        drop(tls.shutdown().await);
                    }
                }
            }
        }
    });
    Ok(ScriptServer {
        authority,
        origin,
        method,
        pin,
        task,
    })
}

/// Native typed dispatch with fake clock and buffered HTTP imports, recording every request.
pub struct Native<P: Provider> {
    clock: Option<SystemTime>,
    monotonic: u64,
    entropy: Option<Vec<u8>>,
    http: Option<HttpScript>,
    stdin: Option<Vec<u8>>,
    requests: Arc<Mutex<Vec<Request>>>,
    _provider: PhantomData<P>,
}

impl<P: Provider> Default for Native<P> {
    fn default() -> Self {
        Self {
            clock: None,
            monotonic: 0,
            entropy: None,
            http: None,
            stdin: None,
            requests: Arc::new(Mutex::new(Vec::new())),
            _provider: PhantomData,
        }
    }
}

impl<P: Provider> Native<P> {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    #[must_use]
    pub fn clock(mut self, instant: SystemTime) -> Self {
        self.clock = Some(instant);
        self
    }
    #[must_use]
    pub fn http(mut self, script: HttpScript) -> Self {
        self.http = Some(script);
        self
    }
    #[must_use]
    pub fn monotonic(mut self, nanos: u64) -> Self {
        self.monotonic = nanos;
        self
    }
    #[must_use]
    pub fn entropy(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.entropy = Some(bytes.into());
        self
    }
    /// Supplies piped bytes to the native provider invocation.
    #[must_use]
    pub fn stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(bytes.into());
        self
    }
    #[must_use]
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().clone()
    }
    /// Runs one call with nothing piped in, capturing its stdout, stderr and exit status.
    #[must_use]
    pub fn call(&self, capability: &str, input: &str) -> NativeOutput {
        let port = FakePort {
            clock: self.clock,
            monotonic: self.monotonic,
            entropy: self.entropy.clone(),
            entropy_cursor: 0,
            http: self.http.clone(),
            requests: Arc::clone(&self.requests),
        };
        let stdout = Captured::default();
        let stdio = NativeStdio {
            stdin: self.stdin.as_ref().map(|bytes| {
                Box::new(std::io::Cursor::new(bytes.clone())) as Box<dyn std::io::Read>
            }),
            stdout: Box::new(stdout.clone()),
        };
        let NativeExit { status, stderr } = provider::with_port(port, || {
            provider::invoke_native::<P>(capability, input, stdio)
        });
        NativeOutput {
            status,
            stdout: std::mem::take(&mut *stdout.0.lock()),
            stderr,
        }
    }
}

/// What a native call wrote and how it exited.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeOutput {
    pub status: u8,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct FakePort {
    clock: Option<SystemTime>,
    monotonic: u64,
    entropy: Option<Vec<u8>>,
    entropy_cursor: usize,
    http: Option<HttpScript>,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl Port for FakePort {
    fn now_unix_millis(&mut self) -> u64 {
        self.clock
            .unwrap_or_else(SystemTime::now)
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
    fn now_nanos(&mut self) -> u64 {
        self.monotonic
    }
    fn fill_random(
        &mut self,
        out: &mut [u8],
    ) -> Result<(), dekopon_provider_sdk::random::RandomError> {
        if let Some(entropy) = &self.entropy {
            let end = self.entropy_cursor.saturating_add(out.len());
            let source = entropy
                .get(self.entropy_cursor..end)
                .ok_or(dekopon_provider_sdk::random::RandomError::SourceUnavailable)?;
            out.copy_from_slice(source);
            self.entropy_cursor = end;
        } else {
            out.fill(0xa5);
        }
        Ok(())
    }
    fn settings(&mut self) -> Option<String> {
        None
    }
    fn send(&mut self, request: Request) -> Result<Response, HttpError> {
        self.requests.lock().push(request.clone());
        let script = self.http.as_ref().ok_or_else(|| HttpError {
            code: HttpErrorCode::Denied,
            message: "no HTTP script".to_owned(),
        })?;
        if request.method != script.method
            || !request
                .uri
                .starts_with(&format!("https://{}/", script.hostname))
        {
            return Err(HttpError {
                code: HttpErrorCode::Denied,
                message: "origin is not scripted".to_owned(),
            });
        }
        Ok(script.response.clone())
    }
    fn open(&mut self, request: Request) -> Result<OpenedResponse, HttpError> {
        let response = self.send(request)?;
        Ok(OpenedResponse {
            status: response.status,
            headers: response.headers,
            body: Body::native(std::io::Cursor::new(response.body)),
        })
    }
    fn stream(&mut self, _: StreamedRequest<'_>) -> Result<StreamedResponse, HttpError> {
        Err(HttpError {
            code: HttpErrorCode::Denied,
            message: "native asset streams require Harness<P>".to_owned(),
        })
    }
}
