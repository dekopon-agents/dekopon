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

use super::{CONTROL_WORDS, MAX_TRACED_COMMANDS, SCRIPT_SPAN, WITHHELD};

/// Serializes the tests whose expectations depend on the process-global payload switch.
///
/// `dekopon_core::telemetry_payloads` is one `AtomicBool` for the whole process, so a test that
/// enables it would otherwise change what a concurrently running redaction test observes.
static PAYLOADS: Mutex<()> = Mutex::new(());

/// Takes the payload-switch lock, ignoring poisoning.
///
/// A test that fails while holding it has already reported its own failure; letting the poison
/// cascade would turn one real failure into twelve misleading ones.
fn serialized() -> std::sync::MutexGuard<'static, ()> {
    PAYLOADS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

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

    /// Every `shell.command` span, as `(kind, name)` pairs.
    ///
    /// Spans are captured on close, so these are in completion order: a command nested inside a
    /// shell function closes before the function does.
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
fn every_command_produces_exactly_one_span() {
    let _serialized = serialized();
    let telemetry =
        capture("greet() { echo hi; }\ngreet\njq -n 1\necho.echo --message two\nnosuchcommand\n:");

    // One span per command word actually executed. `greet`'s body runs `echo`, so the function
    // call and the command inside it are both here — which is the point of instrumenting the one
    // seam every command passes through.
    assert_eq!(
        telemetry.commands(),
        vec![
            // `echo` runs inside `greet`, so it closes first. Spans are captured on close, which
            // makes this completion order rather than start order.
            ("builtin", "echo"),
            ("function", WITHHELD),
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
    assert_eq!(spans, 6);
}

#[test]
fn a_span_carries_the_outcome_exit_code_and_argument_count() {
    let _serialized = serialized();
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
    let _serialized = serialized();
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
            .spans
            .iter()
            .find(|span| span.span.as_deref() == Some("shell.command"))
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
    let _serialized = serialized();
    let telemetry = capture("eval 'echo hi'");
    let completed = telemetry
        .spans
        .iter()
        .find(|span| span.span.as_deref() == Some("shell.command"))
        .expect("a shell.command span");

    assert_eq!(completed.field("shell.command.kind"), Some("rejected"));
    // The word comes from this crate's own refusal table, so naming it exports nothing the script
    // authored.
    assert_eq!(completed.field("shell.command.name"), Some("eval"));
    assert_eq!(completed.field("outcome"), Some("rejected"));
    assert_eq!(telemetry.outcome.exit_code.get(), 2);
}

#[test]
fn an_exhausted_budget_is_reported_as_a_limit_rather_than_a_failure() {
    let _serialized = serialized();
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
        .spans
        .iter()
        .rfind(|span| span.span.as_deref() == Some("shell.command"))
        .expect("a shell.command span");
    assert_eq!(completed.field("outcome"), Some("limit-exceeded"));
}

#[test]
fn argument_values_never_reach_telemetry() {
    let _serialized = serialized();
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
        !telemetry.spans.is_empty(),
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
    let _serialized = serialized();
    // A shell function's name and an unresolved word are both whatever the script's author typed.
    // The runner already refuses to copy a model-selected invalid tool name into a rejection
    // event; a command word is the same class of text and gets the same treatment.
    let telemetry = capture("secret_helper() { echo hi; }\nsecret_helper\nsecret_typo");

    assert_eq!(
        telemetry.commands(),
        vec![
            // Completion order: the body of `greet` closes before `greet` itself.
            ("builtin", "echo"),
            ("function", WITHHELD),
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
    let _serialized = serialized();
    // One script word that maps a command over three items really did run three commands, so a
    // trace that showed one would be describing a script nobody wrote.
    // The list is built through `jq` rather than `echo`, because `echo` produces one string and
    // `xargs` would then have a single element to map over, passing for the wrong reason.
    let telemetry = capture("echo.echo --a a --b b --c c | jq '[.a,.b,.c]' | xargs echo");

    let echoes = telemetry
        .commands()
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
    let _serialized = serialized();
    // `dekopon-run` enters `prompt.script` (or `runner.shell`) and calls straight into the
    // interpreter on the same thread, so nesting should need no propagation code at all. This
    // pins that; `crates/dekopon-run/src/prompt.rs` pins it again across a real `spawn_blocking`.
    let telemetry = capture_with("echo hi", Limits::default(), true);

    let span = telemetry
        .spans
        .iter()
        .find(|span| span.span.as_deref() == Some("shell.command"))
        .expect("a command span");
    // The script's own span sits between the command and the caller's: the totals need a home that
    // costs the same whatever the script did, and the caller's span is not this crate's to write to.
    assert_eq!(
        span.parents,
        vec![SCRIPT_SPAN.to_owned(), "caller.enclosing".to_owned()]
    );
}

#[test]
fn one_script_span_carries_the_totals_for_the_whole_run() {
    let _serialized = serialized();
    let telemetry = capture("greet() { echo hi; }\ngreet\nnosuchcommand\necho.echo --message two");

    let script = telemetry
        .spans
        .iter()
        .find(|span| span.span.as_deref() == Some(SCRIPT_SPAN))
        .expect("one script span");
    // `greet`, the `echo` inside it, `nosuchcommand`, and the capability call.
    assert_eq!(script.field("shell.script.commands"), Some("4"));
    assert_eq!(script.field("shell.script.commands_traced"), Some("4"));
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
fn a_loop_heavy_script_stops_exporting_a_span_per_command() {
    let _serialized = serialized();
    // A model-authored `while` loop is bounded only by the step budget, so a span per command word
    // is tens of thousands of exported spans from a single tool call. Past the cap the spans drop
    // to DEBUG — the capture layer here is level-agnostic, so what it proves is the accounting: the
    // script span still reports every command, and only the first `MAX_TRACED_COMMANDS` are traced.
    let commands = MAX_TRACED_COMMANDS + 40;
    let telemetry = capture(&format!(
        "i=0\nwhile [ $i -lt {commands} ]; do echo x; i=$(( i + 1 )); done"
    ));

    let script = telemetry
        .spans
        .iter()
        .find(|span| span.span.as_deref() == Some(SCRIPT_SPAN))
        .expect("one script span");
    assert_eq!(
        script.field("shell.script.commands_traced"),
        Some(MAX_TRACED_COMMANDS.to_string().as_str())
    );
    let total = script
        .field("shell.script.commands")
        .and_then(|value| value.parse::<u64>().ok())
        .expect("a command total");
    assert!(
        total > MAX_TRACED_COMMANDS,
        "the loop must outrun the cap for this to prove anything: {total}"
    );
    // The totals survive the cap; the per-command detail past it does not have to. The one failure
    // is the `[` that finally reports false and ends the loop, counted like any other non-zero
    // status — the counters describe the whole run, including the part with no spans left.
    assert_eq!(script.field("shell.script.failed_commands"), Some("1"));
}

#[test]
fn control_words_and_their_dispatcher_agree() {
    let _serialized = serialized();
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

/// A word the session was not granted, in a namespace it holds, is a different fact from a typo.
///
/// The namespace comes from the session's own granted set, so exporting it reveals nothing the
/// deployment did not already choose. The word stays withheld: everything after the namespace is
/// whatever the script typed.
#[test]
fn an_ungranted_word_in_a_granted_namespace_reports_its_namespace() {
    let _serialized = serialized();
    let telemetry = capture("echo.nonexistent\nnosuch.capability");

    assert_eq!(
        telemetry.commands(),
        vec![("not-granted", WITHHELD), ("not-found", WITHHELD)]
    );

    let namespaces = telemetry
        .spans
        .iter()
        .filter(|span| span.span.as_deref() == Some("shell.command"))
        .map(|span| span.field("capability.namespace"))
        .collect::<Vec<_>>();
    assert_eq!(
        namespaces,
        vec![Some("echo"), None],
        "only a word inside a granted namespace carries one"
    );

    for value in telemetry.all_values() {
        assert!(!value.contains("nonexistent"), "{value:?}");
        assert!(!value.contains("nosuch"), "{value:?}");
    }
}

/// The script must not be able to tell the two apart.
///
/// A model that could distinguish "no such command" from "you were not granted that" would have an
/// oracle for enumerating the deployment's capabilities one guess at a time.
#[test]
fn a_script_cannot_distinguish_ungranted_from_unknown() {
    let _serialized = serialized();
    let ungranted =
        Interpreter::new(Limits::default()).run("echo.nonexistent", &Fixture::default());
    let unknown = Interpreter::new(Limits::default()).run("nosuch.capability", &Fixture::default());
    assert_eq!(
        ungranted.output.replace("echo.nonexistent", "WORD"),
        unknown.output.replace("nosuch.capability", "WORD")
    );
    assert_eq!(ungranted.exit_code, unknown.exit_code);
}

/// The exact word is available, but only where an operator has accepted retention for data the
/// model influences — the same switch that already governs provider payloads and HTTP queries.
#[test]
fn enabling_payloads_exports_the_missed_word() {
    let _serialized = serialized();
    dekopon_core::set_telemetry_payloads(true);
    let telemetry = capture("echo.nonexistent");
    dekopon_core::set_telemetry_payloads(false);

    assert_eq!(
        telemetry.commands(),
        vec![("not-granted", "echo.nonexistent")]
    );
}
