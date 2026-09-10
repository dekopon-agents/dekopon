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
//! # What is recorded
//!
//! Every command word, whoever wrote it: a builtin name, a control word, a capability identifier, a
//! shell function the model declared, and a word that resolved to nothing. The operator's trace is
//! the record of what an agent did, and a word replaced by a placeholder makes the run
//! unreconstructible for the reader the trace exists for. Alongside it go the resolution kind, the
//! argument *count*, the duration, the exit code, and a stable outcome label.
//!
//! Argument *values* are the one exclusion, and it is goal 1's rather than this module's:
//! `curl -d '{"apiKey":...}'` puts a credential in argv, and secret bytes are the material no part
//! of this workspace exports. That exclusion is unconditional and has no switch.
//!
//! # How much is recorded
//!
//! One span per command word, always. A model-authored `while` loop bounded by
//! [`crate::limits::DEFAULT_MAX_STEPS`] can run tens of thousands of them, and every one is
//! exported: a span is never dropped, and a trace that thins out partway through is a trace that
//! answers "what happened" with "up to here". Nothing is special-cased by construct — a loop body
//! and an `xargs` sub-invocation are ordinary command words here.
//!
//! Each script run also opens one [`SCRIPT_SPAN`] carrying [`ScriptCounters`]' totals, which cost
//! the same whether a script ran three commands or thirty thousand and give a reader the shape of
//! the run before they walk it.

use crate::{ExitCode, builtins::FatalError, dispatch::Resolution, limits::LimitExceeded};

/// The span one whole script run opens, and the home of its totals.
pub(crate) const SCRIPT_SPAN: &str = "shell.script";

/// Per-script command totals.
///
/// Whatever a script did, these describe its whole run in three bounded integers, recorded on the
/// [`SCRIPT_SPAN`] when it closes — the shape of the run, ahead of the per-command spans that give
/// it in full.
#[derive(Debug, Default)]
pub(crate) struct ScriptCounters {
    commands: u64,
    capability_commands: u64,
    failed_commands: u64,
}

impl ScriptCounters {
    /// Charges one command word.
    pub(crate) fn charge(&mut self, kind: CommandKind) {
        self.commands = self.commands.saturating_add(1);
        if matches!(kind, CommandKind::Capability | CommandKind::ProviderCommand) {
            self.capability_commands = self.capability_commands.saturating_add(1);
        }
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
        span.record("shell.script.capability_commands", self.capability_commands);
        span.record("shell.script.failed_commands", self.failed_commands);
    }
}

/// Opens the span for one command word.
pub(crate) fn command_span(name: &str, kind: CommandKind, argument_count: usize) -> tracing::Span {
    tracing::info_span!(
        "shell.command",
        shell.command.name = name,
        shell.command.kind = kind.label(),
        shell.command.argument_count = argument_count,
        shell.command.exit_code = tracing::field::Empty,
        outcome = tracing::field::Empty,
    )
}

/// Opens the span covering one whole script run.
pub(crate) fn script_span() -> tracing::Span {
    tracing::info_span!(
        SCRIPT_SPAN,
        shell.script.commands = tracing::field::Empty,
        shell.script.capability_commands = tracing::field::Empty,
        shell.script.failed_commands = tracing::field::Empty,
    )
}

/// Command words [`super::Evaluator::run_control_word`] executes itself, before dispatch.
///
/// Classification has to happen *before* the word runs, so that the span and its opening event
/// carry the kind from the start; `run_control_word` cannot be asked, because it executes as it
/// matches. This list is that question's answer, and
/// [`super::tests::control_words_and_their_dispatcher_agree`] pins the two together.
pub(crate) const CONTROL_WORDS: &[&str] = &[
    "break", "continue", "exit", "local", "read", "return", "set", "shift", "unset", ":",
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
            Resolution::NotGranted => Self::NotGranted,
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
