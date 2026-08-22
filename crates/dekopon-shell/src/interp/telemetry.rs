//! Per-command `tracing` vocabulary, and the redaction rule it follows.
//!
//! # Why this lives at one seam
//!
//! Every command word a script runs passes through [`super::Evaluator::run_argv`], after
//! [`crate::dispatch::resolve`] has decided what it is. One span there therefore covers every
//! builtin, capability call, shell function, refused word, and unknown word — including builtins
//! that do not exist yet. Instrumenting the individual builtin implementations instead would leave
//! each newly added one silently untraced, and would put the same twenty-line preamble in twenty
//! files.
//!
//! # What is recorded, and what is withheld
//!
//! This crate knows nothing about where its spans go: it emits plain `tracing` spans and events,
//! and the embedding binary's subscriber decides whether they reach a terminal, a file, or a
//! remote collector. It must therefore assume they leave the process, and record only what is safe
//! to export:
//!
//! - the resolution kind, the argument *count*, the duration, the exit code, and a stable outcome
//!   label — all bounded, low-cardinality, and derived rather than copied from the script;
//! - the command word itself, but **only when it came from a fixed vocabulary this crate owns**: a
//!   builtin name, a control word, a word the shell refuses by name, or a capability identifier.
//!
//! Argument *values* are never recorded in any form. `curl -d '{"apiKey":...}'` and
//! `cap some.id '{"token":...}'` put secrets in argv exactly the way capability input does, and
//! capability input is already excluded from this workspace's telemetry. For the same reason a
//! model-authored command word — a shell function's name, or a word that resolved to nothing — is
//! reported as [`WITHHELD`] rather than copied, mirroring the runner's existing refusal to copy a
//! model-selected invalid tool name into a rejection event.
//!
//! # How much is recorded
//!
//! One span per command word is the right reading for a script a person wrote and an unaffordable
//! one for a script a model wrote: a `while` loop is bounded only by
//! [`crate::limits::DEFAULT_MAX_STEPS`], so one tool call can execute tens of thousands of command
//! words, and exporting a span for each is tens of megabytes over OTLP from a workload whose whole
//! point was to be one round trip.
//!
//! So the volume is capped rather than the detail. Each script run opens one [`SCRIPT_SPAN`]
//! carrying [`ScriptCounters`]' totals, which cost the same whether a script ran three commands or
//! thirty thousand. Inside it, the first [`MAX_TRACED_COMMANDS`] command words get their span at
//! INFO and the rest at DEBUG — still emitted, still complete, and off by default at the level
//! production runs at. Nothing is special-cased by construct: a loop body and an `xargs`
//! sub-invocation are ordinary command words here, and the cap treats them as such.

use crate::{ExitCode, builtins::FatalError, dispatch::Resolution, limits::LimitExceeded};

/// The span one whole script run opens, and the home of its totals.
pub(crate) const SCRIPT_SPAN: &str = "shell.script";

/// How many `shell.command` spans one script run emits at INFO before the rest drop to DEBUG.
///
/// Large enough that every script a person would read in a trace is complete, small enough that a
/// runaway loop costs a bounded number of exported spans instead of one per step.
pub(crate) const MAX_TRACED_COMMANDS: u64 = 256;

/// The level one `shell.command` span is emitted at.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SpanLevel {
    /// Within the per-script cap: exported wherever INFO goes.
    Info,
    /// Past the cap: emitted, but off at the level production runs at.
    Debug,
}

/// Per-script command totals, and the span cap they enforce.
///
/// These are what survives the cap: whatever a script did, the counters describe its whole run in
/// four bounded integers, recorded on the [`SCRIPT_SPAN`] when it closes.
#[derive(Debug, Default)]
pub(crate) struct ScriptCounters {
    commands: u64,
    traced: u64,
    capability_commands: u64,
    failed_commands: u64,
}

impl ScriptCounters {
    /// Charges one command word, returning the level its span belongs at.
    pub(crate) fn charge(&mut self, kind: CommandKind) -> SpanLevel {
        self.commands = self.commands.saturating_add(1);
        if matches!(kind, CommandKind::Capability | CommandKind::ProviderCommand) {
            self.capability_commands = self.capability_commands.saturating_add(1);
        }
        if self.traced >= MAX_TRACED_COMMANDS {
            return SpanLevel::Debug;
        }
        self.traced = self.traced.saturating_add(1);
        SpanLevel::Info
    }

    /// Records the status one command reported.
    pub(crate) fn record_status(&mut self, status: ExitCode) {
        if status != ExitCode::SUCCESS {
            self.failed_commands = self.failed_commands.saturating_add(1);
        }
    }

    /// Writes the totals onto the enclosing script span.
    pub(crate) fn record_on(&self, span: &tracing::Span) {
        span.record("shell.script.commands", self.commands);
        span.record("shell.script.commands_traced", self.traced);
        span.record("shell.script.capability_commands", self.capability_commands);
        span.record("shell.script.failed_commands", self.failed_commands);
    }
}

/// Opens the span for one command word at the level the per-script cap allows.
///
/// The two arms are written out rather than parameterized because `tracing`'s level is part of a
/// span's static callsite metadata, which is exactly what makes a filtered-out DEBUG span nearly
/// free: the subscriber's interest is cached per callsite, so a capped command word costs an atomic
/// load rather than a formatted span.
pub(crate) fn command_span(
    level: SpanLevel,
    name: &str,
    kind: CommandKind,
    argument_count: usize,
) -> tracing::Span {
    match level {
        SpanLevel::Info => tracing::info_span!(
            "shell.command",
            shell.command.name = name,
            shell.command.kind = kind.label(),
            shell.command.argument_count = argument_count,
            shell.command.exit_code = tracing::field::Empty,
            capability.namespace = tracing::field::Empty,
            outcome = tracing::field::Empty,
        ),
        SpanLevel::Debug => tracing::debug_span!(
            "shell.command",
            shell.command.name = name,
            shell.command.kind = kind.label(),
            shell.command.argument_count = argument_count,
            shell.command.exit_code = tracing::field::Empty,
            capability.namespace = tracing::field::Empty,
            outcome = tracing::field::Empty,
        ),
    }
}

/// Opens the span covering one whole script run.
pub(crate) fn script_span() -> tracing::Span {
    tracing::info_span!(
        SCRIPT_SPAN,
        shell.script.commands = tracing::field::Empty,
        shell.script.commands_traced = tracing::field::Empty,
        shell.script.capability_commands = tracing::field::Empty,
        shell.script.failed_commands = tracing::field::Empty,
    )
}

/// Stands in for a command word this crate declines to copy into telemetry.
///
/// A fixed placeholder rather than an absent field: "this word was model-authored" is itself worth
/// knowing, and a missing field would be indistinguishable from instrumentation that never ran.
pub(crate) const WITHHELD: &str = "<withheld>";

/// Command words [`super::Evaluator::run_control_word`] executes itself, before dispatch.
///
/// Classification has to happen *before* the word runs, so that the span and its opening event
/// carry the kind from the start; `run_control_word` cannot be asked, because it executes as it
/// matches. This list is that question's answer, and
/// [`super::tests::control_words_and_their_dispatcher_agree`] pins the two together.
pub(crate) const CONTROL_WORDS: &[&str] = &[
    "break", "continue", "return", "exit", "local", "set", "shift", "unset", ":",
];

/// Reports whether the evaluator owns this command word rather than the dispatch table.
pub(crate) fn is_control_word(word: &str) -> bool {
    CONTROL_WORDS.contains(&word)
}

/// How one command word resolved, as a stable telemetry label.
///
/// This mirrors [`Resolution`] rather than reusing it: `Resolution` carries a `&'static dyn
/// Builtin` and a rejection reason that telemetry has no business holding, and it gains variants
/// for dispatch reasons, not for reporting ones.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CommandKind {
    /// A control word the evaluator executes itself, such as `break` or `local`.
    Control,
    /// A shell function declared earlier in the same script.
    Function,
    /// A builtin from the fixed registry.
    Builtin,
    /// A granted capability, dispatched through the invoker seam.
    Capability,
    /// A command word a loaded provider contributed.
    ProviderCommand,
    /// A word this shell refuses by name, such as `eval`.
    Rejected,
    /// Nothing matched; the script sees exit code 127.
    NotFound,
    /// A capability the session did not get, in a namespace it did; the script sees 127 too.
    NotGranted,
}

impl CommandKind {
    /// Classifies one already-computed resolution.
    pub(crate) const fn of(resolution: &Resolution) -> Self {
        match resolution {
            Resolution::Function => Self::Function,
            Resolution::Builtin(_) => Self::Builtin,
            Resolution::Capability => Self::Capability,
            Resolution::ProviderCommand => Self::ProviderCommand,
            Resolution::NotGranted { .. } => Self::NotGranted,
            Resolution::Rejected(_) => Self::Rejected,
            Resolution::NotFound => Self::NotFound,
        }
    }

    /// Returns the stable label recorded in `shell.command.kind`.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Function => "function",
            Self::Builtin => "builtin",
            Self::Capability => "capability",
            Self::ProviderCommand => "provider-command",
            Self::Rejected => "rejected",
            Self::NotFound => "not-found",
            Self::NotGranted => "not-granted",
        }
    }

    /// Reports whether a word of this kind came from a fixed vocabulary rather than the script.
    ///
    /// A builtin, control, or rejected word was matched against a table compiled into this crate,
    /// so recording it can only ever emit one of that table's own entries. A capability identifier
    /// is already this workspace's canonical `capability.id` telemetry field. A function name and
    /// an unresolved word are neither: they are whatever the script's author typed.
    pub(crate) const fn name_is_fixed_vocabulary(self) -> bool {
        match self {
            // A provider command word came from a loaded manifest, so it is as much fixed
            // vocabulary as a builtin name: the deployment chose it, not the script.
            Self::Control
            | Self::Builtin
            | Self::Rejected
            | Self::Capability
            | Self::ProviderCommand => true,
            // `NotGranted` is the interesting one. Its *namespace* comes from the session's granted
            // set and is exported; the word itself is still whatever the script typed, so it is
            // still withheld. Knowing the model reached into a provider and missed is the trend worth
            // having, and it costs no channel to record.
            Self::Function | Self::NotFound | Self::NotGranted => false,
        }
    }
}

/// Returns the command word when exporting it is safe, and [`WITHHELD`] when it is not.
pub(crate) fn traceable_name(kind: CommandKind, command: &str) -> &str {
    if kind.name_is_fixed_vocabulary() || dekopon_core::telemetry_payloads() {
        command
    } else {
        WITHHELD
    }
}

/// Maps one command's exit code onto a stable outcome label.
///
/// A denial, a missing capability, and a generic failure stay distinct here for the same reason
/// [`crate::CapabilityCallResult`] keeps them distinct: an authorization refusal is materially
/// different telemetry from a capability that ran and errored, and flattening the two hides the
/// refusal behind the noise of ordinary failures.
pub(crate) fn outcome_label(status: ExitCode) -> &'static str {
    match status {
        ExitCode::SUCCESS => "succeeded",
        ExitCode::SYNTAX => "usage-error",
        ExitCode::TIMEOUT => "timed-out",
        ExitCode::DENIED => "denied",
        ExitCode::NOT_FOUND => "not-found",
        _ => "failed",
    }
}

/// Returns the exit code a fatal error will make the whole script report.
///
/// [`super::Evaluator::report_fatal`] renders its message from the same match, so a command's
/// recorded exit code cannot drift from the one the script actually exits with.
pub(crate) fn fatal_exit_code(fatal: &FatalError) -> ExitCode {
    match fatal {
        FatalError::Limit(LimitExceeded::Deadline { .. }) => ExitCode::TIMEOUT,
        FatalError::Limit(_) | FatalError::Unsupported(_) => ExitCode::SYNTAX,
        // Matches bash, which exits 1 when `${NAME:?}` fires in a script.
        FatalError::Assertion(_) => ExitCode::FAILURE,
    }
}

/// Maps a fatal error onto its outcome label.
///
/// These do not go through [`outcome_label`]: both a refused construct and an exhausted budget
/// exit with code 2, and "the script asked for `eval`" is a different operational story from "the
/// script ran out of steps".
pub(crate) fn fatal_outcome(fatal: &FatalError) -> &'static str {
    match fatal {
        FatalError::Limit(LimitExceeded::Deadline { .. }) => "timed-out",
        FatalError::Limit(_) => "limit-exceeded",
        FatalError::Unsupported(_) => "rejected",
        FatalError::Assertion(_) => "assertion-failed",
    }
}

#[cfg(test)]
mod tests;
