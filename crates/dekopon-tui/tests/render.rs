//! Rendering tests over a `TestBackend`, which is a terminal only in the sense that it has a size.
//!
//! These assert on what reaches the buffer rather than on how it looks: that hostile text cannot
//! carry a control sequence into a frame, that a secret is not drawn, and that each pane says the
//! thing an operator would otherwise have to guess.

use dekopon_protocol::{Agent, AgentKind, AgentSpec, ApiVersion, ObjectMeta};
use dekopon_tui::{App, Mode, Notice, Pane, ui};
use ratatui::{Terminal, backend::TestBackend};

fn agent(name: &str, description: &str) -> Agent {
    Agent {
        api_version: ApiVersion::V1Alpha1,
        kind: AgentKind::Agent,
        metadata: ObjectMeta::named(name),
        spec: AgentSpec {
            description: description.to_owned(),
            enabled: true,
            instructions: None,
            skills: Vec::new(),
            model_class: Some("reasoning".to_owned()),
            instructions_file: None,
        },
        status: None,
    }
}

fn console(agents: Vec<Agent>) -> App {
    App::new(
        agents,
        "slack.t0123abc.u9xyz".to_owned(),
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
    assert!(
        drawn.contains("slack.t0123abc.u9xyz"),
        "the subject is missing"
    );
    assert!(
        drawn.contains("chatgpt-auth.console.json"),
        "the credential file is missing"
    );
}

#[test]
fn header_identifies_synthetic_session_without_claiming_real_scope() {
    let mut app = console(vec![agent("reviewer", "fixture")]);
    app.scope_label = "console-smoke".into();
    let drawn = frame(&app);
    assert!(drawn.contains("session: console-smoke"), "{drawn}");
    assert!(!drawn.contains("REQUESTED scope"), "{drawn}");
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
fn selected_model_provenance_is_persistent_at_ordinary_widths() {
    use dekopon_tui::session::{ModelSource, selected_model};
    let mut app = console(Vec::new());
    for width in [80, 100, 120] {
        for (cli, profile, source) in [
            (
                Some("cli-model"),
                Some("distinct-profile-model"),
                ModelSource::CliOverride,
            ),
            (None, Some("profile-model"), ModelSource::Profile),
            (None, None, ModelSource::Default),
        ] {
            (app.model, app.model_source) = selected_model(cli, profile);
            for pane in Pane::ORDER {
                app.pane = pane;
                let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
                terminal.draw(|f| ui::draw(f, &app)).unwrap();
                let drawn: String = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect();
                assert!(
                    drawn.contains(&format!("model ({}): {}", source.label(), app.model)),
                    "{width}: {drawn}"
                );
                assert!(drawn.contains("LIVE PROVIDERS — effects are real"));
                assert!(!drawn.contains("distinct-profile-model"));
            }
        }
    }
}

fn shell_rows(app: &App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| ui::draw(frame, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(width as usize)
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
        })
        .collect()
}

#[test]
fn completed_shell_commands_render_zero_and_nonzero_boundaries() {
    let mut app = console(Vec::new());
    app.pane = Pane::Shell;
    for status in [0, 7] {
        app.start_shell("probe".into());
        app.finish_shell(dekopon_tui::app::ShellEntry {
            input: "probe".into(),
            output: "result".into(),
            exit_code: Some(status),
            truncated: false,
        });
    }
    let drawn = shell_rows(&app, 80, 20).join("\n");
    assert!(drawn.contains("[exit code: 0]"), "{drawn}");
    assert!(drawn.contains("[exit code: 7]"), "{drawn}");
}

#[test]
fn shell_command_output_and_prompt_share_one_wrapped_viewport() {
    let mut app = console(Vec::new());
    app.pane = Pane::Shell;
    app.mode = Mode::Composing;
    app.start_shell("echo abcdefghijklmnop".into());
    let running = shell_rows(&app, 18, 19).join("\n");
    assert_eq!(
        running.matches("echo ").count(),
        1,
        "echo only once: {running}"
    );
    assert!(running.contains("[running"));
    app.finish_shell(dekopon_tui::app::ShellEntry {
        input: "echo abcdefghijklmnop".into(),
        output: "漢字 test   spaced  \u{1b}[2Ktail\nxyz01234567890123456789".into(),
        exit_code: Some(7),
        truncated: true,
    });
    let rows = shell_rows(&app, 32, 22);
    let drawn = rows.join("\n");
    assert!(drawn.contains("echo abcdefghijklmnop"));
    assert!(drawn.contains("test   spaced"), "{drawn}");
    assert!(drawn.contains("[exit code: 7]"));
    assert!(drawn.contains("[output truncated"));
    assert!(!drawn.contains('\u{1b}'));
    let prompt = rows.iter().rposition(|row| row.contains("│> ")).unwrap();
    assert!(
        prompt
            > rows
                .iter()
                .position(|row| row.contains("truncated"))
                .unwrap()
    );
}

#[test]
fn visual_row_paging_and_resize_clamp_without_losing_input() {
    let mut app = console(Vec::new());
    app.pane = Pane::Shell;
    app.mode = Mode::Composing;
    app.shell_viewport = (10, 5);
    app.composer = "still typing".into();
    for index in 0..12 {
        app.start_shell(format!("cmd{index}長長長長"));
        app.finish_shell(dekopon_tui::app::ShellEntry {
            input: format!("cmd{index}長長長長"),
            output: "a long unbroken output value  ".into(),
            exit_code: Some(0),
            truncated: false,
        });
    }
    dekopon_tui::ui::shell::home(&mut app);
    let first = shell_rows(&app, 12, 15).join("\n");
    assert!(first.contains("cmd0"));
    dekopon_tui::ui::shell::page(&mut app, 1);
    let next = shell_rows(&app, 12, 15).join("\n");
    assert!(
        !next.contains("cmd0"),
        "page should count wrapped rows: {next}"
    );
    assert_eq!(app.composer, "still typing");
    app.shell_viewport = (38, 14);
    dekopon_tui::ui::shell::page(&mut app, 1);
    app.shell_top = None;
    let bottom = shell_rows(&app, 40, 24).join("\n");
    assert!(
        bottom.contains("still typing"),
        "End follows editable prompt: {bottom}"
    );
}
