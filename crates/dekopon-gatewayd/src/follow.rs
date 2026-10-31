use std::{
    collections::{HashMap, HashSet},
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dekopon_broker_protocol::ConversationMatch;
use dekopon_process::TreeContext;
use dekopon_shell::{
    CapabilityCallResult, CapabilityDescription, CapabilityInvoker, CommandProposal, JobControl,
    Streams,
};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::{
    journal::{FollowMarker, Journal, JournalError},
    transport::{InboundMessage, MAX_INBOUND_TEXT_BYTES, MessageId},
};

const LAPSE: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_TICKETS: usize = 1024;
const QUIET: Duration = Duration::from_secs(30);
const MAX_WAIT: Duration = Duration::from_secs(120);

struct Pending {
    message: InboundMessage,
    speakers: Vec<dekopon_core::Subject>,
    text: String,
    first: tokio::time::Instant,
    latest: tokio::time::Instant,
}

#[derive(Default)]
pub(crate) struct Debounce {
    pending: HashMap<String, Pending>,
}

impl Debounce {
    pub(crate) fn offer(
        &mut self,
        key: String,
        message: InboundMessage,
        now: tokio::time::Instant,
    ) -> bool {
        if let Some(pending) = self.pending.get_mut(&key) {
            let speaker = pending
                .speakers
                .iter()
                .position(|subject| subject == message.subject.name());
            let label = speaker.unwrap_or(pending.speakers.len()) + 1;
            if pending.text.len() < MAX_INBOUND_TEXT_BYTES {
                if speaker.is_none() {
                    pending.speakers.push(message.subject.name().clone());
                }
                pending
                    .text
                    .push_str(&format!("\n\n[follow speaker {label}]\n{}", message.text));
                let end = crate::transport::floor_boundary(
                    &pending.text,
                    MAX_INBOUND_TEXT_BYTES.min(pending.text.len()),
                );
                pending.text.truncate(end);
            }
            pending.message = message;
            pending.latest = now;
            return true;
        }
        if self.pending.len() == MAX_TICKETS {
            return false;
        }
        self.pending.insert(
            key,
            Pending {
                speakers: vec![message.subject.name().clone()],
                text: crate::transport::bound_inbound(&format!(
                    "[follow speaker 1]\n{}",
                    message.text
                )),
                message,
                first: now,
                latest: now,
            },
        );
        true
    }

    pub(crate) fn cancel(&mut self, key: &str, subject: &dekopon_core::Subject) {
        if self
            .pending
            .get(key)
            .is_some_and(|pending| pending.message.subject.name() == subject)
        {
            self.pending.remove(key);
        }
    }

    pub(crate) fn remove(&mut self, key: &str) {
        self.pending.remove(key);
    }

    pub(crate) fn deadline(&self) -> Option<tokio::time::Instant> {
        self.pending
            .values()
            .map(|pending| (pending.latest + QUIET).min(pending.first + MAX_WAIT))
            .min()
    }

    pub(crate) fn due(&mut self, now: tokio::time::Instant) -> Vec<InboundMessage> {
        let keys: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, pending)| (pending.latest + QUIET).min(pending.first + MAX_WAIT) <= now)
            .map(|(key, _)| key.clone())
            .collect();
        keys.into_iter()
            .filter_map(|key| self.pending.remove(&key))
            .map(|mut pending| {
                pending.message.text = pending.text;
                if let MessageId::Native(id) = pending.message.message_id {
                    pending.message.message_id = MessageId::Follow(id);
                }
                pending.message
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FollowTicket {
    Active { until_ms: u64 },
    Lapsed,
    Absent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MarkerSeq(u64);

#[derive(Clone, Copy)]
struct Marked {
    marker: FollowMarker,
    seq: MarkerSeq,
}

#[derive(Clone, Copy)]
pub(crate) enum LeaveCause {
    Reaction,
    Mute,
    StopWord,
    Lapse,
}

impl LeaveCause {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Reaction => "reaction",
            Self::Mute => "mute",
            Self::StopWord => "stop-word",
            Self::Lapse => "lapse",
        }
    }
}

pub(crate) struct FollowTable {
    journal: Arc<Journal>,
    resident: Mutex<HashMap<String, Marked>>,
    written: Mutex<HashMap<String, MarkerSeq>>,
    writer: Mutex<HashSet<String>>,
    disk: Mutex<()>,
    next_seq: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    sweeping: AtomicBool,
}

#[must_use]
pub(crate) struct PendingFollow {
    table: Arc<FollowTable>,
    key: String,
    marked: Marked,
    lapsed: Vec<String>,
    route: usize,
}

impl PendingFollow {
    pub(crate) fn write(self) {
        let table = &self.table;
        let _disk = table.disk.lock();
        for old in &self.lapsed {
            if table.resident.lock().contains_key(old) {
                continue;
            }
            #[cfg(test)]
            table.sweeping.store(true, Ordering::Release);
            let removed = table.journal.remove_follow(old);
            #[cfg(test)]
            table.sweeping.store(false, Ordering::Release);
            if removed.is_ok() {
                table.written.lock().remove(old);
            }
        }
        let current = table
            .resident
            .lock()
            .get(&self.key)
            .is_some_and(|marked| marked.seq == self.marked.seq);
        if !current || table.writer.lock().contains(&self.key) {
            return;
        }
        match table.journal.write_follow(&self.key, self.marked.marker) {
            Ok(()) => {
                table
                    .written
                    .lock()
                    .insert(self.key.clone(), self.marked.seq);
            }
            Err(error) => {
                let mut resident = table.resident.lock();
                if resident
                    .get(&self.key)
                    .is_some_and(|marked| marked.seq == self.marked.seq)
                {
                    resident.remove(&self.key);
                }
                tracing::warn!(
                    audit.event = "gateway.follow",
                    operation = "refused",
                    route = self.route,
                    reason = error.label(),
                    "gateway.follow"
                );
            }
        }
    }
}

impl FollowTable {
    pub(crate) fn load(journal: Arc<Journal>) -> Result<Self, JournalError> {
        let mut resident = HashMap::new();
        let mut written = HashMap::new();
        for (index, (key, marker)) in journal.load_follow(MAX_TICKETS)?.into_iter().enumerate() {
            let seq = MarkerSeq(index as u64);
            resident.insert(key.clone(), Marked { marker, seq });
            written.insert(key, seq);
        }
        Ok(Self {
            next_seq: std::sync::atomic::AtomicU64::new(resident.len() as u64),
            resident: Mutex::new(resident),
            written: Mutex::new(written),
            writer: Mutex::new(HashSet::new()),
            disk: Mutex::new(()),
            #[cfg(test)]
            sweeping: AtomicBool::new(false),
            journal,
        })
    }

    fn mark(&self, now: SystemTime) -> Result<Marked, JournalError> {
        let at_ms = millis(now);
        let until_ms = at_ms
            .checked_add(LAPSE.as_millis() as u64)
            .ok_or(JournalError::Io {
                kind: io::ErrorKind::InvalidInput,
            })?;
        Ok(Marked {
            marker: FollowMarker { at_ms, until_ms },
            seq: MarkerSeq(self.next_seq.fetch_add(1, Ordering::Relaxed)),
        })
    }

    pub(crate) fn ticket(&self, key: &str, now: SystemTime) -> FollowTicket {
        match self.resident.lock().get(key) {
            Some(marked) if millis(now) < marked.marker.until_ms => FollowTicket::Active {
                until_ms: marked.marker.until_ms,
            },
            Some(_) => FollowTicket::Lapsed,
            None => FollowTicket::Absent,
        }
    }

    pub(crate) fn stay(
        self: &Arc<Self>,
        key: &str,
        now: SystemTime,
        route: usize,
    ) -> Result<PendingFollow, JournalError> {
        let marked = self.mark(now)?;
        let writer = self.writer.lock();
        if writer.contains(key) {
            return Err(JournalError::Io {
                kind: io::ErrorKind::PermissionDenied,
            });
        }
        let mut resident = self.resident.lock();
        let lapsed: Vec<String> = resident
            .iter()
            .filter(|(_, marked)| marked.marker.until_ms <= millis(now))
            .map(|(key, _)| key.clone())
            .collect();
        for old in &lapsed {
            if resident.remove(old).is_some() {
                tracing::info!(
                    audit.event = "gateway.follow",
                    operation = "banished",
                    cause = "lapse",
                    "gateway.follow"
                );
            }
        }
        if !resident.contains_key(key) && resident.len() + writer.len() >= MAX_TICKETS {
            return Err(JournalError::Io {
                kind: io::ErrorKind::OutOfMemory,
            });
        }
        resident.insert(key.to_owned(), marked);
        Ok(PendingFollow {
            table: Arc::clone(self),
            key: key.to_owned(),
            marked,
            lapsed,
            route,
        })
    }

    pub(crate) fn refresh(&self, key: &str, now: SystemTime) -> Result<(), JournalError> {
        let _disk = self.disk.lock();
        if self.writer.lock().contains(key)
            || !matches!(self.ticket(key, now), FollowTicket::Active { .. })
        {
            return Err(JournalError::Io {
                kind: io::ErrorKind::NotFound,
            });
        }
        let marked = self.mark(now)?;
        match self.journal.write_follow(key, marked.marker) {
            Ok(()) => {
                if self.writer.lock().contains(key) {
                    return Err(JournalError::Io {
                        kind: io::ErrorKind::PermissionDenied,
                    });
                }
                self.resident.lock().insert(key.to_owned(), marked);
                self.written.lock().insert(key.to_owned(), marked.seq);
                Ok(())
            }
            Err(error) => {
                let last = self.written.lock().get(key).copied();
                let mut resident = self.resident.lock();
                if last
                    .is_some_and(|last| resident.get(key).is_some_and(|marked| marked.seq == last))
                {
                    resident.remove(key);
                }
                Err(error)
            }
        }
    }

    pub(crate) fn leave(&self, key: &str) -> Result<(), JournalError> {
        self.suspend(key);
        let _disk = self.disk.lock();
        self.journal.remove_follow(key)?;
        self.written.lock().remove(key);
        let mut writer = self.writer.lock();
        writer.remove(key);
        self.resident.lock().remove(key);
        Ok(())
    }

    pub(crate) fn suspend(&self, key: &str) -> bool {
        let mut writer = self.writer.lock();
        let removed = self.resident.lock().remove(key).is_some();
        if removed {
            writer.insert(key.to_owned());
        }
        removed
    }
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn route_identity(transport: &str, agent: &str, selector: &ConversationMatch) -> Vec<u8> {
    let mut kinds = match &selector.kind {
        dekopon_broker_protocol::ConversationKindMatch::Any => vec!["any"],
        dekopon_broker_protocol::ConversationKindMatch::Kinds(kinds) => {
            kinds.iter().map(|kind| kind.as_str()).collect()
        }
    };
    kinds.sort_unstable();
    let mut ids = selector.ids.clone().unwrap_or_default();
    ids.sort_unstable();
    let mut identity = Vec::new();
    for field in [
        transport,
        agent,
        &kinds.join(","),
        selector.container.as_deref().unwrap_or_default(),
    ] {
        append_field(&mut identity, field);
    }
    for id in ids {
        append_field(&mut identity, &id);
    }
    identity
}

fn append_field(bytes: &mut Vec<u8>, field: &str) {
    bytes.extend_from_slice(&(field.len() as u64).to_be_bytes());
    bytes.extend_from_slice(field.as_bytes());
}

pub(crate) fn follow_key(
    transport: &str,
    agent: &str,
    selector: &ConversationMatch,
    conversation: &str,
) -> String {
    let mut digest = Sha256::new();
    digest.update(route_identity(transport, agent, selector));
    digest.update((conversation.len() as u64).to_be_bytes());
    digest.update(conversation.as_bytes());
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) type PendingFollows = Arc<Mutex<Vec<PendingFollow>>>;

pub(crate) struct FollowInvoker<I> {
    inner: I,
    ticket: Option<(Arc<FollowTable>, String, usize)>,
    leave: Arc<AtomicBool>,
    pending: PendingFollows,
    subscriptions: Option<crate::wake::ReviewSubscriptions>,
}

impl<I> FollowInvoker<I> {
    pub(crate) fn new(inner: I, ticket: Option<(Arc<FollowTable>, String, usize)>) -> Self {
        Self {
            inner,
            ticket,
            leave: Arc::new(AtomicBool::new(false)),
            pending: PendingFollows::default(),
            subscriptions: None,
        }
    }

    pub(crate) fn with_subscriptions(
        mut self,
        subscriptions: Option<crate::wake::ReviewSubscriptions>,
    ) -> Self {
        self.subscriptions = subscriptions;
        self
    }

    pub(crate) fn leave_request(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.leave)
    }

    pub(crate) fn pending_follows(&self) -> PendingFollows {
        Arc::clone(&self.pending)
    }
}

impl<I: CapabilityInvoker> CapabilityInvoker for FollowInvoker<I> {
    fn participant_handle(&self) -> Option<&str> {
        self.inner.participant_handle()
    }

    fn cancel_signals(&self) -> Vec<dekopon_process::CancelSignal> {
        self.inner.cancel_signals()
    }
    fn granted(&self) -> Vec<String> {
        self.inner.granted()
    }
    fn is_granted(&self, capability: &str) -> bool {
        self.inner.is_granted(capability)
    }
    fn command_words(&self) -> Vec<String> {
        self.inner.command_words()
    }
    fn command_word_help(&self) -> std::collections::BTreeMap<String, String> {
        self.inner.command_word_help()
    }
    fn has_command_word(&self, word: &str) -> bool {
        self.inner.has_command_word(word)
    }
    fn run(
        &self,
        call: dekopon_shell::RunCall,
        streams: Streams,
        tree: &TreeContext,
    ) -> dekopon_shell::RunResult {
        use dekopon_shell::RunResult;
        if call.word == "follow" && self.ticket.is_none() {
            return RunResult::Returned(CapabilityCallResult::Failed {
                error: "follow is not enabled on this route".to_owned(),
                detail: None,
            });
        }
        match self.inner.run(call, streams, tree) {
            RunResult::Invoked {
                capability,
                input,
                secret_use,
                result,
            } => {
                let result = self.mark(&capability, &input, secret_use.as_ref(), result);
                RunResult::Invoked {
                    capability,
                    input,
                    secret_use,
                    result,
                }
            }
            RunResult::Returned(result) => RunResult::Returned(result),
            RunResult::Uncertain(result) => RunResult::Uncertain(result),
        }
    }

    fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
        self.inner.describe(capability)
    }
    fn note(&self, text: &str, eta: Option<Duration>) {
        self.inner.note(text, eta);
    }
    fn script_started(&self, tree: &mut TreeContext) {
        self.inner.script_started(tree);
    }
    fn script_finished(&self) {
        self.inner.script_finished();
    }
    fn job_control(&self) -> Option<&dyn JobControl> {
        self.inner.job_control()
    }
    fn invoke(
        &self,
        proposal: CommandProposal,
        streams: Streams,
        tree: &TreeContext,
    ) -> CapabilityCallResult {
        let follow_capability = proposal.capability.starts_with("dekopon-provider-follow.");
        if follow_capability && self.ticket.is_none() {
            return CapabilityCallResult::Failed {
                error: "follow is not enabled on this route".to_owned(),
                detail: None,
            };
        }
        let capability = proposal.capability.clone();
        let input = proposal.input.clone();
        let secret_use = proposal.secret_use.clone();
        let result = self.inner.invoke(proposal, streams, tree);
        self.mark(&capability, &input, secret_use.as_ref(), result)
    }
}

impl<I: CapabilityInvoker> FollowInvoker<I> {
    fn mark(
        &self,
        capability: &str,
        input: &serde_json::Value,
        secret_use: Option<&dekopon_core::SecretUseProposal>,
        result: CapabilityCallResult,
    ) -> CapabilityCallResult {
        let result = match &self.subscriptions {
            Some(subscriptions) => subscriptions.reviewed(capability, input, result),
            None => result,
        };
        let stay = capability == "dekopon-provider-follow.stay"
            && secret_use.is_none()
            && input.as_object().is_some_and(serde_json::Map::is_empty);
        let leave = capability == "dekopon-provider-follow.leave"
            && secret_use.is_none()
            && input.as_object().is_some_and(serde_json::Map::is_empty);
        if leave
            && matches!(
                result,
                CapabilityCallResult::Succeeded | CapabilityCallResult::SucceededWithStderr(_)
            )
            && self.ticket.is_some()
        {
            self.leave.store(true, Ordering::Release);
        }
        if stay
            && matches!(
                result,
                CapabilityCallResult::Succeeded | CapabilityCallResult::SucceededWithStderr(_)
            )
            && let Some((table, key, route)) = &self.ticket
        {
            match table.stay(key, SystemTime::now(), *route) {
                Ok(pending) => {
                    self.pending.lock().push(pending);
                    tracing::info!(
                        audit.event = "gateway.follow",
                        operation = "invited",
                        route,
                        "gateway.follow"
                    )
                }
                Err(error) => {
                    tracing::warn!(
                        audit.event = "gateway.follow",
                        operation = "refused",
                        route,
                        reason = error.label(),
                        "gateway.follow"
                    );
                    return CapabilityCallResult::Failed {
                        error: "follow marker was not persisted".to_owned(),
                        detail: None,
                    };
                }
            }
        }
        result
    }
}

#[cfg(test)]
impl FollowTable {
    pub(crate) fn sweeping_for_test(&self) -> bool {
        self.sweeping.load(Ordering::Acquire)
    }

    pub(crate) fn stay_and_write(
        self: &Arc<Self>,
        key: &str,
        now: SystemTime,
    ) -> Result<(), JournalError> {
        self.stay(key, now, 0)?.write();
        Ok(())
    }

    pub(crate) fn writing_for_test(&self) -> bool {
        self.disk.try_lock().is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dekopon_broker_protocol::{ConversationKind, ConversationKindMatch};
    use serde_json::json;
    use std::os::fd::OwnedFd;

    mod provider_fixture {
        use dekopon_provider_sdk::{
            EffectKind, RiskLevel,
            clap::{self, Parser, Subcommand},
            provider::{self, Capability, Code, Failure, Proposal, Provider, Stdout, Usage},
        };
        use schemars::JsonSchema;
        use serde::{Deserialize, Serialize};

        pub(super) struct Follow;
        #[derive(Parser)]
        #[command(name = "follow")]
        pub(super) struct Args {
            #[command(subcommand)]
            verb: Verb,
        }
        #[derive(Subcommand)]
        enum Verb {
            Stay,
            Leave,
        }
        #[derive(Deserialize, Serialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub(super) struct Empty {}
        #[derive(Debug)]
        pub(super) struct Never;
        impl std::fmt::Display for Never {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("unreachable")
            }
        }
        impl Failure for Never {
            fn code(&self) -> Code {
                Code::USAGE
            }
        }
        pub(super) struct Stay;
        pub(super) struct Leave;
        macro_rules! capability {
            ($name:ident, $verb:literal) => {
                impl Capability for $name {
                    type Provider = Follow;
                    const NAME: &'static str = $verb;
                    const DESCRIPTION: &'static str = "Request a follow lifecycle transition";
                    const EFFECT: EffectKind = EffectKind::LocalWrite;
                    const RISK: RiskLevel = RiskLevel::Low;
                    type Input = Empty;
                    type Needs = ();
                    type Error = Never;
                    fn run(_: Empty, (): (), _: &mut Stdout) -> Result<(), Self::Error> {
                        Ok(())
                    }
                }
            };
        }
        capability!(Stay, "stay");
        capability!(Leave, "leave");
        impl Provider for Follow {
            const ID: &'static str = "dekopon-provider-follow";
            const COMMAND_WORDS: &'static [&'static str] = &["follow"];
            const DESCRIPTION: &'static str = "Test-only follow provider";
            type Args = Args;
            type Capabilities = (Stay, Leave);
            fn propose(args: Args, _: bool) -> Result<Proposal<Self>, Usage> {
                Ok(match args.verb {
                    Verb::Stay => Proposal::to::<Stay>(Empty {}),
                    Verb::Leave => Proposal::to::<Leave>(Empty {}),
                })
            }
        }
        #[test]
        fn sdk_declares_closed_no_argument_stay_and_leave() {
            let manifest = provider::manifest::<Follow>().unwrap();
            let capabilities = manifest.capabilities;
            assert_eq!(
                capabilities
                    .iter()
                    .map(|cap| cap.id.to_string())
                    .collect::<Vec<_>>(),
                [
                    "dekopon-provider-follow.stay",
                    "dekopon-provider-follow.leave"
                ]
            );
            for capability in capabilities {
                assert_eq!(capability.input_schema["type"], "object");
                assert_eq!(capability.input_schema["additionalProperties"], false);
                assert_eq!(
                    capability.input_schema["properties"]
                        .as_object()
                        .map(serde_json::Map::len)
                        .unwrap_or(0),
                    0
                );
            }
        }
    }

    struct RunLeg {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        input: serde_json::Value,
        secret_use: Option<dekopon_core::SecretUseProposal>,
        result: CapabilityCallResult,
    }

    impl CapabilityInvoker for RunLeg {
        fn granted(&self) -> Vec<String> {
            Vec::new()
        }
        fn run(
            &self,
            _: dekopon_shell::RunCall,
            _: Streams,
            _: &TreeContext,
        ) -> dekopon_shell::RunResult {
            self.calls.fetch_add(1, Ordering::Relaxed);
            dekopon_shell::RunResult::Invoked {
                capability: "dekopon-provider-follow.stay".to_owned(),
                input: self.input.clone(),
                secret_use: self.secret_use.clone(),
                result: self.result.clone(),
            }
        }
        fn invoke(&self, _: CommandProposal, _: Streams, _: &TreeContext) -> CapabilityCallResult {
            panic!("Run owns dispatch")
        }
    }

    fn run_word<I: CapabilityInvoker>(invoker: &I, word: &str) -> dekopon_shell::RunResult {
        let (output, _reader) = std::os::unix::net::UnixStream::pair().unwrap();
        invoker.run(
            dekopon_shell::RunCall {
                word: word.to_owned(),
                argv: Vec::new(),
            },
            Streams {
                stdin: None,
                stdout: OwnedFd::from(output),
            },
            &TreeContext::new(
                dekopon_process::Limits::default(),
                dekopon_process::CallBudget::new(1),
                dekopon_test_support::shell_threads(),
            ),
        )
    }

    #[test]
    fn follow_without_a_ticket_refuses_before_any_frame() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let invoker = FollowInvoker::new(
            RunLeg {
                calls: Arc::clone(&calls),
                input: json!({}),
                secret_use: None,
                result: CapabilityCallResult::Succeeded,
            },
            None,
        );
        assert!(matches!(
            run_word(&invoker, "follow"),
            dekopon_shell::RunResult::Returned(CapabilityCallResult::Failed { .. })
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn follow_marks_stay_from_the_reported_proposal() {
        let directory = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            Journal::open(
                directory.path(),
                Duration::from_secs(604800),
                Duration::from_secs(604800),
            )
            .unwrap(),
        );
        let table = Arc::new(FollowTable::load(journal).unwrap());
        let secret = dekopon_core::SecretUseProposal::HttpBasic {
            secret: "drn:com.xrl:secret:prod:api/token".parse().unwrap(),
            username: "deploy".to_owned(),
        };
        for (index, (input, secret_use, result, expected)) in [
            (
                json!({"forged":true}),
                None,
                CapabilityCallResult::Succeeded,
                false,
            ),
            (
                json!({}),
                Some(secret),
                CapabilityCallResult::Succeeded,
                false,
            ),
            (
                json!({}),
                None,
                CapabilityCallResult::Denied {
                    reason: "policy".to_owned(),
                },
                false,
            ),
            (json!({}), None, CapabilityCallResult::Succeeded, true),
        ]
        .into_iter()
        .enumerate()
        {
            let key = format!("{index:064x}");
            let invoker = FollowInvoker::new(
                RunLeg {
                    calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    input,
                    secret_use,
                    result,
                },
                Some((Arc::clone(&table), key.clone(), 0)),
            );
            assert!(matches!(
                run_word(&invoker, "another-word"),
                dekopon_shell::RunResult::Invoked { .. }
            ));
            assert_eq!(
                matches!(
                    table.ticket(&key, SystemTime::now()),
                    FollowTicket::Active { .. }
                ),
                expected
            );
        }
    }

    struct ResultLeg(CapabilityCallResult);

    impl CapabilityInvoker for ResultLeg {
        fn granted(&self) -> Vec<String> {
            vec!["dekopon-provider-follow.stay".into()]
        }
        fn invoke(
            &self,
            _proposal: CommandProposal,
            _streams: Streams,
            _tree: &TreeContext,
        ) -> CapabilityCallResult {
            self.0.clone()
        }
    }

    fn call(
        invoker: &FollowInvoker<ResultLeg>,
        capability: &str,
        input: serde_json::Value,
    ) -> CapabilityCallResult {
        let (output, _reader) = std::os::unix::net::UnixStream::pair().unwrap();
        let tree = TreeContext::new(
            dekopon_process::Limits::default(),
            dekopon_process::CallBudget::new(2),
            dekopon_test_support::shell_threads(),
        );
        invoker.invoke(
            CommandProposal {
                capability: capability.into(),
                input,
                secret_use: None,
                report: None,
            },
            Streams {
                stdin: None,
                stdout: OwnedFd::from(output),
            },
            &tree,
        )
    }

    #[test]
    fn only_a_successful_same_leg_no_argument_stay_invites_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            Journal::open(
                dir.path(),
                Duration::from_secs(7 * 86400),
                Duration::from_secs(7 * 86400),
            )
            .unwrap(),
        );
        let table = Arc::new(FollowTable::load(Arc::clone(&journal)).unwrap());
        let key = "c".repeat(64);
        let ticket = Some((Arc::clone(&table), key.clone(), 0));
        let denied = FollowInvoker::new(
            ResultLeg(CapabilityCallResult::Denied {
                reason: "policy".into(),
            }),
            ticket.clone(),
        );
        assert!(matches!(
            call(&denied, "dekopon-provider-follow.stay", json!({})),
            CapabilityCallResult::Denied { .. }
        ));
        assert_eq!(table.ticket(&key, SystemTime::now()), FollowTicket::Absent);
        let success = FollowInvoker::new(ResultLeg(CapabilityCallResult::Succeeded), ticket);
        assert!(matches!(
            call(&success, "dekopon-provider-follow.leave", json!({})),
            CapabilityCallResult::Succeeded
        ));
        assert!(matches!(
            call(
                &success,
                "dekopon-provider-follow.stay",
                json!({"coordinate": "forged"})
            ),
            CapabilityCallResult::Succeeded
        ));
        assert_eq!(table.ticket(&key, SystemTime::now()), FollowTicket::Absent);
        let (capture, _guard) = crate::tests::capture_spans();
        assert!(matches!(
            call(&success, "dekopon-provider-follow.stay", json!({})),
            CapabilityCallResult::Succeeded
        ));
        let events = capture.events_where("dekopon_gatewayd", &[("audit.event", "gateway.follow")]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].field("operation"), Some("invited"));
        assert_eq!(events[0].field("route"), Some("0"));
        assert!(events[0].field("subject").is_none());
        assert!(events[0].field("conversation").is_none());
        for pending in std::mem::take(&mut *success.pending_follows().lock()) {
            pending.write();
        }
        assert!(matches!(
            FollowTable::load(journal)
                .unwrap()
                .ticket(&key, SystemTime::now()),
            FollowTicket::Active { .. }
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refresh_racing_a_banish_cannot_reinsert_its_ticket() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            Journal::open(
                dir.path(),
                Duration::from_secs(7 * 86400),
                Duration::from_secs(7 * 86400),
            )
            .unwrap(),
        );
        let table = Arc::new(FollowTable::load(Arc::clone(&journal)).unwrap());
        let key = "c".repeat(64);
        table.stay_and_write(&key, SystemTime::now()).unwrap();
        let (locked, ready) = tokio::sync::oneshot::channel();
        let (release, unblock) = std::sync::mpsc::sync_channel::<()>(1);
        let held = Arc::clone(&journal);
        let blocker = tokio::task::spawn_blocking(move || {
            let _lock = held.hold_io_for_test();
            locked.send(()).unwrap();
            unblock
                .recv_timeout(Duration::from_secs(8))
                .expect("release");
        });
        tokio::time::timeout(Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
        let writing = Arc::clone(&table);
        let writing_key = key.clone();
        let refresh =
            tokio::task::spawn_blocking(move || writing.refresh(&writing_key, SystemTime::now()));
        tokio::time::timeout(Duration::from_secs(2), async {
            while !table.writing_for_test() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("refresh entered disk stage");
        table.suspend(&key);
        assert_eq!(table.ticket(&key, SystemTime::now()), FollowTicket::Absent);
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), blocker)
            .await
            .unwrap()
            .unwrap();
        // The disk lock is taken one statement before the writer check, so the banish can land
        // on either side of it: NotFound before, PermissionDenied after the marker write.
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), refresh)
                .await
                .unwrap()
                .unwrap(),
            Err(JournalError::Io {
                kind: io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
            })
        ));
        assert_eq!(table.ticket(&key, SystemTime::now()), FollowTicket::Absent);
        table.leave(&key).unwrap();
        assert!(!dir.path().join(format!("{key}.followed")).exists());
    }

    #[test]
    fn a_banished_ticket_cannot_be_reinvited_before_marker_removal() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            Journal::open(
                dir.path(),
                Duration::from_secs(7 * 86400),
                Duration::from_secs(7 * 86400),
            )
            .unwrap(),
        );
        let table = Arc::new(FollowTable::load(journal).unwrap());
        let now = SystemTime::now();
        let key = "c".repeat(64);
        table.stay_and_write(&key, now).unwrap();
        table.suspend(&key);
        assert_eq!(table.ticket(&key, now), FollowTicket::Absent);
        assert!(matches!(
            table.stay_and_write(&key, now),
            Err(JournalError::Io {
                kind: io::ErrorKind::PermissionDenied
            })
        ));
        table.leave(&key).unwrap();
        assert!(table.stay_and_write(&key, now).is_ok());
    }

    #[test]
    fn a_failed_refresh_removes_the_resident_ticket() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            Journal::open(
                dir.path(),
                Duration::from_secs(7 * 86400),
                Duration::from_secs(7 * 86400),
            )
            .unwrap(),
        );
        let table = Arc::new(FollowTable::load(journal).unwrap());
        let key = "d".repeat(64);
        let now = SystemTime::now();
        table.stay_and_write(&key, now).unwrap();
        std::fs::remove_file(dir.path().join(format!("{key}.followed"))).unwrap();
        std::fs::create_dir(dir.path().join(format!("{key}.followed"))).unwrap();
        assert!(matches!(
            table.refresh(&key, now),
            Err(JournalError::Io { .. })
        ));
        assert_eq!(table.ticket(&key, now), FollowTicket::Absent);
    }

    #[test]
    fn a_successful_leave_waits_for_delivery_before_expiring_its_ticket() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            Journal::open(
                dir.path(),
                Duration::from_secs(7 * 86400),
                Duration::from_secs(7 * 86400),
            )
            .unwrap(),
        );
        let table = Arc::new(FollowTable::load(journal).unwrap());
        let key = "a".repeat(64);
        let other = "b".repeat(64);
        table.stay_and_write(&key, SystemTime::now()).unwrap();
        table.stay_and_write(&other, SystemTime::now()).unwrap();
        let success = FollowInvoker::new(
            ResultLeg(CapabilityCallResult::Succeeded),
            Some((Arc::clone(&table), key.clone(), 0)),
        );
        assert!(matches!(
            call(&success, "dekopon-provider-follow.leave", json!({})),
            CapabilityCallResult::Succeeded
        ));
        assert!(success.leave_request().load(Ordering::Acquire));
        assert!(matches!(
            table.ticket(&key, SystemTime::now()),
            FollowTicket::Active { .. }
        ));
        table.leave(&key).unwrap();
        assert_eq!(table.ticket(&key, SystemTime::now()), FollowTicket::Absent);
        assert!(matches!(
            table.ticket(&other, SystemTime::now()),
            FollowTicket::Active { .. }
        ));
        let failed = FollowInvoker::new(
            ResultLeg(CapabilityCallResult::Denied {
                reason: "policy".into(),
            }),
            Some((Arc::clone(&table), other.clone(), 0)),
        );
        assert!(matches!(
            call(&failed, "dekopon-provider-follow.leave", json!({})),
            CapabilityCallResult::Denied { .. }
        ));
        assert!(!failed.leave_request().load(Ordering::Acquire));
        let invalid = FollowInvoker::new(
            ResultLeg(CapabilityCallResult::Succeeded),
            Some((Arc::clone(&table), other.clone(), 0)),
        );
        assert!(matches!(
            call(
                &invalid,
                "dekopon-provider-follow.leave",
                json!({"claim": "forged"})
            ),
            CapabilityCallResult::Succeeded
        ));
        assert!(!invalid.leave_request().load(Ordering::Acquire));
        let unavailable = FollowInvoker::new(ResultLeg(CapabilityCallResult::Succeeded), None);
        assert!(matches!(
            call(&unavailable, "dekopon-provider-follow.stay", json!({})),
            CapabilityCallResult::Failed { .. }
        ));
    }

    #[test]
    fn a_failed_follow_write_leaves_the_ticket_absent() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            Journal::open(
                dir.path(),
                Duration::from_secs(7 * 86400),
                Duration::from_secs(7 * 86400),
            )
            .unwrap(),
        );
        let table = Arc::new(FollowTable::load(journal).unwrap());
        let key = "d".repeat(64);
        std::fs::create_dir(dir.path().join(format!("{key}.followed"))).unwrap();
        let success = FollowInvoker::new(
            ResultLeg(CapabilityCallResult::Succeeded),
            Some((Arc::clone(&table), key.clone(), 0)),
        );
        let (capture, _guard) = crate::tests::capture_spans();
        assert!(matches!(
            call(&success, "dekopon-provider-follow.stay", json!({})),
            CapabilityCallResult::Succeeded
        ));
        for pending in std::mem::take(&mut *success.pending_follows().lock()) {
            pending.write();
        }
        assert_eq!(table.ticket(&key, SystemTime::now()), FollowTicket::Absent);
        let refused = capture.events_where(
            "dekopon_gatewayd",
            &[("audit.event", "gateway.follow"), ("operation", "refused")],
        );
        assert_eq!(refused.len(), 1);
    }

    #[test]
    fn stable_route_key_is_shared_by_speakers_and_survives_selector_reordering() {
        let selector = ConversationMatch {
            kind: ConversationKindMatch::Kinds(vec![
                ConversationKind::Thread,
                ConversationKind::Channel,
            ]),
            container: None,
            ids: Some(vec!["b".into(), "a".into()]),
        };
        let reordered = ConversationMatch {
            kind: ConversationKindMatch::Kinds(vec![
                ConversationKind::Channel,
                ConversationKind::Thread,
            ]),
            container: None,
            ids: Some(vec!["a".into(), "b".into()]),
        };
        let key = follow_key("discord", "agent", &selector, "channel");
        assert_eq!(key, follow_key("discord", "agent", &reordered, "channel"));
        assert_ne!(key, follow_key("discord", "other", &selector, "channel"));
        assert_ne!(key, follow_key("discord", "agent", &selector, "another"));
        let with_delimiter = ConversationMatch {
            ids: Some(vec!["a,b".into()]),
            ..selector.clone()
        };
        assert_ne!(
            follow_key("discord", "agent", &with_delimiter, "channel"),
            follow_key("discord", "agent", &selector, "channel")
        );
    }

    #[test]
    fn a_live_marker_with_an_old_mtime_survives_journal_boot() {
        let dir = tempfile::tempdir().unwrap();
        let at = SystemTime::now();
        let key = "f".repeat(64);
        let journal = Journal::open(
            dir.path(),
            Duration::from_secs(7 * 86400),
            Duration::from_secs(7 * 86400),
        )
        .unwrap();
        journal
            .write_follow(
                &key,
                FollowMarker {
                    at_ms: millis(at),
                    until_ms: millis(at + LAPSE),
                },
            )
            .unwrap();
        let path = dir.path().join(format!("{key}.followed"));
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(at - Duration::from_secs(2 * 86400))
            .unwrap();
        let restarted = Arc::new(
            Journal::open(
                dir.path(),
                Duration::from_secs(7 * 86400),
                Duration::from_secs(7 * 86400),
            )
            .unwrap(),
        );
        assert!(matches!(
            FollowTable::load(restarted).unwrap().ticket(&key, at),
            FollowTicket::Active { .. }
        ));
        assert!(path.exists());
        let later = Arc::new(
            Journal::open_at(
                dir.path(),
                Duration::from_secs(7 * 86400),
                Duration::from_secs(7 * 86400),
                at + LAPSE,
            )
            .unwrap(),
        );
        assert_eq!(
            FollowTable::load(later).unwrap().ticket(&key, at + LAPSE),
            FollowTicket::Absent
        );
        assert!(!path.exists());
    }

    fn follow_table() -> (tempfile::TempDir, Arc<Journal>, Arc<FollowTable>) {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            Journal::open(
                dir.path(),
                Duration::from_secs(7 * 86400),
                Duration::from_secs(7 * 86400),
            )
            .unwrap(),
        );
        let table = Arc::new(FollowTable::load(Arc::clone(&journal)).unwrap());
        (dir, journal, table)
    }

    fn lapse(table: &FollowTable, dir: &std::path::Path, key: &str, now: SystemTime) {
        let marker = FollowMarker {
            at_ms: millis(now) - 2,
            until_ms: millis(now) - 1,
        };
        std::fs::write(
            dir.join(format!("{key}.followed")),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
        let seq = MarkerSeq(table.next_seq.fetch_add(1, Ordering::Relaxed));
        table
            .resident
            .lock()
            .insert(key.to_owned(), Marked { marker, seq });
        table.written.lock().insert(key.to_owned(), seq);
    }

    fn reloaded(journal: &Arc<Journal>, key: &str, now: SystemTime) -> FollowTicket {
        FollowTable::load(Arc::clone(journal))
            .unwrap()
            .ticket(key, now)
    }

    #[test]
    fn lapsed_tickets_never_block_a_new_invite() {
        let (dir, journal, table) = follow_table();
        let now = SystemTime::now();
        let lapsed: Vec<String> = (0..MAX_TICKETS)
            .map(|index| format!("{index:064x}"))
            .collect();
        for key in &lapsed {
            lapse(&table, dir.path(), key, now);
        }
        let fresh = "f".repeat(64);
        let pending = table.stay(&fresh, now, 0).unwrap();
        assert!(matches!(
            table.ticket(&fresh, now),
            FollowTicket::Active { .. }
        ));
        assert!(dir.path().join(format!("{}.followed", lapsed[0])).exists());
        assert!(!dir.path().join(format!("{fresh}.followed")).exists());
        pending.write();
        for key in &lapsed {
            assert!(!dir.path().join(format!("{key}.followed")).exists());
        }
        assert!(matches!(
            reloaded(&journal, &fresh, now),
            FollowTicket::Active { .. }
        ));
    }

    #[test]
    fn a_lapsed_unlink_never_drops_a_rewritten_marker() {
        let (dir, journal, table) = follow_table();
        let now = SystemTime::now();
        let key = "a".repeat(64);
        lapse(&table, dir.path(), &key, now);
        let first = table.stay(&"b".repeat(64), now, 0).unwrap();
        let second = table.stay(&key, now, 1).unwrap();
        second.write();
        first.write();
        assert!(matches!(
            reloaded(&journal, &key, now),
            FollowTicket::Active { .. }
        ));
    }

    #[test]
    fn a_failed_refresh_never_drops_a_new_invite() {
        for held in [true, false] {
            let (dir, journal, table) = follow_table();
            let now = SystemTime::now();
            let key = "c".repeat(64);
            table.stay_and_write(&key, now).unwrap();
            let marker = dir.path().join(format!("{key}.followed"));
            std::fs::remove_file(&marker).unwrap();
            std::fs::create_dir(&marker).unwrap();
            let pending = if held {
                std::thread::scope(|scope| {
                    let (locked, ready) = std::sync::mpsc::sync_channel(1);
                    let (release, unblock) = std::sync::mpsc::sync_channel::<()>(1);
                    let held = &journal;
                    let holder = scope.spawn(move || {
                        let _lock = held.hold_io_for_test();
                        locked.send(()).unwrap();
                        unblock.recv_timeout(Duration::from_secs(8)).unwrap();
                    });
                    ready.recv_timeout(Duration::from_secs(2)).unwrap();
                    let refresh = scope.spawn(|| table.refresh(&key, now));
                    while !table.writing_for_test() {
                        std::thread::yield_now();
                    }
                    let pending = table.stay(&key, now, 1).unwrap();
                    release.send(()).unwrap();
                    holder.join().unwrap();
                    assert!(matches!(
                        refresh.join().unwrap(),
                        Err(JournalError::Io { .. })
                    ));
                    pending
                })
            } else {
                let pending = table.stay(&key, now, 1).unwrap();
                assert!(matches!(
                    table.refresh(&key, now),
                    Err(JournalError::Io { .. })
                ));
                pending
            };
            assert!(matches!(
                table.ticket(&key, now),
                FollowTicket::Active { .. }
            ));
            std::fs::remove_dir(&marker).unwrap();
            pending.write();
            assert!(matches!(
                reloaded(&journal, &key, now),
                FollowTicket::Active { .. }
            ));
        }
    }

    #[test]
    fn stay_never_waits_on_disk() {
        let (dir, journal, table) = follow_table();
        let now = SystemTime::now();
        let other = "d".repeat(64);
        let key = "e".repeat(64);
        table.stay_and_write(&other, now).unwrap();
        let pending = std::thread::scope(|scope| {
            let (locked, ready) = std::sync::mpsc::sync_channel(1);
            let (release, unblock) = std::sync::mpsc::sync_channel::<()>(1);
            let held = &journal;
            let holder = scope.spawn(move || {
                let _lock = held.hold_io_for_test();
                locked.send(()).unwrap();
                unblock.recv_timeout(Duration::from_secs(8)).unwrap();
            });
            ready.recv_timeout(Duration::from_secs(2)).unwrap();
            let refresh = scope.spawn(|| table.refresh(&other, now));
            while !table.writing_for_test() {
                std::thread::yield_now();
            }
            let (sent, stayed) = std::sync::mpsc::sync_channel(1);
            let (staying, staying_key) = (&table, &key);
            let staying =
                scope.spawn(move || sent.send(staying.stay(staying_key, now, 0)).unwrap());
            let pending = stayed.recv_timeout(Duration::from_secs(2));
            assert!(table.writing_for_test());
            release.send(()).unwrap();
            holder.join().unwrap();
            staying.join().unwrap();
            refresh.join().unwrap().unwrap();
            pending.expect("stay never waits on disk").unwrap()
        });
        assert!(!dir.path().join(format!("{key}.followed")).exists());
        pending.write();
        assert!(dir.path().join(format!("{key}.followed")).exists());
        assert!(matches!(
            reloaded(&journal, &key, now),
            FollowTicket::Active { .. }
        ));
    }

    #[test]
    fn corrupt_marker_is_removed_and_missing_marker_stays_absent() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            Journal::open(
                dir.path(),
                Duration::from_secs(7 * 86400),
                Duration::from_secs(7 * 86400),
            )
            .unwrap(),
        );
        let key = "b".repeat(64);
        std::fs::write(dir.path().join(format!("{key}.followed")), b"{broken").unwrap();
        let (capture, _guard) = crate::tests::capture_spans();
        let table = Arc::new(FollowTable::load(journal).unwrap());
        let events = capture.events_where("dekopon_gatewayd", &[("event", "gateway_follow")]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].field("reason"), Some("corrupt"));
        assert!(events[0].field("subject").is_none());
        assert_eq!(table.ticket(&key, SystemTime::now()), FollowTicket::Absent);
        assert!(!dir.path().join(format!("{key}.followed")).exists());
        std::fs::write(dir.path().join(format!("{key}.followed")), vec![b'x'; 4097]).unwrap();
        let journal = Arc::new(
            Journal::open(
                dir.path(),
                Duration::from_secs(7 * 86400),
                Duration::from_secs(7 * 86400),
            )
            .unwrap(),
        );
        assert_eq!(
            FollowTable::load(journal)
                .unwrap()
                .ticket(&key, SystemTime::now()),
            FollowTicket::Absent
        );
        assert!(!dir.path().join(format!("{key}.followed")).exists());
    }

    #[test]
    fn boot_keeps_latest_deadlines_and_removes_one_excess_marker() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            Journal::open(
                dir.path(),
                Duration::from_secs(7 * 86400),
                Duration::from_secs(7 * 86400),
            )
            .unwrap(),
        );
        let now = SystemTime::now();
        for index in 0..=MAX_TICKETS {
            let key = format!("{index:064x}");
            journal
                .write_follow(
                    &key,
                    FollowMarker {
                        at_ms: millis(now),
                        until_ms: millis(now)
                            + 500_000
                            + if index == MAX_TICKETS / 2 {
                                0
                            } else {
                                index as u64 + 1
                            },
                    },
                )
                .unwrap();
        }
        let (capture, _guard) = crate::tests::capture_spans();
        let table =
            Arc::new(FollowTable::load(journal).expect("valid overflow must not refuse boot"));
        assert_eq!(table.resident.lock().len(), MAX_TICKETS);
        let earliest = format!("{:064x}", MAX_TICKETS / 2);
        let latest = format!("{:064x}", MAX_TICKETS);
        assert_eq!(table.ticket(&earliest, now), FollowTicket::Absent);
        assert!(matches!(
            table.ticket(&latest, now),
            FollowTicket::Active { .. }
        ));
        assert!(!dir.path().join(format!("{earliest}.followed")).exists());
        let events = capture.events_where(
            "dekopon_gatewayd",
            &[("event", "gateway_follow"), ("reason", "over-capacity")],
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].field("count"), Some("1"));
        assert!(events[0].field("subject").is_none());
    }

    #[test]
    fn unreadable_marker_refuses_boot_and_capacity_refuses_an_invite() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            Journal::open(
                dir.path(),
                Duration::from_secs(7 * 86400),
                Duration::from_secs(7 * 86400),
            )
            .unwrap(),
        );
        let key = "e".repeat(64);
        std::fs::create_dir(dir.path().join(format!("{key}.followed"))).unwrap();
        assert!(matches!(
            FollowTable::load(Arc::clone(&journal)),
            Err(JournalError::Io { .. })
        ));
        std::fs::remove_dir(dir.path().join(format!("{key}.followed"))).unwrap();
        let table = Arc::new(FollowTable::load(journal).unwrap());
        let at = SystemTime::now();
        for index in 0..MAX_TICKETS {
            table.resident.lock().insert(
                format!("{index:064x}"),
                Marked {
                    marker: FollowMarker {
                        at_ms: millis(at),
                        until_ms: millis(at) + 86_400_000,
                    },
                    seq: MarkerSeq(u64::MAX - index as u64),
                },
            );
        }
        assert!(matches!(
            table.stay_and_write(&key, at),
            Err(JournalError::Io {
                kind: io::ErrorKind::OutOfMemory
            })
        ));
        assert_eq!(table.ticket(&key, at), FollowTicket::Absent);
    }

    #[test]
    fn persisted_ticket_lapses_on_injected_clock_and_leaves_without_history() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            Journal::open(
                dir.path(),
                Duration::from_secs(7 * 86400),
                Duration::from_secs(7 * 86400),
            )
            .unwrap(),
        );
        let table = Arc::new(FollowTable::load(Arc::clone(&journal)).unwrap());
        let at = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert_eq!(table.ticket("test", at), FollowTicket::Absent);
        let key = "a".repeat(64);
        table.stay_and_write(&key, at).unwrap();
        let restarted = FollowTable::load(Arc::clone(&journal)).unwrap();
        std::fs::remove_file(dir.path().join(format!("{key}.followed"))).unwrap();
        assert!(matches!(
            restarted.ticket(&key, at),
            FollowTicket::Active { .. }
        ));
        journal
            .write_follow(
                &key,
                FollowMarker {
                    at_ms: millis(at),
                    until_ms: millis(at + LAPSE),
                },
            )
            .unwrap();
        assert!(matches!(
            restarted.ticket(&key, at),
            FollowTicket::Active { .. }
        ));
        assert_eq!(restarted.ticket(&key, at + LAPSE), FollowTicket::Lapsed);
        restarted.leave(&key).unwrap();
        assert_eq!(restarted.ticket(&key, at), FollowTicket::Absent);
    }
}
