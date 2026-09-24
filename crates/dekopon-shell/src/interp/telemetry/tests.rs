use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
};

use tracing::{
    Subscriber,
    field::{Field, Visit},
    span,
};
use tracing_subscriber::{
    Layer,
    layer::{Context, SubscriberExt},
    registry::LookupSpan,
};

use crate::{Interpreter, Limits, ScriptOutcome, interp::tests::Fixture};

use super::{CONTROL_WORDS, SCRIPT_SPAN};

#[derive(Clone, Debug)]
struct Captured {
    span: Option<String>,
    parents: Vec<String>,
    fields: BTreeMap<String, String>,
}

impl Captured {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }
}

#[derive(Default)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }
}

struct CaptureLayer {
    spans: Arc<Mutex<Vec<Captured>>>,
}

impl<S> Layer<S> for CaptureLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        attributes: &span::Attributes<'_>,
        id: &span::Id,
        context: Context<'_, S>,
    ) {
        let mut fields = Fields::default();
        attributes.record(&mut fields);
        if let Some(span) = context.span(id) {
            span.extensions_mut().insert(fields);
        }
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, context: Context<'_, S>) {
        if let Some(span) = context.span(id) {
            let mut extensions = span.extensions_mut();
            if let Some(fields) = extensions.get_mut::<Fields>() {
                values.record(fields);
            }
        }
    }

    fn on_close(&self, id: span::Id, context: Context<'_, S>) {
        let Some(span) = context.span(&id) else {
            return;
        };
        let fields = span
            .extensions()
            .get::<Fields>()
            .map(|fields| fields.0.clone())
            .unwrap_or_default();
        self.spans.lock().expect("span lock").push(Captured {
            span: Some(span.name().to_owned()),
            parents: span
                .scope()
                .skip(1)
                .map(|parent| parent.name().to_owned())
                .collect(),
            fields,
        });
    }
}

struct Telemetry {
    outcome: ScriptOutcome,
    spans: Vec<Captured>,
}

impl Telemetry {
    fn commands(&self) -> Vec<(&str, &str)> {
        self.spans
            .iter()
            .filter(|span| span.span.as_deref() == Some("shell.command"))
            .map(|span| {
                (
                    span.field("shell.command.kind").unwrap_or("<missing>"),
                    span.field("shell.command.name").unwrap_or("<missing>"),
                )
            })
            .collect()
    }

    fn command_spans(&self, name: &str) -> Vec<&Captured> {
        self.spans
            .iter()
            .filter(|span| {
                span.span.as_deref() == Some("shell.command")
                    && span.field("shell.command.name") == Some(name)
            })
            .collect()
    }
}

fn capture(script: &str) -> Telemetry {
    capture_with(script, Limits::default(), false)
}

fn capture_with(script: &str, limits: Limits, enclose: bool) -> Telemetry {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(CaptureLayer {
        spans: Arc::clone(&spans),
    });

    let outcome = tracing::subscriber::with_default(subscriber, || {
        let enclosing = enclose.then(|| tracing::info_span!("caller.enclosing"));
        let _entered = enclosing.as_ref().map(tracing::Span::enter);
        Interpreter::new(limits).run(script, &Fixture::default())
    });

    let spans = spans.lock().expect("span lock").clone();
    Telemetry { outcome, spans }
}

fn assert_recorded(span: &Captured, field: &str, recorded: &str, total: usize) {
    assert_eq!(span.field(field), Some(recorded), "{field}");
    let bytes = format!("{field}.bytes");
    assert_eq!(
        span.field(&bytes),
        Some(total.to_string().as_str()),
        "{bytes}"
    );
}

#[test]
fn every_command_produces_exactly_one_span() {
    let telemetry =
        capture("greet() { echo hi; }\ngreet\njq -n 1\nprobe upper --text two\nnosuchcommand\n:");

    assert_eq!(
        telemetry.commands(),
        vec![
            ("builtin", "echo"),
            ("function", "greet"),
            ("builtin", "jq"),
            ("provider-command", "probe"),
            ("not-found", "nosuchcommand"),
            ("control", ":"),
        ]
    );

    let spans = telemetry
        .spans
        .iter()
        .filter(|span| span.span.as_deref() == Some("shell.command"))
        .count();
    assert_eq!(spans, 6);
}

#[test]
fn a_span_carries_the_outcome_exit_code_and_argument_count() {
    let telemetry = capture("echo one two three");
    let span = telemetry
        .spans
        .iter()
        .find(|span| span.span.as_deref() == Some("shell.command"))
        .expect("a command span");

    assert_eq!(span.field("shell.command.name"), Some("echo"));
    assert_eq!(span.field("shell.command.kind"), Some("builtin"));
    assert_eq!(span.field("shell.command.argument_count"), Some("3"));
    assert_eq!(
        span.field("shell.command.arguments"),
        Some(r#"["one","two","three"]"#)
    );
    assert_eq!(span.field("shell.command.exit_code"), Some("0"));
    assert_eq!(span.field("outcome"), Some("succeeded"));
}

#[test]
fn a_denied_capability_is_not_flattened_into_a_generic_failure() {
    for (script, outcome, exit_code) in [
        ("probe upper --text hi", "succeeded", "0"),
        ("probe broken", "failed", "1"),
        ("probe denied", "denied", "126"),
        ("probe ungranted", "not-found", "127"),
        ("nosuchcommand", "not-found", "127"),
    ] {
        let telemetry = capture(script);
        let completed = telemetry
            .spans
            .iter()
            .find(|span| span.span.as_deref() == Some("shell.command"))
            .unwrap_or_else(|| panic!("{script}: a completed span"));
        assert_eq!(completed.field("outcome"), Some(outcome), "{script}");
        assert_eq!(
            completed.field("shell.command.exit_code"),
            Some(exit_code),
            "{script}"
        );
    }
}

#[test]
fn a_refused_word_reports_the_reason_it_aborted_the_script() {
    let telemetry = capture("eval 'echo hi'");
    let completed = telemetry
        .spans
        .iter()
        .find(|span| span.span.as_deref() == Some("shell.command"))
        .expect("a shell.command span");

    assert_eq!(completed.field("shell.command.kind"), Some("rejected"));
    assert_eq!(completed.field("shell.command.name"), Some("eval"));
    assert_eq!(completed.field("outcome"), Some("rejected"));
    assert_eq!(telemetry.outcome.exit_code.get(), 2);
}

#[test]
fn an_exhausted_budget_is_reported_as_a_limit_rather_than_a_failure() {
    let telemetry = capture_with(
        "while true; do probe upper --text x; done",
        Limits {
            max_capability_calls: 2,
            ..Limits::default()
        },
        false,
    );
    let completed = telemetry
        .spans
        .iter()
        .rfind(|span| span.span.as_deref() == Some("shell.command"))
        .expect("a shell.command span");
    assert_eq!(completed.field("outcome"), Some("limit-exceeded"));
}

#[test]
fn a_command_span_records_its_arguments_stdin_and_output() {
    let telemetry = capture("probe upper --text hello\necho piped | probe upper -");
    let probes = telemetry.command_spans("probe");
    let [flag, piped] = probes.as_slice() else {
        panic!("two probe spans: {probes:?}");
    };

    let arguments = r#"["upper","--text","hello"]"#;
    assert_recorded(flag, "shell.command.arguments", arguments, arguments.len());
    let output = r#"{"text":"HELLO"}"#;
    assert_recorded(flag, "shell.command.output", output, output.len());
    assert_eq!(flag.field("shell.command.stdin"), None);
    assert_eq!(flag.field("shell.command.stdin.bytes"), None);

    let arguments = r#"["upper","-"]"#;
    assert_recorded(piped, "shell.command.arguments", arguments, arguments.len());
    assert_recorded(piped, "shell.command.stdin", "piped", "piped".len());
    let output = r#"{"text":"PIPED"}"#;
    assert_recorded(piped, "shell.command.output", output, output.len());
}

#[test]
fn an_oversized_attribute_keeps_a_4096_byte_head_a_marker_and_its_full_length() {
    const CAP: usize = 4096;
    const MARKER: &str = "…[truncated]";

    let payload = "x".repeat(CAP + 904);
    let telemetry = capture(&format!(
        "probe upper --text {payload}\necho {payload} | probe upper -"
    ));
    let probes = telemetry.command_spans("probe");
    let [flag, piped] = probes.as_slice() else {
        panic!("two probe spans: {probes:?}");
    };

    let arguments = format!(r#"["upper","--text","{payload}"]"#);
    let output = format!(r#"{{"text":"{}"}}"#, payload.to_uppercase());
    for (span, field, full) in [
        (flag, "shell.command.arguments", &arguments),
        (flag, "shell.command.output", &output),
        (piped, "shell.command.stdin", &payload),
        (piped, "shell.command.output", &output),
    ] {
        let head = format!("{}{MARKER}", &full[..CAP]);
        assert_recorded(span, field, &head, full.len());
    }

    let exact = "x".repeat(CAP - r#"["upper","--text",""]"#.len());
    let telemetry = capture(&format!("probe upper --text {exact}"));
    let probes = telemetry.command_spans("probe");
    let [span] = probes.as_slice() else {
        panic!("one probe span: {probes:?}");
    };
    let arguments = format!(r#"["upper","--text","{exact}"]"#);
    assert_eq!(arguments.len(), CAP);
    assert_recorded(span, "shell.command.arguments", &arguments, CAP);
}

#[test]
fn a_model_authored_command_word_is_recorded_verbatim() {
    let telemetry = capture("model_helper() { echo hi; }\nmodel_helper\nmodel_typo");

    assert_eq!(
        telemetry.commands(),
        vec![
            ("builtin", "echo"),
            ("function", "model_helper"),
            ("not-found", "model_typo"),
        ]
    );
}

#[test]
fn a_capability_shaped_word_is_recorded_as_not_found_with_its_arguments() {
    let telemetry = capture("cli-probe.upper --text hi\nwikipedia_page --title x");

    assert_eq!(
        telemetry.commands(),
        vec![
            ("not-found", "cli-probe.upper"),
            ("not-found", "wikipedia_page"),
        ]
    );
    let spans = telemetry.command_spans("wikipedia_page");
    let [span] = spans.as_slice() else {
        panic!("one wikipedia_page span: {spans:?}");
    };
    let arguments = r#"["--title","x"]"#;
    assert_recorded(span, "shell.command.arguments", arguments, arguments.len());
    assert_eq!(span.field("shell.command.exit_code"), Some("127"));
}

#[test]
fn xargs_records_every_command_it_actually_drove() {
    let telemetry =
        capture("probe object --a a --b b --c c | jq '[.a,.b,.c]' | xargs probe upper --text");

    let probes = telemetry.command_spans("probe");
    assert_eq!(probes.len(), 4, "the producer plus one per element");
    let nested = probes
        .iter()
        .filter(|span| span.parents.iter().any(|parent| parent == "shell.command"))
        .map(|span| span.field("shell.command.arguments").unwrap_or("<missing>"))
        .collect::<Vec<_>>();
    assert_eq!(
        nested,
        vec![
            r#"["upper","--text","a"]"#,
            r#"["upper","--text","b"]"#,
            r#"["upper","--text","c"]"#,
        ]
    );
}

#[test]
fn command_spans_nest_under_the_callers_active_span() {
    let telemetry = capture_with("echo hi", Limits::default(), true);

    let span = telemetry
        .spans
        .iter()
        .find(|span| span.span.as_deref() == Some("shell.command"))
        .expect("a command span");
    assert_eq!(
        span.parents,
        vec![SCRIPT_SPAN.to_owned(), "caller.enclosing".to_owned()]
    );
}

#[test]
fn one_script_span_carries_the_totals_for_the_whole_run() {
    let telemetry = capture("greet() { echo hi; }\ngreet\nnosuchcommand\nprobe upper --text two");

    let script = telemetry
        .spans
        .iter()
        .find(|span| span.span.as_deref() == Some(SCRIPT_SPAN))
        .expect("one script span");
    assert_eq!(script.field("shell.script.commands"), Some("4"));
    assert_eq!(script.field("shell.script.capability_commands"), Some("1"));
    assert_eq!(script.field("shell.script.failed_commands"), Some("1"));

    let scripts = telemetry
        .spans
        .iter()
        .filter(|span| span.span.as_deref() == Some(SCRIPT_SPAN))
        .count();
    assert_eq!(scripts, 1, "one span per run, not one per statement");
}

#[test]
fn a_loop_heavy_script_still_spans_every_command_it_ran() {
    let commands = 300;
    let telemetry = capture(&format!(
        "i=0\nwhile [ $i -lt {commands} ]; do echo x; i=$(( i + 1 )); done"
    ));

    let script = telemetry
        .spans
        .iter()
        .find(|span| span.span.as_deref() == Some(SCRIPT_SPAN))
        .expect("one script span");
    let total = script
        .field("shell.script.commands")
        .and_then(|value| value.parse::<usize>().ok())
        .expect("a command total");
    assert!(
        total > commands,
        "the loop must run more commands than any plausible cap for this to prove anything: {total}"
    );

    let spans = telemetry
        .spans
        .iter()
        .filter(|span| span.span.as_deref() == Some("shell.command"))
        .count();
    assert_eq!(spans, total, "one span per command word, all the way down");

    assert_eq!(script.field("shell.script.failed_commands"), Some("1"));
}

#[test]
fn control_words_and_their_dispatcher_agree() {
    for word in CONTROL_WORDS {
        let outcome = capture(word).outcome;
        assert!(
            !outcome.output.contains("command not found"),
            "{word}: {}",
            outcome.output
        );
    }

    assert!(
        capture("definitelynotacontrolword")
            .outcome
            .output
            .contains("command not found")
    );
}
