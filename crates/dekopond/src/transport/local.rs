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
//! the broker's attested `invoke`, and the broker still needs an attestor grant covering that namespace
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
    time::Duration,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use dekopon_agent::CancelVia;
use dekopon_broker_protocol::{ChatTransportKind, Conversation, ConversationKind};
use dekopon_core::ExternalSubject;
use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot},
};

use crate::{
    config::{LivenessMode, LivenessSettings},
    progress::ProgressText,
    transport::{
        AckToken, CancelButton, CancelPress, CancelRequest, ChatDriver, ChatTransport,
        InboundMessage, InboundReaction, LivenessTarget, MAX_OUTBOUND_TEXT_BYTES, MessageRef,
        NativeStatus, OutboundReply, ProgressLimits, ProgressMessage, ReplyTarget, Status,
        StreamLimits, StreamedText, TextStream, TransportError, TransportEvent, TransportIdentity,
        TypingLease, bound_inbound, receive_span, record_conversation,
    },
};

/// Longest line the development transport accepts, matching the inbound text bound plus envelope.
const MAX_LINE_BYTES: u64 = 64 * 1024;
/// How often the reference driver re-emits its typing line.
///
/// A JSON line expires from nothing, so this interval exists only to drive the policy's renewal
/// path against a driver a test can read. Deliberately the same order as Discord's eight seconds,
/// so a test written against one reads on the other.
const TYPING_RENEWAL: Duration = Duration::from_secs(8);
/// What a stream line shows where the policy cut the model's text.
///
/// The same ellipsis every other surface renders, so a reader following the line stream sees the
/// cut the way a person on a chat service sees it.
const TRUNCATION_MARKER: char = '…';

/// One line-delimited JSON request from a local caller.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalRequest {
    /// A canonical external subject the caller claims to be.
    subject: ExternalSubject,
    /// Where this line was posted, defaulting to a direct message in `dev`.
    ///
    /// The local line is the only source of every conversation kind, which is what makes this
    /// transport the one that can exercise a route table, a liveness override, and a memory key
    /// for a kind no other transport in a test harness produces.
    #[serde(default = "default_conversation")]
    conversation: Conversation,
    /// The message to route. Absent only on a stop line.
    text: Option<String>,
    /// Asks the gateway to cancel whatever this conversation has in flight.
    ///
    /// A line rather than a press: there is no component to acknowledge here, so it arrives as
    /// [`CancelVia::StopReply`], which is the path every transport has even without buttons.
    #[serde(default)]
    stop: bool,
}

fn default_conversation() -> Conversation {
    Conversation {
        kind: ConversationKind::DirectMessage,
        container: None,
        id: "dev".to_owned(),
        thread: None,
    }
}

pub(crate) struct LocalTransport {
    name: String,
    socket_path: PathBuf,
    listener: Option<UnixListener>,
    guard: Option<SocketGuard>,
    inbound: Option<mpsc::UnboundedReceiver<TransportEvent>>,
    sender: mpsc::UnboundedSender<TransportEvent>,
    driver: Arc<LocalDriver>,
    connections: AtomicU64,
    boot_nonce: Option<String>,
    /// `liveness.mode: native` — an inbound line carries coordinates for transient signals.
    ///
    /// One decision rather than the whole block: this driver implements every surface, and which
    /// of them the session uses is the policy's decision from progress, stream, cancel button,
    /// keep-alive, and templates. Withholding the coordinates is how `off` keeps the development
    /// transport as reply-only as it was.
    native: bool,
}

impl LocalTransport {
    pub(crate) fn new(name: String, socket_path: PathBuf, liveness: LivenessSettings) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            name,
            socket_path,
            listener: None,
            guard: None,
            inbound: Some(receiver),
            sender,
            driver: Arc::new(LocalDriver::default()),
            connections: AtomicU64::new(1),
            boot_nonce: None,
            native: liveness.mode == LivenessMode::Native,
        }
    }

    /// Serves one connection: JSON lines in, JSON lines out, until the caller hangs up.
    fn serve(&self, stream: UnixStream) {
        let connection = self.connections.fetch_add(1, Ordering::Relaxed);
        let (outbound_send, mut outbound_receive) = mpsc::unbounded_channel::<LocalWrite>();
        self.driver.register(connection, outbound_send);

        let name = self.name.clone();
        let native = self.native;
        let boot_nonce = self.boot_nonce.clone().unwrap_or_default();
        let inbound = self.sender.clone();
        let driver = Arc::clone(&self.driver);
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
                        reason = "a oneshot send fails only when the writer's caller stopped \
                                  waiting for the acknowledgement, which is the same hung-up \
                                  caller the `accepted` check below already ends the loop for"
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
                // One span per inbound line, opened before the request is parsed, so a refusal
                // and the message a good line becomes share this receipt's trace.
                let received = receive_span(ChatTransportKind::Local);
                let Some(request) = received.in_scope(|| {
                    serde_json::from_str::<LocalRequest>(text).ok().or_else(|| {
                        tracing::debug!(
                            event = "gateway_local_request_rejected",
                            transport = %name,
                            reason = "malformed-request"
                        );
                        None
                    })
                }) else {
                    continue;
                };
                if request.stop {
                    if request.text.is_some() {
                        received.in_scope(|| {
                            tracing::debug!(
                                event = "gateway_local_request_rejected",
                                transport = %name,
                                reason = "ambiguous-request"
                            );
                        });
                        continue;
                    }
                    let subject = request.subject.canonical();
                    let press = CancelPress {
                        target: LivenessTarget::Local { connection },
                        subject: subject.clone(),
                        ack: AckToken::Local,
                    };
                    // Acknowledged here, inside the reader, before the event is handed on: that is
                    // the ordering a service deadline forces on Discord and Telegram, and the
                    // reference driver models it so a test can watch the acknowledgment land first.
                    if let Some(button) = driver.cancel_button() {
                        #[allow(
                            clippy::let_underscore_must_use,
                            reason = "the only failure this emit has is a caller that hung up \
                                      between its stop line and the acknowledgment, which the \
                                      read loop below ends the connection for on its next pass; \
                                      the cancellation itself is still worth delivering"
                        )]
                        let _ = button.ack(&press).await;
                    }
                    let cancelled = TransportEvent::CancelRequested(CancelRequest {
                        transport: name.clone(),
                        conversation_id: request.conversation.key(),
                        subject,
                        // A line in the conversation rather than a component press: the local
                        // socket has no interaction to acknowledge, which is what separates this
                        // origin from `CancelVia::Button`.
                        via: CancelVia::StopReply,
                    });
                    if inbound.send(cancelled).is_err() {
                        break;
                    }
                    continue;
                }
                let Some(text) = request.text else {
                    received.in_scope(|| {
                        tracing::debug!(
                            event = "gateway_local_request_rejected",
                            transport = %name,
                            reason = "malformed-request"
                        );
                    });
                    continue;
                };
                sequence += 1;
                let message_id = format!("{boot_nonce}-{connection}-{sequence}");
                received.record("message.id", message_id.as_str());
                // The caller names its own conversation, and it defaults to a direct message in
                // `dev`. There is nothing else here to derive one from — the connection number
                // would restart the conversation every time a developer reconnected.
                let conversation = request.conversation;
                record_conversation(&received, &conversation);
                let addressed = conversation.kind == ConversationKind::DirectMessage;
                let message = InboundMessage {
                    transport: name.clone(),
                    transport_kind: ChatTransportKind::Local,
                    subject: request.subject,
                    conversation,
                    message_id,
                    text: bound_inbound(&text),
                    // The development transport speaks line-delimited JSON and carries no files.
                    assets: Vec::new(),
                    // A direct-message line is addressed by definition; anything else is ambient
                    // traffic the routing loop applies the same addressing rule to as a channel.
                    addressed: addressed.then_some(true),
                    thread_continuation: None,
                    reply: ReplyTarget::Local { connection },
                    liveness: native.then_some(LivenessTarget::Local { connection }),
                    receive_span: received,
                };
                if inbound
                    .send(TransportEvent::Message(Box::new(message)))
                    .is_err()
                {
                    break;
                }
            }
            driver.forget(connection);
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
                    event = receiver.recv() => {
                        return event.ok_or(TransportError::Closed);
                    }
                }
            }
        })
    }

    fn driver(&self) -> Arc<dyn ChatDriver> {
        Arc::clone(&self.driver) as Arc<dyn ChatDriver>
    }
}

/// The reference driver: every capability object, each rendered as one JSON line.
///
/// The only driver that implements all six, which is what makes it the surface the gateway's own
/// tests read: a decision the policy makes that a real service cannot show — a native status on
/// Discord, a reaction on WhatsApp — still appears here as a line, so the policy is testable
/// without a chat service at all.
#[derive(Default)]
pub(crate) struct LocalDriver {
    connections: Mutex<BTreeMap<u64, mpsc::UnboundedSender<LocalWrite>>>,
    /// Mints the identifier a progress or stream message is re-emitted under.
    ///
    /// A number rather than a service snowflake, because there is no service: the identifier
    /// exists so a reader can tell an edit of the message it already saw from a new one.
    messages: AtomicU64,
}

struct LocalWrite {
    line: String,
    ack: oneshot::Sender<bool>,
}

impl LocalDriver {
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

    /// Writes one JSON line and resolves only once the kernel accepted it.
    ///
    /// A caller that hung up mid-session is [`TransportError::Closed`] rather than a panic or a
    /// silent success: the line simply has nowhere to go, and every capability object reports it
    /// the same way.
    #[allow(
        clippy::map_err_ignore,
        reason = "both discards are the same hung-up connection: SendError hands back the \
                  LocalWrite this call just built, and oneshot::error::RecvError is a unit struct \
                  meaning the writer task dropped the acknowledgement"
    )]
    async fn emit(&self, connection: u64, line: &Value) -> Result<(), TransportError> {
        let sender = self
            .connections
            .lock()
            .expect("local connection registry")
            .get(&connection)
            .cloned();
        let Some(sender) = sender else {
            return Err(TransportError::Closed);
        };
        let (ack, received) = oneshot::channel();
        sender
            .send(LocalWrite {
                line: line.to_string(),
                ack,
            })
            .map_err(|_| TransportError::Closed)?;
        if received.await.map_err(|_| TransportError::Closed)? {
            Ok(())
        } else {
            Err(TransportError::Closed)
        }
    }

    /// The connection one transient signal names, refusing another transport's coordinates.
    fn connection(target: &LivenessTarget) -> Result<u64, TransportError> {
        match target {
            LivenessTarget::Local { connection } => Ok(*connection),
            LivenessTarget::Slack { .. }
            | LivenessTarget::Discord { .. }
            | LivenessTarget::Telegram { .. }
            | LivenessTarget::WhatsApp { .. } => Err(TransportError::Response),
        }
    }

    fn mint(&self) -> String {
        format!("m{}", self.messages.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// The line one progress or stream message is written and re-emitted under.
    ///
    /// One shape for both objects, differing only in the key that names which of them wrote it: a
    /// reader follows a single message through its post, every edit, and its deletion by the
    /// identifier, and `cancel` is on the line because whether the policy asked for a stop control
    /// is a decision worth being able to read back.
    fn message_line(kind: &str, id: &str, text: &str, cancel: bool) -> Value {
        json!({ kind: { "id": id, "text": text, "cancel": cancel } })
    }

    /// The answer line, naming the message it landed in when it replaced one in place.
    fn answer(reply: &OutboundReply, message: Option<&str>) -> Value {
        let mut response = json!({ "reply": reply.text });
        if let Some(id) = message {
            response["id"] = Value::String(id.to_owned());
        }
        if !reply.images.is_empty() {
            response["images"] = Value::Array(
                reply
                    .images
                    .iter()
                    .enumerate()
                    .map(|(index, image)| {
                        json!({
                            "filename": image.filename(index),
                            "mediaType": image.media_type(),
                            "data": STANDARD.encode(image.bytes()),
                        })
                    })
                    .collect(),
            );
        }
        response
    }

    /// Turns a progress or stream message into the answer, under the same identifier.
    ///
    /// One implementation for both objects: on this transport a line is a line, so the difference
    /// between finalizing a progress message and finalizing a stream is only which one minted the
    /// identifier. Attachments ride it too, because a JSON line carries them as well as a reply
    /// line does — the delete-and-reply fallback exists for services that cannot.
    async fn finalize_in_place(
        &self,
        message: &MessageRef,
        reply: &OutboundReply,
    ) -> Result<(), TransportError> {
        let connection = Self::connection(&message.target)?;
        self.emit(connection, &Self::answer(reply, Some(message.id.as_str())))
            .await
    }
}

#[async_trait]
impl ChatDriver for LocalDriver {
    async fn reply(
        &self,
        target: &ReplyTarget,
        reply: OutboundReply,
    ) -> Result<(), TransportError> {
        let &ReplyTarget::Local { connection } = target else {
            return Err(TransportError::Response);
        };
        self.emit(connection, &Self::answer(&reply, None)).await
    }

    fn typing(&self) -> Option<&dyn TypingLease> {
        Some(self)
    }

    fn status(&self) -> Option<&dyn NativeStatus> {
        Some(self)
    }

    fn progress(&self) -> Option<&dyn ProgressMessage> {
        Some(self)
    }

    fn stream(&self) -> Option<&dyn TextStream> {
        Some(self)
    }

    fn reaction(&self) -> Option<&dyn InboundReaction> {
        Some(self)
    }

    fn cancel_button(&self) -> Option<&dyn CancelButton> {
        Some(self)
    }
}

#[async_trait]
impl TypingLease for LocalDriver {
    fn renew_every(&self) -> Duration {
        TYPING_RENEWAL
    }

    async fn renew(&self, target: &LivenessTarget) -> Result<(), TransportError> {
        self.emit(Self::connection(target)?, &json!({ "typing": true }))
            .await
    }
}

#[async_trait]
impl NativeStatus for LocalDriver {
    async fn set(&self, target: &LivenessTarget, status: Status) -> Result<(), TransportError> {
        let status = match status {
            Status::Working => "working",
            Status::Idle => "idle",
        };
        self.emit(Self::connection(target)?, &json!({ "status": status }))
            .await
    }
}

#[async_trait]
impl ProgressMessage for LocalDriver {
    fn limits(&self) -> ProgressLimits {
        ProgressLimits {
            // A line has no service ceiling. The gateway's own outbound bound is the only one that
            // applies, and a character is at least a byte, so text this long still fits the
            // [`MAX_LINE_BYTES`] line the reader on the other end accepts.
            max_chars: MAX_OUTBOUND_TEXT_BYTES,
            // A Unix socket rate-limits nothing, so every event the policy folds produces its own
            // line here instead of being coalesced into the next one. Coalescing is what the
            // drivers with a real edit floor exercise.
            min_edit_interval: Duration::ZERO,
        }
    }

    async fn post(
        &self,
        target: &LivenessTarget,
        text: &ProgressText,
        cancel: bool,
    ) -> Result<MessageRef, TransportError> {
        let connection = Self::connection(target)?;
        let id = self.mint();
        self.emit(
            connection,
            &Self::message_line("progress", &id, text.as_str(), cancel),
        )
        .await?;
        Ok(MessageRef {
            target: target.clone(),
            id,
        })
    }

    async fn edit(
        &self,
        message: &MessageRef,
        text: &ProgressText,
        cancel: bool,
    ) -> Result<(), TransportError> {
        let connection = Self::connection(&message.target)?;
        self.emit(
            connection,
            &Self::message_line("progress", &message.id, text.as_str(), cancel),
        )
        .await
    }

    async fn delete(&self, message: &MessageRef) -> Result<(), TransportError> {
        let connection = Self::connection(&message.target)?;
        self.emit(
            connection,
            &json!({ "progress": { "id": message.id, "deleted": true } }),
        )
        .await
    }

    async fn finalize(
        &self,
        message: &MessageRef,
        reply: &OutboundReply,
    ) -> Result<(), TransportError> {
        self.finalize_in_place(message, reply).await
    }
}

#[async_trait]
impl TextStream for LocalDriver {
    fn limits(&self) -> StreamLimits {
        StreamLimits {
            min_interval: Duration::ZERO,
            max_chars: MAX_OUTBOUND_TEXT_BYTES,
        }
    }

    async fn show(
        &self,
        target: &LivenessTarget,
        message: Option<&MessageRef>,
        text: &StreamedText,
        cancel: bool,
    ) -> Result<MessageRef, TransportError> {
        let connection = Self::connection(target)?;
        let id = match message {
            Some(message) => message.id.clone(),
            None => self.mint(),
        };
        self.emit(
            connection,
            &Self::message_line("delta", &id, &shown_text(text), cancel),
        )
        .await?;
        Ok(MessageRef {
            target: target.clone(),
            id,
        })
    }

    async fn finalize(
        &self,
        message: &MessageRef,
        reply: &OutboundReply,
    ) -> Result<(), TransportError> {
        self.finalize_in_place(message, reply).await
    }
}

#[async_trait]
impl InboundReaction for LocalDriver {
    async fn set(&self, target: &LivenessTarget, present: bool) -> Result<(), TransportError> {
        self.emit(Self::connection(target)?, &json!({ "reaction": present }))
            .await
    }
}

#[async_trait]
impl CancelButton for LocalDriver {
    async fn ack(&self, press: &CancelPress) -> Result<(), TransportError> {
        let AckToken::Local = &press.ack else {
            return Err(TransportError::Response);
        };
        let connection = Self::connection(&press.target)?;
        self.emit(connection, &json!({ "cancel": { "acked": true } }))
            .await
    }
}

/// The cumulative text as the line shows it: a cut the policy made says so with a marker.
///
/// Rendering the cut is the driver's because only the driver knows what its surface has room for
/// past its own ceiling. Here that is a JSON line, which takes the extra character without
/// argument, so the marker is appended to the text the policy already bounded.
fn shown_text(text: &StreamedText) -> String {
    let mut shown = text.text.as_str().to_owned();
    if text.truncated {
        shown.push(TRUNCATION_MARKER);
    }
    shown
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

#[cfg(test)]
mod unit_tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt as _,
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use dekopon_agent::CancelVia;
    use serde_json::{Value, json};
    use tokio::{
        io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
        sync::mpsc,
    };

    use super::{LocalDriver, LocalTransport, LocalWrite};
    use crate::{
        config::{LivenessMode, LivenessSettings},
        progress::ProgressText,
        transport::{
            AckToken, CancelPress, ChatDriver as _, ChatTransport as _, LivenessTarget, MessageRef,
            OutboundReply, ReplyTarget, Status, StreamedText, TransportEvent,
        },
    };

    const SUBJECT: &str = "tel.16034700182";

    /// A temporary directory private enough to hold the socket.
    ///
    /// `tempfile` creates its directory with the process umask, which on an ordinary machine
    /// leaves it group- and world-readable, and `bind` refuses a socket whose parent another user
    /// can reach. The mode is tightened here rather than relaxed there: a subject is a claim
    /// anyone who can open the socket may make, so that refusal is the point of the check.
    fn private_directory() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("a temporary directory");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("a private temporary directory");
        directory
    }

    /// A transport that publishes liveness, which is what every test here is about.
    fn transport(socket_path: PathBuf) -> LocalTransport {
        LocalTransport::new(
            "dev".to_owned(),
            socket_path,
            LivenessSettings {
                mode: LivenessMode::Native,
                ..LivenessSettings::default()
            },
        )
    }

    /// Operator-authored progress text, which only the policy module otherwise renders.
    fn progress_text(text: &str) -> ProgressText {
        ProgressText::for_test(text)
    }

    /// The cumulative text of a recorded stream, taken through the model crate's own parser
    /// because that parser is the only thing that builds model text from bytes.
    fn streamed() -> StreamedText {
        let events = dekopon_model::events_from_transcript(
            dekopon_test_support::OPENAI_CHAT_COMPLETIONS_TWO_DELTAS,
        )
        .expect("the recorded transcript parses");
        StreamedText {
            text: dekopon_test_support::scripted_text(&events),
            truncated: false,
        }
    }

    /// Registers one connection on the driver and records every line written to it.
    ///
    /// The acknowledgement matters: [`LocalDriver::emit`] resolves only once the writer says the
    /// bytes were accepted, so a recorder that never answered would hang every call under test
    /// exactly as a hung-up caller does.
    fn connect(driver: &LocalDriver, connection: u64) -> Arc<Mutex<Vec<Value>>> {
        let (sender, mut receiver) = mpsc::unbounded_channel::<LocalWrite>();
        driver.register(connection, sender);
        let lines = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&lines);
        tokio::spawn(async move {
            while let Some(write) = receiver.recv().await {
                recorded
                    .lock()
                    .expect("recorded lines")
                    .push(serde_json::from_str::<Value>(&write.line).expect("a JSON line"));
                #[allow(
                    clippy::let_underscore_must_use,
                    reason = "the acknowledgement fails only when the call under test stopped \
                              waiting for it, which is the hung-up caller its own assertions cover"
                )]
                let _ = write.ack.send(true);
            }
        });
        lines
    }

    /// Every rung the policy can reach on this transport writes its own line, and a message keeps
    /// one identifier from the post that minted it through to the answer that replaced it.
    #[tokio::test]
    async fn every_capability_object_writes_one_line() {
        let driver = LocalDriver::default();
        let lines = connect(&driver, 7);
        let target = LivenessTarget::Local { connection: 7 };
        let text = streamed();

        driver
            .typing()
            .expect("the reference driver renews typing")
            .renew(&target)
            .await
            .expect("the line is written");
        driver
            .status()
            .expect("the reference driver has a native status")
            .set(&target, Status::Working)
            .await
            .expect("the line is written");
        driver
            .reaction()
            .expect("the reference driver reacts")
            .set(&target, true)
            .await
            .expect("the line is written");
        let progress = driver
            .progress()
            .expect("the reference driver posts progress");
        let posted = progress
            .post(&target, &progress_text("Working on it…"), true)
            .await
            .expect("the progress message is posted");
        progress
            .edit(&posted, &progress_text("Still working (15 s)…"), true)
            .await
            .expect("the same message is rewritten");
        progress.delete(&posted).await.expect("and removed");
        let stream = driver.stream().expect("the reference driver streams");
        let streaming = stream
            .show(&target, None, &text, true)
            .await
            .expect("the first delta mints a message");
        let again = stream
            .show(&target, Some(&streaming), &text, true)
            .await
            .expect("a later delta re-emits the same message");
        assert_eq!(again, streaming, "cumulative text stays in one message");
        stream
            .finalize(&streaming, &OutboundReply::text("the answer"))
            .await
            .expect("the line is written");
        driver
            .reply(
                &ReplyTarget::Local { connection: 7 },
                OutboundReply::text("a reply"),
            )
            .await
            .expect("the line is written");

        let lines = lines.lock().expect("recorded lines").clone();
        assert_eq!(
            lines,
            vec![
                json!({ "typing": true }),
                json!({ "status": "working" }),
                json!({ "reaction": true }),
                json!({ "progress": { "id": "m1", "text": "Working on it…", "cancel": true } }),
                json!({ "progress": { "id": "m1", "text": "Still working (15 s)…", "cancel": true } }),
                json!({ "progress": { "id": "m1", "deleted": true } }),
                json!({ "delta": { "id": "m2", "text": text.text.as_str(), "cancel": true } }),
                json!({ "delta": { "id": "m2", "text": text.text.as_str(), "cancel": true } }),
                json!({ "reply": "the answer", "id": "m2" }),
                json!({ "reply": "a reply" }),
            ]
        );
        assert_eq!(posted.target, target, "every line names its connection");
    }

    /// The two message objects share one line shape, so a reader that follows a progress message
    /// through its edits follows a streamed answer the same way.
    #[test]
    fn a_progress_line_and_a_delta_line_differ_only_in_the_key() {
        assert_eq!(
            LocalDriver::message_line("progress", "m3", "Working on it…", false),
            json!({ "progress": { "id": "m3", "text": "Working on it…", "cancel": false } })
        );
        assert_eq!(
            LocalDriver::message_line("delta", "m3", "half an answ", true),
            json!({ "delta": { "id": "m3", "text": "half an answ", "cancel": true } })
        );
    }

    /// A cut the policy made is on the line: the marker is the only thing that says the text on
    /// screen is not the whole answer yet.
    #[tokio::test]
    async fn a_cut_stream_line_carries_the_truncation_marker() {
        let driver = LocalDriver::default();
        let lines = connect(&driver, 4);
        let target = LivenessTarget::Local { connection: 4 };
        let text = StreamedText {
            truncated: true,
            ..streamed()
        };

        driver
            .stream()
            .expect("the reference driver streams")
            .show(&target, None, &text, false)
            .await
            .expect("the line is written");

        assert_eq!(
            lines.lock().expect("recorded lines").clone(),
            vec![json!({
                "delta": {
                    "id": "m1",
                    "text": format!("{}…", text.text.as_str()),
                    "cancel": false
                }
            })]
        );
    }

    /// A stop line is acknowledged on the connection and becomes a cancel request carrying the
    /// caller's own canonical subject. The acknowledgement is written before the event is handed
    /// to the routing loop, which is the ordering a service deadline forces everywhere else.
    ///
    /// The two malformed lines before it are refused rather than routed: one names both a message
    /// and a stop, the other names neither.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stop_line_is_acknowledged_and_becomes_a_cancel_request() {
        let directory = private_directory();
        let socket_path = directory.path().join("dev.sock");
        let mut transport = transport(socket_path.clone());
        transport
            .connect()
            .await
            .expect("the development transport binds");

        let client = tokio::net::UnixStream::connect(&socket_path)
            .await
            .expect("a local caller connects");
        let (reader, mut writer) = client.into_split();
        for request in [
            json!({ "subject": SUBJECT, "text": "stop", "stop": true }),
            json!({ "subject": SUBJECT }),
            json!({
                "subject": SUBJECT,
                "conversation": { "kind": "directMessage", "id": "session-7" },
                "stop": true
            }),
        ] {
            writer
                .write_all(format!("{request}\n").as_bytes())
                .await
                .expect("the request is written");
        }

        let event = transport.next().await.expect("an event arrives");
        let TransportEvent::CancelRequested(request) = event else {
            panic!("a stop line is a cancel request, not a message");
        };
        assert_eq!(request.transport, "dev");
        assert_eq!(request.conversation_id, "session-7");
        assert_eq!(request.subject, SUBJECT);
        assert_eq!(
            request.via,
            CancelVia::StopReply,
            "a line in the conversation is a stop reply, not a button press"
        );

        let mut lines = BufReader::new(reader).lines();
        let acknowledgement = lines
            .next_line()
            .await
            .expect("the connection stays open")
            .expect("the acknowledgement was written");
        assert_eq!(
            serde_json::from_str::<Value>(&acknowledgement).expect("a JSON line"),
            json!({ "cancel": { "acked": true } })
        );
    }

    /// A routed line carries the connection every capability object writes to. `liveness.mode:
    /// off` withholds it, which is how the development transport stays reply-only.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_routed_line_carries_its_connection_unless_liveness_is_off() {
        for (settings, expected) in [
            (
                LivenessSettings {
                    mode: LivenessMode::Native,
                    ..LivenessSettings::default()
                },
                Some(LivenessTarget::Local { connection: 1 }),
            ),
            (LivenessSettings::default(), None),
        ] {
            let directory = private_directory();
            let socket_path = directory.path().join("dev.sock");
            let mut transport =
                LocalTransport::new("dev".to_owned(), socket_path.clone(), settings);
            transport
                .connect()
                .await
                .expect("the development transport binds");
            let mut client = tokio::net::UnixStream::connect(&socket_path)
                .await
                .expect("a local caller connects");
            client
                .write_all(
                    format!("{}\n", json!({ "subject": SUBJECT, "text": "hello" })).as_bytes(),
                )
                .await
                .expect("the request is written");

            let TransportEvent::Message(message) =
                transport.next().await.expect("an event arrives")
            else {
                panic!("a text line is a message");
            };
            assert_eq!(message.text, "hello");
            assert_eq!(message.liveness, expected);
        }
    }

    /// Coordinates from another service are refused rather than written to whatever connection
    /// happens to be numbered alike, and so is an acknowledgement token another transport issued.
    #[tokio::test]
    async fn coordinates_and_tokens_from_another_service_are_refused() {
        let driver = LocalDriver::default();
        let lines = connect(&driver, 1);
        let elsewhere = LivenessTarget::Discord {
            channel_id: "12".to_owned(),
            message_id: "34".to_owned(),
        };

        let refused = driver
            .typing()
            .expect("typing is implemented")
            .renew(&elsewhere)
            .await
            .expect_err("a Discord target is not a local connection");
        assert_eq!(refused.category(), "response");

        let refused = driver
            .cancel_button()
            .expect("the acknowledgement is implemented")
            .ack(&CancelPress {
                target: LivenessTarget::Local { connection: 1 },
                subject: SUBJECT.to_owned(),
                ack: AckToken::Telegram {
                    callback_query_id: "9".to_owned(),
                },
            })
            .await
            .expect_err("a Telegram callback query is not a local acknowledgement");
        assert_eq!(refused.category(), "response");

        assert!(
            lines.lock().expect("recorded lines").is_empty(),
            "a refused call writes nothing"
        );
    }

    /// A caller that hung up leaves every object reporting the closed connection rather than
    /// succeeding silently: the policy needs the difference to stop rendering.
    #[tokio::test]
    async fn a_hung_up_caller_closes_every_object() {
        let driver = LocalDriver::default();
        let target = LivenessTarget::Local { connection: 4 };

        let closed = driver
            .status()
            .expect("a native status is implemented")
            .set(&target, Status::Idle)
            .await
            .expect_err("connection 4 was never registered");
        assert_eq!(closed.category(), "closed");

        let closed = driver
            .progress()
            .expect("progress is implemented")
            .finalize(
                &MessageRef {
                    target,
                    id: "m1".to_owned(),
                },
                &OutboundReply::text("the answer"),
            )
            .await
            .expect_err("the answer has nowhere to go");
        assert_eq!(closed.category(), "closed");
    }
}
