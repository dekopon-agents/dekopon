//! A secret referenced in traced command arguments appears only as its public reference name,
//! resolved solely by the broker, while literal secret text a script writes is recorded as written,
//! and a command span is never dropped no matter how long the script runs.

use std::{
    cell::RefCell,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde_json::Value;

static NEXT_PIPELINE: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static STAGES: RefCell<Vec<StageRecord>> = const { RefCell::new(Vec::new()) };
}

struct StageRecord {
    pipeline: u64,
    index: usize,
    started: Instant,
    elapsed: Duration,
    paused: bool,
    meter: Arc<StageMeter>,
    last_span: Option<tracing::Span>,
    compound_span: Option<tracing::Span>,
}

#[derive(Debug, Default)]
pub(crate) struct StageMeter {
    input: AtomicU64,
    output: AtomicU64,
    reader_gone: AtomicBool,
}

pub(crate) struct StageTrace;

impl StageTrace {
    pub(crate) fn enter(pipeline: u64, index: usize, meter: Arc<StageMeter>) -> Self {
        STAGES.with_borrow_mut(|stages| {
            stages.push(StageRecord {
                pipeline,
                index,
                started: Instant::now(),
                elapsed: Duration::ZERO,
                paused: false,
                meter,
                last_span: None,
                compound_span: None,
            });
        });
        Self
    }

    pub(crate) fn pause(&self) {
        STAGES.with_borrow_mut(|stages| {
            if let Some(stage) = stages.last_mut() {
                stage.elapsed += stage.started.elapsed();
                stage.paused = true;
            }
        });
    }

    pub(crate) fn resume(&self) {
        STAGES.with_borrow_mut(|stages| {
            if let Some(stage) = stages.last_mut() {
                stage.started = Instant::now();
                stage.paused = false;
            }
        });
    }
}

impl Drop for StageTrace {
    fn drop(&mut self) {
        STAGES.with_borrow_mut(|stages| {
            let Some(record) = stages.pop() else { return };
            if let Some(span) = record.compound_span.as_ref().or(record.last_span.as_ref()) {
                record_on(
                    span,
                    &record,
                    record.elapsed
                        + if record.paused {
                            Duration::ZERO
                        } else {
                            record.started.elapsed()
                        },
                );
            }
        });
    }
}

pub(crate) fn next_pipeline() -> u64 {
    NEXT_PIPELINE.fetch_add(1, Ordering::Relaxed)
}

fn record_on(span: &tracing::Span, stage: &StageRecord, duration: Duration) {
    span.record("shell.command.pipeline_id", stage.pipeline);
    span.record("shell.command.stage_index", stage.index);
    span.record(
        "shell.command.input.bytes",
        stage.meter.input.load(Ordering::Relaxed),
    );
    span.record(
        "shell.command.stdout.bytes",
        stage.meter.output.load(Ordering::Relaxed),
    );
    span.record(
        "shell.command.duration_ns",
        duration.as_nanos().min(u64::MAX as u128) as u64,
    );
    span.record(
        "shell.command.close_reason",
        if stage.meter.reader_gone.load(Ordering::Relaxed) {
            "reader_gone"
        } else {
            "end"
        },
    );
}

pub(crate) fn record_compound_stage(span: &tracing::Span) {
    STAGES.with_borrow_mut(|stages| {
        if let Some(stage) = stages.last_mut() {
            stage.compound_span = Some(span.clone());
        }
    });
}

pub(crate) fn record_stage_command(span: &tracing::Span) {
    STAGES.with_borrow_mut(|stages| {
        if let Some(record) = stages.last_mut() {
            record_on(span, record, record.elapsed + record.started.elapsed());
            if record.compound_span.is_none() {
                record.last_span = Some(span.clone());
            }
        }
    });
}

pub(crate) fn stage_read(bytes: usize, owner: Option<&Arc<StageMeter>>) {
    if let Some(owner) = owner {
        owner.input.fetch_add(bytes as u64, Ordering::Relaxed);
    }
    STAGES.with_borrow(|stages| {
        if let Some(stage) = stages.last()
            && owner.is_none_or(|owner| !Arc::ptr_eq(owner, &stage.meter))
        {
            stage.meter.input.fetch_add(bytes as u64, Ordering::Relaxed);
        }
    });
}

pub(crate) fn stage_terminal_write(bytes: usize, terminal: bool) {
    stage_write(bytes * usize::from(terminal), true, None);
}

pub(crate) fn stage_write(bytes: usize, accepted: bool, owner: Option<&Arc<StageMeter>>) {
    let record = |meter: &StageMeter| {
        if accepted {
            meter.output.fetch_add(bytes as u64, Ordering::Relaxed);
        } else {
            meter.reader_gone.store(true, Ordering::Relaxed);
        }
    };
    if let Some(owner) = owner {
        record(owner);
    }
    STAGES.with_borrow(|stages| {
        if owner.is_none() {
            for stage in stages {
                record(&stage.meter);
            }
        } else if let Some(stage) = stages.last()
            && owner.is_some_and(|owner| !Arc::ptr_eq(owner, &stage.meter))
        {
            record(&stage.meter);
        }
    });
}

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
        shell.command.pipeline_id = tracing::field::Empty,
        shell.command.stage_index = tracing::field::Empty,
        shell.command.input.bytes = tracing::field::Empty,
        shell.command.stdout.bytes = tracing::field::Empty,
        shell.command.duration_ns = tracing::field::Empty,
        shell.command.close_reason = tracing::field::Empty,
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
        outcome = tracing::field::Empty,
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
    Compound,
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
            Self::Compound => "compound",
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
        FatalError::Limit(LimitExceeded::Cancelled) => ExitCode::CANCELLED,
        FatalError::Limit(LimitExceeded::Steps { .. })
        | FatalError::Limit(LimitExceeded::RecursionDepth { .. })
        | FatalError::Limit(LimitExceeded::CapabilityCalls { .. })
        | FatalError::Limit(LimitExceeded::ValueBytes { .. })
        | FatalError::Unsupported(_) => ExitCode::SYNTAX,
        FatalError::Assertion(_) => ExitCode::FAILURE,
    }
}

pub(crate) fn fatal_outcome(fatal: &FatalError) -> &'static str {
    match fatal {
        FatalError::Limit(LimitExceeded::Deadline { .. }) => "timed-out",
        FatalError::Limit(LimitExceeded::Cancelled) => "cancelled",
        FatalError::Limit(LimitExceeded::Steps { .. })
        | FatalError::Limit(LimitExceeded::RecursionDepth { .. })
        | FatalError::Limit(LimitExceeded::CapabilityCalls { .. })
        | FatalError::Limit(LimitExceeded::ValueBytes { .. }) => "limit-exceeded",
        FatalError::Unsupported(_) => "rejected",
        FatalError::Assertion(_) => "assertion-failed",
    }
}

#[cfg(test)]
mod tests;
