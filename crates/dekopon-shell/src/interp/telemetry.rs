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

use std::time::Duration;

use crate::{ExitCode, builtins::FatalError, dispatch::Resolution, limits::LimitExceeded};

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
    "break", "continue", "return", "exit", "local", "shift", "unset", ":",
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
    /// A word this shell refuses by name, such as `eval`.
    Rejected,
    /// Nothing matched; the script sees exit code 127.
    NotFound,
}

impl CommandKind {
    /// Classifies one already-computed resolution.
    pub(crate) const fn of(resolution: &Resolution) -> Self {
        match resolution {
            Resolution::Function => Self::Function,
            Resolution::Builtin(_) => Self::Builtin,
            Resolution::Capability => Self::Capability,
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
            Self::Rejected => "rejected",
            Self::NotFound => "not-found",
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
            Self::Control | Self::Builtin | Self::Rejected | Self::Capability => true,
            Self::Function | Self::NotFound => false,
        }
    }
}

/// Returns the command word when exporting it is safe, and [`WITHHELD`] when it is not.
pub(crate) fn traceable_name(kind: CommandKind, command: &str) -> &str {
    if kind.name_is_fixed_vocabulary() {
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
    }
}

/// Renders a duration the way every other `duration_ms` field in this workspace is rendered.
pub(crate) fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests;
