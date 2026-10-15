use std::{
    collections::HashMap,
    io,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dekopon_broker_protocol::ConversationMatch;
use dekopon_shell::{
    CapabilityCallResult, CapabilityDescription, CapabilityInvoker, CommandProposal, CommandRun,
    JobControl, Streams, TreeContext,
};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::journal::{FollowMarker, Journal, JournalError};

const LAPSE: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_TICKETS: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(
    dead_code,
    reason = "follow dispatch lands in the subsequent S3b change"
)]
pub(crate) enum FollowTicket {
    Active { until_ms: u64 },
    Lapsed,
    Absent,
}

pub(crate) struct FollowTable {
    journal: Arc<Journal>,
    resident: Mutex<HashMap<String, FollowMarker>>,
    writer: Mutex<()>,
}

impl FollowTable {
    pub(crate) fn load(journal: Arc<Journal>) -> Result<Self, JournalError> {
        Ok(Self {
            resident: Mutex::new(journal.load_follow()?.into_iter().collect()),
            writer: Mutex::new(()),
            journal,
        })
    }

    #[allow(
        dead_code,
        reason = "follow dispatch lands in the subsequent S3b change"
    )]
    pub(crate) fn ticket(&self, key: &str, now: SystemTime) -> FollowTicket {
        match self.resident.lock().get(key) {
            Some(marker) if millis(now) < marker.until_ms => FollowTicket::Active {
                until_ms: marker.until_ms,
            },
            Some(_) => FollowTicket::Lapsed,
            None => FollowTicket::Absent,
        }
    }

    pub(crate) fn stay(&self, key: &str, now: SystemTime) -> Result<(), JournalError> {
        let at_ms = millis(now);
        let until_ms = at_ms
            .checked_add(LAPSE.as_millis() as u64)
            .ok_or(JournalError::Io {
                kind: io::ErrorKind::InvalidInput,
            })?;
        let _writer = self.writer.lock();
        let lapsed = self
            .resident
            .lock()
            .iter()
            .filter(|(_, marker)| marker.until_ms <= at_ms)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for old in lapsed {
            self.journal.remove_follow(&old)?;
            self.resident.lock().remove(&old);
        }
        let resident = self.resident.lock();
        if !resident.contains_key(key) && resident.len() == MAX_TICKETS {
            return Err(JournalError::Io {
                kind: io::ErrorKind::OutOfMemory,
            });
        }
        drop(resident);
        let marker = FollowMarker { at_ms, until_ms };
        self.journal.write_follow(key, marker)?;
        self.resident.lock().insert(key.to_owned(), marker);
        Ok(())
    }

    pub(crate) fn leave(&self, key: &str) -> Result<(), JournalError> {
        let _writer = self.writer.lock();
        self.journal.remove_follow(key)?;
        self.resident.lock().remove(key);
        Ok(())
    }
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

// Length-delimited fields prevent a delimiter inside an operator-authored selector from aliasing a different route.
pub(crate) fn follow_key(
    transport: &str,
    agent: &str,
    selector: &ConversationMatch,
    conversation: &str,
) -> String {
    let mut digest = Sha256::new();
    let mut kinds = match &selector.kind {
        dekopon_broker_protocol::ConversationKindMatch::Any => vec!["any"],
        dekopon_broker_protocol::ConversationKindMatch::Kinds(kinds) => {
            kinds.iter().map(|kind| kind.as_str()).collect()
        }
    };
    kinds.sort_unstable();
    let mut ids = selector.ids.clone().unwrap_or_default();
    ids.sort_unstable();
    let fields = [
        transport.to_owned(),
        agent.to_owned(),
        kinds.join(","),
        selector.container.clone().unwrap_or_default(),
        ids.join(","),
        conversation.to_owned(),
    ];
    for field in fields {
        digest.update((field.len() as u64).to_be_bytes());
        digest.update(field.as_bytes());
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) struct FollowInvoker<I> {
    inner: I,
    ticket: Option<(Arc<FollowTable>, String, usize)>,
}

impl<I> FollowInvoker<I> {
    pub(crate) fn new(inner: I, ticket: Option<(Arc<FollowTable>, String, usize)>) -> Self {
        Self { inner, ticket }
    }
}

impl<I: CapabilityInvoker> CapabilityInvoker for FollowInvoker<I> {
    fn cancelled(&self) -> bool {
        self.inner.cancelled()
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
    fn run_command(&self, word: &str, argv: &[String], stdin_piped: bool) -> Option<CommandRun> {
        self.inner.run_command(word, argv, stdin_piped)
    }
    fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
        self.inner.describe(capability)
    }
    fn note(&self, text: &str, eta: Option<Duration>) {
        self.inner.note(text, eta);
    }
    fn script_started(&self, timeout: Duration) {
        self.inner.script_started(timeout);
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
        let stay = proposal.capability == "dekopon-provider-follow.stay"
            && proposal.secret_use.is_none()
            && proposal
                .input
                .as_object()
                .is_some_and(serde_json::Map::is_empty);
        let result = self.inner.invoke(proposal, streams, tree);
        if stay
            && matches!(
                result,
                CapabilityCallResult::Succeeded | CapabilityCallResult::SucceededWithStderr(_)
            )
            && let Some((table, key, route)) = &self.ticket
        {
            match table.stay(key, SystemTime::now()) {
                Ok(()) => {
                    tracing::info!(audit.event = "gateway.follow", operation = "invited", route)
                }
                Err(error) => {
                    tracing::warn!(
                        audit.event = "gateway.follow",
                        operation = "refused",
                        route,
                        reason = error.label()
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
            dekopon_shell::Limits::default(),
            dekopon_shell::CallBudget::new(2),
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
        let journal = Arc::new(Journal::open(dir.path(), Duration::from_secs(7 * 86400)).unwrap());
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
        assert!(matches!(
            FollowTable::load(journal)
                .unwrap()
                .ticket(&key, SystemTime::now()),
            FollowTicket::Active { .. }
        ));
    }

    #[test]
    fn a_failed_marker_write_never_activates_a_ticket() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(dir.path(), Duration::from_secs(7 * 86400)).unwrap());
        let table = Arc::new(FollowTable::load(journal).unwrap());
        let key = "d".repeat(64);
        std::fs::create_dir(dir.path().join(format!("{key}.followed"))).unwrap();
        let success = FollowInvoker::new(
            ResultLeg(CapabilityCallResult::Succeeded),
            Some((Arc::clone(&table), key.clone(), 0)),
        );
        assert!(matches!(
            call(&success, "dekopon-provider-follow.stay", json!({})),
            CapabilityCallResult::Failed { .. }
        ));
        assert_eq!(table.ticket(&key, SystemTime::now()), FollowTicket::Absent);
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
    }

    #[test]
    fn a_live_marker_with_an_old_mtime_survives_journal_boot() {
        let dir = tempfile::tempdir().unwrap();
        let at = SystemTime::now();
        let key = "f".repeat(64);
        let journal = Journal::open(dir.path(), Duration::from_secs(7 * 86400)).unwrap();
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
        let restarted =
            Arc::new(Journal::open(dir.path(), Duration::from_secs(7 * 86400)).unwrap());
        assert!(matches!(
            FollowTable::load(restarted).unwrap().ticket(&key, at),
            FollowTicket::Active { .. }
        ));
        assert!(path.exists());
        let later = Arc::new(
            Journal::open_at(dir.path(), Duration::from_secs(7 * 86400), at + LAPSE).unwrap(),
        );
        assert_eq!(
            FollowTable::load(later).unwrap().ticket(&key, at + LAPSE),
            FollowTicket::Absent
        );
        assert!(!path.exists());
    }

    #[test]
    fn elapsed_resident_tickets_release_capacity_for_a_new_invitation() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(dir.path(), Duration::from_secs(7 * 86400)).unwrap());
        let table = FollowTable::load(Arc::clone(&journal)).unwrap();
        let now = SystemTime::now();
        for index in 0..MAX_TICKETS {
            table.resident.lock().insert(
                format!("{index:064x}"),
                FollowMarker {
                    at_ms: millis(now) - 2,
                    until_ms: millis(now) - 1,
                },
            );
        }
        let first = format!("{:064x}", 0);
        journal
            .write_follow(
                &first,
                FollowMarker {
                    at_ms: millis(now) - 2,
                    until_ms: millis(now) - 1,
                },
            )
            .unwrap();
        let fresh = "f".repeat(64);
        table.stay(&fresh, now).unwrap();
        assert!(matches!(
            table.ticket(&fresh, now),
            FollowTicket::Active { .. }
        ));
        assert!(!dir.path().join(format!("{first}.followed")).exists());
    }

    #[test]
    fn corrupt_marker_is_removed_and_missing_marker_stays_absent() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(dir.path(), Duration::from_secs(7 * 86400)).unwrap());
        let key = "b".repeat(64);
        std::fs::write(dir.path().join(format!("{key}.followed")), b"{broken").unwrap();
        let (capture, _guard) = crate::tests::capture_spans();
        let table = FollowTable::load(journal).unwrap();
        let events = capture.events_where("dekopon_gatewayd", &[("event", "gateway_follow")]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].field("reason"), Some("corrupt"));
        assert!(events[0].field("subject").is_none());
        assert_eq!(table.ticket(&key, SystemTime::now()), FollowTicket::Absent);
        assert!(!dir.path().join(format!("{key}.followed")).exists());
        std::fs::write(dir.path().join(format!("{key}.followed")), vec![b'x'; 4097]).unwrap();
        let journal = Arc::new(Journal::open(dir.path(), Duration::from_secs(7 * 86400)).unwrap());
        assert_eq!(
            FollowTable::load(journal)
                .unwrap()
                .ticket(&key, SystemTime::now()),
            FollowTicket::Absent
        );
        assert!(!dir.path().join(format!("{key}.followed")).exists());
    }

    #[test]
    fn unreadable_marker_refuses_boot_and_capacity_refuses_an_invite() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(dir.path(), Duration::from_secs(7 * 86400)).unwrap());
        let key = "e".repeat(64);
        std::fs::create_dir(dir.path().join(format!("{key}.followed"))).unwrap();
        assert!(matches!(
            FollowTable::load(Arc::clone(&journal)),
            Err(JournalError::Io { .. })
        ));
        std::fs::remove_dir(dir.path().join(format!("{key}.followed"))).unwrap();
        let table = FollowTable::load(journal).unwrap();
        let at = SystemTime::now();
        for index in 0..MAX_TICKETS {
            table.resident.lock().insert(
                format!("{index:064x}"),
                FollowMarker {
                    at_ms: millis(at),
                    until_ms: millis(at) + 86_400_000,
                },
            );
        }
        assert!(matches!(
            table.stay(&key, at),
            Err(JournalError::Io {
                kind: io::ErrorKind::OutOfMemory
            })
        ));
        assert_eq!(table.ticket(&key, at), FollowTicket::Absent);
    }

    #[test]
    fn persisted_ticket_lapses_on_injected_clock_and_leaves_without_history() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(dir.path(), Duration::from_secs(7 * 86400)).unwrap());
        let table = FollowTable::load(Arc::clone(&journal)).unwrap();
        let at = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert_eq!(table.ticket("test", at), FollowTicket::Absent);
        let key = "a".repeat(64);
        table.stay(&key, at).unwrap();
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
