//! Rendering tests over a `TestBackend`, which is a terminal only in the sense that it has a size.
//!
//! These assert on what reaches the buffer rather than on how it looks: that hostile text cannot
//! carry a control sequence into a frame, that a secret is not drawn, and that each pane says the
//! thing an operator would otherwise have to guess.

use dekopon_protocol::{Agent, AgentKind, AgentSpec, ApiVersion, ObjectMeta};
use dekopon_tui::{
    App, Mode, Notice, Pane,
    record::{CallOutcome, CapabilityCall, ScriptRun, SessionEvent},
    ui,
};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::json;
use std::time::Duration;

fn agent(name: &str, description: &str) -> Agent {
    Agent {
        api_version: ApiVersion::V1Alpha1,
        kind: AgentKind::Agent,
        metadata: ObjectMeta::named(name),
        spec: AgentSpec {
            description: description.to_owned(),
            enabled: true,
            instructions: None,
            capabilities: Vec::new(),
            providers: Vec::new(),
            model_class: Some("reasoning".to_owned()),
            policy_profile: None,
        },
        status: None,
    }
}

fn console(agents: Vec<Agent>) -> App {
    App::new(
        agents,
        "tel.15550100000".to_owned(),
        "/run/dekopon/broker.sock".to_owned(),
        "/config/dekopon/chatgpt-auth.console.json".to_owned(),
    )
}

/// Renders one frame and returns everything drawn, as text.
fn frame(app: &App) -> String {
    let mut terminal =
        Terminal::new(TestBackend::new(140, 34)).expect("a test backend always builds");
    terminal
        .draw(|frame| ui::draw(frame, app))
        .expect("the test backend never fails to draw");
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect()
}

#[test]
fn every_pane_draws_without_panicking_on_an_empty_console() {
    for pane in Pane::ORDER {
        let mut app = console(Vec::new());
        app.pane = pane;
        let drawn = frame(&app);
        assert!(
            drawn.contains(pane.title()),
            "{pane:?} must name itself in the tab bar"
        );
    }
}

#[test]
fn the_status_line_shows_the_subject_and_the_credential_file() {
    // Both are resolved rather than typed, and both decide what a session may do and whose token it
    // spends. An operator must never have to guess either.
    let drawn = frame(&console(Vec::new()));
    assert!(drawn.contains("tel.15550100000"), "the subject is missing");
    assert!(
        drawn.contains("chatgpt-auth.console.json"),
        "the credential file is missing"
    );
}

#[test]
fn the_agent_list_renders_a_catalog() {
    let mut app = console(vec![agent("ville-github", "the GitHub assistant")]);
    app.pane = Pane::Agents;
    let drawn = frame(&app);

    assert!(drawn.contains("ville-github"));
    assert!(drawn.contains("the GitHub assistant"));
    assert!(drawn.contains("reasoning"));
}

#[test]
fn hostile_agent_text_reaches_the_buffer_without_its_control_sequences() {
    // A description is catalog text, but a title, an issue body, and a provider error all arrive
    // through read-only capabilities and are drawn by the same path.
    let mut app = console(vec![agent(
        "ville-github",
        "safe\u{1b}[2Koverwritten\u{9b}31m\u{202e}reversed",
    )]);
    app.pane = Pane::Agents;
    let drawn = frame(&app);

    assert!(!drawn.contains('\u{1b}'), "ESC reached the buffer");
    assert!(
        !drawn.contains('\u{9b}'),
        "eight-bit CSI reached the buffer"
    );
    assert!(
        !drawn.contains('\u{202e}'),
        "a bidi override reached the buffer"
    );
    assert!(drawn.contains("safe"), "the readable text was lost with it");
}

#[test]
fn the_turn_pane_draws_the_call_tree_and_hides_the_secret_in_it() {
    let mut app = console(Vec::new());
    app.pane = Pane::Turns;
    app.transcript.open("list the open issues".to_owned());
    app.on_session_event(SessionEvent::ScriptStarted {
        sequence: 0,
        script: "gh issue list -R dekopon-agents/dekopon".to_owned(),
    });
    app.on_session_event(SessionEvent::Capability(Box::new(CapabilityCall {
        sequence: 1,
        capability: "gh.issue.list".to_owned(),
        input: json!({"owner": "dekopon-agents", "token": "ghp_0123456789abcdefghijklmnop"}),
        outcome: CallOutcome::Succeeded(json!([{"number": 47}])),
        elapsed: Duration::from_millis(340),
    })));
    app.on_session_event(SessionEvent::ScriptFinished(Box::new(ScriptRun {
        sequence: 0,
        script: "gh issue list -R dekopon-agents/dekopon".to_owned(),
        output: String::new(),
        exit_code: 0,
        truncated: false,
        capability_calls: 1,
        steps: 6,
        elapsed: Duration::from_millis(400),
    })));

    let drawn = frame(&app);
    assert!(
        drawn.contains("list the open issues"),
        "the prompt is missing"
    );
    assert!(drawn.contains("gh issue list"), "the script is missing");
    assert!(drawn.contains("gh.issue.list"), "the capability is missing");
    assert!(drawn.contains("succeeded"), "the outcome is missing");
    assert!(
        !drawn.contains("ghp_0123456789"),
        "the token was drawn: {drawn}"
    );
    assert!(
        drawn.contains("redacted"),
        "the redaction was not announced"
    );
}

#[test]
fn a_denial_is_drawn_with_its_reason() {
    let mut app = console(Vec::new());
    app.pane = Pane::Turns;
    app.transcript.open("merge it".to_owned());
    app.on_session_event(SessionEvent::Capability(Box::new(CapabilityCall {
        sequence: 0,
        capability: "gh.pull-request.merge".to_owned(),
        input: json!({"number": 7}),
        outcome: CallOutcome::Denied("unconstrained-capability".to_owned()),
        elapsed: Duration::from_millis(12),
    })));

    let drawn = frame(&app);
    assert!(drawn.contains("unconstrained-capability"));
}

#[test]
fn the_shell_pane_says_state_does_not_carry_over() {
    // Otherwise it is discovered by setting a variable and watching it vanish, which reads as a bug
    // in the interpreter rather than as how one script per line works.
    let mut app = console(Vec::new());
    app.pane = Pane::Shell;
    let drawn = frame(&app);
    assert!(
        drawn.contains("hop into an agent first"),
        "an unhopped shell must say why it cannot run anything"
    );
}

#[test]
fn the_help_overlay_says_a_stop_is_cooperative() {
    let mut app = console(Vec::new());
    app.mode = Mode::Help;
    let drawn = frame(&app);

    assert!(drawn.contains("quit"));
    assert!(
        drawn.contains("calls already sent still complete"),
        "the overlay must not let a stop read as a rollback"
    );
}

#[test]
fn a_refusal_is_drawn_on_the_status_line() {
    let mut app = console(Vec::new());
    app.notice = Some(Notice::refusal("policy grants this subject nothing here"));
    let drawn = frame(&app);
    assert!(drawn.contains("policy grants this subject nothing here"));
}

#[test]
fn a_forgotten_turn_is_marked_as_outside_the_replay_window() {
    let mut app = console(Vec::new());
    app.pane = Pane::Turns;
    for index in 0..3 {
        app.transcript.open(format!("question {index}"));
        app.on_session_event(SessionEvent::Finished(Box::new(Ok(
            dekopon_agent::prompt::PromptOutcome {
                answer: format!("answer {index}"),
                disposition: dekopon_agent::prompt::ReplyDisposition::Send,
                model_turns: 1,
                script_calls: 0,
                capability_invocations: 0,
            },
        ))));
    }
    app.on_session_complete(1);

    let drawn = frame(&app);
    assert!(
        drawn.contains("outside the model's replay window"),
        "the one thing no other surface can show must actually be drawn"
    );
}
