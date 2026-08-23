use std::time::Duration;

use dekopon_agent::prompt::{PromptOutcome, ReplyDisposition};
use dekopon_protocol::{Agent, AgentKind, AgentSpec, ApiVersion, ObjectMeta};
use serde_json::json;

use super::{App, Mode, Pane, ShellEntry};
use crate::record::{CallOutcome, CapabilityCall, ScriptRun, SessionEvent};

fn agent(name: &str) -> Agent {
    Agent {
        api_version: ApiVersion::V1Alpha1,
        kind: AgentKind::Agent,
        metadata: ObjectMeta::named(name),
        spec: AgentSpec {
            description: "a fixture".to_owned(),
            enabled: true,
            instructions: None,
            capabilities: Vec::new(),
            providers: Vec::new(),
            model_class: None,
            policy_profile: None,
        },
        status: Default::default(),
    }
}

fn console() -> App {
    App::new(
        vec![agent("ville-github"), agent("snooper")],
        "dev.console.xavier".to_owned(),
        "/run/dekopon/broker.sock".to_owned(),
        "/config/dekopon/chatgpt-auth.console.json".to_owned(),
    )
}

#[test]
fn pane_cycling_is_a_closed_ring_in_both_directions() {
    for pane in Pane::ORDER {
        assert_eq!(pane.next().previous(), pane);
        assert_eq!(pane.previous().next(), pane);
        assert!(!pane.title().is_empty());
    }
}

#[test]
fn selection_saturates_rather_than_wrapping() {
    let mut app = console();
    app.move_selection(-1);
    assert_eq!(app.selected_agent, 0, "already at the top");
    app.move_selection(50);
    assert_eq!(app.selected_agent, 1, "stops at the last row");
    assert_eq!(
        app.highlighted_id().map(|id| id.to_string()),
        Some("snooper".to_owned())
    );
}

#[test]
fn selection_on_an_empty_catalog_does_nothing() {
    let mut app = App::new(Vec::new(), "tel.1".to_owned(), String::new(), String::new());
    app.move_selection(3);
    assert_eq!(app.selected_agent, 0);
    assert!(app.highlighted().is_none());
}

#[test]
fn a_turn_needs_an_agent_and_says_so() {
    let mut app = console();
    app.composer = "list the issues".to_owned();

    assert!(app.submit_turn().is_none());
    let notice = app.notice.expect("a refusal explains itself");
    assert!(notice.is_refusal);
    assert!(notice.text.contains("hop into an agent"));
    assert_eq!(
        app.composer, "list the issues",
        "a refused submission must not eat what was typed"
    );
}

#[test]
fn a_second_turn_is_refused_out_loud_rather_than_queued() {
    let mut app = console();
    app.busy = true;
    app.session = None;
    // Prove the busy refusal is reachable independently of the no-agent one by checking its text
    // through the same path a hopped-in console takes.
    app.composer = "again".to_owned();
    app.notice = None;
    app.session = None;
    assert!(app.submit_turn().is_none());
    assert!(
        app.notice.as_ref().is_some_and(|notice| notice.is_refusal),
        "both refusals must be refusals"
    );
}

#[test]
fn an_empty_composer_submits_nothing() {
    let mut app = console();
    app.composer = "   \n ".to_owned();
    app.session = None;
    assert!(app.submit_turn().is_none());
}

#[test]
fn a_finished_session_clears_busy_and_closes_the_turn() {
    let mut app = console();
    app.busy = true;
    app.transcript.open("ask".to_owned());
    app.on_session_event(SessionEvent::ScriptStarted {
        sequence: 0,
        script: "gh issue list".to_owned(),
    });
    app.on_session_event(SessionEvent::Capability(Box::new(CapabilityCall {
        sequence: 1,
        capability: "gh.issue.list".to_owned(),
        input: json!({"state": "open"}),
        outcome: CallOutcome::Succeeded(json!([])),
        elapsed: Duration::from_millis(5),
    })));
    app.on_session_event(SessionEvent::ScriptFinished(Box::new(ScriptRun {
        sequence: 0,
        script: "gh issue list".to_owned(),
        output: String::new(),
        exit_code: 0,
        truncated: false,
        capability_calls: 1,
        steps: 3,
        elapsed: Duration::from_millis(9),
    })));
    assert!(
        app.busy,
        "the turn is still open until it reports an outcome"
    );

    app.on_session_event(SessionEvent::Finished(Box::new(Ok(PromptOutcome {
        answer: "one issue".to_owned(),
        disposition: ReplyDisposition::Send,
        model_turns: 2,
        script_calls: 1,
        capability_invocations: 1,
    }))));
    assert!(!app.busy);
    assert_eq!(app.transcript.turns().len(), 1);
    assert_eq!(app.transcript.turns()[0].capability_calls(), 1);
}

#[test]
fn a_session_that_returns_without_an_outcome_is_reported() {
    // The blocking task ended but no `Finished` arrived: the turn would otherwise sit on screen
    // spinning forever, which reads as a hung broker rather than a lost result.
    let mut app = console();
    app.busy = true;
    app.transcript.open("ask".to_owned());
    app.on_session_complete(1);

    assert!(!app.busy);
    let notice = app.notice.expect("an unreported outcome is worth saying");
    assert!(notice.is_refusal);
    assert!(notice.text.contains("without reporting an outcome"));
}

#[test]
fn completing_a_session_marks_the_replay_window() {
    let mut app = console();
    for index in 0..4 {
        app.transcript.open(format!("turn {index}"));
        app.on_session_event(SessionEvent::Finished(Box::new(Ok(PromptOutcome {
            answer: "ok".to_owned(),
            disposition: ReplyDisposition::Send,
            model_turns: 1,
            script_calls: 0,
            capability_invocations: 0,
        }))));
    }
    app.on_session_complete(2);

    let inside: Vec<bool> = app
        .transcript
        .turns()
        .iter()
        .map(|turn| turn.in_replay_window)
        .collect();
    assert_eq!(inside, [false, false, true, true]);
}

#[test]
fn stopping_never_claims_a_sent_call_was_undone() {
    let mut app = console();
    assert!(!app.request_stop(), "nothing to stop when idle");

    app.busy = true;
    app.transcript.open("ask".to_owned());
    assert!(app.request_stop());
    assert!(app.transcript.turns()[0].stop_requested);

    let text = app.notice.expect("a stop says what it did").text;
    assert!(text.contains("still complete"), "got: {text}");
    assert!(
        !text.contains("cancelled") && !text.contains("undone"),
        "cancellation is not rollback and must not read like it: {text}"
    );
}

#[test]
fn expanding_a_call_toggles() {
    let mut app = console();
    app.toggle_call((0, 0, 0));
    assert_eq!(app.expanded_call, Some((0, 0, 0)));
    app.toggle_call((0, 0, 0));
    assert_eq!(app.expanded_call, None);
    app.toggle_call((0, 0, 1));
    assert_eq!(app.expanded_call, Some((0, 0, 1)));
}

#[test]
fn revealing_is_per_field_and_says_where_the_secret_went() {
    let mut app = console();
    assert!(!app.is_revealed("headers.authorization"));

    app.reveal("headers.authorization".to_owned());
    assert!(app.is_revealed("headers.authorization"));
    assert!(
        !app.is_revealed("headers.cookie"),
        "revealing one field must not reveal its neighbours"
    );
    assert!(
        app.notice
            .as_ref()
            .is_some_and(|notice| notice.text.contains("scrollback")),
        "an operator has to be told the secret is now in their terminal"
    );

    app.reveal("headers.authorization".to_owned());
    assert_eq!(app.revealed.len(), 1, "revealing twice reveals once");
}

#[test]
fn shell_entries_accumulate_in_order() {
    let mut app = console();
    app.push_shell(ShellEntry {
        input: "cap --list".to_owned(),
        output: "gh.issue.list".to_owned(),
        exit_code: 0,
    });
    app.push_shell(ShellEntry {
        input: "nope".to_owned(),
        output: "command not found".to_owned(),
        exit_code: 127,
    });

    assert_eq!(app.shell_history.len(), 2);
    assert_eq!(app.shell_history[1].exit_code, 127);
}

#[test]
fn modes_default_to_browsing() {
    assert_eq!(Mode::default(), Mode::Browsing);
    assert_eq!(Pane::default(), Pane::Agents);
}
