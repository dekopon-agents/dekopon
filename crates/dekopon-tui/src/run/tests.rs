use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use dekopon_protocol::{Agent, AgentKind, AgentSpec, ApiVersion, ObjectMeta};

use super::{Action, on_key};
use crate::{
    app::{App, Mode, Pane},
    session::StopFlag,
};

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

fn console() -> App {
    let agent = Agent {
        api_version: ApiVersion::V1Alpha1,
        kind: AgentKind::Agent,
        metadata: ObjectMeta::named("ville-github"),
        spec: AgentSpec {
            description: "a fixture".to_owned(),
            enabled: true,
            instructions: None,
            capabilities: Vec::new(),
            providers: Vec::new(),
            model_class: None,
            policy_profile: None,
        },
        status: None,
    };
    App::new(
        vec![agent],
        "tel.15550100000".to_owned(),
        "/run/dekopon/broker.sock".to_owned(),
        "/config/dekopon/chatgpt-auth.console.json".to_owned(),
    )
}

#[test]
fn control_c_quits_from_any_mode() {
    for mode in [Mode::Browsing, Mode::Composing, Mode::Help] {
        let mut app = console();
        app.mode = mode.clone();
        let key = KeyEvent {
            modifiers: KeyModifiers::CONTROL,
            ..press(KeyCode::Char('c'))
        };
        assert!(on_key(&mut app, key, &StopFlag::default()).is_none());
        assert!(app.should_quit, "ctrl-c must work in {mode:?}");
    }
}

#[test]
fn any_key_dismisses_the_overlay_without_acting() {
    let mut app = console();
    app.mode = Mode::Help;
    assert!(on_key(&mut app, press(KeyCode::Char('q')), &StopFlag::default()).is_none());
    assert_eq!(app.mode, Mode::Browsing);
    assert!(
        !app.should_quit,
        "dismissing the overlay must not also quit"
    );
}

#[test]
fn tab_cycles_panes_in_both_directions() {
    let mut app = console();
    let stop = StopFlag::default();
    on_key(&mut app, press(KeyCode::Tab), &stop);
    assert_eq!(app.pane, Pane::Detail);
    on_key(&mut app, press(KeyCode::BackTab), &stop);
    assert_eq!(app.pane, Pane::Agents);
}

#[test]
fn enter_on_the_agent_list_asks_to_hop() {
    let mut app = console();
    assert_eq!(
        on_key(&mut app, press(KeyCode::Enter), &StopFlag::default()),
        Some(Action::Enter)
    );
}

#[test]
fn composing_collects_text_and_enter_submits_a_shell_line() {
    let mut app = console();
    let stop = StopFlag::default();
    app.pane = Pane::Shell;
    on_key(&mut app, press(KeyCode::Char('i')), &stop);
    assert_eq!(app.mode, Mode::Composing);

    for character in "cap --list".chars() {
        on_key(&mut app, press(KeyCode::Char(character)), &stop);
    }
    assert_eq!(app.composer, "cap --list");

    assert_eq!(
        on_key(&mut app, press(KeyCode::Enter), &stop),
        Some(Action::Shell("cap --list".to_owned()))
    );
    assert!(app.composer.is_empty());
    assert_eq!(app.mode, Mode::Browsing);
}

#[test]
fn escape_while_composing_discards_rather_than_stopping_a_turn() {
    let mut app = console();
    let stop = StopFlag::default();
    app.pane = Pane::Turns;
    app.mode = Mode::Composing;
    app.composer = "half a thought".to_owned();
    app.busy = true;

    on_key(&mut app, press(KeyCode::Esc), &stop);
    assert_eq!(app.mode, Mode::Browsing);
    assert!(app.composer.is_empty());
    assert!(
        !stop.is_requested(),
        "leaving the composer must not stop the running turn"
    );
}

#[test]
fn escape_while_browsing_stops_a_running_turn() {
    let mut app = console();
    let stop = StopFlag::default();
    app.busy = true;
    app.transcript.open("ask".to_owned());

    on_key(&mut app, press(KeyCode::Esc), &stop);
    assert!(stop.is_requested());
    assert!(app.transcript.turns()[0].stop_requested);
}

#[test]
fn escape_with_nothing_running_requests_nothing() {
    let mut app = console();
    let stop = StopFlag::default();
    on_key(&mut app, press(KeyCode::Esc), &stop);
    assert!(!stop.is_requested());
}

#[test]
fn backspace_removes_one_character() {
    let mut app = console();
    let stop = StopFlag::default();
    app.mode = Mode::Composing;
    app.composer = "abc".to_owned();
    on_key(&mut app, press(KeyCode::Backspace), &stop);
    assert_eq!(app.composer, "ab");
}

#[test]
fn composing_is_only_offered_where_there_is_something_to_type_into() {
    let mut app = console();
    let stop = StopFlag::default();
    app.pane = Pane::Agents;
    on_key(&mut app, press(KeyCode::Char('i')), &stop);
    assert_eq!(
        app.mode,
        Mode::Browsing,
        "the agent list has no composer, so `i` must not open one"
    );
}
