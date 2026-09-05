//! Scaffolding every Dekopon test suite needs, defined once.
//!
//! Nine copies of the provider-fixture path, thirteen hand-rolled loopback HTTP servers, nine
//! `tracing` capture layers, six directory walkers, and twelve identical `#[allow]` blocks had
//! already drifted apart from each other: one loopback server truncated a request to its
//! `content-length` and its twin did not, one capture layer filtered non-workspace callsites and
//! its twin did not. A fixture that quietly differs between two suites is worse than a fixture
//! that is merely duplicated, because the difference is what the two suites then disagree about.
//!
//! This crate is `publish = false` and is only ever a path `[dev-dependencies]` entry. It depends
//! on no crate in this workspace — in particular on nothing under the broker boundary — so adding
//! it to `dekopond`'s dev-dependencies cannot put a broker crate in the gateway's dependency tree.

use std::{
    io::{Read as _, Write as _},
    net::TcpListener,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::Duration,
};

mod capture;

pub use capture::{CaptureLayer, CaptureSession, Record};

/// How long a fixture waits on a peer before deciding the test, not the network, is stuck.
const FIXTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// The path to one built provider component under `examples/providers/`.
///
/// Three of those components are fetched rather than built, so the failure worth naming is the
/// precondition an operator forgot rather than `NotFound` on a path they have never seen.
///
/// # Panics
///
/// When the component is absent, naming the script that fetches it.
#[must_use]
pub fn provider_fixture(name: &str) -> PathBuf {
    let path = workspace_root().join("examples/providers").join(name);
    assert!(
        path.exists(),
        "provider fixture {} is missing; run ci/fetch-external-provider-components.sh \
         examples/providers first",
        path.display()
    );
    path
}

/// The repository root, resolved from this crate's own manifest rather than the caller's.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root resolves from this crate's manifest directory")
}

/// Awaits a shutdown signal, treating a dropped sender as the signal.
///
/// Twelve server fixtures wrote this future inline with a byte-identical `#[allow]` and reason.
/// The discard is correct exactly once — here — because a test body that panicked or returned
/// early drops its sender, and "stop" is what both outcomes mean.
pub async fn shutdown_on<T, E>(signal: impl Future<Output = Result<T, E>>) {
    #[allow(
        clippy::let_underscore_must_use,
        reason = "a dropped sender is how a test's shutdown fires when the body panics or returns \
                  early, so the receive error is a normal outcome rather than a discarded cause"
    )]
    let _ = signal.await;
}

/// The `content-length` a request or response header block declares, or zero when it declares none.
#[must_use]
pub fn content_length(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0)
}

/// One entry of a directory tree, as [`snapshot_tree`] recorded it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeEntry {
    /// Path relative to the snapshot root.
    pub relative: PathBuf,
    /// Unix mode bits from `symlink_metadata`.
    pub mode: u32,
    /// Length from `symlink_metadata`; for a directory this is the directory's own size.
    pub len: u64,
    /// Whether the entry is a directory, without following a symlink to one.
    pub is_dir: bool,
    /// File contents, empty for anything that is not a regular file.
    pub contents: Vec<u8>,
}

/// Every entry under `root`, depth first, each directory's children sorted by file name.
///
/// Symlinks are recorded and never followed: half the callers are proving that a substituted
/// symlink was *not* traversed, and a walker that followed one would assert the opposite of what
/// it was written for.
///
/// # Panics
///
/// When the tree cannot be read, which in a test is the fixture failing rather than the subject.
#[must_use]
pub fn snapshot_tree(root: &Path) -> Vec<TreeEntry> {
    fn visit(root: &Path, path: &Path, output: &mut Vec<TreeEntry>) {
        let mut entries = std::fs::read_dir(path)
            .expect("snapshot directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("snapshot entries");
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).expect("snapshot metadata");
            let contents = if metadata.is_file() {
                std::fs::read(&path).expect("snapshot file")
            } else {
                Vec::new()
            };
            output.push(TreeEntry {
                relative: path
                    .strip_prefix(root)
                    .expect("snapshot entry is under the snapshot root")
                    .to_path_buf(),
                #[cfg(unix)]
                mode: std::os::unix::fs::MetadataExt::mode(&metadata),
                #[cfg(not(unix))]
                mode: 0,
                len: metadata.len(),
                is_dir: metadata.is_dir(),
                contents,
            });
            if metadata.is_dir() {
                visit(root, &path, output);
            }
        }
    }
    let mut output = Vec::new();
    visit(root, root, &mut output);
    output
}

/// A loopback HTTP server that records what it was asked and answers a scripted reply.
///
/// Every constructor records complete requests: headers plus exactly the `content-length` bytes
/// the peer declared, truncated to it. That is the behavior the broker-host copy had and the
/// broker copy did not, and it is the one worth keeping — a recorded request carrying whatever
/// else happened to be in the socket buffer makes a body assertion depend on packet timing.
pub struct LoopbackServer {
    authority: String,
    requests: Receiver<Vec<u8>>,
    handle: Option<JoinHandle<()>>,
}

impl LoopbackServer {
    /// Answers `response` to exactly one request on one connection.
    #[must_use]
    pub fn once(response: &[u8]) -> Self {
        Self::serving(response, 1)
    }

    /// Answers `response` to `calls` requests, one connection each.
    ///
    /// Requests arrive on the channel in the order they were served, which is the order the client
    /// dispatched them.
    #[must_use]
    pub fn serving(response: &[u8], calls: usize) -> Self {
        Self::sequence(std::iter::repeat_n(response.to_vec(), calls))
    }

    /// Answers each response in order, one connection each.
    #[must_use]
    pub fn sequence(responses: impl IntoIterator<Item = Vec<u8>>) -> Self {
        let responses = responses.into_iter().collect::<Vec<_>>();
        Self::bind_with("127.0.0.1:0", move |listener, sender| {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                stream
                    .set_read_timeout(Some(FIXTURE_TIMEOUT))
                    .expect("set fixture timeout");
                sender
                    .send(read_request(&mut stream))
                    .expect("record fixture request");
                stream.write_all(&response).expect("write fixture response");
                stream.flush().expect("flush fixture response");
            }
        })
        .expect("bind loopback fixture")
    }

    /// Accepts one request, records it, and never answers, so a call is dispatched but unresolved.
    #[must_use]
    pub fn stalled() -> Self {
        Self::bind_with("127.0.0.1:0", |listener, sender| {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream
                .set_read_timeout(Some(FIXTURE_TIMEOUT))
                .expect("set fixture timeout");
            #[allow(
                clippy::let_underscore_must_use,
                reason = "this fixture exists to hang rather than to be read; a test that gave up \
                          on the channel and dropped the receiver is a normal way for this send \
                          to fail"
            )]
            let _ = sender.send(read_request(&mut stream));
            // Hold the connection open past the caller's deadline without ever responding.
            thread::sleep(FIXTURE_TIMEOUT);
        })
        .expect("bind loopback fixture")
    }

    /// Answers `response` to `calls` requests across however many connections the client opens.
    ///
    /// A pooling client may or may not have checked its connection back in as idle by the time the
    /// next request starts, which is a race decided by a background task rather than by anything
    /// the caller controls. Losing it closes the connection instead of hanging it, so this accepts
    /// a follow-up connection rather than treating that EOF as a fixture failure.
    #[must_use]
    pub fn pooled(response: &[u8], calls: usize) -> Self {
        let response = response.to_vec();
        Self::bind_with("127.0.0.1:0", move |listener, sender| {
            let mut served = 0;
            while served < calls {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                stream
                    .set_read_timeout(Some(FIXTURE_TIMEOUT))
                    .expect("set fixture timeout");
                while served < calls {
                    let request = read_request(&mut stream);
                    if request.is_empty() {
                        break;
                    }
                    sender.send(request).expect("record fixture request");
                    stream.write_all(&response).expect("write fixture response");
                    stream.flush().expect("flush fixture response");
                    served += 1;
                }
            }
        })
        .expect("bind loopback fixture")
    }

    /// Answers `response` once on an explicit bind address.
    ///
    /// `None` when that address family is unavailable on this host, which is the only reason
    /// binding a loopback port fails and the only reason a caller needs to tell the two apart.
    #[must_use]
    pub fn bound(bind: &str, response: &[u8]) -> Option<Self> {
        let response = response.to_vec();
        Self::bind_with(bind, move |listener, sender| {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream
                .set_read_timeout(Some(FIXTURE_TIMEOUT))
                .expect("set fixture timeout");
            sender
                .send(read_request(&mut stream))
                .expect("record fixture request");
            stream.write_all(&response).expect("write fixture response");
            stream.flush().expect("flush fixture response");
        })
    }

    fn bind_with(
        bind: &str,
        serve: impl FnOnce(TcpListener, &mpsc::Sender<Vec<u8>>) + Send + 'static,
    ) -> Option<Self> {
        let listener = TcpListener::bind(bind).ok()?;
        // `SocketAddr`'s rendering is the authority grammar a host allowlist uses, brackets and
        // all, so an IPv6 fixture agrees with the constraint that permits it.
        let authority = listener.local_addr().expect("fixture address").to_string();
        let (sender, requests) = mpsc::channel();
        let handle = thread::spawn(move || serve(listener, &sender));
        Some(Self {
            authority,
            requests,
            handle: Some(handle),
        })
    }

    /// The `host:port` this fixture answers on.
    #[must_use]
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// The plaintext base URL this fixture answers on.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.authority)
    }

    /// The next recorded request, waiting for it to arrive.
    ///
    /// # Panics
    ///
    /// When no request arrives before the fixture deadline.
    #[must_use]
    pub fn request(&self) -> Vec<u8> {
        self.requests
            .recv_timeout(FIXTURE_TIMEOUT)
            .expect("the fixture recorded a request")
    }

    /// The next recorded request as text.
    ///
    /// # Panics
    ///
    /// When no request arrives before the fixture deadline.
    #[must_use]
    pub fn request_text(&self) -> String {
        String::from_utf8(self.request()).expect("the recorded request is UTF-8")
    }

    /// Every request recorded so far, without waiting for more.
    #[must_use]
    pub fn recorded(&self) -> Vec<Vec<u8>> {
        self.requests.try_iter().collect()
    }

    /// Waits for the serving thread to finish.
    ///
    /// # Panics
    ///
    /// When the fixture thread panicked, which is a fixture failure rather than a test result.
    pub fn join(mut self) {
        if let Some(handle) = self.handle.take() {
            handle.join().expect("fixture server exits");
        }
    }
}

/// Reads one complete request: headers, then exactly the declared `content-length` bytes.
///
/// Returns what it has when the peer closes first, which is how a pooled connection ends.
fn read_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    let mut expected = None;
    loop {
        if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            let complete = header_end + 4 + content_length(&request[..header_end + 4]);
            expected = Some(complete);
            if request.len() >= complete {
                break;
            }
        }
        let Ok(read) = stream.read(&mut buffer) else {
            break;
        };
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
    }
    if let Some(expected) = expected {
        request.truncate(expected);
    }
    request
}
