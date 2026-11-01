use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use dekopon_broker_protocol::{ChatTransportKind, Conversation, ConversationKind};
use futures_util::future::BoxFuture;
use tokio::sync::mpsc;

use super::{
    AssetFetcher, ChatDriver, ChatTransport, InboundMessage, LivenessTarget, MessageId, MessageRef,
    OutboundReply, ProgressLimits, ProgressMessage, ReplyTarget, TransportError, TransportEvent,
    TransportIdentity, bound_inbound, receive_span, record_conversation,
};
use crate::{
    asset::{AssetSourceRef, PendingAsset},
    fauxlack::{
        File, Message, Server,
        world::{Draft, Progress, Publication},
    },
    progress::ProgressText,
};

pub(crate) struct FauxlackTransport {
    name: String,
    driver: Arc<Driver>,
    receiver: Option<mpsc::Receiver<Message>>,
}
struct Driver {
    server: Server,
    bot: String,
}
impl FauxlackTransport {
    pub(crate) fn new(name: String, server: Server, bot: String) -> Self {
        Self {
            name,
            driver: Arc::new(Driver { server, bot }),
            receiver: None,
        }
    }
}
impl Drop for FauxlackTransport {
    fn drop(&mut self) {
        if self.receiver.is_some() {
            self.driver.server.detach(&self.driver.bot);
        }
    }
}
impl ChatTransport for FauxlackTransport {
    fn name(&self) -> &str {
        &self.name
    }
    fn connect(&mut self) -> BoxFuture<'_, Result<TransportIdentity, TransportError>> {
        Box::pin(async move {
            let (identity, receiver) = self
                .driver
                .server
                .attach(&self.driver.bot)
                .ok_or(TransportError::Response)?;
            self.receiver = Some(receiver);
            Ok(identity)
        })
    }
    fn retryable(&self, _: &TransportError) -> bool {
        false
    }
    fn next(&mut self) -> BoxFuture<'_, Result<TransportEvent, TransportError>> {
        Box::pin(async move {
            loop {
                let event = self
                    .receiver
                    .as_mut()
                    .ok_or(TransportError::Closed)?
                    .recv()
                    .await
                    .ok_or(TransportError::Closed)?;
                let received = receive_span(ChatTransportKind::Fauxlack);
                received.record("message.id", event.id.as_str());
                let user = self
                    .driver
                    .server
                    .person(&event.author)
                    .ok_or(TransportError::Response)?;
                if user.bot {
                    received.record("drop.reason", "bot-authored");
                    continue;
                }
                let container = self
                    .driver
                    .server
                    .container(&event.container)
                    .ok_or(TransportError::Response)?;
                let kind = match container.kind {
                    ConversationKind::Channel | ConversationKind::Thread
                        if event.thread.as_deref() != Some(event.id.as_str()) =>
                    {
                        ConversationKind::Thread
                    }
                    ConversationKind::Channel | ConversationKind::Thread => {
                        ConversationKind::Channel
                    }
                    ConversationKind::DirectMessage => ConversationKind::DirectMessage,
                    ConversationKind::GroupDirectMessage => ConversationKind::GroupDirectMessage,
                };
                let conversation = Conversation {
                    kind,
                    container: Some(self.driver.server.id().into()),
                    id: event.container.clone(),
                    thread: event.thread.clone(),
                };
                record_conversation(&received, &conversation);
                let addressed = match kind {
                    ConversationKind::DirectMessage | ConversationKind::GroupDirectMessage => None,
                    ConversationKind::Channel | ConversationKind::Thread => {
                        Some(event.mentions.contains(&self.driver.bot))
                    }
                };
                return Ok(TransportEvent::Message(Box::new(InboundMessage {
                    control_text: None,
                    pull_request_links: None,
                    transport: self.name.clone(),
                    transport_kind: ChatTransportKind::Fauxlack,
                    subject: user.subject.clone(),
                    conversation,
                    message_id: MessageId::Native(event.id),
                    text: bound_inbound(&event.text),
                    assets: event
                        .files
                        .into_iter()
                        .map(|file| PendingAsset {
                            name: file.name,
                            mime: file.mime,
                            size: Some(file.bytes.len() as u64),
                            source: Some(AssetSourceRef::Fauxlack {
                                server: self.driver.server.id().into(),
                                file: file.id,
                            }),
                        })
                        .collect(),
                    asset_overflow: false,
                    input_refusal: None,
                    addressed,
                    thread_continuation: None,
                    reply: ReplyTarget::Fauxlack {
                        server: self.driver.server.id().into(),
                        container: event.container.clone(),
                        thread: event.thread.clone(),
                    },
                    liveness: Some(LivenessTarget::Fauxlack {
                        server: self.driver.server.id().into(),
                        container: event.container,
                        thread: event.thread,
                    }),
                    receive_span: received,
                    received_at: tokio::time::Instant::now(),
                    native_group: None,
                    constituents: Vec::new(),
                    folded: Vec::new(),
                })));
            }
        })
    }
    fn driver(&self) -> Arc<dyn ChatDriver> {
        Arc::clone(&self.driver) as Arc<dyn ChatDriver>
    }
    fn asset_fetcher(&self) -> Option<Arc<dyn AssetFetcher>> {
        Some(Arc::clone(&self.driver) as Arc<dyn AssetFetcher>)
    }
}
impl Driver {
    fn target<'a>(
        &self,
        target: &'a LivenessTarget,
    ) -> Result<(&'a str, Option<&'a str>), TransportError> {
        match target {
            LivenessTarget::Fauxlack {
                server,
                container,
                thread,
            } if server == self.server.id() => Ok((container, thread.as_deref())),
            LivenessTarget::Fauxlack { .. }
            | LivenessTarget::Slack { .. }
            | LivenessTarget::Discord { .. }
            | LivenessTarget::Telegram { .. }
            | LivenessTarget::WhatsApp { .. }
            | LivenessTarget::Local { .. } => Err(TransportError::Response),
        }
    }
    async fn images(&self, reply: OutboundReply) -> Result<(String, Vec<File>), TransportError> {
        let images = super::hydration::hydrate_images(reply.images).await?;
        Ok((
            reply.text,
            self.server.store_files(
                images
                    .into_iter()
                    .map(|image| (image.filename, image.media_type, image.bytes))
                    .collect(),
            ),
        ))
    }
}
#[async_trait]
impl ChatDriver for Driver {
    async fn reply(
        &self,
        target: &ReplyTarget,
        reply: OutboundReply,
    ) -> Result<(), TransportError> {
        let ReplyTarget::Fauxlack {
            server,
            container,
            thread,
        } = target
        else {
            return Err(TransportError::Response);
        };
        if server != self.server.id() {
            return Err(TransportError::Response);
        }
        let context = reply.footer.as_ref().map(|footer| footer.line().to_owned());
        let (text, files) = self.images(reply).await?;
        self.server
            .publish(Draft {
                author: &self.bot,
                container,
                thread: thread.as_deref(),
                text: &text,
                context: context.as_deref(),
                files,
                publication: Publication::Final,
            })
            .await
            .map_err(|error| TransportError::Request(Box::new(error)))?;
        Ok(())
    }
    fn progress(&self) -> Option<&dyn ProgressMessage> {
        Some(self)
    }
}
#[async_trait]
impl ProgressMessage for Driver {
    fn limits(&self) -> ProgressLimits {
        ProgressLimits {
            max_chars: super::MAX_OUTBOUND_TEXT_BYTES,
            min_edit_interval: Duration::ZERO,
        }
    }
    async fn post(
        &self,
        target: &LivenessTarget,
        text: &ProgressText,
        _cancel: bool,
    ) -> Result<MessageRef, TransportError> {
        let (container, thread) = self.target(target)?;
        let posted = self
            .server
            .publish(Draft {
                author: &self.bot,
                container,
                thread,
                text: text.as_str(),
                context: None,
                files: Vec::new(),
                publication: Publication::Progress,
            })
            .await
            .map_err(|error| TransportError::Request(Box::new(error)))?;
        Ok(MessageRef {
            target: target.clone(),
            id: posted.id().into(),
        })
    }
    async fn edit(
        &self,
        message: &MessageRef,
        text: &ProgressText,
        _cancel: bool,
    ) -> Result<(), TransportError> {
        self.target(&message.target)?;
        if self
            .server
            .progress(&message.id, text.as_str(), Progress::Edit)
        {
            Ok(())
        } else {
            Err(TransportError::Response)
        }
    }
    async fn delete(&self, message: &MessageRef) -> Result<(), TransportError> {
        self.target(&message.target)?;
        if self.server.progress(&message.id, "", Progress::Delete) {
            Ok(())
        } else {
            Err(TransportError::Response)
        }
    }
    async fn finalize(
        &self,
        message: &MessageRef,
        reply: &OutboundReply,
    ) -> Result<(), TransportError> {
        self.target(&message.target)?;
        if !reply.images.is_empty() {
            return Err(TransportError::Service {
                code: "answer-has-attachments".into(),
            });
        }
        if self.server.progress(
            &message.id,
            &reply.text,
            Progress::Finalize {
                files: Vec::new(),
                context: reply.footer.as_ref().map(|footer| footer.line().to_owned()),
            },
        ) {
            Ok(())
        } else {
            Err(TransportError::Response)
        }
    }
}
impl AssetFetcher for Driver {
    fn fetch(
        &self,
        source: &AssetSourceRef,
        max_bytes: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, TransportError>> {
        let file = match source {
            AssetSourceRef::Fauxlack { server, file } if server == self.server.id() => {
                self.server.read_file(file)
            }
            AssetSourceRef::Fauxlack { .. }
            | AssetSourceRef::Generated { .. }
            | AssetSourceRef::Slack { .. }
            | AssetSourceRef::Discord { .. }
            | AssetSourceRef::WhatsApp { .. }
            | AssetSourceRef::Telegram { .. } => None,
        };
        Box::pin(async move {
            let file = file.ok_or(TransportError::Response)?;
            if file.bytes.len() as u64 > max_bytes {
                return Err(TransportError::Service {
                    code: "asset-too-large".into(),
                });
            }
            Ok(file.bytes)
        })
    }
}
