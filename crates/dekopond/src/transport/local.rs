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

use base64::{Engine as _, engine::general_purpose::STANDARD};
use dekopon_broker_protocol::ChatTransportKind;
use dekopon_core::ExternalSubject;
use futures_util::future::BoxFuture;
use serde::Deserialize;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot},
};

use crate::transport::{
    ChatReplier, ChatTransport, ConversationKind, DeliveryReceipt, InboundMessage, OutboundReply,
    ReplyTarget, TransportError, TransportEvent, TransportIdentity, bound_inbound,
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
    boot_nonce: Option<String>,
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
            boot_nonce: None,
        }
    }

    /// Serves one connection: JSON lines in, JSON lines out, until the caller hangs up.
    fn serve(&self, stream: UnixStream) {
        let connection = self.connections.fetch_add(1, Ordering::Relaxed);
        let (outbound_send, mut outbound_receive) = mpsc::unbounded_channel::<LocalWrite>();
        self.replier.register(connection, outbound_send);

        let name = self.name.clone();
        let boot_nonce = self.boot_nonce.clone().unwrap_or_default();
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
                    let line = format!("{}\n", reply.line);
                    let accepted = writer.write_all(line.as_bytes()).await.is_ok()
                        && writer.flush().await.is_ok();
                    #[allow(
                        clippy::let_underscore_must_use,
                        reason = "a oneshot send fails only when the replier stopped waiting for \
                                  the acknowledgement, which is the same hung-up caller the \
                                  `accepted` check below already ends the loop for"
                    )]
                    let _ = reply.ack.send(accepted);
                    if !accepted {
                        break;
                    }
                }
            });
            let mut sequence = 0_u64;
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
                sequence += 1;
                let message = InboundMessage {
                    transport: name.clone(),
                    transport_kind: ChatTransportKind::Local,
                    subject: request.subject,
                    channel: request.channel.clone(),
                    thread: None,
                    // The caller names its own conversation, and `channel` defaults to `dev` when
                    // it does not. There is nothing else here to derive one from — a local session
                    // has no threads, and the connection number would restart the conversation
                    // every time a developer reconnected.
                    conversation_id: request.channel,
                    message_id: format!("{boot_nonce}-{connection}-{sequence}"),
                    text: bound_inbound(&request.text),
                    // The development transport speaks line-delimited JSON and carries no files.
                    assets: Vec::new(),
                    // Always a direct message: a local caller is talking to the daemon
                    // one-to-one, so there is no ambient traffic to filter and no mention to
                    // require. Channel routes are a chat-service concept.
                    conversation: ConversationKind::DirectMessage,
                    addressed: Some(true),
                    thread_continuation: None,
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
            let mut nonce = [0_u8; 16];
            getrandom::fill(&mut nonce)
                .map_err(|source| TransportError::Io(std::io::Error::other(source)))?;
            self.boot_nonce = Some(nonce.iter().map(|byte| format!("{byte:02x}")).collect());
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
    connections: Mutex<BTreeMap<u64, mpsc::UnboundedSender<LocalWrite>>>,
}

struct LocalWrite {
    line: String,
    ack: oneshot::Sender<bool>,
}

impl LocalReplier {
    fn register(&self, connection: u64, sender: mpsc::UnboundedSender<LocalWrite>) {
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
    #[allow(
        clippy::map_err_ignore,
        reason = "both discards are the same hung-up connection: SendError hands back the \
                  LocalWrite this call just built, and oneshot::error::RecvError is a unit struct \
                  meaning the writer task dropped the acknowledgement"
    )]
    fn reply(
        &self,
        target: ReplyTarget,
        reply: OutboundReply,
    ) -> BoxFuture<'_, Result<DeliveryReceipt, TransportError>> {
        Box::pin(async move {
            let ReplyTarget::Local { connection } = target else {
                return Err(TransportError::Response);
            };
            let OutboundReply { text, image } = reply;
            let mut response = serde_json::json!({ "reply": text });
            if let Some(image) = image {
                response["images"] = serde_json::json!([{
                    "filename": image.filename(),
                    "mediaType": image.media_type(),
                    "data": STANDARD.encode(image.bytes()),
                }]);
            }
            let line = response.to_string();
            let sender = self
                .connections
                .lock()
                .expect("local connection registry")
                .get(&connection)
                .cloned();
            // A caller that hung up mid-session is not an error worth failing a session over: the
            // answer simply has nowhere to go.
            match sender {
                Some(sender) => {
                    let (ack, received) = oneshot::channel();
                    sender
                        .send(LocalWrite { line, ack })
                        .map_err(|_| TransportError::Closed)?;
                    if received.await.map_err(|_| TransportError::Closed)? {
                        Ok(DeliveryReceipt::new(format!("local:{connection}")))
                    } else {
                        Err(TransportError::Closed)
                    }
                }
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
        #[allow(
            clippy::let_underscore_must_use,
            reason = "rollback of a socket that is about to be reported as unusable anyway; the \
                      set_permissions error below is the one that explains the failure"
        )]
        let _ = fs::remove_file(path);
        return Err(TransportError::Io(error));
    }
    let metadata = fs::symlink_metadata(path).map_err(TransportError::Io)?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "rollback of a socket this function has just judged insecure; the refusal \
                      below is the reported outcome either way"
        )]
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
            #[allow(
                clippy::let_underscore_must_use,
                reason = "teardown in Drop, where there is no caller to report to; the inode was \
                          just confirmed to be this transport's own socket"
            )]
            let _ = fs::remove_file(&self.path);
        }
    }
}
