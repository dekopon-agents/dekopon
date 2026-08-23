use std::time::Duration;

use dekopon_agent::prompt::{PromptOutcome, ReplyDisposition};
use dekopon_model::model::ModelUsage;
use serde_json::json;

use super::{Transcript, TurnStatus};
use crate::record::{CallOutcome, CapabilityCall, ScriptRun, SessionEvent};

fn call(sequence: u64, capability: &str, outcome: CallOutcome) -> SessionEvent {
    SessionEvent::Capability(Box::new(CapabilityCall {
        sequence,
        capability: capability.to_owned(),
        input: json!({"owner": "dekopon-agents"}),
        outcome,
        elapsed: Duration::from_millis(12),
    }))
}

fn finished(sequence: u64, calls: u32) -> SessionEvent {
    SessionEvent::ScriptFinished(Box::new(ScriptRun {
        sequence,
        script: String::new(),
        output: "done".to_owned(),
        exit_code: 0,
        truncated: false,
        capability_calls: calls,
        steps: 4,
        elapsed: Duration::from_millis(30),
    }))
}

fn answered(answer: &str) -> SessionEvent {
    SessionEvent::Finished(Box::new(Ok(PromptOutcome {
        answer: answer.to_owned(),
        disposition: ReplyDisposition::Send,
        model_turns: 2,
        script_calls: 1,
        capability_invocations: 2,
    })))
}

#[test]
fn nests_calls_under_the_script_that_dispatched_them() {
    let mut transcript = Transcript::default();
    transcript.open("list the open issues".to_owned());

    transcript.absorb(SessionEvent::ScriptStarted {
        sequence: 0,
        script: "gh issue list".to_owned(),
    });
    transcript.absorb(call(1, "gh.issue.list", CallOutcome::Succeeded(json!([]))));
    transcript.absorb(finished(0, 1));
    transcript.absorb(SessionEvent::ScriptStarted {
        sequence: 3,
        script: "gh issue comment".to_owned(),
    });
    transcript.absorb(call(
        4,
        "gh.issue.comment",
        CallOutcome::Denied("unconstrained-capability".to_owned()),
    ));
    transcript.absorb(finished(3, 1));
    transcript.absorb(answered("two issues"));

    let turn = &transcript.turns()[0];
    assert_eq!(turn.scripts.len(), 2);
    assert_eq!(turn.scripts[0].calls[0].capability, "gh.issue.list");
    assert_eq!(turn.scripts[1].calls[0].capability, "gh.issue.comment");
    assert_eq!(turn.capability_calls(), 2);
    assert_eq!(
        turn.denied_calls(),
        1,
        "a denial is a policy question and is counted apart from a failure"
    );
    assert_eq!(turn.status, TurnStatus::Answered("two issues".to_owned()));
    assert!(!turn.status.is_running());
}

#[test]
fn a_call_with_no_open_script_is_kept_rather_than_dropped() {
    let mut transcript = Transcript::default();
    transcript.open("ask".to_owned());
    transcript.absorb(call(9, "gh.repo.read", CallOutcome::NotFound));

    let turn = &transcript.turns()[0];
    assert_eq!(
        turn.capability_calls(),
        1,
        "a call that really happened must never vanish from the view that exists to show it"
    );
    assert!(turn.scripts[0].script.is_empty());
}

#[test]
fn a_finished_script_stops_collecting_calls() {
    let mut transcript = Transcript::default();
    transcript.open("ask".to_owned());
    transcript.absorb(SessionEvent::ScriptStarted {
        sequence: 0,
        script: "first".to_owned(),
    });
    transcript.absorb(finished(0, 0));
    transcript.absorb(call(2, "gh.repo.read", CallOutcome::NotFound));

    let turn = &transcript.turns()[0];
    assert_eq!(turn.scripts.len(), 2, "the late call opened its own node");
    assert!(turn.scripts[0].calls.is_empty());
}

#[test]
fn a_failed_session_still_closes_its_turn() {
    let mut transcript = Transcript::default();
    transcript.open("ask".to_owned());
    transcript.absorb(SessionEvent::Finished(Box::new(Err(
        "session cancelled".to_owned()
    ))));

    assert_eq!(
        transcript.turns()[0].status,
        TurnStatus::Failed("session cancelled".to_owned())
    );
}

#[test]
fn a_suppressed_reply_is_not_an_empty_answer() {
    let mut transcript = Transcript::default();
    transcript.open("ask".to_owned());
    transcript.absorb(SessionEvent::Finished(Box::new(Ok(PromptOutcome {
        answer: String::new(),
        disposition: ReplyDisposition::Suppress,
        model_turns: 1,
        script_calls: 0,
        capability_invocations: 0,
    }))));

    assert_eq!(transcript.turns()[0].status, TurnStatus::Suppressed);
}

#[test]
fn token_accounting_distinguishes_unreported_from_zero() {
    let mut transcript = Transcript::default();
    transcript.open("ask".to_owned());
    transcript.absorb(SessionEvent::ModelUsage(Some(ModelUsage {
        input_tokens: Some(100),
        cached_input_tokens: None,
        output_tokens: Some(20),
        reasoning_output_tokens: None,
        total_tokens: Some(120),
    })));
    transcript.absorb(SessionEvent::ModelUsage(None));
    transcript.absorb(SessionEvent::ModelUsage(Some(ModelUsage {
        input_tokens: Some(50),
        cached_input_tokens: None,
        output_tokens: None,
        reasoning_output_tokens: None,
        total_tokens: None,
    })));

    let tokens = transcript.turns()[0].tokens;
    assert_eq!(tokens.input, Some(150));
    assert_eq!(tokens.output, Some(20));
    assert_eq!(
        tokens.unreported, 1,
        "a response reporting nothing is not a response reporting zero"
    );
}

#[test]
fn the_replay_window_is_the_suffix_the_model_still_sees() {
    let mut transcript = Transcript::default();
    for index in 0..5 {
        transcript.open(format!("turn {index}"));
        transcript.absorb(answered("ok"));
    }

    transcript.mark_replay_window(2);
    let inside: Vec<bool> = transcript
        .turns()
        .iter()
        .map(|turn| turn.in_replay_window)
        .collect();
    assert_eq!(
        inside,
        [false, false, false, true, true],
        "the window is a suffix, so the oldest turns are the ones the model forgot"
    );

    transcript.mark_replay_window(99);
    assert!(
        transcript.turns().iter().all(|turn| turn.in_replay_window),
        "a window wider than the conversation leaves nothing outside it"
    );
}

#[test]
fn events_with_no_open_turn_are_dropped() {
    let mut transcript = Transcript::default();
    transcript.absorb(call(0, "gh.repo.read", CallOutcome::NotFound));

    assert!(
        transcript.is_empty(),
        "a turn exists because somebody typed a prompt; none may be invented"
    );
}
