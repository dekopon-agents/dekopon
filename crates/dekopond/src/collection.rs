//! Bounded input collection, not an execution queue. The routing loop is its sole owner.
use std::{collections::BTreeMap, time::Duration};

use dekopon_broker_protocol::ChatTransportKind;
use tokio::time::Instant;

use crate::{
    asset::MAX_ASSETS_PER_CONVERSATION,
    config::TransportConfig,
    transport::{CancelRequest, InboundMessage, MAX_INBOUND_TEXT_BYTES, ReplyTarget},
};

const MAX_ENVELOPES: usize = 8;
const TELEGRAM_GROUP_WINDOW: Duration = Duration::from_secs(3);

pub(crate) struct Collector {
    windows: BTreeMap<String, CollectionWindow>,
    capacity: usize,
    pending: Vec<Batch>,
}

#[derive(Clone, Copy)]
struct CollectionWindow {
    quiet: Duration,
    max_wait: Duration,
}

struct Batch {
    route: usize,
    deadline: Instant,
    hard_deadline: Instant,
    members: Vec<InboundMessage>,
}

pub(crate) enum Offered {
    Pending,
    Immediate(InboundMessage),
    Refused(InboundMessage, &'static str),
}

impl Collector {
    pub(crate) fn new(transports: &[TransportConfig], capacity: usize) -> Self {
        Self {
            windows: transports
                .iter()
                .filter_map(|transport| match transport {
                    TransportConfig::WhatsappCloudApi {
                        name,
                        debounce_ms,
                        debounce_max_wait_ms,
                        ..
                    } => Some((
                        name.clone(),
                        CollectionWindow {
                            quiet: Duration::from_millis(u64::from(*debounce_ms)),
                            max_wait: Duration::from_millis(u64::from(*debounce_max_wait_ms)),
                        },
                    )),
                    _ => None,
                })
                .collect(),
            capacity,
            pending: Vec::new(),
        }
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.pending.iter().map(|batch| batch.deadline).min()
    }

    /// An authenticated native album can continue its addressed lead, never create a wakeup.
    pub(crate) fn is_native_continuation(&self, route: usize, message: &InboundMessage) -> bool {
        message.transport_kind == ChatTransportKind::Telegram
            && message.native_group.is_some()
            && self.pending.iter().any(|batch| {
                batch.route == route
                    && batch.deadline > Instant::now()
                    && compatible(&batch.members[0], message)
                    && batch.members[0].native_group == message.native_group
            })
    }

    pub(crate) fn offer(&mut self, route: usize, message: InboundMessage) -> Offered {
        if message.asset_overflow || message.assets.len() > MAX_ASSETS_PER_CONVERSATION {
            message.receive_span.in_scope(|| record_received(&message));
            return Offered::Refused(message, "input-limit");
        }
        let window = match message.transport_kind {
            ChatTransportKind::Whatsapp => self
                .windows
                .get(&message.transport)
                .copied()
                .filter(|window| !window.quiet.is_zero()),
            ChatTransportKind::Telegram if message.native_group.is_some() => {
                Some(CollectionWindow {
                    quiet: TELEGRAM_GROUP_WINDOW,
                    max_wait: TELEGRAM_GROUP_WINDOW,
                })
            }
            _ => None,
        };
        let Some(window) = window else {
            return Offered::Immediate(message);
        };
        if let Some(batch) = self
            .pending
            .iter_mut()
            .find(|batch| batch.route == route && compatible(&batch.members[0], &message))
        {
            message.receive_span.in_scope(|| record_received(&message));
            if batch.members[0].late_photos.is_some() && !message.text.trim().is_empty() {
                return Offered::Refused(message, "late-instructions");
            }
            if !crate::session::LatePhotoReceipt::same_batch(
                batch.members[0].late_photos.as_ref(),
                message.late_photos.as_ref(),
            ) {
                return Offered::Refused(message, "different-run");
            }
            if batch.members[0].native_group != message.native_group {
                return Offered::Refused(message, "incompatible-group");
            }
            let assets: usize = batch.members.iter().map(|member| member.assets.len()).sum();
            if batch.members.len() == MAX_ENVELOPES
                || assets + message.assets.len() > MAX_ASSETS_PER_CONVERSATION
                || combined_text(batch.members.iter().chain(std::iter::once(&message))).len()
                    > MAX_INBOUND_TEXT_BYTES
            {
                return Offered::Refused(message, "batch-limit");
            }
            if message.transport_kind == ChatTransportKind::Whatsapp {
                let Some(deadline) = message.received_at.checked_add(window.quiet) else {
                    return Offered::Refused(message, "deadline-overflow");
                };
                batch.deadline = batch.deadline.max(deadline.min(batch.hard_deadline));
            }
            batch.members.push(message);
            return Offered::Pending;
        }
        if message.assets.is_empty() && message.native_group.is_none() {
            return Offered::Immediate(message);
        }
        message.receive_span.in_scope(|| record_received(&message));
        if message.text.len() > MAX_INBOUND_TEXT_BYTES {
            return Offered::Refused(message, "input-limit");
        }
        if self.pending.len() == self.capacity {
            return Offered::Refused(message, "collection-full");
        }
        let Some(deadline) = message.received_at.checked_add(window.quiet) else {
            return Offered::Refused(message, "deadline-overflow");
        };
        let Some(hard_deadline) = message.received_at.checked_add(window.max_wait) else {
            return Offered::Refused(message, "deadline-overflow");
        };
        self.pending.push(Batch {
            route,
            deadline: deadline.min(hard_deadline),
            hard_deadline,
            members: vec![message],
        });
        Offered::Pending
    }

    pub(crate) fn take_due(&mut self, now: Instant) -> Vec<InboundMessage> {
        let mut ready = Vec::new();
        let mut index = 0;
        while index < self.pending.len() {
            if self.pending[index].deadline <= now {
                ready.push(self.pending.remove(index).assemble());
            } else {
                index += 1;
            }
        }
        ready
    }

    pub(crate) fn cancel(&mut self, request: &CancelRequest) -> bool {
        let mut cancelled = false;
        self.pending.retain(|batch| {
            let lead = &batch.members[0];
            let owned = lead.transport == request.transport
                && lead.conversation.key() == request.conversation_id
                && lead.subject.canonical() == request.subject;
            if owned {
                cancelled = true;
                for member in &batch.members {
                    disposition(&member.receive_span, "stopped");
                }
            }
            !owned
        });
        cancelled
    }

    pub(crate) fn shutdown(&mut self) {
        for batch in self.pending.drain(..) {
            for member in batch.members {
                disposition(&member.receive_span, "shutdown");
            }
        }
    }
}

fn compatible(left: &InboundMessage, right: &InboundMessage) -> bool {
    left.subject == right.subject
        && left.transport == right.transport
        && left.transport_kind == right.transport_kind
        && left.conversation == right.conversation
        && audience(&left.reply) == audience(&right.reply)
}

// Native reply-to message IDs differ among group members; audience coordinates must not.
fn audience(reply: &ReplyTarget) -> ReplyTarget {
    match reply {
        ReplyTarget::Telegram {
            chat_id,
            message_thread_id,
            ..
        } => ReplyTarget::Telegram {
            chat_id: *chat_id,
            message_thread_id: *message_thread_id,
            reply_to: None,
        },
        other => other.clone(),
    }
}

fn combined_text<'a>(members: impl Iterator<Item = &'a InboundMessage>) -> String {
    members
        .enumerate()
        .map(|(index, member)| {
            format!(
                "[Input {}; {} attachments; caption {}]\n{}\n",
                index + 1,
                member.assets.len(),
                if member.text.is_empty() {
                    "absent"
                } else {
                    "present"
                },
                member.text
            )
        })
        .collect()
}

impl Batch {
    fn assemble(mut self) -> InboundMessage {
        let text = (self.members.len() > 1 && self.members[0].late_photos.is_none())
            .then(|| combined_text(self.members.iter()));
        let receipts = self
            .members
            .iter()
            .map(|member| member.receive_span.clone())
            .collect();
        let mut lead = self.members.remove(0);
        for member in self.members {
            lead.assets.extend(member.assets);
        }
        if let Some(text) = text {
            lead.text = text;
        }
        lead.constituents = receipts;
        lead
    }
}

/// Ensures aborts and panics also terminate each constituent's receipt trace.
pub(crate) struct Dispositions(pub(crate) Vec<tracing::Span>);

impl Dispositions {
    pub(crate) fn finish(mut self, outcome: &'static str) {
        for receipt in self.0.drain(..) {
            disposition(&receipt, outcome);
        }
    }
}

impl Drop for Dispositions {
    fn drop(&mut self) {
        for receipt in &self.0 {
            disposition(receipt, "abandoned");
        }
    }
}

pub(crate) fn record_received(message: &InboundMessage) {
    tracing::info!(
        target: "dekopond::audit",
        { audit.event = "gateway.message.received",
        subject = %message.subject,
        channel = message.conversation.id.as_str(),
        text = message.text.as_str(), },
        "gateway message received"
    );
}

pub(crate) fn disposition(receipt: &tracing::Span, outcome: &'static str) {
    receipt.in_scope(|| tracing::info!(event = "gateway_input_disposition", outcome));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{asset::PendingAsset, transport::receive_span};
    use dekopon_broker_protocol::{Conversation, ConversationKind};

    fn message(photos: usize, text: &str) -> InboundMessage {
        InboundMessage {
            transport: "wa".into(),
            transport_kind: ChatTransportKind::Whatsapp,
            subject: "whatsapp.15551234567".parse().unwrap(),
            conversation: Conversation {
                kind: ConversationKind::DirectMessage,
                container: Some("123:456".into()),
                id: "15551234567".into(),
                thread: None,
            },
            message_id: "wamid.test".into(),
            text: text.into(),
            assets: (0..photos)
                .map(|_| PendingAsset {
                    name: "photo.jpg".into(),
                    mime: "image/jpeg".into(),
                    size: Some(12),
                    source: None,
                })
                .collect(),
            asset_overflow: false,
            addressed: None,
            thread_continuation: None,
            reply: ReplyTarget::WhatsApp {
                recipient: "15551234567".into(),
            },
            liveness: None,
            receive_span: receive_span(ChatTransportKind::Whatsapp),
            received_at: Instant::now(),
            native_group: None,
            constituents: Vec::new(),
            late_photos: None,
        }
    }
    fn collector(millis: u64, capacity: usize) -> Collector {
        Collector {
            windows: BTreeMap::from([(
                "wa".into(),
                CollectionWindow {
                    quiet: Duration::from_millis(millis),
                    max_wait: Duration::from_secs(15),
                },
            )]),
            capacity,
            pending: Vec::new(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn media_first_quiet_window_collects_three_photos_and_prompt() {
        for millis in [3000, 900] {
            let mut collector = collector(millis, 4);
            assert!(matches!(
                collector.offer(0, message(0, "text first")),
                Offered::Immediate(_)
            ));
            let start = Instant::now();
            assert!(matches!(
                collector.offer(0, message(1, "caption")),
                Offered::Pending
            ));
            tokio::time::advance(Duration::from_millis(millis - 1)).await;
            for _ in 0..2 {
                assert!(matches!(
                    collector.offer(0, message(1, "")),
                    Offered::Pending
                ));
            }
            assert!(matches!(
                collector.offer(0, message(0, "edit these")),
                Offered::Pending
            ));
            assert_eq!(
                collector.deadline(),
                Some(start + Duration::from_millis(2 * millis - 1))
            );
            assert!(collector.take_due(Instant::now()).is_empty());
            tokio::time::advance(Duration::from_millis(millis)).await;
            let ready = collector.take_due(Instant::now());
            assert_eq!(ready.len(), 1);
            assert_eq!(ready[0].assets.len(), 3);
            assert_eq!(ready[0].constituents.len(), 4);
            assert!(ready[0].text.contains("caption absent"));
            assert!(ready[0].text.ends_with("edit these\n"));
            assert_eq!(collector.deadline(), None);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn sustained_burst_dispatches_at_configured_hard_deadline_not_after_last_receipt() {
        for (quiet, maximum) in [(5000, 15000), (900, 2400)] {
            let mut collector = collector(quiet, 4);
            collector.windows.get_mut("wa").unwrap().max_wait = Duration::from_millis(maximum);
            let start = Instant::now();
            assert!(matches!(
                collector.offer(0, message(1, "")),
                Offered::Pending
            ));
            // More receipts within each quiet interval cannot postpone the hard deadline.
            for _ in 0..3 {
                tokio::time::advance(Duration::from_millis(quiet - 1)).await;
                assert!(collector.take_due(Instant::now()).is_empty());
                assert!(matches!(
                    collector.offer(0, message(1, "")),
                    Offered::Pending
                ));
                if Instant::now() + Duration::from_millis(quiet)
                    >= start + Duration::from_millis(maximum)
                {
                    break;
                }
            }
            let deadline = start + Duration::from_millis(maximum);
            assert_eq!(collector.deadline(), Some(deadline));
            assert!(
                collector
                    .take_due(deadline - Duration::from_nanos(1))
                    .is_empty()
            );
            assert_eq!(collector.take_due(deadline).len(), 1);
            assert!(
                collector
                    .take_due(deadline + Duration::from_nanos(1))
                    .is_empty()
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn another_actor_and_refused_overflow_do_not_extend_a_quiet_deadline() {
        let mut collector = collector(5000, 4);
        let start = Instant::now();
        assert!(matches!(
            collector.offer(0, message(32, "")),
            Offered::Pending
        ));
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(matches!(
            collector.offer(0, message(1, "")),
            Offered::Refused(_, "batch-limit")
        ));
        let mut other = message(1, "");
        other.subject = "whatsapp.15557654321".parse().unwrap();
        assert!(matches!(collector.offer(0, other), Offered::Pending));
        assert_eq!(collector.deadline(), Some(start + Duration::from_secs(5)));
        assert_eq!(
            collector.take_due(start + Duration::from_secs(5))[0]
                .assets
                .len(),
            32
        );
        assert_eq!(collector.deadline(), Some(start + Duration::from_secs(9)));
    }

    #[tokio::test(start_paused = true)]
    async fn zero_bypasses_collection_and_native_transport_policies_are_independent() {
        for millis in [0, 3000, 8000] {
            let mut collector = collector(millis, 4);
            let result = collector.offer(0, message(1, ""));
            assert_eq!(matches!(result, Offered::Immediate(_)), millis == 0);
            for kind in [
                ChatTransportKind::Slack,
                ChatTransportKind::Discord,
                ChatTransportKind::Local,
                ChatTransportKind::Telegram,
            ] {
                let mut incoming = message(3, "native array");
                incoming.transport_kind = kind;
                assert!(
                    matches!(collector.offer(1, incoming), Offered::Immediate(message) if message.assets.len() == 3)
                );
            }
            let mut group = message(1, "caption");
            group.transport_kind = ChatTransportKind::Telegram;
            group.native_group = Some("album".into());
            let start = Instant::now();
            assert!(matches!(
                collector.offer(2, group.clone()),
                Offered::Pending
            ));
            assert!(matches!(
                collector.offer(2, group.clone()),
                Offered::Pending
            ));
            group.native_group = Some("other".into());
            assert!(matches!(
                collector.offer(2, group),
                Offered::Refused(_, "incompatible-group")
            ));
            let ready = collector.take_due(start + TELEGRAM_GROUP_WINDOW);
            let group = ready
                .iter()
                .find(|message| message.transport_kind == ChatTransportKind::Telegram)
                .unwrap();
            assert_eq!(group.assets.len(), 2);
        }
    }

    #[test]
    fn immediate_truncated_text_bypasses_collection_limits_but_not_native_overflow() {
        let text = crate::transport::bound_inbound(&"é".repeat(MAX_INBOUND_TEXT_BYTES));
        assert!(text.len() > MAX_INBOUND_TEXT_BYTES);
        assert!(text.ends_with("[message truncated by the gateway]"));
        for millis in [0, 3000] {
            for kind in [
                ChatTransportKind::Local,
                ChatTransportKind::Slack,
                ChatTransportKind::Discord,
                ChatTransportKind::Telegram,
                ChatTransportKind::Whatsapp,
            ] {
                let mut collector = collector(millis, 4);
                let mut input = message(0, &text);
                input.transport_kind = kind;
                assert!(matches!(
                    collector.offer(0, input.clone()),
                    Offered::Immediate(message) if message.text == text
                ));
                input.assets = message(3, "").assets;
                if kind != ChatTransportKind::Whatsapp || millis == 0 {
                    assert!(matches!(
                        collector.offer(0, input.clone()),
                        Offered::Immediate(message) if message.text == text && message.assets.len() == 3
                    ));
                } else {
                    assert!(matches!(
                        collector.offer(0, input.clone()),
                        Offered::Refused(_, "input-limit")
                    ));
                }
                input.asset_overflow = true;
                assert!(matches!(
                    collector.offer(0, input),
                    Offered::Refused(_, "input-limit")
                ));
                assert!(collector.deadline().is_none());
            }
        }
        let mut collector = collector(3000, 4);
        assert!(matches!(
            collector.offer(0, message(1, "")),
            Offered::Pending
        ));
        assert!(matches!(
            collector.offer(0, message(0, &text)),
            Offered::Refused(_, "batch-limit")
        ));
        let ready = collector.take_due(collector.deadline().unwrap());
        assert_eq!(ready[0].assets.len(), 1);
        assert!(ready[0].text.is_empty());
    }

    #[test]
    fn native_group_ignores_member_reply_ids_but_never_associates_standalone_text_or_other_actors()
    {
        let mut collector = collector(0, 4);
        let mut member = message(1, "caption");
        member.transport_kind = ChatTransportKind::Telegram;
        member.subject = "telegram.123456".parse().unwrap();
        member.native_group = Some("same-label".into());
        member.reply = ReplyTarget::Telegram {
            chat_id: 123,
            reply_to: Some(1),
            message_thread_id: Some(45),
        };
        assert!(matches!(
            collector.offer(0, member.clone()),
            Offered::Pending
        ));
        member.reply = ReplyTarget::Telegram {
            chat_id: 123,
            reply_to: Some(2),
            message_thread_id: Some(45),
        };
        assert!(matches!(
            collector.offer(0, member.clone()),
            Offered::Pending
        ));
        let mut text = member.clone();
        text.assets.clear();
        text.native_group = None;
        assert!(matches!(collector.offer(0, text), Offered::Immediate(_)));
        member.subject = "telegram.654321".parse().unwrap();
        assert!(matches!(collector.offer(0, member), Offered::Pending));
        assert_eq!(collector.pending.len(), 2);
        assert_eq!(collector.pending[0].members.len(), 2);
    }

    #[test]
    fn exact_envelope_asset_and_utf8_text_bounds_refuse_only_new_input() {
        let mut envelopes = collector(3000, 1);
        for _ in 0..8 {
            assert!(matches!(
                envelopes.offer(0, message(1, "")),
                Offered::Pending
            ));
        }
        assert!(matches!(
            envelopes.offer(0, message(1, "")),
            Offered::Refused(_, "batch-limit")
        ));
        assert_eq!(envelopes.pending[0].members.len(), 8);
        let mut assets = collector(3000, 1);
        assert!(matches!(assets.offer(0, message(32, "")), Offered::Pending));
        assert!(matches!(
            assets.offer(0, message(1, "")),
            Offered::Refused(_, "batch-limit")
        ));
        assert!(matches!(
            assets.offer(0, message(33, "")),
            Offered::Refused(_, "input-limit")
        ));
        let mut text = collector(3000, 1);
        let first = message(1, "caption");
        let second = message(0, "é");
        let overhead = combined_text([&first, &second].into_iter()).len() - second.text.len();
        let exact = format!(
            "{}{}",
            "é".repeat((MAX_INBOUND_TEXT_BYTES - overhead) / 2),
            "x".repeat((MAX_INBOUND_TEXT_BYTES - overhead) % 2)
        );
        assert!(matches!(text.offer(0, first), Offered::Pending));
        assert!(matches!(
            text.offer(0, message(0, &exact)),
            Offered::Pending
        ));
        assert!(matches!(
            text.offer(0, message(0, "x")),
            Offered::Refused(_, "batch-limit")
        ));
        let batch = text.pending.remove(0).assemble();
        assert_eq!(batch.text.len(), MAX_INBOUND_TEXT_BYTES);
    }

    #[test]
    fn actor_route_transport_conversation_thread_and_reply_audience_are_isolated() {
        let first = message(1, "");
        for axis in 0..7 {
            let mut collector = collector(3000, 8);
            assert!(matches!(
                collector.offer(0, first.clone()),
                Offered::Pending
            ));
            let mut other = first.clone();
            let mut route = 0;
            match axis {
                0 => other.subject = "whatsapp.15557654321".parse().unwrap(),
                1 => route = 1,
                2 => {
                    other.transport = "wa-other".into();
                    collector.windows.insert(
                        other.transport.clone(),
                        CollectionWindow {
                            quiet: Duration::from_secs(5),
                            max_wait: Duration::from_secs(15),
                        },
                    );
                }
                3 => other.conversation.id = "another".into(),
                4 => other.conversation.thread = Some("topic".into()),
                5 => other.conversation.container = Some("987:654".into()),
                _ => {
                    other.reply = ReplyTarget::WhatsApp {
                        recipient: "15557654321".into(),
                    }
                }
            }
            assert!(matches!(collector.offer(route, other), Offered::Pending));
            assert_eq!(collector.pending.len(), 2);
        }
    }

    #[test]
    fn capacity_stop_and_shutdown_dispose_without_a_deferred_queue() {
        let mut collector = collector(3000, 1);
        let first = message(1, "");
        assert!(matches!(
            collector.offer(0, first.clone()),
            Offered::Pending
        ));
        assert!(matches!(
            collector.offer(1, first.clone()),
            Offered::Refused(_, "collection-full")
        ));
        let mut request = CancelRequest {
            transport: first.transport.clone(),
            conversation_id: first.conversation.key(),
            subject: "whatsapp.15557654321".into(),
            via: dekopon_agent::CancelVia::StopReply,
        };
        assert!(!collector.cancel(&request));
        request.subject = first.subject.canonical();
        assert!(collector.cancel(&request));
        assert_eq!(collector.deadline(), None);
        assert!(matches!(collector.offer(0, first), Offered::Pending));
        collector.shutdown();
        assert!(
            collector
                .take_due(Instant::now() + Duration::from_secs(60))
                .is_empty()
        );
    }
}
