//! The shared chat-progress doubles, exercised against their own contracts.
//!
//! A fixture nobody checks is a fixture that quietly stops doing what its callers assume. Each of
//! these pins the one property its users depend on and nothing else: that the runtime really does
//! park, that the stream really does hand out one event per release and interrupts on `Break`, that
//! a parked stream emits nothing at all, and that the driver's per-object switches and failure
//! injection are independent of each other.

use std::{
    ops::ControlFlow,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use dekopon_agent::prompt::{
    CancellationProbe, History, PromptError, PromptLimits, ScriptRuntime as _, SessionInputs,
    run_prompt_session,
};
use dekopon_model::{
    TurnEvent,
    model::{AssistantTurn, ChatModel as _, CompletionOptions},
};
use dekopon_test_support::{
    BlockedRuntime, CODEX_RESPONSES_TOOL_CALL, CODEX_RESPONSES_TWO_DELTAS, DriverCall, FailureKind,
    OPENAI_CHAT_COMPLETIONS_TOOL_CALL, OPENAI_CHAT_COMPLETIONS_TWO_DELTAS, RecordingDriver,
    ScriptedStreamModel, scripted_text,
};

fn turn(text: &str) -> AssistantTurn {
    AssistantTurn {
        content: Some(text.to_owned()),
        tool_calls: Vec::new(),
        usage: None,
        replay_items: Vec::new(),
    }
}

fn scripted(body: &str) -> Vec<TurnEvent> {
    dekopon_model::events_from_transcript(body).expect("the recorded transcript parses")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_runtime_parks_inside_run_script_until_it_is_released() {
    let runtime = Arc::new(BlockedRuntime::new("done").offering(&["echo.echo"]));
    let parked = Arc::clone(&runtime);
    let running =
        tokio::task::spawn_blocking(move || parked.run_script("echo.echo --message hi", 4));

    tokio::time::timeout(Duration::from_secs(5), runtime.wait_until_parked())
        .await
        .expect("the script reaches the park");
    assert!(!running.is_finished(), "a parked script has not returned");
    assert_eq!(runtime.scripts(), ["echo.echo --message hi"]);
    assert_eq!(runtime.command_words(), ["echo.echo"]);

    runtime.release();
    let outcome = running.await.expect("the released script finishes");
    assert_eq!(outcome.output, "done");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scripted_stream_hands_out_one_event_per_release() {
    let events = scripted(OPENAI_CHAT_COMPLETIONS_TWO_DELTAS);
    assert_eq!(scripted_text(&events).as_str(), "Echoed hello.");
    let model = Arc::new(ScriptedStreamModel::scripted(events, turn("Echoed hello.")));
    let running = {
        let model = Arc::clone(&model);
        tokio::task::spawn_blocking(move || {
            model.complete(&[], &[], &CompletionOptions::default(), &mut |_| {
                ControlFlow::Continue(())
            })
        })
    };

    model.release_next();
    tokio::time::timeout(Duration::from_secs(5), model.wait_for_event())
        .await
        .expect("the first event arrives");
    assert_eq!(model.emitted(), 1, "the second event waits on its release");

    model.release_next();
    tokio::time::timeout(Duration::from_secs(5), model.wait_for_event())
        .await
        .expect("the second event arrives");
    model.release_next();
    let answered = running.await.expect("the stream returns its turn");
    assert_eq!(
        answered.expect("an uninterrupted stream answers").content,
        Some("Echoed hello.".to_owned())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_the_caller_breaks_reports_the_interruption_rather_than_a_turn() {
    let model = Arc::new(ScriptedStreamModel::scripted(
        scripted(OPENAI_CHAT_COMPLETIONS_TWO_DELTAS),
        turn("never returned"),
    ));
    let running = {
        let model = Arc::clone(&model);
        tokio::task::spawn_blocking(move || {
            model.complete(&[], &[], &CompletionOptions::default(), &mut |_| {
                ControlFlow::Break(())
            })
        })
    };
    model.release_next();

    let outcome = running.await.expect("the interrupted stream returns");
    let error = outcome.expect_err("breaking the callback is not an answer");
    assert!(
        matches!(error, dekopon_model::model::ModelError::Interrupted),
        "the surfaced cause names the interruption: {error}"
    );
    assert_eq!(model.emitted(), 1, "nothing is read past the break");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_parked_stream_emits_nothing_at_all_until_it_is_released() {
    let model = Arc::new(ScriptedStreamModel::parked(
        turn("after the silence"),
        Duration::from_secs(30),
    ));
    let running = {
        let model = Arc::clone(&model);
        tokio::task::spawn_blocking(move || {
            model.complete(&[], &[], &CompletionOptions::default(), &mut |_| {
                ControlFlow::Continue(())
            })
        })
    };

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        model.emitted(),
        0,
        "this is the reasoning phase: the socket is silent and there is no event boundary"
    );
    assert!(!running.is_finished());

    model.release_next();
    let answered = running.await.expect("the released stream returns");
    assert_eq!(
        answered.expect("the turn arrives whole").content,
        Some("after the silence".to_owned())
    );
}

/// The accepted limit, pinned: a phase that emits nothing cannot be stopped before its deadline.
///
/// `docs/chat-progress.md` and `docs/dekopond.md` both promise this in prose — a reasoning phase
/// sends no events, the loop's cancellation check runs only at an event boundary, and a stop
/// pressed during one therefore lands at the client's global deadline rather than at the press.
/// A promise about what does *not* happen is exactly the kind that rots unpinned, so the deadline
/// here is short and the press is real: the run must still be going after the stop, and the ending
/// when it finally arrives must be the cancellation rather than the answer the model had ready.
#[tokio::test(flavor = "multi_thread")]
async fn a_parked_no_event_stream_is_interrupted_only_when_its_deadline_elapses() {
    /// The stand-in for `timeout_global`: long enough to watch the press do nothing, short enough
    /// to wait out.
    const DEADLINE: Duration = Duration::from_millis(600);
    /// How long the stop is given to fail to interrupt anything.
    const WATCHED: Duration = Duration::from_millis(150);

    /// One person's press, as the loop sees it.
    struct Pressed(AtomicBool);

    impl CancellationProbe for Pressed {
        fn is_cancelled(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    let model = Arc::new(ScriptedStreamModel::parked(
        turn("the answer nobody reads"),
        DEADLINE,
    ));
    let runtime = Arc::new(BlockedRuntime::new("no script runs").offering(&["echo.echo"]));
    let stop = Arc::new(Pressed(AtomicBool::new(false)));
    let session = {
        let (model, runtime, stop) = (Arc::clone(&model), Arc::clone(&runtime), Arc::clone(&stop));
        tokio::task::spawn_blocking(move || {
            let mut history = History::default();
            run_prompt_session(
                model.as_ref(),
                runtime.as_ref(),
                SessionInputs::new(
                    "think about it",
                    PromptLimits {
                        max_steps: 2,
                        max_capability_calls: 2,
                    },
                )
                .with_cancellation(stop.as_ref()),
                &mut history,
            )
        })
    };

    // The request is open and the socket is silent, which is the only moment this property is
    // about; a sleep here would be asserting on the loop's start-up instead.
    tokio::time::timeout(Duration::from_secs(5), model.wait_until_asked())
        .await
        .expect("the turn reaches the model");
    stop.0.store(true, Ordering::SeqCst);
    tokio::time::sleep(WATCHED).await;

    assert_eq!(
        model.emitted(),
        0,
        "a silent phase hands the loop no event, so there is no boundary to stop at"
    );
    assert!(
        !session.is_finished(),
        "the stop is observed at the next boundary, and this phase has none until its deadline"
    );

    let outcome = tokio::time::timeout(Duration::from_secs(10), session)
        .await
        .expect("the parked turn ends when its deadline elapses")
        .expect("the prompt thread joins");
    let error = outcome.expect_err("a stopped session has no answer to give");
    assert!(
        matches!(error, PromptError::Cancelled),
        "the ending names the stop rather than the turn that outlasted it: {error}"
    );
    assert!(
        runtime.scripts().is_empty(),
        "the turn that outlasted the stop ran nothing: {:?}",
        runtime.scripts()
    );
}

#[test]
fn a_recording_driver_offers_only_the_objects_a_test_switched_on() {
    let bare = RecordingDriver::default();
    assert!(bare.typing_object().is_none());
    assert!(bare.status_object().is_none());
    assert!(bare.progress_object().is_none());
    assert!(bare.stream_object().is_none());
    assert!(bare.reaction_object().is_none());
    assert!(bare.cancel_button_object().is_none());

    let equipped = RecordingDriver::default()
        .with_typing(Duration::from_secs(8))
        .with_progress(2_000, Duration::from_secs(2));
    assert_eq!(
        equipped
            .typing_object()
            .expect("typing is switched on")
            .renew_every(),
        Duration::from_secs(8)
    );
    assert_eq!(
        equipped
            .progress_object()
            .expect("progress is switched on")
            .max_chars(),
        2_000
    );
    assert!(
        equipped.stream_object().is_none(),
        "switching one object on must not switch on its neighbours"
    );
}

#[test]
fn failure_injection_is_per_object_and_per_call() {
    let driver = RecordingDriver::default()
        .with_status()
        .with_reaction()
        .configure_status(|status| status.failing_from(1, FailureKind::RateLimited));
    let status = driver.status_object().expect("status is switched on");
    let reaction = driver.reaction_object().expect("reaction is switched on");

    assert_eq!(
        status.charge(),
        None,
        "the first call is the one that works"
    );
    assert_eq!(status.charge(), Some(FailureKind::RateLimited));
    assert_eq!(status.charge(), Some(FailureKind::RateLimited));
    assert_eq!(
        reaction.charge(),
        None,
        "one rate-limited surface does not take the others down with it"
    );

    driver.record_reply("answered".to_owned(), vec![12, 34]);
    assert_eq!(driver.replies(), ["answered"]);
    assert_eq!(driver.image_bytes(), [vec![12, 34]]);
    assert_eq!(driver.rendered(), ["reply:answered+2"]);
    assert!(matches!(
        driver.calls().first(),
        Some(DriverCall::Reply { images, .. }) if images == &[12, 34]
    ));
}

#[test]
fn every_recorded_transcript_parses_to_the_events_its_backend_sends() {
    // Both routes and both turn shapes, because the four combinations are what the parser has to
    // get right and a fixture that only covers one of them hides the other three.
    for (name, body, text) in [
        (
            "openai two deltas",
            OPENAI_CHAT_COMPLETIONS_TWO_DELTAS,
            "Echoed hello.",
        ),
        (
            "codex two deltas",
            CODEX_RESPONSES_TWO_DELTAS,
            "Echoed hello.",
        ),
    ] {
        let events = scripted(body);
        assert_eq!(scripted_text(&events).as_str(), text, "{name}");
        assert!(
            events
                .iter()
                .all(|event| matches!(event, TurnEvent::TextDelta(_))),
            "{name} carries visible text only: {events:?}"
        );
    }
    for (name, body) in [
        ("openai tool call", OPENAI_CHAT_COMPLETIONS_TOOL_CALL),
        ("codex tool call", CODEX_RESPONSES_TOOL_CALL),
    ] {
        let events = scripted(body);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TurnEvent::ToolCallStarted { index: 0 })),
            "{name} announces the call before its arguments: {events:?}"
        );
        assert!(
            scripted_text(&events).is_empty(),
            "{name} shows the person nothing: a tool call is not an answer"
        );
    }
}

#[test]
fn a_failure_plan_can_name_one_call_and_replies_have_their_own_switch() {
    let driver = RecordingDriver::default()
        .with_stream(2_000, Duration::from_millis(10))
        .with_progress(2_000, Duration::from_secs(2))
        .configure_stream(|stream| stream.failing_call(1, FailureKind::Response))
        .configure_progress(|progress| progress.failing_from(0, FailureKind::RateLimited))
        .failing_replies_from(0, FailureKind::Closed);
    let stream = driver.stream_object().expect("stream is switched on");
    let progress = driver.progress_object().expect("progress is switched on");

    assert_eq!(
        progress.charge(),
        Some(FailureKind::RateLimited),
        "a rung can be degraded from its very first call"
    );
    assert_eq!(stream.charge(), None);
    assert_eq!(stream.charge(), Some(FailureKind::Response));
    assert_eq!(
        stream.charge(),
        None,
        "`failing_call` names one call and leaves the rest alone"
    );
    assert_eq!(stream.calls(), 3);
    assert_eq!(
        driver.charge_reply(),
        Some(FailureKind::Closed),
        "the reply switch is independent of every surface's"
    );
}
