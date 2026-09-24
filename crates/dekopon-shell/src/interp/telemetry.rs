//! A secret referenced in traced command arguments appears only as its public reference name,
//! resolved solely by the broker, while literal secret text a script writes is recorded as written,
//! and a command span is never dropped no matter how long the script runs.

use serde_json::Value;

use crate::{
    ExitCode, builtins::FatalError, dispatch::Resolution, limits::LimitExceeded, value::display,
};

pub(crate) const SCRIPT_SPAN: &str = "shell.script";

#[derive(Debug, Default)]
pub(crate) struct ScriptCounters {
    commands: u64,
    capability_commands: u64,
    failed_commands: u64,
}

impl ScriptCounters {
    pub(crate) fn charge(&mut self, kind: CommandKind) {
        self.commands = self.commands.saturating_add(1);
        if kind == CommandKind::ProviderCommand {
            self.capability_commands = self.capability_commands.saturating_add(1);
        }
    }

    pub(crate) fn record_status(&mut self, status: ExitCode) {
        if status != ExitCode::SUCCESS {
            self.failed_commands = self.failed_commands.saturating_add(1);
        }
    }

    pub(crate) fn record_on(&self, span: &tracing::Span) {
        span.record("shell.script.commands", self.commands);
        span.record("shell.script.capability_commands", self.capability_commands);
        span.record("shell.script.failed_commands", self.failed_commands);
    }
}

pub(crate) fn command_span(name: &str, kind: CommandKind, argument_count: usize) -> tracing::Span {
    tracing::info_span!(
        "shell.command",
        shell.command.name = name,
        shell.command.kind = kind.label(),
        shell.command.argument_count = argument_count,
        shell.command.arguments = tracing::field::Empty,
        shell.command.arguments.bytes = tracing::field::Empty,
        shell.command.stdin = tracing::field::Empty,
        shell.command.stdin.bytes = tracing::field::Empty,
        shell.command.output = tracing::field::Empty,
        shell.command.output.bytes = tracing::field::Empty,
        shell.command.exit_code = tracing::field::Empty,
        outcome = tracing::field::Empty,
    )
}

pub(crate) fn record_arguments(span: &tracing::Span, arguments: &[String]) {
    let encoded = arguments
        .iter()
        .map(String::as_str)
        .collect::<Value>()
        .to_string();
    record_bounded(
        span,
        "shell.command.arguments",
        "shell.command.arguments.bytes",
        &encoded,
    );
}

pub(crate) fn record_stdin(span: &tracing::Span, piped: &Value) {
    record_bounded(
        span,
        "shell.command.stdin",
        "shell.command.stdin.bytes",
        &display(piped),
    );
}

pub(crate) fn record_output(span: &tracing::Span, output: &Value) {
    record_bounded(
        span,
        "shell.command.output",
        "shell.command.output.bytes",
        &display(output),
    );
}

fn record_bounded(span: &tracing::Span, field: &str, bytes_field: &str, text: &str) {
    span.record(field, &*dekopon_core::bounded_attribute(text));
    span.record(bytes_field, text.len());
}

pub(crate) fn script_span() -> tracing::Span {
    tracing::info_span!(
        SCRIPT_SPAN,
        shell.script.commands = tracing::field::Empty,
        shell.script.capability_commands = tracing::field::Empty,
        shell.script.failed_commands = tracing::field::Empty,
    )
}

pub(crate) const CONTROL_WORDS: &[&str] = &[
    "break", "continue", "exit", "local", "read", "return", "set", "shift", "unset", ":",
];

pub(crate) fn is_control_word(word: &str) -> bool {
    CONTROL_WORDS.contains(&word)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CommandKind {
    Control,
    Function,
    Builtin,
    ProviderCommand,
    Rejected,
    NotFound,
}

impl CommandKind {
    pub(crate) const fn of(resolution: &Resolution) -> Self {
        match resolution {
            Resolution::Function => Self::Function,
            Resolution::Builtin(_) => Self::Builtin,
            Resolution::ProviderCommand => Self::ProviderCommand,
            Resolution::Rejected(_) => Self::Rejected,
            Resolution::NotFound => Self::NotFound,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Function => "function",
            Self::Builtin => "builtin",
            Self::ProviderCommand => "provider-command",
            Self::Rejected => "rejected",
            Self::NotFound => "not-found",
        }
    }
}

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

pub(crate) fn fatal_exit_code(fatal: &FatalError) -> ExitCode {
    match fatal {
        FatalError::Limit(LimitExceeded::Deadline { .. }) => ExitCode::TIMEOUT,
        FatalError::Limit(_) | FatalError::Unsupported(_) => ExitCode::SYNTAX,
        FatalError::Assertion(_) => ExitCode::FAILURE,
    }
}

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
