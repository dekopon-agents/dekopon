use super::*;
use crate::{
    config::memory::{ResolvedPolicy, Routine, RoutineKind, SelectionMode},
    memory::{
        generation,
        journal::{Entry, JournalMemoryTools},
    },
};
use dekopon_agent::{
    memory_tools::MemoryTools as _,
    prompt::{Exchange, Representation},
};
use dekopon_test_support::BrokerFixture;
use std::num::NonZeroU64;

const CANARY: &str = "SOURCE_ONLY_CANARY_43";
const LOG: &str = "check-log: build passed; lint passed; integration passed; coverage passed; SOURCE_ONLY_CANARY_43";
const HEADER: &str = "\n[tool: check-log, outcome: succeeded, exit: Some(0)]";
const LABEL: &str = "\n[result] ";

fn policy(mode: SelectionMode) -> ResolvedPolicy {
    let mut policy = ResolvedPolicy::default();
    policy.routines.push(Routine {
        id: "summary".into(),
        kind: RoutineKind::Prefix,
        max_bytes: NonZeroU64::new(256),
        model: None,
        parts: None,
    });
    policy.retain.push("summary".into());
    policy.selection.mode = mode;
    policy.selection.routine = Some("summary".into());
    policy
}

#[expect(
    clippy::too_many_arguments,
    reason = "one table row compares the prompt and every required selection field"
)]
async fn selected_summary(
    summary: &str,
    pin: bool,
    expected: &str,
    requested: &str,
    actual: &str,
    savings: i64,
    reason: &str,
    decider_choice: Option<&str>,
) {
    selected_summary_with_limit(
        summary,
        pin,
        expected,
        requested,
        actual,
        savings,
        reason,
        decider_choice,
        None,
        false,
    )
    .await;
}

#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "one gateway fixture checks the same request, broker and selection event across all cases"
)]
async fn selected_summary_with_limit(
    summary: &str,
    pin: bool,
    expected: &str,
    requested: &str,
    actual: &str,
    savings: i64,
    reason: &str,
    decider_choice: Option<&str>,
    max_bytes: Option<usize>,
    summary_only: bool,
) {
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let journal_dir = temporary();
    let decider_mode = decider_choice.is_some_and(|choice| choice != "preference");
    let decider_granted = decider_mode && decider_choice != Some("ungranted");
    let invoked = decider_granted && decider_choice != Some("budget");
    let mut granted_ids = vec!["cli-probe.upper"];
    if decider_granted {
        granted_ids.push("decider.choose");
    }
    let mut capabilities = vec![capability("cli-probe.upper")];
    if decider_granted {
        capabilities.push(capability("decider.choose"));
    }
    let mut responses = std::collections::VecDeque::from([ResponseEnvelope::chat_capabilities(
        capabilities,
        Vec::new(),
        BTreeMap::new(),
        None,
        None,
    )]);
    let choice = decider_choice.map(str::to_owned);
    let mut broker = BrokerFixture::scripted(directory.path(), move |request, mut descriptors| {
        if let Some(response) = responses.pop_front() {
            return Some((response, Vec::new()));
        }
        if let Some(choice) = &choice
            && choice != "budget"
            && choice != "ungranted"
            && choice != "preference"
        {
            let BrokerRequest::Invoke { invocation, .. } = &request.request else {
                panic!("expected decision invocation")
            };
            assert_eq!(invocation.capability.to_string(), "decider.choose");
            if choice == "failed" {
                return Some((
                    ResponseEnvelope::invocation(
                        record_result(InvocationOutcome::Failed, Some("provider refused")),
                        Vec::new(),
                        Vec::new(),
                        Vec::new(),
                    ),
                    Vec::new(),
                ));
            }
            let metadata = invocation.input.to_string();
            let evidence: Value = serde_json::from_str(&metadata).expect("bounded decision input");
            assert_eq!(
                evidence["evidence"][0]["bytes"],
                "status".len() + "ok".len() + LOG.len()
            );
            assert!(
                !metadata.contains(CANARY),
                "the broker must not see the check-log text"
            );
            let mut stdout =
                std::os::unix::net::UnixStream::from(descriptors.pop().expect("stdout"));
            use std::io::Write as _;
            writeln!(
                stdout,
                "{}",
                if choice == "refused" { json!({"refused":true,"backend":"fixture","model":"fixture"}) }
                else { json!({"choice": if choice == "invalid" { "unknown" } else if choice == "missing" { "summary" } else { choice }, "backend":"fixture", "model":"fixture"}) }
            )
            .unwrap();
            return Some((
                ResponseEnvelope::invocation(record_success(), Vec::new(), Vec::new(), Vec::new()),
                Vec::new(),
            ));
        }
        panic!("unexpected broker request")
    });
    let client = ResolvedBroker {
        socket_path: broker.socket().to_owned(),
        server_uid: crate::current_uid(),
        frame: FrameLimits::default(),
    };
    let model = ModelScript::new([answer("follow-up")]);
    let runner = journaled_runner(client, Arc::new(Arc::clone(&model)), journal_dir.path());
    let mut window = recall_window(RecallSource::Journal);
    if let Some(bytes) = max_bytes {
        window.limits.max_bytes = bytes;
    }
    let mut route = persistent_route(model_config(), window);
    let mut selection = policy(if decider_mode {
        SelectionMode::Decider
    } else if decider_choice == Some("preference") {
        SelectionMode::AgentPreference
    } else {
        SelectionMode::Fixed
    });
    if decider_mode {
        selection.selection.routine = None;
        selection.selection.fallback = Some(
            if decider_choice == Some("missing") {
                "original"
            } else {
                "summary"
            }
            .into(),
        );
        selection.selection.decision_capability = Some("decider.choose".into());
    }
    if decider_choice == Some("preference") {
        selection.selection.routine = Some("original".into());
        selection.preferred_routine = Some("summary".into());
    }
    if decider_choice == Some("budget") {
        route.limits.max_capability_calls = 0;
    }
    route.memory_policy = Some(selection);
    if summary_only {
        route.memory_policy.as_mut().unwrap().retain = vec!["summary".into()];
    }
    let config = route.memory_policy.as_ref().unwrap();
    let original_version = generation::version(&config.routines[0]);
    let summary_version = generation::version(&config.routines[1]);
    let mut meta = Representation::original(LOG, &original_version);
    meta.text = None;
    let result = Representation {
        routine_id: "original".into(),
        version: original_version.clone(),
        source_record_id: 0,
        part: None,
        kind: "original".into(),
        captured_bytes: LOG.len() as u64,
        text: Some(LOG.into()),
    };
    let replacement = Representation {
        routine_id: "summary".into(),
        version: summary_version.clone(),
        source_record_id: 0,
        part: None,
        kind: "prefix".into(),
        captured_bytes: summary.len() as u64,
        text: Some(summary.into()),
    };
    let turn = ConversationTurn::completed("status", "ok").with_exchanges(vec![Exchange {
        tool: "check-log".into(),
        request: Vec::new(),
        result: vec![result],
        outcome: "succeeded".into(),
        exit_status: Some(0),
    }]);
    let turn = if summary_only {
        turn.with_memory_id(crate::journal::mint_turn_id())
    } else {
        turn
    };
    let inbound = message("next question");
    let key = ConversationKey::private(
        &route.agent,
        &route.transport,
        &inbound.conversation.key(),
        &inbound.subject,
    );
    let journal = runner.journal.as_ref().unwrap();
    let stem = key.journal_stem();
    let grant = crate::journal::grant_digest(&granted(&granted_ids));
    if decider_choice == Some("missing") {
        journal
            .append(
                &stem,
                &Entry {
                    at: SystemTime::now(),
                    grant: &grant,
                    turn: &turn,
                    inventory: &[],
                    next_asset_id: 1,
                    prepared: Some(&generation::Prepared {
                        include_chat: true,
                        representations: vec![meta.clone(), replacement.clone()],
                        exchanges: turn.exchanges().to_vec(),
                        original: true,
                        availability: vec!["original".into(), "summary".into()],
                        failed_routines: Vec::new(),
                    }),
                },
                route.memory.window().unwrap(),
            )
            .unwrap();
    }
    journal
        .append(
            &stem,
            &Entry {
                at: SystemTime::now(),
                grant: &grant,
                turn: &turn,
                inventory: &[],
                next_asset_id: 1,
                prepared: Some(&generation::Prepared {
                    include_chat: true,
                    representations: if decider_choice == Some("missing") {
                        vec![meta]
                    } else {
                        vec![meta, replacement]
                    },
                    exchanges: if summary_only {
                        vec![Exchange {
                            tool: "check-log".into(),
                            request: Vec::new(),
                            result: Vec::new(),
                            outcome: "succeeded".into(),
                            exit_status: Some(0),
                        }]
                    } else {
                        turn.exchanges().to_vec()
                    },
                    original: !summary_only,
                    availability: if summary_only {
                        vec!["summary".into()]
                    } else if decider_choice == Some("missing") {
                        vec!["original".into()]
                    } else {
                        vec!["original".into(), "summary".into()]
                    },
                    failed_routines: Vec::new(),
                }),
            },
            route.memory.window().unwrap(),
        )
        .unwrap();
    if summary_only {
        let seed =
            runner
                .conversations
                .begin(&key, &granted(&granted_ids), window, None, Instant::now());
        assert!(seed.lease.commit(
            window,
            turn.clone(),
            TakenIn::default(),
            &seed.cache_key,
            Instant::now()
        ));
        let disk =
            std::fs::read_to_string(journal_dir.path().join(format!("{stem}.jsonl"))).unwrap();
        assert!(
            !disk.contains(CANARY),
            "disposed source must not survive on disk"
        );
    }
    if pin {
        let tools = JournalMemoryTools {
            journal,
            stem: stem.clone(),
            grant: grant.clone(),
            versions: [
                ("original".to_owned(), original_version),
                ("summary".to_owned(), summary_version),
            ]
            .into(),
        };
        assert!(tools.set_pin(1, "summary").starts_with("pinned record 1"));
    }
    let before = std::fs::read(journal_dir.path().join(format!("{stem}.jsonl"))).unwrap();
    run_session(
        Arc::clone(&runner),
        route,
        inbound,
        Arc::new(RecordingDriver::default()) as Arc<dyn ChatDriver>,
    )
    .await;
    let after = std::fs::read(journal_dir.path().join(format!("{stem}.jsonl"))).unwrap();
    assert!(
        after.starts_with(&before),
        "projection and append must not rewrite prior records"
    );
    let disk = std::fs::read_to_string(journal_dir.path().join(format!("{stem}.jsonl"))).unwrap();
    let stored: serde_json::Value = serde_json::from_str(disk.lines().next().unwrap()).unwrap();
    assert_eq!(stored["recordId"], 1);
    assert!(
        stored.get("selections").is_none(),
        "selection is telemetry-only"
    );
    if summary_only {
        assert!(
            !disk.contains(CANARY),
            "disposed original must stay disposed"
        );
    }
    let original = format!("status\nok{HEADER}{LABEL}{LOG}");
    if summary_only {
        assert!(
            !model
                .prompt(0)
                .iter()
                .any(|(_, text)| text.contains(CANARY)),
            "disposed source cannot reach the model"
        );
    }
    let replacement = format!("{summary}{HEADER}");
    if actual == "omitted" {
        assert!(!model.prompt(0).iter().any(|(_, text)| text == expected));
    } else {
        let eligible = if summary_only {
            vec!["summary"]
        } else if decider_choice == Some("missing") {
            vec!["original"]
        } else {
            vec!["original", "summary"]
        };
        let label = record_label(
            if decider_choice == Some("missing") {
                2
            } else {
                1
            },
            actual,
            &eligible,
            0,
        );
        assert_eq!(
            model.prompt(0)[if decider_choice == Some("missing") {
                2
            } else {
                1
            }],
            ("user".to_owned(), format!("{label}{expected}"))
        );
    }
    let events = capture.events_where("dekopon_gatewayd", &[("event", "gateway_memory_selection")]);
    assert_eq!(
        events.len(),
        if decider_choice == Some("missing") {
            2
        } else {
            1
        },
        "{events:?}"
    );
    let event = events
        .iter()
        .find(|event| {
            event.field("record_id")
                == Some(if decider_choice == Some("missing") {
                    "2"
                } else {
                    "1"
                })
        })
        .unwrap();
    assert_eq!(event.field("requested"), Some(requested));
    assert_eq!(event.field("actual"), Some(actual));
    let record_id = if decider_choice == Some("missing") {
        2
    } else {
        1
    };
    let eligible = if summary_only {
        vec!["summary"]
    } else if decider_choice == Some("missing") {
        vec!["original"]
    } else {
        vec!["original", "summary"]
    };
    let original_bytes = if summary_only {
        "status".len() + "ok".len() + LOG.len()
    } else {
        original.len() + record_label(record_id, "original", &eligible, 0).len()
    };
    let replacement_bytes = if requested == "original" || decider_choice == Some("missing") {
        original.len() + record_label(record_id, "original", &eligible, 0).len()
    } else {
        replacement.len() + record_label(record_id, "summary", &eligible, 0).len()
    };
    assert_eq!(
        event.field("original_bytes"),
        Some(original_bytes.to_string().as_str())
    );
    assert_eq!(
        event.field("replacement_bytes"),
        Some(replacement_bytes.to_string().as_str())
    );
    assert_eq!(
        event.field("savings_bytes"),
        Some(savings.to_string().as_str())
    );
    assert_eq!(event.field("reason"), Some(reason));
    let fallback = match decider_choice {
        Some("invalid") => "invalid-choice",
        Some("refused") => "refused",
        Some("budget" | "failed") => "invocation-failed",
        Some("ungranted") => "not-granted",
        _ => "none",
    };
    assert_eq!(event.field("fallback_reason"), Some(fallback));
    assert_eq!(
        event.field("pin"),
        Some(if pin { "summary" } else { "none" })
    );
    assert_eq!(
        event.field("pin_bytes"),
        Some(
            if pin {
                replacement.len() + record_label(record_id, "summary", &eligible, 0).len()
            } else {
                0
            }
            .to_string()
            .as_str()
        )
    );
    assert_eq!(
        event.field("pinned"),
        Some(if pin { "true" } else { "false" })
    );
    assert_eq!(broker.recorded().len(), if invoked { 2 } else { 1 });
}

fn record_label(
    record_id: u64,
    representation_id: &str,
    eligible: &[&str],
    omitted_bytes: u64,
) -> String {
    format!(
        "[memory: {}]\n",
        json!({"recordId":record_id,"representationId":representation_id,"eligibleRepresentationIds":eligible,"omittedBytes":omitted_bytes})
    )
}

fn original() -> String {
    format!("status\nok{HEADER}{LABEL}{LOG}")
}
fn replacement(summary: &str) -> String {
    format!("{summary}{HEADER}")
}
fn envelope_savings(summary: &str) -> i64 {
    (original().len() + record_label(1, "original", &["original", "summary"], 0).len()) as i64
        - (replacement(summary).len()
            + record_label(1, "summary", &["original", "summary"], 0).len()) as i64
}
fn equal_summary() -> String {
    "x".repeat(
        original().len() - replacement("").len()
            + record_label(1, "original", &["original", "summary"], 0).len()
            - record_label(1, "summary", &["original", "summary"], 0).len(),
    )
}

#[tokio::test]
async fn smaller_check_log_summary_saves_envelope_bytes() {
    let summary = "passed";
    selected_summary(
        summary,
        false,
        &replacement(summary),
        "summary",
        "summary",
        envelope_savings(summary),
        "selected",
        None,
    )
    .await;
}

#[tokio::test]
async fn equal_check_log_summary_keeps_original_without_savings() {
    let summary = equal_summary();
    selected_summary(
        &summary,
        false,
        &original(),
        "summary",
        "original",
        0,
        "kept-original: no-saving",
        None,
    )
    .await;
}

#[tokio::test]
async fn larger_check_log_summary_keeps_original_and_reports_negative_savings() {
    let summary = format!("{}plus", equal_summary());
    selected_summary(
        &summary,
        false,
        &original(),
        "summary",
        "original",
        -4,
        "kept-original: no-saving",
        None,
    )
    .await;
}

#[tokio::test]
async fn shorter_bare_check_log_summary_with_larger_envelope_keeps_original() {
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let journal_dir = temporary();
    let mut broker = BrokerFixture::answering(
        directory.path(),
        [ResponseEnvelope::chat_capabilities(
            vec![capability("cli-probe.upper")],
            Vec::new(),
            BTreeMap::new(),
            None,
            None,
        )],
    );
    let client = ResolvedBroker {
        socket_path: broker.socket().to_owned(),
        server_uid: crate::current_uid(),
        frame: FrameLimits::default(),
    };
    let model = ModelScript::new([answer("follow-up")]);
    let runner = journaled_runner(client, Arc::new(Arc::clone(&model)), journal_dir.path());
    let window = recall_window(RecallSource::Journal);
    let mut route = persistent_route(model_config(), window);
    route.memory_policy = Some(policy(SelectionMode::Fixed));
    let config = route.memory_policy.as_ref().unwrap();
    let inbound = message("next question");
    let key = ConversationKey::private(
        &route.agent,
        &route.transport,
        &inbound.conversation.key(),
        &inbound.subject,
    );
    let original_text = "hi\nok";
    let summary = "sum";
    assert!(summary.len() < original_text.len());
    let original_version = generation::version(&config.routines[0]);
    let summary_version = generation::version(&config.routines[1]);
    let mut metadata = Representation::original(original_text, &original_version);
    metadata.text = None;
    let replacement = Representation {
        routine_id: "summary".into(),
        version: summary_version,
        source_record_id: 0,
        part: None,
        kind: "prefix".into(),
        captured_bytes: 1_000_003,
        text: Some(summary.into()),
    };
    let turn = ConversationTurn::completed("hi", "ok");
    runner
        .journal
        .as_ref()
        .unwrap()
        .append(
            &key.journal_stem(),
            &Entry {
                at: SystemTime::now(),
                grant: &crate::journal::grant_digest(&granted(&["cli-probe.upper"])),
                turn: &turn,
                inventory: &[],
                next_asset_id: 1,
                prepared: Some(&generation::Prepared {
                    include_chat: true,
                    representations: vec![metadata, replacement],
                    exchanges: Vec::new(),
                    original: true,
                    availability: vec!["original".into(), "summary".into()],
                    failed_routines: Vec::new(),
                }),
            },
            window,
        )
        .unwrap();
    run_session(
        runner,
        route,
        inbound,
        Arc::new(RecordingDriver::default()) as Arc<dyn ChatDriver>,
    )
    .await;
    let original_label = record_label(1, "original", &["original", "summary"], 0);
    let replacement_label = record_label(1, "summary", &["original", "summary"], 1_000_000);
    let original_bytes = original_label.len() + "hi".len() + "ok".len();
    let replacement_bytes = replacement_label.len() + summary.len();
    assert!(
        replacement_bytes > original_bytes,
        "the longer model-facing label reverses bare-text savings"
    );
    assert_eq!(
        model.prompt(0)[1..],
        [
            ("user".into(), format!("{original_label}hi")),
            ("assistant".into(), "ok".into()),
            ("user".into(), "next question".into()),
        ]
    );
    let events = capture.events_where("dekopon_gatewayd", &[("event", "gateway_memory_selection")]);
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.field("requested"), Some("summary"));
    assert_eq!(event.field("actual"), Some("original"));
    assert_eq!(event.field("reason"), Some("kept-original: no-saving"));
    assert_eq!(
        event.field("original_bytes"),
        Some(original_bytes.to_string().as_str())
    );
    assert_eq!(
        event.field("replacement_bytes"),
        Some(replacement_bytes.to_string().as_str())
    );
    assert_eq!(
        event.field("savings_bytes"),
        Some(
            ((original_bytes as i64) - (replacement_bytes as i64))
                .to_string()
                .as_str()
        )
    );
    assert_eq!(broker.recorded().len(), 1);
}

#[tokio::test]
async fn pinned_expanding_check_log_summary_is_sent_with_its_negative_savings() {
    let summary = format!("{}plus", equal_summary());
    selected_summary(
        &summary,
        true,
        &replacement(&summary),
        "summary",
        "summary",
        -4,
        "selected",
        None,
    )
    .await;
}

#[tokio::test]
async fn a_selected_omission_never_replays_its_resident_original() {
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let journal_dir = temporary();
    let mut broker = BrokerFixture::answering(
        directory.path(),
        [ResponseEnvelope::chat_capabilities(
            vec![capability("cli-probe.upper")],
            Vec::new(),
            BTreeMap::new(),
            None,
            None,
        )],
    );
    let client = ResolvedBroker {
        socket_path: broker.socket().to_owned(),
        server_uid: crate::current_uid(),
        frame: FrameLimits::default(),
    };
    let model = ModelScript::new([answer("follow-up")]);
    let runner = journaled_runner(client, Arc::new(Arc::clone(&model)), journal_dir.path());
    let mut window = recall_window(RecallSource::Journal);
    window.limits.max_bytes = 30;
    let mut route = persistent_route(model_config(), window);
    route.memory_policy = Some(ResolvedPolicy::default());
    let inbound = message("next question");
    let key = ConversationKey::private(
        &route.agent,
        &route.transport,
        &inbound.conversation.key(),
        &inbound.subject,
    );
    let grant = crate::journal::grant_digest(&granted(&["cli-probe.upper"]));
    let turn = ConversationTurn::completed("X".repeat(40), "ok")
        .with_memory_id(crate::journal::mint_turn_id());
    runner
        .journal
        .as_ref()
        .unwrap()
        .append(
            &key.journal_stem(),
            &Entry {
                at: SystemTime::now(),
                grant: &grant,
                turn: &turn,
                inventory: &[],
                next_asset_id: 1,
                prepared: None,
            },
            window,
        )
        .unwrap();
    let seed = runner.conversations.begin(
        &key,
        &granted(&["cli-probe.upper"]),
        window,
        None,
        Instant::now(),
    );
    assert!(seed.lease.commit(
        window,
        turn,
        TakenIn::default(),
        &seed.cache_key,
        Instant::now()
    ));
    run_session(
        Arc::clone(&runner),
        route,
        inbound,
        Arc::new(RecordingDriver::default()) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(
        model.prompt(0)[1..],
        [("user".into(), "next question".into())]
    );
    let omitted = capture.events_where(
        "dekopon_gatewayd",
        &[
            ("event", "gateway_memory_selection"),
            ("record_id", "1"),
            ("actual", "omitted"),
            ("reason", "record-exceeds-budget"),
        ],
    );
    assert_eq!(
        omitted.len(),
        1,
        "selection cannot claim omission while replaying resident text"
    );
    assert_eq!(broker.recorded().len(), 1);
}

#[tokio::test]
async fn a_lagging_journal_keeps_its_newer_committed_resident_turn_once() {
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let journal_dir = temporary();
    let mut broker = BrokerFixture::answering(
        directory.path(),
        [ResponseEnvelope::chat_capabilities(
            vec![capability("cli-probe.upper")],
            Vec::new(),
            BTreeMap::new(),
            None,
            None,
        )],
    );
    let client = ResolvedBroker {
        socket_path: broker.socket().to_owned(),
        server_uid: crate::current_uid(),
        frame: FrameLimits::default(),
    };
    let model = ModelScript::new([answer("follow-up")]);
    let runner = journaled_runner(client, Arc::new(Arc::clone(&model)), journal_dir.path());
    let window = recall_window(RecallSource::Journal);
    let mut route = persistent_route(model_config(), window);
    route.memory_policy = Some(ResolvedPolicy::default());
    let inbound = message("next question");
    let key = ConversationKey::private(
        &route.agent,
        &route.transport,
        &inbound.conversation.key(),
        &inbound.subject,
    );
    let grant_ids = granted(&["cli-probe.upper"]);
    let grant = crate::journal::grant_digest(&grant_ids);
    let older =
        ConversationTurn::completed("older", "ok").with_memory_id(crate::journal::mint_turn_id());
    runner
        .journal
        .as_ref()
        .unwrap()
        .append(
            &key.journal_stem(),
            &Entry {
                at: SystemTime::now(),
                grant: &grant,
                turn: &older,
                inventory: &[],
                next_asset_id: 1,
                prepared: None,
            },
            window,
        )
        .unwrap();
    let first = runner
        .conversations
        .begin(&key, &grant_ids, window, None, Instant::now());
    assert!(first.lease.commit(
        window,
        older,
        TakenIn::default(),
        &first.cache_key,
        Instant::now()
    ));
    let newer =
        ConversationTurn::completed("newer", "ok").with_memory_id(crate::journal::mint_turn_id());
    let next = runner
        .conversations
        .begin(&key, &grant_ids, window, None, Instant::now());
    assert!(next.lease.commit(
        window,
        newer,
        TakenIn::default(),
        &next.cache_key,
        Instant::now()
    ));
    run_session(
        Arc::clone(&runner),
        route,
        inbound,
        Arc::new(RecordingDriver::default()) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(
        model.prompt(0)[1..],
        [
            ("user".into(), "[memory: {\"recordId\":1,\"representationId\":\"original\",\"eligibleRepresentationIds\":[\"original\"],\"omittedBytes\":0}]\nolder".into()),
            ("assistant".into(), "ok".into()),
            ("user".into(), "newer".into()),
            ("assistant".into(), "ok".into()),
            ("user".into(), "next question".into())
        ]
    );
    assert_eq!(
        capture
            .events_where(
                "dekopon_gatewayd",
                &[
                    ("event", "gateway_memory_selection"),
                    ("record_id", "1"),
                    ("actual", "original"),
                    ("reason", "selected")
                ]
            )
            .len(),
        1
    );
    let prompt = &model.prompt(0)[1].1;
    let (label, body) = prompt
        .split_once("]\n")
        .expect("addressable projected envelope");
    let coordinate: Value =
        serde_json::from_str(label.trim_start_matches("[memory: ").trim_end_matches(']')).unwrap();
    assert_eq!(coordinate["recordId"], 1);
    assert_eq!(coordinate["eligibleRepresentationIds"], json!(["original"]));
    let tools = JournalMemoryTools {
        journal: runner.journal.as_ref().unwrap(),
        stem: key.journal_stem(),
        grant,
        versions: [(
            "original".to_owned(),
            generation::version(&ResolvedPolicy::default().routines[0]),
        )]
        .into(),
    };
    let id = coordinate["recordId"].as_u64().unwrap();
    let representation = coordinate["representationId"].as_str().unwrap();
    assert_eq!(tools.read(id, representation, None), format!("{body}\nok"));
    assert!(
        tools
            .set_pin(id, representation)
            .starts_with("pinned record 1")
    );
    assert_eq!(broker.recorded().len(), 1);
}

#[tokio::test]
async fn a_missing_journal_line_does_not_erase_a_committed_resident_window() {
    let directory = temporary();
    let journal_dir = temporary();
    let broker = BrokerFixture::answering(
        directory.path(),
        [ResponseEnvelope::chat_capabilities(
            vec![capability("cli-probe.upper")],
            Vec::new(),
            BTreeMap::new(),
            None,
            None,
        )],
    );
    let client = ResolvedBroker {
        socket_path: broker.socket().to_owned(),
        server_uid: crate::current_uid(),
        frame: FrameLimits::default(),
    };
    let model = ModelScript::new([answer("follow-up")]);
    let runner = journaled_runner(client, Arc::new(Arc::clone(&model)), journal_dir.path());
    let window = recall_window(RecallSource::Journal);
    let route = persistent_route(model_config(), window);
    let inbound = message("next question");
    let key = ConversationKey::private(
        &route.agent,
        &route.transport,
        &inbound.conversation.key(),
        &inbound.subject,
    );
    let granted = granted(&["cli-probe.upper"]);
    let seed = runner
        .conversations
        .begin(&key, &granted, window, None, Instant::now());
    assert!(seed.lease.commit(
        window,
        ConversationTurn::completed("unpersisted", "ok"),
        TakenIn::default(),
        &seed.cache_key,
        Instant::now()
    ));
    run_session(
        Arc::clone(&runner),
        route,
        inbound,
        Arc::new(RecordingDriver::default()) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(
        model.prompt(0)[1..3],
        [
            ("user".into(), "unpersisted".into()),
            ("assistant".into(), "ok".into())
        ]
    );
}

#[tokio::test]
async fn journal_projection_fits_newest_whole_turns_without_downgrading() {
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let journal_dir = temporary();
    let mut broker = BrokerFixture::answering(
        directory.path(),
        [ResponseEnvelope::chat_capabilities(
            vec![capability("cli-probe.upper")],
            Vec::new(),
            BTreeMap::new(),
            None,
            None,
        )],
    );
    let client = ResolvedBroker {
        socket_path: broker.socket().to_owned(),
        server_uid: crate::current_uid(),
        frame: FrameLimits::default(),
    };
    let model = ModelScript::new([answer("follow-up")]);
    let runner = journaled_runner(client, Arc::new(Arc::clone(&model)), journal_dir.path());
    let mut window = recall_window(RecallSource::Journal);
    window.limits.max_bytes = 252;
    let mut route = persistent_route(model_config(), window);
    route.memory_policy = Some(ResolvedPolicy::default());
    let inbound = message("next question");
    let key = ConversationKey::private(
        &route.agent,
        &route.transport,
        &inbound.conversation.key(),
        &inbound.subject,
    );
    let stem = key.journal_stem();
    let grant = crate::journal::grant_digest(&granted(&["cli-probe.upper"]));
    for (user, answer) in [("oldest", "a"), ("middle", "b"), ("latest", "c")] {
        let turn = ConversationTurn::completed(user, answer);
        runner
            .journal
            .as_ref()
            .unwrap()
            .append(
                &stem,
                &Entry {
                    at: SystemTime::now(),
                    grant: &grant,
                    turn: &turn,
                    inventory: &[],
                    next_asset_id: 1,
                    prepared: None,
                },
                window,
            )
            .unwrap();
    }
    run_session(
        runner,
        route,
        inbound,
        Arc::new(RecordingDriver::default()) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(
        model.prompt(0)[1..],
        [
            ("user".into(), "[memory: {\"recordId\":2,\"representationId\":\"original\",\"eligibleRepresentationIds\":[\"original\"],\"omittedBytes\":0}]\nmiddle".into()),
            ("assistant".into(), "b".into()),
            ("user".into(), "[memory: {\"recordId\":3,\"representationId\":\"original\",\"eligibleRepresentationIds\":[\"original\"],\"omittedBytes\":0}]\nlatest".into()),
            ("assistant".into(), "c".into()),
            ("user".into(), "next question".into())
        ]
    );
    assert_eq!(broker.recorded().len(), 1);
    let dropped = capture.events_where(
        "dekopon_gatewayd",
        &[
            ("event", "gateway_memory_selection"),
            ("record_id", "1"),
            ("actual", "omitted"),
            ("reason", "window-fit"),
        ],
    );
    assert_eq!(
        dropped.len(),
        1,
        "the oldest turn was selected but never sent"
    );
}

#[tokio::test]
async fn disposed_original_with_expanding_summary_sends_replacement_and_signed_cost() {
    let summary = format!("{}plus", equal_summary());
    selected_summary_with_limit(
        &summary,
        false,
        &replacement(&summary),
        "summary",
        "summary",
        ("status".len() + "ok".len() + LOG.len()) as i64
            - (replacement(&summary).len() + record_label(1, "summary", &["summary"], 0).len())
                as i64,
        "kept-replacement: original-unavailable",
        None,
        None,
        true,
    )
    .await;
}

#[tokio::test]
async fn an_oversized_pinned_check_log_turn_is_omitted_not_downgraded() {
    let summary = format!("{}plus", equal_summary());
    selected_summary_with_limit(
        &summary,
        true,
        &replacement(&summary),
        "summary",
        "omitted",
        -4,
        "record-exceeds-budget",
        None,
        Some(32),
        false,
    )
    .await;
}

#[tokio::test]
async fn a_decider_choice_cannot_silently_override_an_expanding_pin() {
    let summary = format!("{}plus", equal_summary());
    selected_summary(
        &summary,
        true,
        &original(),
        "original",
        "original",
        0,
        "decider-overrode-pin",
        Some("original"),
    )
    .await;
}

#[tokio::test]
async fn a_decider_choice_missing_on_a_record_uses_the_available_fallback() {
    selected_summary(
        "passed",
        false,
        &original(),
        "summary",
        "original",
        0,
        "fallback: choice-unavailable",
        Some("missing"),
    )
    .await;
}

#[tokio::test]
async fn an_agent_preference_selects_its_retained_routine() {
    let summary = "passed";
    selected_summary(
        summary,
        false,
        &replacement(summary),
        "summary",
        "summary",
        envelope_savings(summary),
        "selected",
        Some("preference"),
    )
    .await;
}

#[tokio::test]
async fn a_decider_choice_may_expand_a_record_and_reports_the_cost() {
    let summary = format!("{}plus", equal_summary());
    selected_summary(
        &summary,
        false,
        &replacement(&summary),
        "summary",
        "summary",
        -4,
        "selected",
        Some("summary"),
    )
    .await;
}

#[tokio::test]
async fn an_invalid_decider_choice_falls_back_once() {
    selected_summary(
        "passed",
        false,
        &replacement("passed"),
        "summary",
        "summary",
        envelope_savings("passed"),
        "invalid-choice",
        Some("invalid"),
    )
    .await;
}

#[tokio::test]
async fn a_decider_refusal_falls_back_without_another_invocation() {
    selected_summary(
        "passed",
        false,
        &replacement("passed"),
        "summary",
        "summary",
        envelope_savings("passed"),
        "refused",
        Some("refused"),
    )
    .await;
}

#[tokio::test]
async fn a_failed_decider_capability_falls_back_without_a_second_inference() {
    selected_summary(
        "passed",
        false,
        &replacement("passed"),
        "summary",
        "summary",
        envelope_savings("passed"),
        "invocation-failed",
        Some("failed"),
    )
    .await;
}

#[tokio::test]
async fn a_spent_capability_budget_falls_back_without_invoking_a_broker() {
    selected_summary(
        "passed",
        false,
        &replacement("passed"),
        "summary",
        "summary",
        envelope_savings("passed"),
        "invocation-failed",
        Some("budget"),
    )
    .await;
}

#[tokio::test]
async fn an_ungranted_decider_falls_back_without_invoking_a_broker() {
    selected_summary(
        "passed",
        false,
        &replacement("passed"),
        "summary",
        "summary",
        envelope_savings("passed"),
        "not-granted",
        Some("ungranted"),
    )
    .await;
}
