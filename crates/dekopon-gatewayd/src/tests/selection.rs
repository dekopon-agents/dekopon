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
const WRAPPER: &str = "[memory: routine=summary, omitted=0 bytes]\n";

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
                else { json!({"choice": if choice == "invalid" { "unknown" } else { choice }, "backend":"fixture", "model":"fixture"}) }
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
        selection.selection.fallback = Some("summary".into());
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
                    representations: vec![meta, replacement],
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
    run_session(
        Arc::clone(&runner),
        route,
        inbound,
        Arc::new(RecordingDriver::default()) as Arc<dyn ChatDriver>,
    )
    .await;
    let disk = std::fs::read_to_string(journal_dir.path().join(format!("{stem}.jsonl"))).unwrap();
    let stored: serde_json::Value = serde_json::from_str(disk.lines().next().unwrap()).unwrap();
    assert_eq!(stored["selections"][0]["reason"], reason);
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
    let replacement = format!("{WRAPPER}{summary}{HEADER}");
    if actual == "omitted" {
        assert!(!model.prompt(0).iter().any(|(_, text)| text == expected));
    } else {
        assert_eq!(model.prompt(0)[1], ("user".to_owned(), expected.to_owned()));
    }
    let events = capture.events_where("dekopon_gatewayd", &[("event", "gateway_memory_selection")]);
    assert_eq!(events.len(), 1, "{events:?}");
    let event = &events[0];
    assert_eq!(event.field("requested"), Some(requested));
    assert_eq!(event.field("actual"), Some(actual));
    assert_eq!(
        event.field("original_bytes"),
        Some(original.len().to_string().as_str())
    );
    assert_eq!(
        event.field("replacement_bytes"),
        Some(
            if requested == "original" {
                original.len()
            } else {
                replacement.len()
            }
            .to_string()
            .as_str()
        )
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
        Some(if pin { replacement.len() } else { 0 }.to_string().as_str())
    );
    assert_eq!(
        event.field("pinned"),
        Some(if pin { "true" } else { "false" })
    );
    assert_eq!(broker.recorded().len(), if invoked { 2 } else { 1 });
}

fn original() -> String {
    format!("status\nok{HEADER}{LABEL}{LOG}")
}
fn replacement(summary: &str) -> String {
    format!("{WRAPPER}{summary}{HEADER}")
}
fn equal_summary() -> String {
    "x".repeat(original().len() - replacement("").len())
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
        original().len() as i64 - replacement(summary).len() as i64,
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
    let summary = "x".repeat(LOG.len() - 1);
    assert!(summary.len() < LOG.len());
    let savings = original().len() as i64 - replacement(&summary).len() as i64;
    assert!(savings < 0);
    selected_summary(
        &summary,
        false,
        &original(),
        "summary",
        "original",
        savings,
        "kept-original: no-saving",
        None,
    )
    .await;
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
    window.limits.max_bytes = 14;
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
            ("user".into(), "middle".into()),
            ("assistant".into(), "b".into()),
            ("user".into(), "latest".into()),
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
        -4,
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
async fn an_agent_preference_selects_its_retained_routine() {
    let summary = "passed";
    selected_summary(
        summary,
        false,
        &replacement(summary),
        "summary",
        "summary",
        original().len() as i64 - replacement(summary).len() as i64,
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
        original().len() as i64 - replacement("passed").len() as i64,
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
        original().len() as i64 - replacement("passed").len() as i64,
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
        original().len() as i64 - replacement("passed").len() as i64,
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
        original().len() as i64 - replacement("passed").len() as i64,
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
        original().len() as i64 - replacement("passed").len() as i64,
        "not-granted",
        Some("ungranted"),
    )
    .await;
}
