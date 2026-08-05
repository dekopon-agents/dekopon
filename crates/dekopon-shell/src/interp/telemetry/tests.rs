//! What the per-command spans carry, and — more importantly — what they must not.
//!
//! These assert on real emitted `tracing` output rather than on the helpers in the parent module,
//! because the redaction guarantee is a property of the call site: a field recorded by mistake in
//! `run_argv` would leave every helper here passing.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
};

use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
    span,
};
use tracing_subscriber::{
    Layer,
    layer::{Context, SubscriberExt},
    registry::LookupSpan,
};

use crate::{Interpreter, Limits, ScriptOutcome, interp::tests::Fixture};

use super::{CONTROL_WORDS, WITHHELD};

/// One span or event, flattened to the strings a remote collector would receive.
#[derive(Clone, Debug)]
struct Captured {
    /// Span name, or `None` for an event.
    span: Option<String>,
    /// Enclosing span names, innermost first.
    parents: Vec<String>,
    fields: BTreeMap<String, String>,
}

impl Captured {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }

    fn audit_event(&self) -> Option<&str> {
        self.field("audit.event")
    }
}

/// Collects every field of a span or event as its rendered string.
///
/// Rendering everything — including values recorded as `Debug` — is the point: a redaction test
/// that inspected only the fields it expected would not notice a new one carrying a secret.
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

/// A subscriber layer that records every span and event for later inspection.
struct CaptureLayer {
    spans: Arc<Mutex<Vec<Captured>>>,
    events: Arc<Mutex<Vec<Captured>>>,
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

    /// Fields recorded after creation — the exit code and outcome — arrive here.
    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, context: Context<'_, S>) {
        if let Some(span) = context.span(id) {
            let mut extensions = span.extensions_mut();
            if let Some(fields) = extensions.get_mut::<Fields>() {
                values.record(fields);
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.events.lock().expect("event lock").push(Captured {
            span: None,
            parents: context
                .event_scope(event)
                .into_iter()
                .flatten()
                .map(|span| span.name().to_owned())
                .collect(),
            fields: fields.0,
        });
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

/// Everything one script emitted.
struct Telemetry {
    outcome: ScriptOutcome,
    spans: Vec<Captured>,
    events: Vec<Captured>,
}

impl Telemetry {
    /// Every field value emitted anywhere, which is what a collector would end up holding.
    fn all_values(&self) -> Vec<&str> {
        self.spans
            .iter()
            .chain(&self.events)
            .flat_map(|record| record.fields.values().map(String::as_str))
            .collect()
    }

    /// The ordered `shell.command.started` events, as `(kind, name)` pairs.
    fn started(&self) -> Vec<(&str, &str)> {
        self.events
            .iter()
            .filter(|event| event.audit_event() == Some("shell.command.started"))
            .map(|event| {
                (
                    event.field("shell.command.kind").unwrap_or("<missing>"),
                    event.field("shell.command.name").unwrap_or("<missing>"),
                )
            })
            .collect()
    }
}

/// Runs one script under a capturing subscriber scoped to this thread.
fn capture(script: &str) -> Telemetry {
    capture_with(script, Limits::default(), false)
}

/// Runs one script, optionally inside an enclosing span standing in for the runner's own.
fn capture_with(script: &str, limits: Limits, enclose: bool) -> Telemetry {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(CaptureLayer {
        spans: Arc::clone(&spans),
        events: Arc::clone(&events),
    });

    // Thread-local rather than global, so these tests stay independent under the default
    // parallel test harness.
    let outcome = tracing::subscriber::with_default(subscriber, || {
        // Created *inside* `with_default`: a span built while no subscriber is installed is
        // disabled forever, and entering it would prove nothing about nesting.
        let enclosing = enclose.then(|| tracing::info_span!("caller.enclosing"));
        let _entered = enclosing.as_ref().map(tracing::Span::enter);
        Interpreter::new(Limits {
            allow_clock: true,
            ..limits
        })
        .with_curl_capability(Some("http-probe.fetch".to_owned()))
        .run(script, &Fixture::default())
    });

    let spans = spans.lock().expect("span lock").clone();
    let events = events.lock().expect("event lock").clone();
    Telemetry {
        outcome,
        spans,
        events,
    }
}

#[test]
fn every_command_produces_one_span_and_one_started_completed_pair() {
    let telemetry =
        capture("greet() { echo hi; }\ngreet\njq -n 1\necho.echo --message two\nnosuchcommand\n:");

    // One span per command word actually executed. `greet`'s body runs `echo`, so the function
    // call and the command inside it are both here — which is the point of instrumenting the one
    // seam every command passes through.
    assert_eq!(
        telemetry.started(),
        vec![
            ("function", WITHHELD),
            ("builtin", "echo"),
            ("builtin", "jq"),
            ("capability", "echo.echo"),
            ("not-found", WITHHELD),
            ("control", ":"),
        ]
    );

    let spans = telemetry
        .spans
        .iter()
        .filter(|span| span.span.as_deref() == Some("shell.command"))
        .count();
    let completed = telemetry
        .events
        .iter()
        .filter(|event| event.audit_event() == Some("shell.command.completed"))
        .count();
    assert_eq!(spans, 6);
    assert_eq!(completed, 6);
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
    assert_eq!(span.field("shell.command.exit_code"), Some("0"));
    assert_eq!(span.field("outcome"), Some("succeeded"));
}

#[test]
fn a_denied_capability_is_not_flattened_into_a_generic_failure() {
    // A refusal, a provider that ran and errored, and a capability that does not exist are three
    // different operational stories; `CapabilityCallResult` keeps them apart and so must this.
    for (script, outcome, exit_code) in [
        ("echo.echo --message hi", "succeeded", "0"),
        ("provider.broken", "failed", "1"),
        ("policy.denied", "denied", "126"),
        // The shared fixture answers every other identifier with success, so 127 is shown here by
        // an unresolved word; both paths land on the same code and the same label.
        ("nosuchcommand", "not-found", "127"),
    ] {
        let telemetry = capture(script);
        let completed = telemetry
            .events
            .iter()
            .find(|event| event.audit_event() == Some("shell.command.completed"))
            .unwrap_or_else(|| panic!("{script}: a completed event"));
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
        .events
        .iter()
        .find(|event| event.audit_event() == Some("shell.command.completed"))
        .expect("a completed event");

    assert_eq!(completed.field("shell.command.kind"), Some("rejected"));
    // The word comes from this crate's own refusal table, so naming it exports nothing the script
    // authored.
    assert_eq!(completed.field("shell.command.name"), Some("eval"));
    assert_eq!(completed.field("outcome"), Some("rejected"));
    assert_eq!(telemetry.outcome.exit_code.get(), 2);
}

#[test]
fn an_exhausted_budget_is_reported_as_a_limit_rather_than_a_failure() {
    // The capability ceiling is the one a *command* trips. The step budget is charged between
    // statements, so it ends the script without any command span ever seeing it.
    let telemetry = capture_with(
        "while true; do echo.echo --message x; done",
        Limits {
            max_capability_calls: 2,
            ..Limits::default()
        },
        false,
    );
    let completed = telemetry
        .events
        .iter()
        .rfind(|event| event.audit_event() == Some("shell.command.completed"))
        .expect("a completed event");
    assert_eq!(completed.field("outcome"), Some("limit-exceeded"));
}

#[test]
fn argument_values_never_reach_telemetry() {
    // The sentinels stand in for what argv really carries: a bearer token in a `curl -d` body, a
    // capability input object, a URL with a signed query string. Asserting their *absence* is the
    // test — asserting that the safe fields are present would pass just as happily while a
    // `shell.command.arguments` field sat beside them leaking every one of these.
    const SECRET: &str = "DEKOPON_SHELL_SECRET_DO_NOT_EXPORT";
    let script = format!(
        "curl -d '{{\"apiKey\":\"{SECRET}\"}}' https://example.test/{SECRET}\n\
         cap echo.echo '{{\"token\":\"{SECRET}\"}}'\n\
         echo.echo --message {SECRET}\n\
         echo {SECRET} | grep {SECRET}\n\
         jq -n '\"{SECRET}\"'\n\
         {SECRET}_command\n\
         helper_{SECRET}() {{ echo inner; }}\n\
         helper_{SECRET}\n\
         x={SECRET}\n\
         echo \"$x\"\n"
    );

    let telemetry = capture(&script);
    assert!(
        !telemetry.spans.is_empty() && !telemetry.events.is_empty(),
        "the capture harness recorded nothing, so absence here would prove nothing"
    );

    for value in telemetry.all_values() {
        assert!(
            !value.contains(SECRET),
            "a script value reached telemetry: {value:?}"
        );
    }

    // The script itself really did carry the sentinel, so the absence above is redaction rather
    // than a script that never mentioned it.
    assert!(telemetry.outcome.output.contains(SECRET));
}

#[test]
fn a_model_authored_command_word_is_withheld_but_its_kind_is_not() {
    // A shell function's name and an unresolved word are both whatever the script's author typed.
    // The runner already refuses to copy a model-selected invalid tool name into a rejection
    // event; a command word is the same class of text and gets the same treatment.
    let telemetry = capture("secret_helper() { echo hi; }\nsecret_helper\nsecret_typo");

    assert_eq!(
        telemetry.started(),
        vec![
            ("function", WITHHELD),
            ("builtin", "echo"),
            ("not-found", WITHHELD),
        ]
    );
    for value in telemetry.all_values() {
        assert!(!value.contains("secret_helper"), "{value:?}");
        assert!(!value.contains("secret_typo"), "{value:?}");
    }
}

#[test]
fn xargs_records_every_command_it_actually_drove() {
    // One script word that maps a command over three items really did run three commands, so a
    // trace that showed one would be describing a script nobody wrote.
    // The list is built through `jq` rather than `echo`, because `echo` produces one string and
    // `xargs` would then have a single element to map over, passing for the wrong reason.
    let telemetry = capture("echo.echo --a a --b b --c c | jq '[.a,.b,.c]' | xargs echo");

    let echoes = telemetry
        .started()
        .into_iter()
        .filter(|(kind, name)| *kind == "builtin" && *name == "echo")
        .count();
    assert_eq!(
        echoes, 3,
        "one `echo` per element, not one for the whole list"
    );

    // Each of those nests inside the `xargs` span rather than beside it, so the relationship
    // between the one script word and the commands it drove survives into the trace.
    let nested = telemetry
        .spans
        .iter()
        .filter(|span| {
            span.span.as_deref() == Some("shell.command")
                && span.field("shell.command.name") == Some("echo")
                && span.parents.iter().any(|parent| parent == "shell.command")
        })
        .count();
    assert_eq!(nested, 3);
}

#[test]
fn command_spans_nest_under_the_callers_active_span() {
    // `dekopon-run` enters `prompt.script` (or `runner.shell`) and calls straight into the
    // interpreter on the same thread, so nesting should need no propagation code at all. This
    // pins that; `crates/dekopon-run/src/prompt.rs` pins it again across a real `spawn_blocking`.
    let telemetry = capture_with("echo hi", Limits::default(), true);

    let span = telemetry
        .spans
        .iter()
        .find(|span| span.span.as_deref() == Some("shell.command"))
        .expect("a command span");
    assert_eq!(span.parents, vec!["caller.enclosing".to_owned()]);
}

#[test]
fn control_words_and_their_dispatcher_agree() {
    // `run_argv` classifies a control word from `CONTROL_WORDS` and only then lets
    // `run_control_word` execute it, so a word dropped from the list stops running and says
    // "command not found" instead. That is the direction this covers.
    //
    // The reverse — an arm added to `run_control_word` but not to the list — is caught by
    // construction rather than here: such a word reaches `dispatch::resolve`, and no control word
    // is in the builtin registry or the rejection table, so it also lands on "command not found"
    // rather than silently doing something else. Both directions fail closed and loudly.
    for word in CONTROL_WORDS {
        let outcome = capture(word).outcome;
        assert!(
            !outcome.output.contains("command not found"),
            "{word}: {}",
            outcome.output
        );
    }

    // The assertion above is only meaningful because an unknown word really does say this.
    assert!(
        capture("definitelynotacontrolword")
            .outcome
            .output
            .contains("command not found")
    );
}
