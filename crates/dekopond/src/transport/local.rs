//! A development transport on an owner-only Unix socket.
//!
//! # Trust
//!
//! This transport **trusts its local caller to declare a subject**. That is the whole point of it —
//! it exists so a developer can drive a routed session without a Slack workspace — and it is also
//! the reason it is not a production transport: any process that can open the socket can claim to
//! be any subject.
//!
//! What it does *not* do is grant anything. The declared subject is still only a claim carried into
//! the broker's `invokeFor`, and the broker still needs an attestor grant covering that namespace
//! plus an owner-controlled mapping before it resolves to a principal. A caller here can therefore
//! reach exactly the authority the owner already configured for the subject it names, and nothing
//! else. The socket's own `0600` mode is what keeps that reachable only by the owner's UID, which
//! is the same trust domain the broker socket already lives in.

use std::{
    collections::BTreeMap,
    fs, io,
    os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use dekopon_core::ExternalSubject;
use futures_util::future::BoxFuture;
use serde::Deserialize;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    net::{UnixListener, UnixStream},
    sync::mpsc,
};

use crate::transport::{
    ChatReplier, ChatTransport, ConversationKind, InboundMessage, ReplyTarget, TransportError,
    TransportEvent, TransportIdentity, bound_inbound,
};

/// Longest line the development transport accepts, matching the inbound text bound plus envelope.
const MAX_LINE_BYTES: u64 = 64 * 1024;

/// One line-delimited JSON request from a local caller.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalRequest {
    /// A canonical external subject the caller claims to be.
    subject: ExternalSubject,
    #[serde(default = "default_channel")]
    channel: String,
    text: String,
}

fn default_channel() -> String {
    "dev".to_owned()
}

pub(crate) struct LocalTransport {
    name: String,
    socket_path: PathBuf,
    listener: Option<UnixListener>,
    guard: Option<SocketGuard>,
    inbound: Option<mpsc::UnboundedReceiver<InboundMessage>>,
    sender: mpsc::UnboundedSender<InboundMessage>,
    replier: Arc<LocalReplier>,
    connections: AtomicU64,
}

impl LocalTransport {
    pub(crate) fn new(name: String, socket_path: PathBuf) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            name,
            socket_path,
            listener: None,
            guard: None,
            inbound: Some(receiver),
            sender,
            replier: Arc::new(LocalReplier::default()),
            connections: AtomicU64::new(1),
        }
    }

    /// Serves one connection: JSON lines in, JSON lines out, until the caller hangs up.
    fn serve(&self, stream: UnixStream) {
        let connection = self.connections.fetch_add(1, Ordering::Relaxed);
        let (outbound_send, mut outbound_receive) = mpsc::unbounded_channel::<String>();
        self.replier.register(connection, outbound_send);

        let name = self.name.clone();
        let inbound = self.sender.clone();
        let replier = Arc::clone(&self.replier);
        tokio::spawn(async move {
            let (reader, mut writer) = stream.into_split();
            // `Take` re-armed per line rather than per connection: a line ceiling has to bound the
            // buffer *before* it is allocated, and a connection ceiling would end a long dev
            // session after enough short requests.
            let mut reader = BufReader::new(reader).take(MAX_LINE_BYTES);
            let writes = tokio::spawn(async move {
                while let Some(reply) = outbound_receive.recv().await {
                    let line = format!("{reply}\n");
                    if writer.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                    let _ = writer.flush().await;
                }
            });
            loop {
                let mut line = Vec::new();
                reader.set_limit(MAX_LINE_BYTES);
                match reader.read_until(b'\n', &mut line).await {
                    Ok(0) => break,
                    Ok(_) if !line.ends_with(b"\n") => {
                        tracing::debug!(
                            event = "gateway_local_request_rejected",
                            transport = %name,
                            reason = "line-too-long"
                        );
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
                let Ok(text) = std::str::from_utf8(&line) else {
                    continue;
                };
                if text.trim().is_empty() {
                    continue;
                }
                let Ok(request) = serde_json::from_str::<LocalRequest>(text) else {
                    tracing::debug!(
                        event = "gateway_local_request_rejected",
                        transport = %name,
                        reason = "malformed-request"
                    );
                    continue;
                };
                let message = InboundMessage {
                    transport: name.clone(),
                    subject: request.subject,
                    channel: request.channel.clone(),
                    thread: None,
                    // The caller names its own conversation, and `channel` defaults to `dev` when
                    // it does not. There is nothing else here to derive one from — a local session
                    // has no threads, and the connection number would restart the conversation
                    // every time a developer reconnected.
                    conversation_id: request.channel,
                    text: bound_inbound(&request.text),
                    // The development transport speaks line-delimited JSON and carries no files.
                    assets: Vec::new(),
                    // Always a direct message: a local caller is talking to the daemon
                    // one-to-one, so there is no ambient traffic to filter and no mention to
                    // require. Channel routes are a chat-service concept.
                    conversation: ConversationKind::DirectMessage,
                    addressed: Some(true),
                    reply: ReplyTarget::Local { connection },
                    activity: None,
                };
                if inbound.send(message).is_err() {
                    break;
                }
            }
            replier.forget(connection);
            writes.abort();
        });
    }
}

impl ChatTransport for LocalTransport {
    fn name(&self) -> &str {
        &self.name
    }

    fn connect(&mut self) -> BoxFuture<'_, Result<TransportIdentity, TransportError>> {
        Box::pin(async move {
            let (listener, guard) = bind(&self.socket_path)?;
            self.listener = Some(listener);
            self.guard = Some(guard);
            Ok(TransportIdentity::default())
        })
    }

    fn next(&mut self) -> BoxFuture<'_, Result<TransportEvent, TransportError>> {
        Box::pin(async move {
            loop {
                let listener = self.listener.as_ref().ok_or(TransportError::Closed)?;
                let receiver = self.inbound.as_mut().ok_or(TransportError::Closed)?;
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.map_err(TransportError::Io)?;
                        self.serve(stream);
                    }
                    message = receiver.recv() => {
                        return message
                            .map(Box::new)
                            .map(TransportEvent::Message)
                            .ok_or(TransportError::Closed);
                    }
                }
            }
        })
    }

    fn replier(&self) -> Arc<dyn ChatReplier> {
        Arc::clone(&self.replier) as Arc<dyn ChatReplier>
    }
}

/// Routes an answer back to the connection its request arrived on.
#[derive(Default)]
pub(crate) struct LocalReplier {
    connections: Mutex<BTreeMap<u64, mpsc::UnboundedSender<String>>>,
}

impl LocalReplier {
    fn register(&self, connection: u64, sender: mpsc::UnboundedSender<String>) {
        self.connections
            .lock()
            .expect("local connection registry")
            .insert(connection, sender);
    }

    fn forget(&self, connection: u64) {
        self.connections
            .lock()
            .expect("local connection registry")
            .remove(&connection);
    }
}

impl ChatReplier for LocalReplier {
    fn reply(
        &self,
        target: ReplyTarget,
        text: String,
    ) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            let ReplyTarget::Local { connection } = target else {
                return Err(TransportError::Response);
            };
            let line = serde_json::json!({ "reply": text }).to_string();
            let sender = self
                .connections
                .lock()
                .expect("local connection registry")
                .get(&connection)
                .cloned();
            // A caller that hung up mid-session is not an error worth failing a session over: the
            // answer simply has nowhere to go.
            match sender {
                Some(sender) => sender.send(line).map_err(|_| TransportError::Closed),
                None => Err(TransportError::Closed),
            }
        })
    }
}

/// Binds an owner-only socket under a private parent, refusing anything it did not create.
///
/// These are `dekopon-brokerd`'s socket checks, kept rather than simplified: the development
/// transport carries a subject claim, so a socket another user could reach or replace would let
/// them make that claim.
fn bind(path: &Path) -> Result<(UnixListener, SocketGuard), TransportError> {
    let uid = rustix::process::geteuid().as_raw();
    let parent = path
        .parent()
        .ok_or_else(|| TransportError::InsecureSocket {
            path: path.display().to_string(),
        })?;
    let parent = fs::canonicalize(parent).map_err(TransportError::Io)?;
    let metadata = fs::symlink_metadata(&parent).map_err(TransportError::Io)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(TransportError::InsecureSocket {
            path: parent.display().to_string(),
        });
    }
    match fs::symlink_metadata(path) {
        Ok(existing) => {
            if !existing.file_type().is_socket()
                || existing.uid() != uid
                || existing.permissions().mode() & 0o077 != 0
                || existing.nlink() != 1
            {
                return Err(TransportError::InsecureSocket {
                    path: path.display().to_string(),
                });
            }
            fs::remove_file(path).map_err(TransportError::Io)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(TransportError::Io(error)),
    }
    let listener = UnixListener::bind(path).map_err(TransportError::Io)?;
    if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        let _ = fs::remove_file(path);
        return Err(TransportError::Io(error));
    }
    let metadata = fs::symlink_metadata(path).map_err(TransportError::Io)?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        let _ = fs::remove_file(path);
        return Err(TransportError::InsecureSocket {
            path: path.display().to_string(),
        });
    }
    Ok((
        listener,
        SocketGuard {
            path: path.to_path_buf(),
            device: metadata.dev(),
            inode: metadata.ino(),
        },
    ))
}

/// Removes only the exact socket inode this transport created.
struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        }) {
            let _ = fs::remove_file(&self.path);
        }
    }
}
