#![cfg_attr(test, allow(clippy::unwrap_used))]
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "tests spawn, join and drain freely; production sites carry their own expectation"
)]
use std::{
    io::{ErrorKind, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

mod capture;
mod driver;
mod model;
#[cfg(feature = "agent-runtime")]
mod runtime;
mod transcripts;

pub use capture::{CaptureLayer, Record};
pub use driver::{
    DriverCall, FailureKind, ProgressCall, RecordingCancelButton, RecordingDriver,
    RecordingProgress, RecordingReaction, RecordingStatus, RecordingStream, RecordingTyping,
    StreamCall,
};
pub use model::{ScriptedStreamModel, scripted_text};
#[cfg(feature = "agent-runtime")]
pub use runtime::BlockedRuntime;
pub use transcripts::{
    CODEX_RESPONSES_TOOL_CALL, CODEX_RESPONSES_TWO_DELTAS, OPENAI_CHAT_COMPLETIONS_TOOL_CALL,
    OPENAI_CHAT_COMPLETIONS_TWO_DELTAS,
};

const FIXTURE_TIMEOUT: Duration = Duration::from_secs(5);

const POOLED_POLL_INTERVAL: Duration = Duration::from_millis(1);

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

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root resolves from this crate's manifest directory")
}

pub async fn shutdown_on<T, E>(signal: impl Future<Output = Result<T, E>>) {
    #[allow(
        clippy::let_underscore_must_use,
        reason = "a dropped sender is how a test's shutdown fires when the body panics or returns \
                  early, so the receive error is a normal outcome rather than a discarded cause"
    )]
    let _ = signal.await;
}

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeEntry {
    pub relative: PathBuf,
    pub mode: u32,
    pub len: u64,
    pub is_dir: bool,
    pub contents: Vec<u8>,
}

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

pub struct LoopbackServer {
    authority: String,
    requests: Receiver<Vec<u8>>,
    handle: Option<JoinHandle<()>>,
}

impl LoopbackServer {
    #[must_use]
    pub fn once(response: &[u8]) -> Self {
        Self::serving(response, 1)
    }

    #[must_use]
    pub fn serving(response: &[u8], calls: usize) -> Self {
        Self::sequence(std::iter::repeat_n(response.to_vec(), calls))
    }

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
            thread::sleep(FIXTURE_TIMEOUT);
        })
        .expect("bind loopback fixture")
    }

    #[must_use]
    pub fn pooled(response: &[u8], calls: usize) -> Self {
        let response = response.to_vec();
        Self::bind_with("127.0.0.1:0", move |listener, sender| {
            listener
                .set_nonblocking(true)
                .expect("poll the fixture listener");
            let mut connection: Option<TcpStream> = None;
            let mut served = 0;
            let mut deadline = Instant::now() + FIXTURE_TIMEOUT;
            while served < calls && Instant::now() < deadline {
                if let Some(stream) = connection.as_mut() {
                    match peer_state(stream) {
                        PeerState::Request => {
                            let request = read_request(stream);
                            if request.is_empty() {
                                connection = None;
                                continue;
                            }
                            sender.send(request).expect("record fixture request");
                            stream.write_all(&response).expect("write fixture response");
                            stream.flush().expect("flush fixture response");
                            served += 1;
                            deadline = Instant::now() + FIXTURE_TIMEOUT;
                            continue;
                        }
                        PeerState::Closed => {
                            connection = None;
                            continue;
                        }
                        PeerState::Idle => {}
                    }
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream
                            .set_read_timeout(Some(FIXTURE_TIMEOUT))
                            .expect("set fixture timeout");
                        connection = Some(stream);
                        deadline = Instant::now() + FIXTURE_TIMEOUT;
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(POOLED_POLL_INTERVAL);
                    }
                    Err(_) => return,
                }
            }
        })
        .expect("bind loopback fixture")
    }

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
        let authority = listener.local_addr().expect("fixture address").to_string();
        let (sender, requests) = mpsc::channel();
        let handle = thread::spawn(move || serve(listener, &sender));
        Some(Self {
            authority,
            requests,
            handle: Some(handle),
        })
    }

    #[must_use]
    pub fn authority(&self) -> &str {
        &self.authority
    }

    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.authority)
    }

    #[must_use]
    pub fn request(&self) -> Vec<u8> {
        self.requests
            .recv_timeout(FIXTURE_TIMEOUT)
            .expect("the fixture recorded a request")
    }

    #[must_use]
    pub fn request_text(&self) -> String {
        String::from_utf8(self.request()).expect("the recorded request is UTF-8")
    }

    #[must_use]
    pub fn recorded(&self) -> Vec<Vec<u8>> {
        self.requests.try_iter().collect()
    }

    pub fn join(mut self) {
        if let Some(handle) = self.handle.take() {
            handle.join().expect("fixture server exits");
        }
    }
}

enum PeerState {
    Request,
    Idle,
    Closed,
}

fn peer_state(stream: &mut TcpStream) -> PeerState {
    stream
        .set_nonblocking(true)
        .expect("poll the fixture connection");
    let mut probe = [0_u8; 1];
    let state = match stream.peek(&mut probe) {
        Ok(0) => PeerState::Closed,
        Ok(_) => PeerState::Request,
        Err(error) if error.kind() == ErrorKind::WouldBlock => PeerState::Idle,
        Err(_) => PeerState::Closed,
    };
    stream
        .set_nonblocking(false)
        .expect("restore the fixture connection to blocking reads");
    state
}

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
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
