use crate::interp::telemetry::StageObservation;
use dekopon_process::pipe::{self, PipeReader, PipeWriter, ReadOutcome, WriteOutcome};
use dekopon_process::{
    CallBudget, ExitCode, JOBS_OFF, LimitExceeded, Limits, ShellThreads, ThreadName, ThreadPermit,
    TreeContext,
};
mod case_pattern;
mod ifs;
use case_pattern::PatternChar;
mod nesting;
pub(crate) mod stage_out;
use ifs::{Action, Boundary, Ifs, Kind};
use stage_out::{OutputMode, StageOut, WriteTarget};

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    io::Write,
    os::{fd::OwnedFd, unix::net::UnixStream},
    sync::{
        Arc,
        atomic::{self, AtomicBool},
    },
    thread::{self, Scope, ScopedJoinHandle},
};

use serde_json::Value;

use crate::{
    CapabilityCallResult, CapabilityInvoker, JobSeed, ScriptOutcome,
    ast::{
        AndOr, AndOrList, ArithBinaryOp, ArithExpr, ArithUnaryOp, Background, CaseStatement,
        Command, Conditional, ConditionalTest, DEV_NULL, ForLoop, IfStatement, Index, Modifier,
        Parameter, Pattern, Pipeline, Program, Redirect, RedirectTarget, SimpleCommand, Statement,
        Stream, WhileLoop, Word, WordPart,
    },
    builtins::{
        self, Builtin, BuiltinContext, CommandFailure, CommandResult, ControlWord, FatalError,
        Resolution, xargs,
    },
    job::ScriptJobs,
    limits::{Budget, OutputBuffer},
    parser::{expanded_pattern, parse, pattern_metacharacter},
    value::{self, display},
};

use telemetry::CommandKind;

#[cfg(test)]
mod local_snapshot_tests;
pub(crate) mod telemetry;

const DEFAULT_IFS: &str = " \t\n";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Flow {
    Normal,
    Break(u32),
    Continue(u32),
    Return(ExitCode),
    Exit(ExitCode),
}

enum Executed {
    Result(CommandResult),
    Flow(Flow),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ShellOptions {
    errexit: bool,
    nounset: bool,
    pipefail: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Sink {
    Value,
    Diagnostics,
    Discard,
    Buffer { name: String, append: bool },
}

#[derive(Debug)]
enum StageInput {
    Inherited,
    Piped(PipeReader<StageObservation>),
}

enum BuiltinMode {
    Help,
    Run,
}

enum ParsedStream<'a> {
    Cat(&'a [String]),
    Base64(&'a [String]),
    Lines(builtins::text::lines::Selection),
    Text(builtins::text::stream::Operation),
    Extra(builtins::text::extra::Operation),
    Jq(builtins::jq::Options),
    Awk(builtins::awk::Options),
}

impl ParsedStream<'_> {
    fn run(
        self,
        input: builtins::awk::StageReader<'_>,
        budget: &mut Budget,
        emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
    ) -> Result<ExitCode, CommandFailure> {
        match self {
            Self::Awk(options) => {
                return builtins::awk::run_stream(options, input, budget, emit)
                    .map_err(CommandFailure::from);
            }
            Self::Cat(_) => unreachable!("named cat streams resolve each buffer separately"),
            Self::Base64(_) | Self::Lines(_) | Self::Text(_) | Self::Extra(_) | Self::Jq(_) => {}
        }
        let (mut reader, _charge) = input.open();
        match self {
            Self::Lines(selection) => {
                builtins::text::lines::run(selection, &mut reader, budget, emit)
                    .map(|()| ExitCode::SUCCESS)
            }
            Self::Text(operation) => {
                builtins::text::stream::run_parsed(operation, &mut reader, budget, emit)
            }
            Self::Extra(operation) => {
                builtins::text::extra::run_parsed(operation, &mut reader, budget, emit)
            }
            Self::Jq(options) => {
                builtins::jq::stream(options, &mut reader, budget, emit).map(|()| ExitCode::SUCCESS)
            }
            Self::Base64(arguments) => {
                builtins::encode::stream(arguments, &mut reader, budget, emit)
                    .map(|()| ExitCode::SUCCESS)
            }
            Self::Cat(_) | Self::Awk(_) => unreachable!("handled before opening inherited input"),
        }
    }
}

fn stage_input<'a>(
    stdin: &'a mut [PipeReader<StageObservation>],
    buffers: &BTreeMap<String, Vec<u8>>,
    budget: &Budget,
    command: &str,
    operand: Option<&str>,
    input: StageInput,
) -> Result<builtins::awk::StageReader<'a>, CommandFailure> {
    use builtins::awk::StageReader;
    if let Some(name) = operand.filter(|name| *name != "-") {
        check_buffer_operand(command, name)?;
        if name == DEV_NULL {
            return Ok(StageReader::Ready(PipeReader::default(), None));
        }
        let bytes = buffers.get(name).ok_or_else(|| CommandFailure::failed(format!(
            "{command}: {name}: no such buffer; buffers exist only after `> {name}` in this script"
        )))?;
        let charge = budget.charge_value_bytes(bytes.len() as u64)?;
        return Ok(StageReader::Ready(
            PipeReader::from_bytes(bytes.clone(), None, telemetry::stage_read),
            Some(charge),
        ));
    }
    Ok(match input {
        StageInput::Piped(reader) => StageReader::Ready(reader, None),
        StageInput::Inherited => StageReader::Inherited(stdin.last_mut()),
    })
}

impl Builtin {
    fn run(
        self,
        evaluator: &mut Evaluator<'_>,
        arguments: &[String],
        input: StageInput,
        mode: BuiltinMode,
        stdout_sink: &Sink,
    ) -> Result<Executed, FatalError> {
        match mode {
            BuiltinMode::Help => {
                return Ok(Executed::Result(builtins::help_result(
                    self.name(),
                    self.help(),
                )));
            }
            BuiltinMode::Run => {}
        };
        let parsed = match self {
            Self::Echo => {
                return evaluator.run_simple_builtin(builtins::misc::echo, arguments, input);
            }
            Self::Printf => {
                return evaluator.run_simple_builtin(builtins::misc::printf, arguments, input);
            }
            Self::Test => {
                return evaluator.run_simple_builtin(builtins::misc::test, arguments, input);
            }
            Self::TestBracket => {
                return evaluator.run_simple_builtin(
                    builtins::misc::test_bracket,
                    arguments,
                    input,
                );
            }
            Self::True => {
                return evaluator.run_simple_builtin(builtins::misc::succeed, arguments, input);
            }
            Self::False => {
                return evaluator.run_simple_builtin(builtins::misc::fail, arguments, input);
            }
            Self::Progress => {
                return evaluator.run_simple_builtin(builtins::misc::progress, arguments, input);
            }
            Self::Sleep => {
                return evaluator.run_simple_builtin(builtins::misc::sleep, arguments, input);
            }
            Self::Jobs => {
                return evaluator.run_simple_builtin(builtins::jobs::jobs, arguments, input);
            }
            Self::Wait => {
                return evaluator.run_simple_builtin(builtins::jobs::wait, arguments, input);
            }
            Self::Kill => {
                return evaluator.run_simple_builtin(builtins::jobs::kill, arguments, input);
            }
            Self::Cap => return evaluator.run_simple_builtin(builtins::cap::cap, arguments, input),
            Self::Head => builtins::text::lines::head(arguments)
                .map(|(operation, operand)| (ParsedStream::Lines(operation), operand)),
            Self::Tail => builtins::text::lines::tail(arguments)
                .map(|(operation, operand)| (ParsedStream::Lines(operation), operand)),
            Self::Grep => builtins::text::stream::grep(arguments)
                .map(|(operation, operand)| (ParsedStream::Text(operation), operand)),
            Self::Sed => builtins::text::stream::sed(arguments)
                .map(|(operation, operand)| (ParsedStream::Text(operation), operand)),
            Self::Cut => builtins::text::extra::cut(arguments)
                .map(|(operation, operand)| (ParsedStream::Extra(operation), operand)),
            Self::Uniq => builtins::text::extra::uniq(arguments)
                .map(|(operation, operand)| (ParsedStream::Extra(operation), operand)),
            Self::Wc => builtins::text::extra::wc(arguments)
                .map(|(operation, operand)| (ParsedStream::Extra(operation), operand)),
            Self::Sort => builtins::text::extra::sort(arguments)
                .map(|(operation, operand)| (ParsedStream::Extra(operation), operand)),
            Self::Jq => {
                builtins::jq::options(arguments).map(|options| (ParsedStream::Jq(options), None))
            }
            Self::Awk => builtins::awk::parse(arguments, &evaluator.budget)
                .map(|(options, operand)| (ParsedStream::Awk(options), operand))
                .map_err(CommandFailure::from),
            Self::Cat => Ok((ParsedStream::Cat(arguments), None)),
            Self::Base64 => Ok((ParsedStream::Base64(arguments), None)),
            Self::Xargs => return evaluator.run_xargs(arguments, input, stdout_sink),
        };
        evaluator.run_stream(self.name(), parsed, input, stdout_sink)
    }
}

/// Each non-final stage thread reserves this much stack, charged against retained bytes until its
/// join, so the default budget admits sixteen concurrent producers.
const STAGE_STACK_BYTES: usize = ShellThreads::STACK_BYTES;

const STAGE_THREAD: ThreadName = ThreadName::new("sh-stage");
const CALL_THREAD: ThreadName = ThreadName::new("sh-call");
const FEED_THREAD: ThreadName = ThreadName::new("sh-feed");

#[derive(Clone, Copy)]
struct StagePosition {
    pipeline: u64,
    index: usize,
}

enum Fed {
    Unpiped,
    Joined(Option<PipeReader<StageObservation>>),
    Unstarted(std::io::Error),
}

impl Fed {
    /// A feeder that never started dropped the provider's stdin, so the provider read EOF instead
    /// of the script's input; whatever it reported cannot stand as success.
    fn settle(
        self,
        capability: &str,
    ) -> Result<Option<PipeReader<StageObservation>>, CommandFailure> {
        match self {
            Self::Unpiped => Ok(None),
            Self::Joined(unread) => Ok(unread),
            Self::Unstarted(error) => Err(CommandFailure::failed(format!(
                "{capability}: failed: its stdin feeder could not start: {error}"
            ))),
        }
    }
}

struct StageSetup {
    position: StagePosition,
    meter: Arc<telemetry::StageMeter>,
    permit: ThreadPermit,
}

/// Ends when upstream ends, the provider stops reading or `finished` is set once the call has
/// returned; either way the dropped socket is the provider's end of input.
fn pump_stdin(
    reader: &mut PipeReader<StageObservation>,
    mut socket: UnixStream,
    budget: &crate::limits::Budget,
    finished: &AtomicBool,
) {
    if socket.set_write_timeout(Some(pipe::POLL)).is_err() {
        return;
    }
    while let Ok(ReadOutcome::Bytes(chunk)) = reader.read_unless(budget.tree(), finished) {
        let mut remaining = chunk.as_slice();
        while !remaining.is_empty() {
            if budget.tree().is_cancelled()
                || budget.check_deadline().is_err()
                || finished.load(atomic::Ordering::Relaxed)
            {
                return;
            }
            match socket.write(remaining) {
                Ok(0) => return,
                Ok(written) => remaining = &remaining[written..],
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return,
            }
        }
    }
}

#[cfg(test)]
mod pump_tests {
    use super::*;
    use dekopon_process::Limits;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn a_stalled_stdin_peer_releases_the_feeder_after_the_deadline() {
        let budget = crate::limits::Budget::start(Limits {
            timeout: Duration::from_millis(100),
            ..Limits::default()
        });
        let (socket, peer) = UnixStream::pair().unwrap();
        let (done, finished) = mpsc::channel();
        let [permit] = budget.tree().shell_threads().reserve().unwrap();
        std::thread::scope(|scope| {
            permit
                .spawn(scope, ThreadName::new("sh-feed-test"), || {
                    pump_stdin(
                        &mut PipeReader::from_bytes(
                            vec![b'x'; 1024 * 1024],
                            None,
                            crate::interp::telemetry::stage_read,
                        ),
                        socket,
                        &budget,
                        &AtomicBool::new(false),
                    );
                    done.send(()).unwrap();
                })
                .unwrap();
            let result = finished.recv_timeout(Duration::from_secs(3));
            drop(peer);
            assert!(
                result.is_ok(),
                "stalled peer kept the feeder past its deadline"
            );
        });
    }
}

fn stage_compound_span(command: &Command, participant: Option<&str>) -> Option<tracing::Span> {
    matches!(command, Command::Compound { .. })
        .then(|| telemetry::command_span("{ ...; }", CommandKind::Compound, 0, participant))
}

fn record_compound_outcome(span: Option<&tracing::Span>, executed: &Result<Executed, FatalError>) {
    let Some(span) = span else { return };
    let (status, outcome) = match executed {
        Ok(Executed::Result(result)) => (result.status, telemetry::outcome_label(result.status)),
        Ok(Executed::Flow(Flow::Exit(status) | Flow::Return(status))) => {
            (*status, telemetry::outcome_label(*status))
        }
        Ok(Executed::Flow(Flow::Normal | Flow::Break(_) | Flow::Continue(_))) => {
            (ExitCode::SUCCESS, "succeeded")
        }
        Err(error) => (
            telemetry::fatal_exit_code(error),
            telemetry::fatal_outcome(error),
        ),
    };
    span.record("shell.command.exit_code", status.get());
    span.record("outcome", outcome);
}

struct StageOutcome {
    status: Result<ExitCode, FatalError>,
    diagnostics: Vec<Diagnostic>,
    enclosing: Option<PipeReader<StageObservation>>,
}

enum Stage<'scope> {
    Running {
        handle: ScopedJoinHandle<'scope, StageOutcome>,
        _stack: dekopon_process::RetainedBytes,
    },
    Refused {
        message: String,
    },
}

enum StageFailure {
    Refused(String),
}

impl Stage<'_> {
    fn join(self) -> Result<StageOutcome, StageFailure> {
        match self {
            Self::Refused { message } => Err(StageFailure::Refused(message)),
            Self::Running { handle, _stack } => match handle.join() {
                Ok(outcome) => Ok(outcome),
                Err(panic) => std::panic::resume_unwind(panic),
            },
        }
    }
}

#[derive(Debug)]
enum Diagnostic {
    Line(String),
    Fragment(String),
}

impl Diagnostic {
    fn from_result(result: &CommandResult) -> Option<Self> {
        if result.value.is_null() {
            return None;
        }
        let text = display(&result.value);
        Some(if result.suppress_newline {
            Self::Fragment(text)
        } else {
            Self::Line(text)
        })
    }

    fn followed_by(self, next: Self) -> Self {
        let mut text = match self {
            Self::Line(mut text) => {
                text.push('\n');
                text
            }
            Self::Fragment(text) => text,
        };
        match next {
            Self::Line(next) => {
                text.push_str(&next);
                Self::Line(text)
            }
            Self::Fragment(next) => {
                text.push_str(&next);
                Self::Fragment(text)
            }
        }
    }

    fn into_result(self) -> CommandResult {
        match self {
            Self::Line(text) => CommandResult::value(Value::String(text)),
            Self::Fragment(text) => CommandResult::value(Value::String(text)).without_newline(),
        }
    }
}

/// A redirected stderr capture must stay bounded by the same output ceilings, since a function can
/// run thousands of commands into it.
#[derive(Debug)]
struct StderrCapture {
    lines: Vec<Diagnostic>,
    pending: String,
    bytes: usize,
    max_lines: usize,
    max_bytes: usize,
    truncated: bool,
}

impl StderrCapture {
    fn new(limits: &Limits) -> Self {
        Self {
            lines: Vec::new(),
            pending: String::new(),
            bytes: 0,
            max_lines: limits.max_output_lines,
            max_bytes: limits.max_output_bytes,
            truncated: false,
        }
    }

    fn push(&mut self, line: &str) {
        self.push_fragment(line);
        self.push_fragment("\n");
    }

    fn push_fragment(&mut self, text: &str) {
        for part in text.split_inclusive('\n') {
            if self.lines.len() >= self.max_lines {
                self.truncated = true;
                return;
            }
            let line = part.strip_suffix('\n').unwrap_or(part);
            let room = self.max_bytes.saturating_sub(self.bytes);
            let mut end = line.len().min(room);
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            self.pending.push_str(&line[..end]);
            self.bytes += end;
            self.truncated |= end < line.len();
            if part.ends_with('\n') {
                self.lines
                    .push(Diagnostic::Line(std::mem::take(&mut self.pending)));
            }
        }
    }

    fn flush_pending(&mut self) {
        if !self.pending.is_empty() {
            self.lines
                .push(Diagnostic::Line(std::mem::take(&mut self.pending)));
        }
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        let room = self.max_bytes.saturating_sub(self.bytes);
        if bytes.is_empty() {
            return;
        }
        if room == 0 || self.lines.len() >= self.max_lines {
            self.truncated = true;
            return;
        }
        let prefix = String::from_utf8_lossy(&bytes[..bytes.len().min(room)]);
        let mut end = prefix.len().min(room);
        while !prefix.is_char_boundary(end) {
            end -= 1;
        }
        self.push_fragment(&prefix[..end]);
        if bytes.len() > room || end < prefix.len() {
            self.truncated = true;
        }
    }

    fn finish(mut self) -> Vec<Diagnostic> {
        if self.truncated {
            self.flush_pending();
            self.lines.push(Diagnostic::Line(
                "... redirected diagnostics truncated ...".to_owned(),
            ));
        } else if !self.pending.is_empty() {
            self.lines.push(Diagnostic::Fragment(self.pending));
        }
        self.lines
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum FrameKind {
    #[default]
    Function,
    Prefix,
}

#[derive(Debug, Default)]
struct Frame {
    kind: FrameKind,
    locals: BTreeMap<String, Option<Value>>,
    positional: Vec<Value>,
    local_charges: BTreeMap<String, Vec<dekopon_process::RetainedBytes>>,
    _positional_charges: Vec<dekopon_process::RetainedBytes>,
}

pub(crate) fn run(
    script: &str,
    prev: Option<&str>,
    invoker: &dyn CapabilityInvoker,
    limits: Limits,
    threads: &ShellThreads,
) -> ScriptOutcome {
    run_with_tree(
        script,
        prev,
        None,
        invoker,
        limits,
        &TreeContext::new(
            limits,
            CallBudget::new(limits.max_capability_calls),
            threads.clone(),
        )
        .with_cancel(invoker.cancel_signals()),
    )
}

pub(crate) fn run_with_tree(
    script: &str,
    prev: Option<&str>,
    stdin: Option<PipeReader<StageObservation>>,
    invoker: &dyn CapabilityInvoker,
    limits: Limits,
    tree: &TreeContext,
) -> ScriptOutcome {
    let calls_before = tree.calls().used();
    let program = match parse(script) {
        Ok(program) => program,
        Err(error) => {
            return ScriptOutcome {
                output: format!("dekopon-shell: syntax error: {error}"),
                exit_code: ExitCode::SYNTAX,
                truncated: false,
                capability_calls: 0,
                steps: 0,
            };
        }
    };

    let evaluator = Evaluator {
        invoker,
        budget: Budget::start_tree(limits, tree.clone()),
        nesting: nesting::Nesting::root(),
        limits,
        output: OutputBuffer::new(&limits),
        globals: Arc::new(
            prev.map(|prev| ("PREV".to_owned(), Value::String(prev.to_owned())))
                .into_iter()
                .chain([("IFS".to_owned(), Value::String(DEFAULT_IFS.to_owned()))])
                .collect(),
        ),
        global_charges: BTreeMap::new(),
        buffer_charges: BTreeMap::new(),
        frames: Vec::new(),
        functions: BTreeMap::new(),
        function_names: BTreeSet::new(),
        buffers: Arc::new(BTreeMap::new()),
        captures: Vec::new(),
        active_buffer: None,
        discard_capture_depth: None,
        diagnostics_depth: None,
        expansion_charges: Vec::new(),
        options: ShellOptions::default(),
        testing_status: 0,
        script_stdin: stdin.is_some(),
        stdin: stdin.into_iter().collect(),
        stdout: None,
        reader_gone: false,
        stdout_redirected: false,
        shared_charges: Vec::new(),
        stderr_capture: Vec::new(),
        counters: telemetry::ScriptCounters::default(),
        last_status: ExitCode::SUCCESS,
        last_substitution_status: ExitCode::SUCCESS,
        jobs: ScriptJobs::default(),
    };
    conclude(evaluator, calls_before, |evaluator| {
        evaluator.execute_program(&program)
    })
}

pub(crate) fn run_seed(
    seed: JobSeed,
    invoker: &dyn CapabilityInvoker,
    limits: Limits,
    tree: &TreeContext,
) -> ScriptOutcome {
    let calls_before = tree.calls().used();
    let JobSeed {
        statement, scope, ..
    } = seed;
    let mut evaluator = Evaluator {
        invoker,
        budget: Budget::start_tree(limits, tree.clone()),
        nesting: nesting::Nesting::root(),
        limits,
        output: OutputBuffer::new(&limits),
        globals: scope.globals,
        global_charges: BTreeMap::new(),
        buffer_charges: BTreeMap::new(),
        frames: Vec::new(),
        functions: scope.functions,
        function_names: scope.function_names,
        buffers: scope.buffers,
        captures: Vec::new(),
        active_buffer: None,
        discard_capture_depth: None,
        diagnostics_depth: None,
        expansion_charges: Vec::new(),
        options: scope.options,
        testing_status: 0,
        script_stdin: false,
        stdin: Vec::new(),
        stdout: None,
        reader_gone: false,
        stdout_redirected: false,
        shared_charges: Vec::new(),
        stderr_capture: Vec::new(),
        counters: telemetry::ScriptCounters::default(),
        last_status: scope.last_status,
        last_substitution_status: ExitCode::SUCCESS,
        jobs: ScriptJobs::default(),
    };
    let charged = evaluator.adopt(scope.frames);
    conclude(evaluator, calls_before, |evaluator| {
        charged.map_err(FatalError::Limit)?;
        evaluator.execute_statement(&statement)
    })
}

fn conclude(
    mut evaluator: Evaluator<'_>,
    calls_before: u32,
    body: impl FnOnce(&mut Evaluator<'_>) -> Result<Flow, FatalError>,
) -> ScriptOutcome {
    let script = telemetry::script_span();
    let exit_code = {
        let _entered = script.enter();
        match body(&mut evaluator) {
            Ok(Flow::Exit(code)) | Ok(Flow::Return(code)) => {
                script.record("outcome", telemetry::outcome_label(code));
                code
            }
            Ok(_) => {
                script.record("outcome", telemetry::outcome_label(evaluator.last_status));
                evaluator.last_status
            }
            Err(fatal) => {
                script.record("outcome", telemetry::fatal_outcome(&fatal));
                evaluator.report_fatal(&fatal)
            }
        }
    };
    evaluator.counters.record_on(&script);

    evaluator.output.finish();
    ScriptOutcome {
        output: evaluator.output.render(),
        exit_code,
        truncated: evaluator.output.is_truncated(),
        capability_calls: evaluator
            .budget
            .capability_calls()
            .saturating_sub(calls_before),
        steps: evaluator.budget.steps(),
    }
}

#[derive(Clone, Copy)]
enum ExpansionOutput {
    Legacy,
    Unsplit,
    Fields,
    Value,
}

#[derive(Clone, Copy)]
enum PreservedExpansion {
    Legacy,
    Unsplit,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum FieldContext {
    Fields,
    CasePattern,
    QuotedCasePattern,
}

struct FieldExpansion<'a> {
    context: FieldContext,
    quoted_elements: bool,
    spans: &'a mut Vec<ExpansionSpan>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Protection {
    Unquoted,
    EmptyIfs,
    Quoted,
    Checked,
}

enum SpanContent {
    Text(String, Protection),
    Elements(Vec<Value>, String),
    Boundary(String),
    SplitSpace,
}

struct ExpansionSpan {
    content: SpanContent,
    charge: dekopon_process::RetainedBytes,
    retained: Vec<dekopon_process::RetainedBytes>,
}

struct Evaluator<'a> {
    invoker: &'a dyn CapabilityInvoker,
    budget: Budget,
    nesting: nesting::Nesting,
    limits: Limits,
    output: OutputBuffer,
    globals: Arc<BTreeMap<String, Value>>,
    global_charges: BTreeMap<String, Vec<dekopon_process::RetainedBytes>>,
    buffer_charges: BTreeMap<String, Vec<dekopon_process::RetainedBytes>>,
    frames: Vec<Frame>,
    functions: BTreeMap<String, Arc<Program>>,
    function_names: BTreeSet<String>,
    buffers: Arc<BTreeMap<String, Vec<u8>>>,
    captures: Vec<Vec<CommandResult>>,
    active_buffer: Option<(String, usize)>,
    discard_capture_depth: Option<usize>,
    diagnostics_depth: Option<usize>,
    expansion_charges: Vec<dekopon_process::RetainedBytes>,
    script_stdin: bool,
    stdin: Vec<PipeReader<StageObservation>>,
    stdout: Option<PipeWriter<StageObservation>>,
    reader_gone: bool,
    stdout_redirected: bool,
    shared_charges: Vec<dekopon_process::RetainedBytes>,
    options: ShellOptions,
    testing_status: u32,
    stderr_capture: Vec<StderrCapture>,
    counters: telemetry::ScriptCounters,
    last_status: ExitCode,
    last_substitution_status: ExitCode,
    jobs: ScriptJobs,
}

struct ScopedFrame {
    kind: FrameKind,
    locals: BTreeMap<String, Option<Value>>,
    positional: Vec<Value>,
}

pub(crate) struct JobScope {
    globals: Arc<BTreeMap<String, Value>>,
    buffers: Arc<BTreeMap<String, Vec<u8>>>,
    functions: BTreeMap<String, Arc<Program>>,
    function_names: BTreeSet<String>,
    frames: Vec<ScopedFrame>,
    options: ShellOptions,
    last_status: ExitCode,
}

pub(crate) fn fatal_message(fatal: &FatalError) -> String {
    match fatal {
        FatalError::Limit(LimitExceeded::Steps { maximum }) => format!(
            "dekopon-shell: step budget exhausted after {maximum} steps; the script is doing too much work or looping without progress"
        ),
        FatalError::Limit(LimitExceeded::RecursionDepth { maximum }) => format!(
            "dekopon-shell: function calls, `$( )` substitutions, `${{ }}` expansions and compound commands (`if`, `for`, `while`, `case`, `{{ }}`, `[[ ]]`) nested deeper than {maximum} levels while running; replace recursion with a loop, or move inner results into variables first"
        ),
        FatalError::Limit(LimitExceeded::Deadline { timeout_ms }) => {
            format!("dekopon-shell: script exceeded its {timeout_ms}ms deadline")
        }
        FatalError::Limit(LimitExceeded::CapabilityCalls { maximum }) => {
            format!("dekopon-shell: the turn's capability-call budget of {maximum} is spent")
        }
        FatalError::Limit(LimitExceeded::ValueBytes { maximum }) => format!(
            "dekopon-shell: script tried to hold more than {maximum} bytes of values in variables, buffers, and substitutions"
        ),
        FatalError::Limit(LimitExceeded::ShellThreads { maximum }) => format!(
            "dekopon-shell: all {maximum} shell threads this process allows are busy; run fewer pipeline stages or provider calls at once, or retry later"
        ),
        FatalError::Limit(LimitExceeded::Cancelled) => "dekopon-shell: script cancelled".to_owned(),
        FatalError::Unsupported(reason) | FatalError::Assertion(reason) => {
            format!("dekopon-shell: {reason}")
        }
    }
}

impl<'a> Evaluator<'a> {
    fn report_fatal(&mut self, fatal: &FatalError) -> ExitCode {
        self.output.push_block(&fatal_message(fatal));
        telemetry::fatal_exit_code(fatal)
    }

    fn write_redirected_diagnostics(&mut self, result: &CommandResult) {
        if let Some(diagnostic) = Diagnostic::from_result(result) {
            self.write_diagnostic(diagnostic);
        }
    }

    fn write_line(&mut self, line: &str) {
        if let Some(capture) = self.stderr_capture.last_mut() {
            capture.push(line);
            return;
        }
        self.output.push_block(line);
    }

    fn write_diagnostic(&mut self, diagnostic: Diagnostic) {
        match diagnostic {
            Diagnostic::Line(text) => self.write_line(&text),
            Diagnostic::Fragment(text) => {
                push_diagnostic_fragment(&text, &mut self.stderr_capture, &mut self.output)
            }
        }
    }

    fn active_buffer_name(&self) -> Option<&str> {
        self.active_buffer
            .as_ref()
            .filter(|(_, depth)| *depth == self.captures.len())
            .map(|(name, _)| name.as_str())
    }

    fn output_discarded(&self) -> bool {
        self.discard_capture_depth
            .is_some_and(|depth| self.captures.len() <= depth)
    }

    fn diagnostics_redirected(&self) -> bool {
        self.diagnostics_depth
            .is_some_and(|depth| self.captures.len() <= depth)
    }

    fn emit(&mut self, result: CommandResult) -> Result<(), LimitExceeded> {
        self.emit_to(result, &Sink::Value)
    }

    fn emit_to(&mut self, mut result: CommandResult, sink: &Sink) -> Result<(), LimitExceeded> {
        if result.value.is_null() {
            return Ok(());
        }
        let mut output = result
            .output
            .take()
            .unwrap_or_else(|| StageOut::new(self, sink, OutputMode::Value));
        output.value(self, result)
    }

    /// Whether a command produced no value has to be recognized before it reaches a capture, not
    /// after, or it silently adds a blank element and an extra blank line to the captured result.
    fn absorb(&mut self, failure: CommandFailure) -> Result<ExitCode, FatalError> {
        match failure {
            CommandFailure::Status { message, status } => {
                self.write_line(&message);
                Ok(status)
            }
            CommandFailure::Fatal(fatal) => Err(fatal),
        }
    }

    fn emit_rendered(
        &mut self,
        stdout: String,
        stderr: String,
        status: u8,
    ) -> Result<Executed, FatalError> {
        let retained = self
            .budget
            .charge_value_bytes((stdout.len() + stderr.len()) as u64)?;
        if !stderr.is_empty() {
            self.write_line(stderr.strip_suffix('\n').unwrap_or(&stderr));
        }
        let status = ExitCode::from(status);
        if stdout.is_empty() {
            return Ok(Executed::Result(CommandResult::status(status)));
        }
        let mut result = match stdout.strip_suffix('\n') {
            Some(terminated) => CommandResult::value(Value::String(terminated.to_owned())),
            None => CommandResult::value(Value::String(stdout)).without_newline(),
        };
        result.status = status;
        result.retained.push(retained);
        Ok(Executed::Result(result))
    }

    fn lookup(&self, name: &str) -> Option<&Value> {
        for frame in self.frames.iter().rev() {
            if let Some(value) = frame.locals.get(name) {
                return value.as_ref();
            }
        }
        self.globals.get(name)
    }

    fn assign(&mut self, name: &str, mut result: CommandResult) -> Result<(), LimitExceeded> {
        if let Some(frame) = self
            .frames
            .iter_mut()
            .rev()
            .find(|frame| frame.locals.contains_key(name))
        {
            frame.locals.remove(name);
            frame.local_charges.remove(name);
            retain_value(&self.budget, &result.value, &mut result.retained)?;
            frame.locals.insert(name.to_owned(), Some(result.value));
            frame.local_charges.insert(name.to_owned(), result.retained);
        } else {
            self.unshare_globals()?;
            Arc::make_mut(&mut self.globals).remove(name);
            self.global_charges.remove(name);
            retain_value(&self.budget, &result.value, &mut result.retained)?;
            Arc::make_mut(&mut self.globals).insert(name.to_owned(), result.value);
            self.global_charges.insert(name.to_owned(), result.retained);
        }
        Ok(())
    }

    fn unshare_globals(&mut self) -> Result<(), LimitExceeded> {
        if Arc::strong_count(&self.globals) > 1 {
            let bytes = self.globals.values().map(value_bytes).sum();
            self.shared_charges
                .push(self.budget.charge_value_bytes(bytes)?);
        }
        Arc::make_mut(&mut self.globals);
        Ok(())
    }

    fn unshare_buffers(&mut self) -> Result<(), LimitExceeded> {
        if Arc::strong_count(&self.buffers) > 1 {
            let bytes = self.buffers.values().map(|bytes| bytes.len() as u64).sum();
            self.shared_charges
                .push(self.budget.charge_value_bytes(bytes)?);
        }
        Arc::make_mut(&mut self.buffers);
        Ok(())
    }

    fn assign_prefix(
        &mut self,
        name: &str,
        result: CommandResult,
        scope: &mut Option<usize>,
    ) -> Result<(), LimitExceeded> {
        if scope.is_none() {
            *scope = Some(self.frames.len());
            self.frames.push(Frame {
                kind: FrameKind::Prefix,
                ..Frame::default()
            });
        }
        if let Some(frame) = self.frames.last_mut() {
            frame.locals.insert(name.to_owned(), None);
        }
        self.assign(name, result)
    }

    fn declare_local(&mut self, name: &str, value: Option<Value>) -> Result<(), LimitExceeded> {
        let Some(index) = self
            .frames
            .iter()
            .rposition(|frame| frame.kind == FrameKind::Function)
        else {
            return Ok(());
        };
        let prefix = self
            .frames
            .iter()
            .rev()
            .find(|frame| frame.locals.contains_key(name))
            .filter(|frame| frame.kind == FrameKind::Prefix);
        if value.is_none() && prefix.is_none() && self.frames[index].locals.contains_key(name) {
            return Ok(());
        }
        let borrowed = value
            .as_ref()
            .or_else(|| prefix.and_then(|frame| frame.locals.get(name).and_then(Option::as_ref)));
        let charge = self
            .budget
            .charge_value_bytes(borrowed.map_or(0, value_bytes))?;
        let value =
            value.or_else(|| prefix.and_then(|frame| frame.locals.get(name).cloned().flatten()));
        let frame = &mut self.frames[index];
        frame.locals.insert(name.to_owned(), value);
        frame.local_charges.insert(name.to_owned(), vec![charge]);
        Ok(())
    }

    fn positional(&self) -> &[Value] {
        self.frames
            .iter()
            .rev()
            .find(|frame| frame.kind == FrameKind::Function)
            .map_or(&[][..], |frame| frame.positional.as_slice())
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
    fn execute_program(&mut self, program: &Program) -> Result<Flow, FatalError> {
        for statement in &program.statements {
            let parent = std::mem::take(&mut self.expansion_charges);
            let result = self.execute_statement(statement);
            self.expansion_charges = parent;
            match result? {
                Flow::Normal => {}
                other => return Ok(other),
            }
        }
        Ok(Flow::Normal)
    }

    fn execute_statement(&mut self, statement: &Statement) -> Result<Flow, FatalError> {
        self.budget.charge_step()?;
        match statement {
            Statement::List(list) => {
                let (status, flow) = self.execute_list(list)?;
                self.last_status = status;
                Ok(flow.unwrap_or(Flow::Normal))
            }
            Statement::If(statement) => self.execute_if(statement),
            Statement::For(statement) => self.execute_for(statement),
            Statement::While(statement) => self.execute_while(statement),
            Statement::Case(statement) => self.execute_case(statement),
            Statement::Group(body) => self.execute_program(body),
            Statement::Conditional(expression) => {
                let status = match self.evaluate_conditional(expression) {
                    Ok(status) => status,
                    Err(failure) => self.absorb(failure)?,
                };
                self.last_status = status;
                Ok(Flow::Normal)
            }
            Statement::Function(definition) => {
                self.function_names.insert(definition.name.clone());
                self.functions
                    .insert(definition.name.clone(), Arc::new(definition.body.clone()));
                self.last_status = ExitCode::SUCCESS;
                Ok(Flow::Normal)
            }
            Statement::Background(background) => {
                self.last_status = self.start_job(background);
                Ok(Flow::Normal)
            }
        }
    }

    fn execute_if(&mut self, statement: &IfStatement) -> Result<Flow, FatalError> {
        for (condition, body) in &statement.branches {
            let (status, flow) =
                self.tested(true, |evaluator| evaluator.execute_list(condition))?;
            self.last_status = status;
            if let Some(flow) = flow {
                return Ok(flow);
            }
            if status == ExitCode::SUCCESS {
                return self.execute_program(body);
            }
        }
        if let Some(otherwise) = &statement.otherwise {
            return self.execute_program(otherwise);
        }
        self.last_status = ExitCode::SUCCESS;
        Ok(Flow::Normal)
    }

    fn execute_case(&mut self, statement: &CaseStatement) -> Result<Flow, FatalError> {
        let subject = match self
            .expand_preserved_fields(&statement.subject.parts, ExpansionOutput::Unsplit)
        {
            Ok(expanded) => expanded.join(" "),
            Err(failure) => {
                let status = self.absorb(failure)?;
                self.last_status = status;
                return Ok(Flow::Normal);
            }
        };

        for clause in &statement.clauses {
            for pattern in &clause.patterns {
                self.budget.charge_step()?;
                let matched = match self
                    .expand_case_pattern(pattern)
                    .and_then(|expanded| case_pattern::matches(&expanded, &subject, &self.budget))
                {
                    Ok(matched) => matched,
                    Err(failure) => {
                        self.last_status = self.absorb(failure)?;
                        return Ok(Flow::Normal);
                    }
                };
                if matched {
                    return self.execute_program(&clause.body);
                }
            }
        }

        self.last_status = ExitCode::SUCCESS;
        Ok(Flow::Normal)
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
    fn execute_for(&mut self, statement: &ForLoop) -> Result<Flow, FatalError> {
        let mut items = Vec::new();
        for word in &statement.words {
            match self.expand_word(word) {
                Ok(expanded) => items.extend(expanded),
                Err(failure) => {
                    let status = self.absorb(failure)?;
                    self.last_status = status;
                    return Ok(Flow::Normal);
                }
            }
        }

        let mut body_status = ExitCode::SUCCESS;
        for item in items {
            self.budget.charge_step()?;
            self.assign(
                &statement.variable,
                CommandResult::value(Value::String(item)),
            )?;
            let flow = self.execute_program(&statement.body)?;
            body_status = self.last_status;
            match flow {
                Flow::Normal => {}
                Flow::Break(level) => {
                    self.last_status = body_status;
                    return Ok(unwind_break(level));
                }
                Flow::Continue(level) => {
                    if level > 1 {
                        self.last_status = body_status;
                        return Ok(Flow::Continue(level - 1));
                    }
                }
                terminal => return Ok(terminal),
            }
        }
        self.last_status = body_status;
        Ok(Flow::Normal)
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
    fn execute_while(&mut self, statement: &WhileLoop) -> Result<Flow, FatalError> {
        let mut body_status = ExitCode::SUCCESS;
        loop {
            self.budget.charge_step()?;
            let (status, flow) = self.tested(true, |evaluator| {
                evaluator.execute_list(&statement.condition)
            })?;
            self.last_status = status;
            if let Some(flow) = flow {
                return Ok(flow);
            }
            let satisfied = if statement.until {
                status != ExitCode::SUCCESS
            } else {
                status == ExitCode::SUCCESS
            };
            if !satisfied {
                self.last_status = body_status;
                return Ok(Flow::Normal);
            }

            let flow = self.execute_program(&statement.body)?;
            body_status = self.last_status;
            match flow {
                Flow::Normal => {}
                Flow::Break(level) => {
                    self.last_status = body_status;
                    return Ok(unwind_break(level));
                }
                Flow::Continue(level) => {
                    if level > 1 {
                        self.last_status = body_status;
                        return Ok(Flow::Continue(level - 1));
                    }
                }
                terminal => return Ok(terminal),
            }
        }
    }

    fn execute_list(&mut self, list: &AndOrList) -> Result<(ExitCode, Option<Flow>), FatalError> {
        let (mut status, flow) = self.tested(!list.rest.is_empty(), |evaluator| {
            evaluator.execute_pipeline(&list.first)
        })?;
        self.last_status = status;
        if flow.is_some() {
            return Ok((status, flow));
        }

        let last = list.rest.len().saturating_sub(1);
        for (index, (operator, pipeline)) in list.rest.iter().enumerate() {
            let should_run = match operator {
                AndOr::And => status == ExitCode::SUCCESS,
                AndOr::Or => status != ExitCode::SUCCESS,
            };
            if !should_run {
                continue;
            }
            let (next, flow) = self.tested(index < last, |evaluator| {
                evaluator.execute_pipeline(pipeline)
            })?;
            status = next;
            self.last_status = status;
            if flow.is_some() {
                return Ok((status, flow));
            }
        }
        if let Some(exit) = self.errexit_trip(status) {
            return Ok((status, Some(exit)));
        }
        Ok((status, None))
    }

    fn run_read(
        &mut self,
        arguments: &[String],
        input: StageInput,
    ) -> Result<CommandResult, CommandFailure> {
        let mut names = arguments;
        let raw = names.first().is_some_and(|first| first == "-r");
        if raw {
            names = &names[1..];
        }
        if let Some(flag) = names.first().filter(|first| first.starts_with('-')) {
            return Err(CommandFailure::usage(format!(
                "read: option {flag:?} is not supported; this shell has only `read [-r] [NAME...]`"
            )));
        }
        for name in names {
            if !is_variable_name(name) {
                return Err(CommandFailure::usage(format!(
                    "read: {name:?} is not a valid variable name"
                )));
            }
        }
        let mut piped = match input {
            StageInput::Piped(reader) => Some(reader),
            StageInput::Inherited => None,
        };
        let mut text = String::new();
        let mut protected = Vec::new();
        let mut retained = self.budget.charge_value_bytes(0)?;
        let mut escaped = false;
        let mut terminated = false;
        loop {
            self.budget.charge_step()?;
            let line = match piped.as_mut().or_else(|| self.stdin.last_mut()) {
                Some(reader) => reader.read_line(self.budget.tree())?,
                None => None,
            };
            let Some(line) = line else { break };
            let (bytes, _charges) = line.into_parts();
            let line = std::str::from_utf8(&bytes).map_err(|_not_utf8| {
                CommandFailure::failed("read: input is not valid UTF-8 text")
            })?;
            for character in line.chars() {
                if escaped {
                    escaped = false;
                    if character == '\n' {
                        continue;
                    }
                    retained.grow(
                        (size_of::<std::ops::Range<usize>>() + character.len_utf8()) as u64,
                    )?;
                    protected.push(text.len()..text.len() + character.len_utf8());
                    text.push(character);
                } else if !raw && character == '\\' {
                    escaped = true;
                } else if character == '\n' {
                    terminated = true;
                } else {
                    retained.grow(character.len_utf8() as u64)?;
                    text.push(character);
                }
            }
            if terminated || !line.ends_with('\n') {
                break;
            }
        }
        if names.is_empty() {
            self.assign("REPLY", CommandResult::value(Value::String(text)))?;
        } else {
            let ifs = self.ifs()?;
            let fields = ifs.read_fields(&text, &protected, names.len(), &mut self.budget)?;
            for (index, name) in names.iter().enumerate() {
                let field = fields.get(&text, index)?;
                let charge = self.budget.charge_value_bytes(field.len() as u64)?;
                let mut result = CommandResult::value(Value::String(field.to_owned()));
                result.retained.push(charge);
                self.assign(name, result)?;
            }
        }
        Ok(CommandResult::status(if terminated {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }))
    }

    fn apply_set_options(&mut self, arguments: &[String]) -> Result<(), CommandFailure> {
        if arguments.is_empty() {
            return Err(unsupported_option(
                "set: listing or setting positional parameters is not supported; \
                 use `set -e`, `set -u`, `set -o pipefail`, or their `+` forms",
            ));
        }
        let mut index = 0;
        while index < arguments.len() {
            let argument = arguments[index].as_str();
            let enable = match argument.chars().next() {
                Some('-') => true,
                Some('+') => false,
                _ => {
                    return Err(unsupported_option(format!(
                        "set: {argument:?} is not an option; this shell supports only -e, -u, and \
                         -o pipefail"
                    )));
                }
            };
            if argument.len() == 1 {
                return Err(unsupported_option(format!(
                    "set: {argument:?} on its own is not an option"
                )));
            }
            if argument == "--" || argument == "++" {
                return Err(unsupported_option(
                    "set: `--` sets positional parameters, which this shell has only inside a \
                     function and only from its arguments",
                ));
            }
            index += 1;
            for letter in argument.chars().skip(1) {
                match letter {
                    'e' => self.options.errexit = enable,
                    'u' => self.options.nounset = enable,
                    'o' => {
                        let Some(name) = arguments.get(index) else {
                            return Err(unsupported_option(
                                "set: -o needs an option name; the only one is `pipefail`",
                            ));
                        };
                        index += 1;
                        self.set_long_option(name, enable)?;
                    }
                    other => {
                        return Err(unsupported_option(format!(
                            "set: option -{other} is not supported; this shell has -e, -u, and \
                             -o pipefail"
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    fn set_long_option(&mut self, name: &str, enable: bool) -> Result<(), CommandFailure> {
        match name {
            "pipefail" => self.options.pipefail = enable,
            "errexit" => self.options.errexit = enable,
            "nounset" => self.options.nounset = enable,
            other => {
                return Err(unsupported_option(format!(
                    "set: -o {other} is not supported; this shell has pipefail, errexit, and nounset"
                )));
            }
        }
        Ok(())
    }

    fn record_pipe_statuses(&mut self, stages: Vec<ExitCode>) -> Result<(), LimitExceeded> {
        self.unshare_globals()?;
        let value = Value::Array(
            stages
                .into_iter()
                .map(|stage| Value::from(stage.get()))
                .collect(),
        );
        self.global_charges.remove("PIPESTATUS");
        let charge = self.budget.charge_value_bytes(value_bytes(&value))?;
        Arc::make_mut(&mut self.globals).insert("PIPESTATUS".to_owned(), value);
        self.global_charges
            .insert("PIPESTATUS".to_owned(), vec![charge]);
        Ok(())
    }

    fn tested<T>(
        &mut self,
        suspended: bool,
        body: impl FnOnce(&mut Self) -> Result<T, FatalError>,
    ) -> Result<T, FatalError> {
        if suspended {
            self.testing_status += 1;
        }
        let outcome = body(self);
        if suspended {
            self.testing_status -= 1;
        }
        outcome
    }

    fn errexit_trip(&mut self, status: ExitCode) -> Option<Flow> {
        if !self.options.errexit || self.testing_status > 0 || status == ExitCode::SUCCESS {
            return None;
        }
        self.write_line(&format!(
            "dekopon-shell: command failed with status {status} and `set -e` is on"
        ));
        Some(Flow::Exit(status))
    }

    fn execute_pipeline(
        &mut self,
        pipeline: &Pipeline,
    ) -> Result<(ExitCode, Option<Flow>), FatalError> {
        self.budget.charge_step()?;
        if self.reader_gone && self.captures.is_empty() {
            return Ok((self.last_status, Some(Flow::Exit(self.last_status))));
        }
        let Some((last, producers)) = pipeline.commands.split_last() else {
            return Ok((ExitCode::SUCCESS, None));
        };

        let permits = self
            .budget
            .tree()
            .shell_threads()
            .reserve_many(producers.len())?;
        let pipeline_id = telemetry::next_pipeline();
        let meters = (0..pipeline.commands.len())
            .map(|_| Arc::new(telemetry::StageMeter::default()))
            .collect::<Vec<_>>();
        #[expect(
            clippy::disallowed_methods,
            reason = "owner: execute_pipeline, which joins every stage before returning; bound: one ShellThreads permit per producer, reserved for the whole pipeline"
        )]
        let (outcomes, diagnostics, executed, last_trace) = thread::scope(|scope| {
            let mut input = StageInput::Inherited;
            let mut running = Vec::with_capacity(producers.len());
            for ((index, command), permit) in producers.iter().enumerate().zip(permits) {
                let (writer, reader) = pipe::pipe(
                    Some(Arc::clone(&meters[index + 1])),
                    Some(Arc::clone(&meters[index])),
                    crate::interp::telemetry::stage_read,
                    crate::interp::telemetry::stage_write,
                );
                let stage_input = std::mem::replace(&mut input, StageInput::Piped(reader));
                running.push(self.spawn_stage(
                    scope,
                    StageSetup {
                        position: StagePosition {
                            pipeline: pipeline_id,
                            index,
                        },
                        meter: Arc::clone(&meters[index]),
                        permit,
                    },
                    command,
                    stage_input,
                    writer,
                ));
            }
            if !producers.is_empty() {
                self.stderr_capture.push(StderrCapture::new(&self.limits));
            }
            let last_trace = telemetry::StageTrace::enter(
                pipeline_id,
                producers.len(),
                Arc::clone(&meters[producers.len()]),
            );
            let compound_span = (!producers.is_empty())
                .then(|| stage_compound_span(last, self.invoker.participant_handle()))
                .flatten();
            if let Some(span) = &compound_span {
                telemetry::record_compound_stage(span);
            }
            let _entered_compound = compound_span.as_ref().map(tracing::Span::enter);
            let executed = self.tested(pipeline.negated, |evaluator| {
                evaluator.execute_command(last, input)
            });
            record_compound_outcome(compound_span.as_ref(), &executed);
            drop(_entered_compound);
            last_trace.pause();
            let diagnostics = (!producers.is_empty())
                .then(|| self.stderr_capture.pop())
                .flatten()
                .map(StderrCapture::finish)
                .unwrap_or_default();
            let outcomes = running.into_iter().map(Stage::join).collect::<Vec<_>>();
            (outcomes, diagnostics, executed, last_trace)
        });
        self.shared_charges.clear();

        let mut stages = Vec::with_capacity(pipeline.commands.len());
        let mut fatal = None;
        for (index, outcome) in outcomes.into_iter().enumerate() {
            match outcome {
                Ok(outcome) => {
                    for diagnostic in outcome.diagnostics {
                        self.write_diagnostic(diagnostic);
                    }
                    match outcome.status {
                        Ok(status) => {
                            if index == 0
                                && let (Some(slot), Some(reader)) =
                                    (self.stdin.last_mut(), outcome.enclosing)
                            {
                                *slot = reader;
                            }
                            stages.push(status);
                        }
                        Err(error) => {
                            fatal.get_or_insert(error);
                            stages.push(ExitCode::FAILURE);
                        }
                    }
                }
                Err(StageFailure::Refused(message)) => {
                    self.write_line(&message);
                    stages.push(ExitCode::FAILURE);
                }
            }
        }
        for diagnostic in diagnostics {
            self.write_diagnostic(diagnostic);
        }
        let executed = executed.map_err(|error| fatal.take().unwrap_or(error))?;
        if let Some(error) = fatal {
            return Err(error);
        }
        let last = match executed {
            Executed::Flow(flow) => return Ok((self.last_status, Some(flow))),
            Executed::Result(result) => result,
        };
        stages.push(last.status);

        let mut status = last.status;
        if self.options.pipefail
            && let Some(failed) = stages
                .iter()
                .rev()
                .find(|stage| **stage != ExitCode::SUCCESS)
        {
            status = *failed;
        }
        self.record_pipe_statuses(stages)?;
        if pipeline.negated {
            status = invert(status);
        }
        last_trace.resume();
        self.emit(last)?;
        drop(last_trace);
        Ok((status, None))
    }

    /// The enclosing compound's stream moves into the first stage for the stage's lifetime and
    /// comes back at its join, so whatever it leaves unread is still there for the next command.
    fn spawn_stage<'scope, 'env>(
        &mut self,
        scope: &'scope Scope<'scope, 'env>,
        setup: StageSetup,
        command: &'env Command,
        input: StageInput,
        writer: PipeWriter<StageObservation>,
    ) -> Stage<'scope>
    where
        'a: 'env,
    {
        let StageSetup {
            position,
            meter,
            permit,
        } = setup;
        let refused = |reason: &dyn std::fmt::Display| Stage::Refused {
            message: format!("dekopon-shell: pipeline stage {}: {reason}", position.index),
        };
        let stack = match self.budget.charge_value_bytes(STAGE_STACK_BYTES as u64) {
            Ok(stack) => stack,
            Err(_limit) => {
                return refused(&format_args!(
                    "cannot reserve {STAGE_STACK_BYTES} bytes of stack within the retained-value budget"
                ));
            }
        };
        let mut stage = match self.snapshot(writer) {
            Ok(stage) => stage,
            Err(limit) => return refused(&format_args!("{limit:?}")),
        };
        stage.script_stdin =
            matches!(input, StageInput::Inherited) && self.provider_reads_script_stdin();
        let enclosing = match input {
            StageInput::Inherited => self.stdin.last_mut().map(std::mem::take),
            StageInput::Piped(_) => None,
        };
        let span = tracing::Span::current();
        let dispatcher = tracing::dispatcher::get_default(Clone::clone);
        let spawned = permit.spawn(scope, STAGE_THREAD, move || {
            tracing::dispatcher::with_default(&dispatcher, || {
                let _entered = span.enter();
                let _stage_trace =
                    telemetry::StageTrace::enter(position.pipeline, position.index, meter);
                let compound_span =
                    stage_compound_span(command, stage.invoker.participant_handle());
                if let Some(span) = &compound_span {
                    telemetry::record_compound_stage(span);
                }
                let _entered_compound = compound_span.as_ref().map(tracing::Span::enter);
                let inherited = enclosing.is_some();
                if let Some(reader) = enclosing {
                    stage.stdin.push(reader);
                }
                let status = (|| {
                    let executed =
                        stage.tested(true, |stage| stage.execute_command(command, input));
                    record_compound_outcome(compound_span.as_ref(), &executed);
                    let executed = executed?;
                    match executed {
                        Executed::Result(result) => {
                            let status = result.status;
                            stage.emit(result)?;
                            Ok(status)
                        }
                        Executed::Flow(Flow::Exit(status) | Flow::Return(status)) => Ok(status),
                        Executed::Flow(Flow::Normal | Flow::Break(_) | Flow::Continue(_)) => {
                            Ok(stage.last_status)
                        }
                    }
                })();
                stage.stdout = None;
                let enclosing = if inherited { stage.stdin.pop() } else { None };
                let diagnostics = stage
                    .stderr_capture
                    .pop()
                    .map(StderrCapture::finish)
                    .unwrap_or_default();
                StageOutcome {
                    status,
                    diagnostics,
                    enclosing,
                }
            })
        });
        match spawned {
            Ok(handle) => Stage::Running {
                handle,
                _stack: stack,
            },
            Err(error) => refused(&error),
        }
    }

    fn provider_reads_script_stdin(&self) -> bool {
        self.script_stdin && self.stdin.len() == 1
    }

    fn copy_frames(&self) -> Result<Vec<Frame>, LimitExceeded> {
        let mut frames = Vec::with_capacity(self.frames.len());
        for frame in &self.frames {
            let mut local_charges = BTreeMap::new();
            for (name, value) in &frame.locals {
                local_charges.insert(
                    name.clone(),
                    vec![
                        self.budget
                            .charge_value_bytes(value.as_ref().map_or(0, value_bytes))?,
                    ],
                );
            }
            let positional_charges = frame
                .positional
                .iter()
                .map(|value| self.budget.charge_value_bytes(value_bytes(value)))
                .collect::<Result<Vec<_>, _>>()?;
            frames.push(Frame {
                kind: frame.kind,
                locals: frame.locals.clone(),
                positional: frame.positional.clone(),
                local_charges,
                _positional_charges: positional_charges,
            });
        }
        Ok(frames)
    }

    fn snapshot(
        &mut self,
        writer: PipeWriter<StageObservation>,
    ) -> Result<Evaluator<'a>, LimitExceeded> {
        let frames = self.copy_frames()?;
        Ok(Evaluator {
            invoker: self.invoker,
            budget: self.budget.fork(),
            nesting: self.nesting.fork(),
            limits: self.limits,
            output: OutputBuffer::new(&self.limits),
            globals: Arc::clone(&self.globals),
            global_charges: BTreeMap::new(),
            buffer_charges: BTreeMap::new(),
            frames,
            functions: self.functions.clone(),
            function_names: self.function_names.clone(),
            buffers: Arc::clone(&self.buffers),
            captures: Vec::new(),
            active_buffer: None,
            discard_capture_depth: None,
            diagnostics_depth: None,
            expansion_charges: Vec::new(),
            options: self.options,
            testing_status: 0,
            script_stdin: false,
            stdin: Vec::new(),
            stdout: Some(writer),
            reader_gone: false,
            stdout_redirected: false,
            shared_charges: Vec::new(),
            stderr_capture: vec![StderrCapture::new(&self.limits)],
            counters: telemetry::ScriptCounters::default(),
            last_status: self.last_status,
            last_substitution_status: ExitCode::SUCCESS,
            jobs: self.jobs.clone(),
        })
    }

    fn adopt(&mut self, frames: Vec<ScopedFrame>) -> Result<(), LimitExceeded> {
        for (name, value) in self.globals.iter() {
            let charge = self.budget.charge_value_bytes(value_bytes(value))?;
            self.global_charges.insert(name.clone(), vec![charge]);
        }
        for (name, bytes) in self.buffers.iter() {
            let charge = self.budget.charge_value_bytes(bytes.len() as u64)?;
            self.buffer_charges.insert(name.clone(), vec![charge]);
        }
        for ScopedFrame {
            kind,
            locals,
            positional,
        } in frames
        {
            let mut local_charges = BTreeMap::new();
            for (name, value) in &locals {
                local_charges.insert(
                    name.clone(),
                    vec![
                        self.budget
                            .charge_value_bytes(value.as_ref().map_or(0, value_bytes))?,
                    ],
                );
            }
            let positional_charges = positional
                .iter()
                .map(|value| self.budget.charge_value_bytes(value_bytes(value)))
                .collect::<Result<Vec<_>, _>>()?;
            self.frames.push(Frame {
                kind,
                locals,
                positional,
                local_charges,
                _positional_charges: positional_charges,
            });
        }
        Ok(())
    }

    fn scope(&self) -> JobScope {
        JobScope {
            globals: Arc::clone(&self.globals),
            buffers: Arc::clone(&self.buffers),
            functions: self.functions.clone(),
            function_names: self.function_names.clone(),
            frames: self
                .frames
                .iter()
                .map(|frame| ScopedFrame {
                    kind: frame.kind,
                    locals: frame.locals.clone(),
                    positional: frame.positional.clone(),
                })
                .collect(),
            options: self.options,
            last_status: self.last_status,
        }
    }

    fn start_job(&mut self, background: &Background) -> ExitCode {
        let Some(control) = self.invoker.job_control() else {
            self.write_line(JOBS_OFF);
            return ExitCode::FAILURE;
        };
        let seed = JobSeed {
            statement: Arc::clone(&background.statement),
            text: Arc::clone(&background.text),
            scope: self.scope(),
            tree: self.budget.tree().clone(),
        };
        match control.start(seed) {
            Ok(id) => {
                self.write_line(&format!("[{id}]"));
                self.jobs.started.push(id);
                self.jobs.last = Some(id);
                ExitCode::SUCCESS
            }
            Err(refusal) => {
                self.write_line(&refusal.to_string());
                ExitCode::FAILURE
            }
        }
    }

    fn copy_stdin(
        &mut self,
        input: StageInput,
        sink: &Sink,
    ) -> Result<CommandResult, CommandFailure> {
        let mut output = StageOut::new(self, sink, OutputMode::Copy);
        let mut piped = match input {
            StageInput::Piped(reader) => Some(reader),
            StageInput::Inherited => None,
        };
        if piped.is_none() && self.stdin.is_empty() {
            return Ok(CommandResult::status(ExitCode::SUCCESS));
        }
        while let Some(reader) = piped.as_mut().or_else(|| self.stdin.last_mut()) {
            let ReadOutcome::Bytes(chunk) = reader.read(self.budget.tree())? else {
                break;
            };
            self.budget.charge_step()?;
            if !output.write(&mut WriteTarget::split(self).1, &chunk)? {
                break;
            }
        }
        output.finish(self, ExitCode::SUCCESS)
    }

    fn execute_command(
        &mut self,
        command: &Command,
        input: StageInput,
    ) -> Result<Executed, FatalError> {
        let parent = std::mem::take(&mut self.expansion_charges);
        let result = match command {
            Command::Simple(command) => self.execute_simple_command(command, input),
            Command::Compound {
                statement,
                redirects,
            } => self.nested(|this| this.execute_compound_command(statement, redirects, input)),
        };
        self.expansion_charges = parent;
        result
    }

    fn execute_compound_command(
        &mut self,
        statement: &Statement,
        redirects: &[Redirect],
        input: StageInput,
    ) -> Result<Executed, FatalError> {
        let (stdout, stderr) = match self.resolve_redirects(redirects) {
            Ok(sinks) => sinks,
            Err(failure) => {
                return self.failed(failure);
            }
        };
        self.open_buffers(&[&stdout, &stderr])?;
        let previous_buffer = match &stdout {
            Sink::Buffer { name, .. } => self
                .active_buffer
                .replace((name.clone(), self.captures.len())),
            Sink::Value => None,
            Sink::Diagnostics | Sink::Discard => self.active_buffer.take(),
        };

        let capturing = stderr != Sink::Diagnostics;
        if capturing {
            self.stderr_capture.push(StderrCapture::new(&self.limits));
        }
        let piped = match input {
            StageInput::Piped(reader) => {
                self.stdin.push(reader);
                true
            }
            StageInput::Inherited => false,
        };
        let previous_discard = self.discard_capture_depth;
        match stdout {
            Sink::Discard => self.discard_capture_depth = Some(self.captures.len()),
            Sink::Buffer { .. } | Sink::Diagnostics => self.discard_capture_depth = None,
            Sink::Value => {}
        }
        let previous_diagnostics = self.diagnostics_depth;
        if stdout == Sink::Diagnostics {
            self.diagnostics_depth = Some(self.captures.len());
        }
        let saved_stdout = (stdout != Sink::Value)
            .then(|| self.stdout.take())
            .flatten();
        let flow = self.execute_statement(statement);
        if stdout != Sink::Value {
            self.stdout = saved_stdout;
        }
        self.discard_capture_depth = previous_discard;
        self.diagnostics_depth = previous_diagnostics;
        if stdout != Sink::Value {
            self.active_buffer = previous_buffer;
        }

        if piped {
            self.stdin.pop();
        }
        let mut diagnostics = capturing
            .then(|| self.stderr_capture.pop())
            .flatten()
            .map(StderrCapture::finish)
            .unwrap_or_default();

        let flow = flow?;
        let status = match flow {
            Flow::Normal => self.last_status,
            Flow::Break(_) | Flow::Continue(_) | Flow::Return(_) | Flow::Exit(_) => {
                self.route_diagnostics(diagnostics, &stderr)?;
                return Ok(Executed::Flow(flow));
            }
        };

        let mut result = CommandResult::status(status);
        if stderr == Sink::Value {
            if let Sink::Buffer { name, .. } = &stdout {
                for diagnostic in diagnostics.drain(..) {
                    self.append_buffer(name, diagnostic.into_result())?;
                }
            } else {
                merge_diagnostics(&mut result, diagnostics);
                diagnostics = Vec::new();
            }
        }
        self.route_diagnostics(diagnostics, &stderr)?;
        retain_value(&self.budget, &result.value, &mut result.retained)?;

        match stdout {
            Sink::Value => Ok(Executed::Result(result)),
            Sink::Diagnostics => self.redirect_result(result, &stdout),
            Sink::Discard | Sink::Buffer { .. } => {
                Ok(Executed::Result(CommandResult::status(result.status)))
            }
        }
    }

    fn expand_command_argv(&mut self, words: &[Word]) -> Result<Vec<crate::Arg>, CommandFailure> {
        let local_assignment = matches!(
            words.first(),
            Some(Word { parts }) if matches!(parts.as_slice(), [WordPart::Literal(name)] if name == "local")
        );
        let mut argv = Vec::new();
        for (index, word) in words.iter().enumerate() {
            let assignment = local_assignment
                && index > 0
                && matches!(
                    word.parts.first(),
                    Some(WordPart::Literal(text)) if text.split_once('=').is_some_and(|(name, _)| is_variable_name(name))
                );
            let expanded = if assignment {
                self.expand_word_with(word, ExpansionOutput::Unsplit)?
            } else {
                self.expand_word(word)?
            };
            if let [WordPart::Asset(number)] = word.parts.as_slice() {
                argv.push(crate::Arg::Asset(*number));
            } else {
                argv.extend(expanded.into_iter().map(crate::Arg::Text));
            }
        }
        Ok(argv)
    }

    fn execute_simple_command(
        &mut self,
        command: &SimpleCommand,
        input: StageInput,
    ) -> Result<Executed, FatalError> {
        self.budget.charge_step()?;

        let argv = match self.expand_command_argv(&command.words) {
            Ok(argv) => argv,
            Err(failure) => {
                return self.failed(failure);
            }
        };

        // `--help` wins only when the script wrote it as the command's one bare, unquoted word.
        let literal_help = argv.len() == 2
            && matches!(
                command.words.as_slice(),
                [_, Word { parts }] if matches!(parts.as_slice(), [WordPart::Literal(only)] if only == "--help")
            );

        let transient = !argv.is_empty();
        let mut restore = None;
        let mut assignment_status = ExitCode::SUCCESS;
        self.last_substitution_status = ExitCode::SUCCESS;
        for assignment in &command.assignments {
            let value = match self.assignment_value(&assignment.value) {
                Ok(value) => value,
                Err(failure) => {
                    self.restore_all(restore);
                    return self.failed(failure);
                }
            };
            assignment_status = self.last_substitution_status;
            let assigned = if transient {
                self.assign_prefix(&assignment.name, value, &mut restore)
            } else {
                self.assign(&assignment.name, value)
            };
            if let Err(limit) = assigned {
                self.restore_all(restore);
                return Err(limit.into());
            }
        }

        let mut here_doc_charges = Vec::new();
        let start = self.expansion_charges.len();
        let input = match &command.here_doc {
            None => input,
            Some(body) => match self.expand_preserved_fields(&body.parts, ExpansionOutput::Unsplit)
            {
                Ok(fields) => {
                    let text = fields.join(" ");
                    here_doc_charges = self.expansion_charges.split_off(start);
                    match self.budget.charge_value_bytes(text.len() as u64) {
                        Ok(charge) => here_doc_charges.push(charge),
                        Err(limit) => {
                            self.restore_all(restore);
                            return Err(limit.into());
                        }
                    }
                    StageInput::Piped(PipeReader::from_bytes(
                        text.into_bytes(),
                        None,
                        crate::interp::telemetry::stage_read,
                    ))
                }
                Err(failure) => {
                    self.restore_all(restore);
                    return self.failed(failure);
                }
            },
        };

        let (stdout, stderr) = match self.resolve_redirects(&command.redirects) {
            Ok(sinks) => sinks,
            Err(failure) => {
                self.restore_all(restore);
                return self.failed(failure);
            }
        };

        self.open_buffers(&[&stdout, &stderr])?;
        if argv.is_empty() {
            let status = if command.assignments.is_empty() {
                ExitCode::SUCCESS
            } else {
                assignment_status
            };
            return Ok(Executed::Result(CommandResult::status(status)));
        }

        let capturing = stderr != Sink::Diagnostics;
        if capturing {
            self.stderr_capture.push(StderrCapture::new(&self.limits));
        }
        let stage_stdout = match stdout {
            Sink::Value => None,
            Sink::Diagnostics | Sink::Discard | Sink::Buffer { .. } => self.stdout.take(),
        };
        let redirected = self.stdout_redirected;
        self.stdout_redirected |= stdout != Sink::Value;
        let active_buffer = (stdout != Sink::Value)
            .then(|| self.active_buffer.take())
            .flatten();
        if let Sink::Buffer { name, .. } = &stdout {
            self.active_buffer = Some((name.clone(), self.captures.len()));
        }
        let executed = self.run_argv(&argv, input, literal_help, &stdout);
        if stdout != Sink::Value {
            self.active_buffer = active_buffer;
        }
        self.stdout_redirected = redirected;
        if let Some(writer) = stage_stdout {
            self.stdout = Some(writer);
        }
        // The stderr capture is removed before the fallible step that follows can return early, so
        // a fatal error can never leave a capture installed that silently swallows the rest of the
        // script's diagnostics.
        let mut diagnostics = capturing
            .then(|| self.stderr_capture.pop())
            .flatten()
            .map(StderrCapture::finish)
            .unwrap_or_default();
        self.restore_all(restore);
        drop(here_doc_charges);
        let executed = executed?;
        let Executed::Result(mut result) = executed else {
            self.route_diagnostics(diagnostics, &stderr)?;
            return Ok(executed);
        };

        if stderr == Sink::Value {
            merge_diagnostics(&mut result, diagnostics);
            diagnostics = Vec::new();
        }

        // A redirect truncates its target once, when it is set up, not on every write, so combining
        // a value redirect with a merged stderr redirect cannot let one silently overwrite the
        // other depending on order.
        self.route_diagnostics(diagnostics, &stderr)?;

        self.redirect_result(result, &stdout)
    }

    fn redirect_result(
        &mut self,
        result: CommandResult,
        sink: &Sink,
    ) -> Result<Executed, FatalError> {
        match sink {
            Sink::Value => Ok(Executed::Result(result)),
            Sink::Discard | Sink::Diagnostics | Sink::Buffer { .. } => {
                let status = result.status;
                self.emit_to(result, sink)?;
                Ok(Executed::Result(CommandResult::status(status)))
            }
        }
    }

    fn open_buffers(&mut self, sinks: &[&Sink]) -> Result<(), LimitExceeded> {
        for sink in sinks {
            if let Sink::Buffer {
                name,
                append: false,
            } = sink
            {
                self.unshare_buffers()?;
                Arc::make_mut(&mut self.buffers).insert(name.clone(), Vec::new());
                self.buffer_charges.remove(name);
            }
        }
        Ok(())
    }

    fn resolve_redirects(
        &mut self,
        redirects: &[Redirect],
    ) -> Result<(Sink, Sink), CommandFailure> {
        let mut stdout = Sink::Value;
        let mut stderr = Sink::Diagnostics;
        for redirect in redirects {
            let sink = match &redirect.target {
                RedirectTarget::Stream(Stream::Stdout) => stdout.clone(),
                RedirectTarget::Stream(Stream::Stderr) => stderr.clone(),
                RedirectTarget::Stream(Stream::Both) => {
                    unreachable!("the lexer never produces `&` as a duplication target")
                }
                RedirectTarget::Buffer { append, target } => {
                    let expanded = self.expand_word(target)?;
                    let [name] = expanded.as_slice() else {
                        return Err(CommandFailure::usage(
                            "dekopon-shell: a redirection target must expand to exactly one buffer name",
                        ));
                    };
                    if name == DEV_NULL {
                        Sink::Discard
                    } else {
                        Sink::Buffer {
                            name: name.clone(),
                            append: *append,
                        }
                    }
                }
            };
            match redirect.source {
                Stream::Stdout => stdout = sink,
                Stream::Stderr => stderr = sink,
                Stream::Both => {
                    stdout = sink.clone();
                    stderr = sink;
                }
            }
        }
        Ok((stdout, stderr))
    }

    fn route_diagnostics(
        &mut self,
        diagnostics: Vec<Diagnostic>,
        sink: &Sink,
    ) -> Result<(), FatalError> {
        if diagnostics.is_empty() {
            return Ok(());
        }
        match sink {
            Sink::Discard => {}
            Sink::Diagnostics | Sink::Value => {
                for diagnostic in diagnostics {
                    self.write_diagnostic(diagnostic);
                }
            }
            Sink::Buffer { name, .. } => {
                let name = name.clone();
                for diagnostic in diagnostics {
                    self.append_buffer(&name, diagnostic.into_result())?;
                }
            }
        }
        Ok(())
    }

    fn restore_all(&mut self, restore: Option<usize>) {
        if let Some(depth) = restore {
            self.frames.truncate(depth);
        }
    }

    fn append_buffer(&mut self, name: &str, result: CommandResult) -> Result<(), LimitExceeded> {
        if result.value.is_null() {
            return Ok(());
        }
        let suppress_newline = result.suppress_newline;
        let text = match result.value {
            Value::String(text) => text,
            value @ (Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::Array(_)
            | Value::Object(_)) => display(&value),
        };
        self.append_buffer_bytes(name, text.as_bytes())?;
        if !suppress_newline {
            self.append_buffer_bytes(name, b"\n")?;
        }
        Ok(())
    }

    fn append_buffer_bytes(&mut self, name: &str, bytes: &[u8]) -> Result<(), LimitExceeded> {
        append_buffer_bytes(
            &self.budget,
            BufferStorage {
                buffers: &mut self.buffers,
                charges: &mut self.buffer_charges,
                shared_charges: &mut self.shared_charges,
            },
            name,
            bytes,
        )
    }

    fn append_charged_buffer_bytes(
        &mut self,
        name: &str,
        bytes: &[u8],
        charges: Vec<dekopon_process::RetainedBytes>,
    ) -> Result<(), LimitExceeded> {
        append_charged_buffer(
            &self.budget,
            BufferStorage {
                buffers: &mut self.buffers,
                charges: &mut self.buffer_charges,
                shared_charges: &mut self.shared_charges,
            },
            name,
            bytes,
            charges,
        )
    }

    fn run_argv(
        &mut self,
        argv: &[crate::Arg],
        input: StageInput,
        literal_help: bool,
        stdout_sink: &Sink,
    ) -> Result<Executed, FatalError> {
        let typed_arguments = &argv[1..];
        let argv = argv.iter().map(ToString::to_string).collect::<Vec<_>>();
        let command = argv[0].as_str();
        let arguments = &argv[1..];

        let (kind, resolution) = if ControlWord::lookup(command).is_some() {
            (CommandKind::Control, None)
        } else {
            let resolution = builtins::resolve(command, &self.function_names, self.invoker);
            (CommandKind::of(&resolution), Some(resolution))
        };
        self.counters.charge(kind);
        let span = telemetry::command_span(
            command,
            kind,
            arguments.len(),
            self.invoker.participant_handle(),
        );
        let _entered = span.enter();
        let traced = !span.is_disabled();
        if traced {
            telemetry::record_arguments(&span, arguments);
        }

        let executed = self.dispatch_command(
            command,
            arguments,
            resolution,
            typed_arguments,
            input,
            literal_help,
            stdout_sink,
        );
        let (status, outcome) = match &executed {
            Ok(Executed::Result(result)) => {
                (result.status, telemetry::outcome_label(result.status))
            }
            Ok(Executed::Flow(flow)) => {
                let status = match flow {
                    Flow::Return(status) | Flow::Exit(status) => *status,
                    Flow::Normal | Flow::Break(_) | Flow::Continue(_) => ExitCode::SUCCESS,
                };
                (status, telemetry::outcome_label(status))
            }
            Err(fatal) => (
                telemetry::fatal_exit_code(fatal),
                telemetry::fatal_outcome(fatal),
            ),
        };
        span.record("shell.command.exit_code", status.get());
        span.record("outcome", outcome);
        if traced && let Ok(Executed::Result(result)) = &executed {
            telemetry::record_output(&span, &result.value);
        }
        if traced {
            telemetry::record_stage_command(&span);
        }
        self.counters.record_status(status);
        executed
    }

    fn run_stream(
        &mut self,
        name: &str,
        parsed: Result<(ParsedStream<'_>, Option<String>), CommandFailure>,
        input: StageInput,
        stdout_sink: &Sink,
    ) -> Result<Executed, FatalError> {
        let (parsed, operand) = match parsed {
            Ok(parsed) => parsed,
            Err(failure) => return self.failed(failure),
        };
        let mode = match &parsed {
            ParsedStream::Cat([]) => {
                if self.stdout.is_none()
                    && self.captures.is_empty()
                    && self.stdout_redirected
                    && self.active_buffer_name().is_none()
                    && stdout_sink != &Sink::Diagnostics
                {
                    return Ok(Executed::Result(
                        CommandResult::value(Value::Null).without_newline(),
                    ));
                }
                let outcome = self.copy_stdin(input, stdout_sink);
                return self.completed(outcome);
            }
            ParsedStream::Cat(_) => OutputMode::Named,
            ParsedStream::Base64(_) => OutputMode::Base64,
            ParsedStream::Awk(_) => OutputMode::Awk,
            ParsedStream::Lines(_)
            | ParsedStream::Text(_)
            | ParsedStream::Extra(_)
            | ParsedStream::Jq(_) => OutputMode::Lines,
        };
        let mut output = StageOut::new(self, stdout_sink, mode);
        let mut budget = self.budget.fork();
        let outcome = match parsed {
            ParsedStream::Cat(arguments) => {
                builtins::misc::cat_stream(arguments, |name| output.named(self, name))
                    .map(|()| ExitCode::SUCCESS)
            }
            parsed @ (ParsedStream::Base64(_)
            | ParsedStream::Lines(_)
            | ParsedStream::Text(_)
            | ParsedStream::Extra(_)
            | ParsedStream::Jq(_)
            | ParsedStream::Awk(_)) => {
                let (stdin, mut target) = WriteTarget::split(self);
                match target.input(stdin, name, operand.as_deref(), input) {
                    Ok(input) => parsed.run(input, &mut budget, &mut |bytes| {
                        output.write(&mut target, bytes)
                    }),
                    Err(failure) => Err(failure),
                }
            }
        };
        output.flush_diagnostics(self);
        let status = outcome.or_else(|failure| self.absorb(failure))?;
        let outcome = output.finish(self, status);
        self.completed(outcome)
    }

    fn failed(&mut self, failure: CommandFailure) -> Result<Executed, FatalError> {
        self.absorb(failure)
            .map(|status| Executed::Result(CommandResult::status(status)))
    }

    fn completed(
        &mut self,
        outcome: Result<CommandResult, CommandFailure>,
    ) -> Result<Executed, FatalError> {
        match outcome {
            Ok(result) => Ok(Executed::Result(result)),
            Err(failure) => self.failed(failure),
        }
    }

    fn append_named_buffer(
        &mut self,
        source: &str,
        destination: &str,
    ) -> Result<(), LimitExceeded> {
        self.unshare_buffers()?;
        let length = self.buffers.get(source).map_or(0, Vec::len);
        let charge = self.budget.charge_value_bytes(length as u64)?;
        let buffers = Arc::make_mut(&mut self.buffers);
        if source == destination {
            if let Some(bytes) = buffers.get_mut(source) {
                bytes.extend_from_within(..length);
            }
        } else {
            let mut target = buffers.remove(destination).unwrap_or_default();
            if let Some(bytes) = buffers.get(source) {
                target.extend_from_slice(bytes);
            }
            buffers.insert(destination.to_owned(), target);
        }
        self.buffer_charges
            .entry(destination.to_owned())
            .or_default()
            .push(charge);
        Ok(())
    }

    fn run_simple_builtin(
        &mut self,
        run: builtins::Run,
        arguments: &[String],
        input: StageInput,
    ) -> Result<Executed, FatalError> {
        drop(input);
        let mut context = BuiltinContext {
            invoker: self.invoker,
            budget: &mut self.budget,
            started_jobs: &self.jobs.started,
        };
        let outcome = run(&mut context, arguments, None);
        self.completed(outcome)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "each argument is an independent piece of run_argv's execution context, not a \
                  group that wants its own type"
    )]
    fn dispatch_command(
        &mut self,
        command: &str,
        arguments: &[String],
        resolution: Option<Resolution>,
        typed_arguments: &[crate::Arg],
        input: StageInput,
        literal_help: bool,
        stdout_sink: &Sink,
    ) -> Result<Executed, FatalError> {
        let Some(resolution) = resolution else {
            if let Some(executed) = self.run_control_word(command, arguments, input)? {
                return Ok(executed);
            }
            self.write_line(&format!("dekopon-shell: {command}: command not found"));
            return Ok(Executed::Result(CommandResult::status(ExitCode::NOT_FOUND)));
        };

        match resolution {
            Resolution::Rejected(reason) => Err(FatalError::Unsupported(reason.to_owned())),
            Resolution::Function => self.call_function(command, arguments, input, stdout_sink),
            Resolution::Builtin(builtin) => builtin.run(
                self,
                arguments,
                input,
                if literal_help {
                    BuiltinMode::Help
                } else {
                    BuiltinMode::Run
                },
                stdout_sink,
            ),
            Resolution::ProviderCommand => {
                if literal_help {
                    if let Some(page) = self.invoker.command_word_help().get(command) {
                        return self.emit_rendered(page.clone(), String::new(), 0);
                    }
                    return self.emit_rendered(
                        String::new(),
                        format!("{command}: help is unavailable\n"),
                        2,
                    );
                }
                let call = crate::RunCall {
                    word: command.to_owned(),
                    argv: typed_arguments.to_vec(),
                };
                let outcome = self.run_provider(call, input, stdout_sink);
                self.completed(outcome)
            }
            Resolution::NotFound => {
                self.write_line(&format!("dekopon-shell: {command}: command not found"));
                Ok(Executed::Result(CommandResult::status(ExitCode::NOT_FOUND)))
            }
        }
    }

    /// The call is charged and the deadline re-read on both sides, since capability calls are
    /// wall-clock expensive but step-cheap. The provider's stdout is copied to this stage's sink
    /// while the call runs, and dropping that copy closes the provider's stdout. An unpiped stage
    /// outside any compound's input reads the script's own stdin when it has one, and whatever the
    /// provider left unread stays there for the next command.
    fn run_provider(
        &mut self,
        call: crate::RunCall,
        input: StageInput,
        sink: &Sink,
    ) -> Result<CommandResult, CommandFailure> {
        self.budget.check_deadline()?;
        let mut capability = call.word.clone();
        let unopened = |_error: std::io::Error| {
            CommandFailure::failed(format!(
                "{capability}: failed: its standard streams could not be opened"
            ))
        };
        let (stdout, provider_stdout) = UnixStream::pair().map_err(unopened)?;
        let output = PipeReader::from_socket(stdout, None, crate::interp::telemetry::stage_read)
            .map_err(unopened)?;
        let stdin = match input {
            StageInput::Inherited if !self.provider_reads_script_stdin() => None,
            StageInput::Piped(_) | StageInput::Inherited => {
                Some(UnixStream::pair().map_err(unopened)?)
            }
        };
        let (feed, provider_stdin) = match stdin {
            Some((socket, provider_stdin)) => {
                let (reader, inherited) = match input {
                    StageInput::Piped(reader) => (reader, false),
                    StageInput::Inherited => (
                        self.stdin
                            .last_mut()
                            .map(std::mem::take)
                            .unwrap_or_default(),
                        true,
                    ),
                };
                (
                    Some((reader, socket, inherited)),
                    Some(OwnedFd::from(provider_stdin)),
                )
            }
            None => (None, None),
        };
        let streams = crate::Streams {
            stdin: provider_stdin,
            stdout: OwnedFd::from(provider_stdout),
        };
        let (call_permit, feed) = self.call_permits(feed)?;
        self.budget.charge_capability_call()?;
        let invoker = self.invoker;
        let tree = self.budget.tree().clone();
        let feeder_budget = self.budget.fork();
        let span = tracing::Span::current();
        let dispatcher = tracing::dispatcher::get_default(Clone::clone);
        let finished = AtomicBool::new(false);
        #[expect(
            clippy::disallowed_methods,
            reason = "owner: this call, which joins the call and feeder threads before returning; bound: their ShellThreads permits"
        )]
        let joined = thread::scope(|scope| {
            let call = call_permit.spawn(scope, CALL_THREAD, || {
                tracing::dispatcher::with_default(&dispatcher, || {
                    span.in_scope(|| invoker.run(call, streams, &tree))
                })
            })?;
            let feeder = feed.map(|((mut reader, socket, inherited), permit)| {
                let (feeder_budget, finished) = (&feeder_budget, &finished);
                permit.spawn(scope, FEED_THREAD, move || {
                    pump_stdin(&mut reader, socket, feeder_budget, finished);
                    inherited.then_some(reader)
                })
            });
            let copied = self.copy_stdin(StageInput::Piped(output), sink);
            let result = call.join();
            finished.store(true, atomic::Ordering::Relaxed);
            let fed = feeder.map(|spawned| spawned.map(ScopedJoinHandle::join));
            let result = result.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            let fed = match fed {
                None => Fed::Unpiped,
                Some(Err(error)) => Fed::Unstarted(error),
                Some(Ok(Err(panic))) => std::panic::resume_unwind(panic),
                Some(Ok(Ok(unread))) => Fed::Joined(unread),
            };
            Ok::<_, std::io::Error>((result, copied, fed))
        });
        let (result, copied, fed) = match joined {
            Ok(joined) => joined,
            Err(error) => {
                self.budget.refund_capability_call();
                return Err(CommandFailure::failed(format!(
                    "{capability}: failed: its call thread could not start: {error}"
                )));
            }
        };
        let result = match result {
            crate::RunResult::Invoked {
                capability: reported,
                result,
                ..
            } => {
                capability = reported;
                result
            }
            crate::RunResult::Uncertain(result) => result,
            crate::RunResult::Returned(result) => {
                self.budget.refund_capability_call();
                result
            }
        };
        if let (Some(slot), Some(reader)) = (self.stdin.last_mut(), fed.settle(&capability)?) {
            *slot = reader;
        }
        self.budget.check_deadline()?;
        let mut copied = copied?;
        let status = ExitCode::from(&result);
        match result {
            CapabilityCallResult::Succeeded => {}
            CapabilityCallResult::SucceededWithStderr(stderr)
            | CapabilityCallResult::Exited { stderr, .. } => {
                for line in stderr.lines() {
                    self.write_line(line);
                }
            }
            CapabilityCallResult::Denied { reason } => {
                return Err(CommandFailure::Status {
                    message: format!("{capability}: denied: {reason}"),
                    status,
                });
            }
            CapabilityCallResult::Failed { error, detail } => {
                let message = match detail {
                    Some(detail) => format!("{capability}: failed: {error}: {detail}"),
                    None => format!("{capability}: failed: {error}"),
                };
                return Err(CommandFailure::Status { message, status });
            }
            CapabilityCallResult::NotFound => {
                return Err(CommandFailure::Status {
                    message: format!("{capability}: capability not found"),
                    status,
                });
            }
        }
        copied.status = status;
        Ok(copied)
    }

    fn call_permits<F>(
        &self,
        feed: Option<F>,
    ) -> Result<(ThreadPermit, Option<(F, ThreadPermit)>), LimitExceeded> {
        let threads = self.budget.tree().shell_threads();
        Ok(match feed {
            Some(feed) => {
                let [call, feeder] = threads.reserve()?;
                (call, Some((feed, feeder)))
            }
            None => {
                let [call] = threads.reserve()?;
                (call, None)
            }
        })
    }

    fn run_control_word(
        &mut self,
        command: &str,
        arguments: &[String],
        input: StageInput,
    ) -> Result<Option<Executed>, FatalError> {
        let Some(control) = ControlWord::lookup(command) else {
            return Ok(None);
        };
        let command = control.name();
        let executed = match control {
            ControlWord::Break | ControlWord::Continue => {
                let level = match parse_level(command, arguments) {
                    Ok(level) => level,
                    Err(failure) => return self.failed(failure).map(Some),
                };
                Executed::Flow(if command == "break" {
                    Flow::Break(level)
                } else {
                    Flow::Continue(level)
                })
            }
            ControlWord::Set => match self.apply_set_options(arguments) {
                Ok(()) => Executed::Result(CommandResult::status(ExitCode::SUCCESS)),
                Err(failure) => self.failed(failure)?,
            },
            ControlWord::Return => {
                if !self
                    .frames
                    .iter()
                    .any(|frame| frame.kind == FrameKind::Function)
                {
                    self.write_line("dekopon-shell: return: only valid inside a function");
                    return Ok(Some(Executed::Result(CommandResult::status(
                        ExitCode::SYNTAX,
                    ))));
                }
                let status = match parse_status(command, arguments, self.last_status) {
                    Ok(status) => status,
                    Err(failure) => return self.failed(failure).map(Some),
                };
                Executed::Flow(Flow::Return(status))
            }
            ControlWord::Exit => {
                let status = match parse_status(command, arguments, self.last_status) {
                    Ok(status) => status,
                    Err(failure) => return self.failed(failure).map(Some),
                };
                Executed::Flow(Flow::Exit(status))
            }
            ControlWord::Read => match self.run_read(arguments, input) {
                Ok(result) => Executed::Result(result),
                Err(failure) => self.failed(failure)?,
            },
            ControlWord::Local => {
                if !self
                    .frames
                    .iter()
                    .any(|frame| frame.kind == FrameKind::Function)
                {
                    self.write_line("dekopon-shell: local: only valid inside a function");
                    return Ok(Some(Executed::Result(CommandResult::status(
                        ExitCode::SYNTAX,
                    ))));
                }
                for argument in arguments {
                    match argument.split_once('=') {
                        Some((name, text)) => {
                            self.declare_local(name, Some(value::scalar_from_token(text)))?;
                        }
                        None => self.declare_local(argument, None)?,
                    }
                }
                Executed::Result(CommandResult::status(ExitCode::SUCCESS))
            }
            ControlWord::Shift => {
                let Some(frame) = self
                    .frames
                    .iter_mut()
                    .rev()
                    .find(|frame| frame.kind == FrameKind::Function)
                else {
                    self.write_line("dekopon-shell: shift: only valid inside a function");
                    return Ok(Some(Executed::Result(CommandResult::status(
                        ExitCode::SYNTAX,
                    ))));
                };
                let count = match parse_shift_count(arguments) {
                    Ok(count) => count,
                    Err(failure) => return self.failed(failure).map(Some),
                };
                if count > frame.positional.len() {
                    Executed::Result(CommandResult::status(ExitCode::FAILURE))
                } else {
                    frame.positional.drain(..count);
                    frame._positional_charges.drain(..count);
                    Executed::Result(CommandResult::status(ExitCode::SUCCESS))
                }
            }
            ControlWord::Unset => {
                for name in arguments {
                    if let Some(index) = self
                        .frames
                        .iter()
                        .rposition(|frame| frame.locals.contains_key(name))
                    {
                        let current = self.frames[index].kind == FrameKind::Function
                            && !self.frames[index + 1..]
                                .iter()
                                .any(|frame| frame.kind == FrameKind::Function);
                        let frame = &mut self.frames[index];
                        if current {
                            frame.locals.insert(name.clone(), None);
                        } else {
                            frame.locals.remove(name);
                        }
                        frame.local_charges.remove(name);
                    } else {
                        self.unshare_globals()?;
                        Arc::make_mut(&mut self.globals).remove(name);
                        self.global_charges.remove(name);
                    }
                }
                Executed::Result(CommandResult::status(ExitCode::SUCCESS))
            }
            ControlWord::Colon => Executed::Result(CommandResult::status(ExitCode::SUCCESS)),
        };
        Ok(Some(executed))
    }

    fn call_function(
        &mut self,
        name: &str,
        arguments: &[String],
        input: StageInput,
        stdout_sink: &Sink,
    ) -> Result<Executed, FatalError> {
        let Some(body) = self.functions.get(name).cloned() else {
            self.write_line(&format!("dekopon-shell: {name}: command not found"));
            return Ok(Executed::Result(CommandResult::status(ExitCode::NOT_FOUND)));
        };

        self.budget.charge_step()?;
        let flow = self.nested(|this| {
            let positional = arguments
                .iter()
                .map(|argument| Value::String(argument.clone()))
                .collect::<Vec<_>>();
            let positional_charges = positional
                .iter()
                .map(|argument| this.budget.charge_value_bytes(value_bytes(argument)))
                .collect::<Result<Vec<_>, _>>()?;
            this.frames.push(Frame {
                kind: FrameKind::Function,
                locals: BTreeMap::new(),
                positional,
                local_charges: BTreeMap::new(),
                _positional_charges: positional_charges,
            });
            let piped = match input {
                StageInput::Piped(reader) => {
                    this.stdin.push(reader);
                    true
                }
                StageInput::Inherited => false,
            };
            let saved_stdout = (*stdout_sink != Sink::Value)
                .then(|| this.stdout.take())
                .flatten();
            let previous_discard = this.discard_capture_depth;
            let previous_diagnostics = this.diagnostics_depth;
            if *stdout_sink == Sink::Diagnostics {
                this.diagnostics_depth = Some(this.captures.len());
            }
            match stdout_sink {
                Sink::Discard => this.discard_capture_depth = Some(this.captures.len()),
                Sink::Buffer { .. } | Sink::Diagnostics => this.discard_capture_depth = None,
                Sink::Value => {}
            }
            let flow = this.execute_program(&body);
            if *stdout_sink != Sink::Value {
                this.stdout = saved_stdout;
            }
            this.discard_capture_depth = previous_discard;
            this.diagnostics_depth = previous_diagnostics;
            if piped {
                this.stdin.pop();
            }
            this.frames.pop();
            Ok::<_, FatalError>(flow)
        })?;

        let mut result = CommandResult::status(self.last_status);
        retain_value(&self.budget, &result.value, &mut result.retained)?;
        Ok(match flow? {
            Flow::Return(status) => {
                result.status = status;
                Executed::Result(result)
            }
            Flow::Exit(status) => Executed::Flow(Flow::Exit(status)),
            Flow::Normal | Flow::Break(_) | Flow::Continue(_) => Executed::Result(result),
        })
    }

    fn run_xargs(
        &mut self,
        arguments: &[String],
        input: StageInput,
        stdout_sink: &Sink,
    ) -> Result<Executed, FatalError> {
        let template = match xargs::parse(arguments) {
            Ok(template) => template,
            Err(failure) => {
                return self.failed(failure);
            }
        };
        let mut piped = match input {
            StageInput::Piped(reader) => Some(reader),
            StageInput::Inherited => None,
        };
        let mut status = ExitCode::SUCCESS;
        loop {
            let line = match piped.as_mut().or_else(|| self.stdin.last_mut()) {
                Some(reader) => reader.read_line(self.budget.tree())?,
                None => None,
            };
            let Some(line) = line else { break };
            self.budget.charge_step()?;
            let (mut bytes, _charges) = line.into_parts();
            if bytes.last() == Some(&b'\n') {
                bytes.pop();
            }
            let text = match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    status = self.absorb(CommandFailure::failed(
                        "xargs: standard input is not valid UTF-8 text",
                    ))?;
                    break;
                }
            };
            let Some(bytes) = template.expanded_bytes(&text) else {
                status = self.absorb(CommandFailure::failed(
                    "xargs: expanded arguments exceed the retained budget",
                ))?;
                break;
            };
            let _argv_charge = self.budget.charge_value_bytes(bytes)?;
            let invocation = template.expand(&text);
            {
                match self.run_argv(
                    &invocation
                        .into_iter()
                        .map(crate::Arg::Text)
                        .collect::<Vec<_>>(),
                    StageInput::Piped(PipeReader::default()),
                    false,
                    stdout_sink,
                )? {
                    Executed::Flow(flow) => return Ok(Executed::Flow(flow)),
                    Executed::Result(result) => {
                        if result.status != ExitCode::SUCCESS {
                            status = result.status;
                        }
                        self.emit_to(result, stdout_sink)?;
                        if self.reader_gone {
                            return Ok(Executed::Result(CommandResult::status(status)));
                        }
                    }
                }
            }
        }
        Ok(Executed::Result(CommandResult::status(status)))
    }

    /// Assigning a variable from a whole command substitution or another whole variable keeps its
    /// structured value instead of flattening it to text, a deliberate deviation from real shells
    /// that lets a script later index into what it captured.
    fn assignment_value(&mut self, word: &Word) -> Result<CommandResult, CommandFailure> {
        if let [WordPart::CommandSubstitution(program)] = word.parts.as_slice() {
            let mut result = self.run_substitution(program)?;
            if let Value::String(text) = &result.value {
                let trimmed = text.trim();
                if (trimmed.starts_with('{') || trimmed.starts_with('['))
                    && let Ok(parsed) = serde_json::from_str(trimmed)
                {
                    result.value = parsed;
                    retain_value(&self.budget, &result.value, &mut result.retained)?;
                }
            }
            return Ok(result);
        }
        let start = self.expansion_charges.len();
        let value = match word.parts.as_slice() {
            [WordPart::Parameter(parameter)] => {
                self.parameter_value_with(parameter, ExpansionOutput::Unsplit)?
            }
            _ => Value::String(
                self.expand_preserved_fields(&word.parts, ExpansionOutput::Unsplit)?
                    .join(" "),
            ),
        };
        let mut result = CommandResult::value(value);
        result.retained = self.expansion_charges.split_off(start);
        Ok(result)
    }

    fn expansion_value(
        &mut self,
        word: &Word,
        output: ExpansionOutput,
    ) -> Result<Value, CommandFailure> {
        match word.parts.as_slice() {
            [WordPart::Parameter(parameter)] => self.parameter_value_with(parameter, output),
            [WordPart::CommandSubstitution(_)] => {
                let result = self.assignment_value(word)?;
                self.expansion_charges.extend(result.retained);
                Ok(result.value)
            }
            _ => Ok(Value::String(
                self.expand_word_with(word, output)?.join(" "),
            )),
        }
    }

    fn expand_word(&mut self, word: &Word) -> Result<Vec<String>, CommandFailure> {
        self.expand_word_with(word, ExpansionOutput::Fields)
    }

    fn expand_word_with(
        &mut self,
        word: &Word,
        output: ExpansionOutput,
    ) -> Result<Vec<String>, CommandFailure> {
        if let [WordPart::Literal(text) | WordPart::SingleQuoted(text)] = word.parts.as_slice() {
            return Ok(vec![text.clone()]);
        }
        match output {
            ExpansionOutput::Fields | ExpansionOutput::Value => {
                let mut spans = Vec::new();
                for part in &word.parts {
                    self.collect_field_part(part, &mut spans, output, FieldContext::Fields)?;
                }
                let resplit = word.parts.iter().any(|part| match part {
                    WordPart::Parameter(_)
                    | WordPart::CommandSubstitution(_)
                    | WordPart::Arithmetic(_) => true,
                    WordPart::DoubleQuoted(parts) => {
                        (word.parts.len() > 1 || parts.len() > 1)
                            && parts.iter().any(|part| match part {
                                WordPart::Parameter(parameter) => splits_inside_quotes(parameter),
                                WordPart::Asset(_)
                                | WordPart::Literal(_)
                                | WordPart::SingleQuoted(_)
                                | WordPart::DoubleQuoted(_)
                                | WordPart::CommandSubstitution(_)
                                | WordPart::Arithmetic(_) => false,
                            })
                    }
                    WordPart::Asset(_) | WordPart::Literal(_) | WordPart::SingleQuoted(_) => false,
                });
                if resplit {
                    for span in &mut spans {
                        if let SpanContent::Text(_, protection @ Protection::Quoted) =
                            &mut span.content
                        {
                            *protection = Protection::Checked;
                        }
                    }
                }
                self.split_spans(spans)
            }
            ExpansionOutput::Legacy => self.expand_preserved_word(word, PreservedExpansion::Legacy),
            ExpansionOutput::Unsplit => {
                self.expand_preserved_word(word, PreservedExpansion::Unsplit)
            }
        }
    }

    fn expand_preserved_word(
        &mut self,
        word: &Word,
        output: PreservedExpansion,
    ) -> Result<Vec<String>, CommandFailure> {
        let mut fields = Vec::new();
        let mut separated = Boundary::Start;
        for part in &word.parts {
            match part {
                WordPart::Literal(text) | WordPart::SingleQuoted(text) => {
                    append_field(&mut fields, &mut separated, text)
                }
                WordPart::Asset(number) => {
                    append_field(&mut fields, &mut separated, &number.to_string())
                }
                WordPart::Arithmetic(expression) => {
                    let text = render_number(self.evaluate_arithmetic(expression)?);
                    append_field(&mut fields, &mut separated, &text);
                }
                WordPart::DoubleQuoted(parts) => {
                    let mut expanded = self.expand_quoted_fields(parts)?.into_iter();
                    if let Some(first) = expanded.next() {
                        append_field(&mut fields, &mut separated, &first);
                    }
                    fields.extend(expanded);
                }
                WordPart::Parameter(parameter) => {
                    let output = match output {
                        PreservedExpansion::Legacy => ExpansionOutput::Legacy,
                        PreservedExpansion::Unsplit => ExpansionOutput::Unsplit,
                    };
                    let value = self.parameter_value_with(parameter, output)?;
                    append_unsplit_value(&mut fields, &mut separated, &value);
                }
                WordPart::CommandSubstitution(program) => {
                    let result = self.run_substitution(program)?;
                    match (&result.value, output) {
                        (Value::String(text), PreservedExpansion::Legacy) => {
                            if !text.is_empty() {
                                let mut lines = text.split('\n');
                                if let Some(first) = lines.next() {
                                    append_field(&mut fields, &mut separated, first);
                                }
                                fields.extend(lines.map(str::to_owned));
                            }
                        }
                        (
                            Value::Null
                            | Value::Bool(_)
                            | Value::Number(_)
                            | Value::Array(_)
                            | Value::Object(_),
                            PreservedExpansion::Legacy,
                        )
                        | (
                            Value::Null
                            | Value::Bool(_)
                            | Value::Number(_)
                            | Value::String(_)
                            | Value::Array(_)
                            | Value::Object(_),
                            PreservedExpansion::Unsplit,
                        ) => append_unsplit_value(&mut fields, &mut separated, &result.value),
                    }
                    self.expansion_charges.extend(result.retained);
                }
            }
        }
        Ok(fields)
    }

    fn record_text(
        &mut self,
        spans: &mut Vec<ExpansionSpan>,
        text: &str,
        protection: Protection,
    ) -> Result<(), CommandFailure> {
        self.budget.charge_step()?;
        let charge = self
            .budget
            .charge_value_bytes((text.len() + size_of::<ExpansionSpan>()) as u64)?;
        spans.push(ExpansionSpan {
            content: SpanContent::Text(text.to_owned(), protection),
            charge,
            retained: Vec::new(),
        });
        Ok(())
    }

    fn record_capture(
        &mut self,
        spans: &mut Vec<ExpansionSpan>,
        result: CommandResult,
        protection: Protection,
    ) -> Result<(), CommandFailure> {
        let text = match result.value {
            Value::String(text) => text,
            value @ (Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::Array(_)
            | Value::Object(_)) => quoted_text(&value),
        };
        let held = result
            .retained
            .iter()
            .map(dekopon_process::RetainedBytes::bytes)
            .sum::<u64>();
        let charge = self.budget.charge_value_bytes(
            size_of::<ExpansionSpan>() as u64 + (text.len() as u64).saturating_sub(held),
        )?;
        spans.push(ExpansionSpan {
            content: SpanContent::Text(text, protection),
            charge,
            retained: result.retained,
        });
        Ok(())
    }

    fn expand_case_pattern(&mut self, word: &Word) -> Result<Vec<PatternChar>, CommandFailure> {
        let mut spans = Vec::new();
        for part in &word.parts {
            self.collect_field_part(
                part,
                &mut spans,
                ExpansionOutput::Unsplit,
                FieldContext::CasePattern,
            )?;
        }
        let mut characters = Vec::new();
        for span in spans {
            let (text, protection) = match span.content {
                SpanContent::Text(text, protection) => (text, protection),
                SpanContent::Elements(items, _) => {
                    (quoted_text(&Value::Array(items)), Protection::Unquoted)
                }
                SpanContent::Boundary(_) => break,
                SpanContent::SplitSpace => continue,
            };
            self.expansion_charges.push(
                self.budget.charge_value_bytes(
                    text.len().saturating_mul(size_of::<PatternChar>()) as u64,
                )?,
            );
            characters.reserve_exact(text.chars().count());
            characters.extend(text.chars().map(|character| match protection {
                Protection::Unquoted | Protection::EmptyIfs => PatternChar::Unquoted(character),
                Protection::Quoted | Protection::Checked => PatternChar::Literal(character),
            }));
        }
        Ok(characters)
    }

    fn record_expanded_value(
        &mut self,
        spans: &mut Vec<ExpansionSpan>,
        value: &Value,
        context: FieldContext,
    ) -> Result<(), CommandFailure> {
        match context {
            FieldContext::Fields => self.record_value(spans, value),
            FieldContext::CasePattern => {
                self.record_text(spans, &quoted_text(value), Protection::Unquoted)
            }
            FieldContext::QuotedCasePattern => {
                self.record_text(spans, &quoted_text(value), Protection::Quoted)
            }
        }
    }

    fn record_split_text(
        &mut self,
        spans: &mut Vec<ExpansionSpan>,
        text: &str,
    ) -> Result<(), CommandFailure> {
        let protection = if self.ifs_separator().is_empty() {
            Protection::EmptyIfs
        } else {
            Protection::Unquoted
        };
        self.record_text(spans, text, protection)
    }

    fn record_value(
        &mut self,
        spans: &mut Vec<ExpansionSpan>,
        value: &Value,
    ) -> Result<(), CommandFailure> {
        match value {
            Value::Array(items) => {
                let separator = self.ifs_separator();
                let charge = self.budget.charge_value_bytes(
                    value_bytes(value) + (size_of::<ExpansionSpan>() + separator.len()) as u64,
                )?;
                spans.push(ExpansionSpan {
                    content: SpanContent::Elements(items.clone(), separator),
                    charge,
                    retained: Vec::new(),
                });
            }
            Value::Null => {}
            Value::String(text) => self.record_split_text(spans, text)?,
            Value::Bool(_) | Value::Number(_) | Value::Object(_) => {
                self.record_split_text(spans, &display(value))?
            }
        }
        Ok(())
    }

    fn collect_literal(
        &mut self,
        spans: &mut Vec<ExpansionSpan>,
        text: &str,
    ) -> Result<(), CommandFailure> {
        let ifs = self.ifs()?;
        let mut start = 0;
        let mut protection = None;
        for (offset, character) in text.char_indices() {
            let next = if ifs.contains(character) {
                Protection::Quoted
            } else {
                Protection::Unquoted
            };
            if let Some(previous) = protection
                && previous != next
            {
                self.record_text(spans, &text[start..offset], previous)?;
                start = offset;
            }
            protection = Some(next);
        }
        if let Some(protection) = protection {
            self.record_text(spans, &text[start..], protection)?;
        }
        Ok(())
    }

    fn collect_field_part(
        &mut self,
        part: &WordPart,
        spans: &mut Vec<ExpansionSpan>,
        output: ExpansionOutput,
        context: FieldContext,
    ) -> Result<Value, CommandFailure> {
        match part {
            WordPart::Literal(text) => {
                match context {
                    FieldContext::Fields => self.collect_literal(spans, text)?,
                    FieldContext::CasePattern => {
                        self.record_text(spans, text, Protection::Unquoted)?
                    }
                    FieldContext::QuotedCasePattern => {
                        self.record_text(spans, text, Protection::Quoted)?
                    }
                }
                self.expansion_text(text, output)
            }
            WordPart::SingleQuoted(text) => {
                self.record_text(spans, text, Protection::Quoted)?;
                self.expansion_text(text, output)
            }
            WordPart::Asset(number) => {
                let text = number.to_string();
                self.record_text(spans, &text, Protection::Quoted)?;
                self.expansion_text(&text, output)
            }
            WordPart::Arithmetic(expression) => {
                let text = render_number(self.evaluate_arithmetic(expression)?);
                let protection = if context == FieldContext::QuotedCasePattern {
                    Protection::Quoted
                } else {
                    Protection::Unquoted
                };
                self.record_text(spans, &text, protection)?;
                Ok(Value::String(text))
            }
            WordPart::DoubleQuoted(parts) => {
                let start = spans.len();
                self.collect_quoted_parts(parts, spans, context)?;
                match output {
                    ExpansionOutput::Value => {
                        let bytes = spans[start..]
                            .iter()
                            .map(|span| match &span.content {
                                SpanContent::Text(part, _) => part.len(),
                                SpanContent::Boundary(_) => 1,
                                SpanContent::Elements(_, _) | SpanContent::SplitSpace => 0,
                            })
                            .sum::<usize>();
                        let charge = self.budget.charge_value_bytes(bytes as u64)?;
                        let mut text = String::with_capacity(bytes);
                        for span in &spans[start..] {
                            match &span.content {
                                SpanContent::Text(part, _) => text.push_str(part),
                                SpanContent::Boundary(_) => text.push(' '),
                                SpanContent::Elements(_, _) | SpanContent::SplitSpace => {}
                            }
                        }
                        self.expansion_charges.push(charge);
                        Ok(Value::String(text))
                    }
                    ExpansionOutput::Fields
                    | ExpansionOutput::Legacy
                    | ExpansionOutput::Unsplit => Ok(Value::Null),
                }
            }
            WordPart::Parameter(parameter) => {
                let joined = match parameter {
                    Parameter::AllPositionalJoined => true,
                    Parameter::Named { .. }
                    | Parameter::Positional(_)
                    | Parameter::AllPositional
                    | Parameter::PositionalCount
                    | Parameter::LastStatus
                    | Parameter::LastJob => false,
                };
                if context == FieldContext::Fields && joined && self.ifs_separator().is_empty() {
                    let charge = self
                        .budget
                        .charge_value_bytes(size_of::<ExpansionSpan>() as u64)?;
                    spans.push(ExpansionSpan {
                        content: SpanContent::SplitSpace,
                        charge,
                        retained: Vec::new(),
                    });
                }
                self.expand_parameter_fields(parameter, spans, context)
            }
            WordPart::CommandSubstitution(program) => {
                let result = self.run_substitution(program)?;
                self.record_expanded_value(spans, &result.value, context)?;
                self.expansion_charges.extend(result.retained);
                Ok(result.value)
            }
        }
    }

    fn collect_quoted_parts(
        &mut self,
        parts: &[WordPart],
        spans: &mut Vec<ExpansionSpan>,
        context: FieldContext,
    ) -> Result<(), CommandFailure> {
        if parts.is_empty() {
            self.record_text(spans, "", Protection::Quoted)?;
        }
        for part in parts {
            match part {
                WordPart::Literal(text) | WordPart::SingleQuoted(text) => {
                    self.record_text(spans, text, Protection::Quoted)?
                }
                WordPart::Asset(number) => {
                    self.record_text(spans, &number.to_string(), Protection::Quoted)?
                }
                WordPart::DoubleQuoted(inner) => {
                    self.collect_quoted_parts(inner, spans, context)?
                }
                WordPart::Arithmetic(expression) => {
                    let text = render_number(self.evaluate_arithmetic(expression)?);
                    self.record_text(spans, &text, Protection::Quoted)?;
                }
                WordPart::Parameter(parameter) => {
                    if context != FieldContext::Fields {
                        self.expand_parameter_fields(
                            parameter,
                            spans,
                            FieldContext::QuotedCasePattern,
                        )?;
                        continue;
                    }
                    let value = self.parameter_value_with(parameter, ExpansionOutput::Unsplit)?;
                    if splits_inside_quotes(parameter) {
                        let protection = match parameter {
                            Parameter::Named { .. } => Protection::Checked,
                            Parameter::Positional(_)
                            | Parameter::AllPositional
                            | Parameter::AllPositionalJoined
                            | Parameter::PositionalCount
                            | Parameter::LastStatus
                            | Parameter::LastJob => Protection::Quoted,
                        };
                        self.record_quoted_elements(spans, &value, protection, context)?;
                    } else {
                        self.record_text(spans, &quoted_text(&value), Protection::Quoted)?;
                    }
                }
                WordPart::CommandSubstitution(program) => {
                    let result = self.run_substitution(program)?;
                    self.record_capture(spans, result, Protection::Quoted)?;
                }
            }
        }
        Ok(())
    }

    fn record_quoted_elements(
        &mut self,
        spans: &mut Vec<ExpansionSpan>,
        value: &Value,
        protection: Protection,
        context: FieldContext,
    ) -> Result<(), CommandFailure> {
        let elements = match value {
            Value::Array(items) => items.as_slice(),
            Value::Null => &[],
            scalar @ (Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Object(_)) => {
                std::slice::from_ref(scalar)
            }
        };
        for (index, element) in elements.iter().enumerate() {
            if index > 0 {
                let mut separator = self.ifs_separator();
                let literal_separator =
                    context == FieldContext::QuotedCasePattern && separator.is_empty();
                if separator.is_empty() {
                    separator.push(' ');
                }
                let charge = self
                    .budget
                    .charge_value_bytes((size_of::<ExpansionSpan>() + separator.len()) as u64)?;
                spans.push(ExpansionSpan {
                    content: if literal_separator {
                        SpanContent::Text(separator, protection)
                    } else {
                        SpanContent::Boundary(separator)
                    },
                    charge,
                    retained: Vec::new(),
                });
            }
            self.record_text(spans, &display(element), protection)?;
        }
        Ok(())
    }

    fn split_spans(&mut self, spans: Vec<ExpansionSpan>) -> Result<Vec<String>, CommandFailure> {
        let force_space = spans.iter().any(|span| match span.content {
            SpanContent::SplitSpace => true,
            SpanContent::Text(_, _) | SpanContent::Elements(_, _) | SpanContent::Boundary(_) => {
                false
            }
        });
        let ifs = if force_space {
            Ifs::new(" ", &self.budget)?
        } else {
            self.ifs()?
        };
        let split_elements = force_space || !self.ifs_separator().is_empty();
        let mut fields = Vec::new();
        let mut separated = Boundary::Start;
        for mut span in spans {
            match span.content {
                SpanContent::Text(text, Protection::Unquoted) => {
                    self.split_text_with(&mut fields, &mut separated, &text, &ifs)?
                }
                SpanContent::Text(text, Protection::EmptyIfs) => {
                    self.split_protected_spaces(&mut fields, &mut separated, &text, &ifs)?;
                }
                SpanContent::Text(text, protection) => {
                    match protection {
                        Protection::Checked => ifs.check_quoted(&text)?,
                        Protection::Quoted | Protection::Unquoted | Protection::EmptyIfs => {}
                    }
                    match separated.accept(Kind::Text) {
                        Action::Append => append(&mut fields, &text),
                        Action::Start => fields.push(text),
                        Action::Empty | Action::Separate => {}
                    }
                    span.charge.shrink(size_of::<ExpansionSpan>() as u64);
                    self.expansion_charges.push(span.charge);
                    self.expansion_charges.extend(span.retained);
                }
                SpanContent::Boundary(separator) => {
                    if split_elements {
                        self.split_text_with(&mut fields, &mut separated, &separator, &ifs)?;
                    } else {
                        separated = Boundary::Start;
                    }
                }
                SpanContent::SplitSpace => {}
                SpanContent::Elements(values, separator) => {
                    if separated == Boundary::Start {
                        separated = Boundary::ArrayStart;
                    }
                    for (index, value) in values.iter().enumerate() {
                        if index > 0 {
                            if split_elements {
                                self.split_text_with(
                                    &mut fields,
                                    &mut separated,
                                    if separator.is_empty() {
                                        " "
                                    } else {
                                        &separator
                                    },
                                    &ifs,
                                )?;
                            } else {
                                separated = Boundary::Start;
                            }
                        }
                        if separator.is_empty() {
                            self.split_protected_spaces(
                                &mut fields,
                                &mut separated,
                                &display(value),
                                &ifs,
                            )?;
                        } else {
                            self.split_text_with(
                                &mut fields,
                                &mut separated,
                                &display(value),
                                &ifs,
                            )?;
                        }
                    }
                }
            }
        }
        Ok(fields)
    }

    fn split_protected_spaces(
        &mut self,
        fields: &mut Vec<String>,
        separated: &mut Boundary,
        text: &str,
        ifs: &Ifs,
    ) -> Result<(), CommandFailure> {
        let mut start = 0;
        while start < text.len() {
            let space = text.as_bytes()[start] == b' ';
            let end = text.as_bytes()[start..]
                .iter()
                .position(|byte| (*byte == b' ') != space)
                .map_or(text.len(), |offset| start + offset);
            let part = &text[start..end];
            if space {
                self.budget.charge_step()?;
                self.expansion_charges.push(
                    self.budget
                        .charge_value_bytes((part.len() + size_of::<String>()) as u64)?,
                );
                append_field(fields, separated, part);
            } else {
                self.split_text_with(fields, separated, part, ifs)?;
            }
            start = end;
        }
        Ok(())
    }

    fn expansion_text(
        &mut self,
        text: &str,
        output: ExpansionOutput,
    ) -> Result<Value, CommandFailure> {
        match output {
            ExpansionOutput::Legacy | ExpansionOutput::Unsplit | ExpansionOutput::Fields => {
                Ok(Value::Null)
            }
            ExpansionOutput::Value => {
                self.expansion_charges
                    .push(self.budget.charge_value_bytes(text.len() as u64)?);
                Ok(Value::String(text.to_owned()))
            }
        }
    }

    fn expand_parameter_fields(
        &mut self,
        parameter: &Parameter,
        spans: &mut Vec<ExpansionSpan>,
        context: FieldContext,
    ) -> Result<Value, CommandFailure> {
        match parameter {
            Parameter::Named {
                name,
                indices,
                modifier,
                length: false,
            } => self.nested(|this| {
                this.named_parameter_value(
                    name,
                    indices,
                    modifier,
                    Some(FieldExpansion {
                        spans,
                        context,
                        quoted_elements: context == FieldContext::QuotedCasePattern
                            && splits_inside_quotes(parameter),
                    }),
                    ExpansionOutput::Unsplit,
                )
            }),
            Parameter::AllPositional | Parameter::AllPositionalJoined => {
                if context != FieldContext::Fields {
                    let value = self.parameter_value_with(parameter, ExpansionOutput::Unsplit)?;
                    if context == FieldContext::QuotedCasePattern && splits_inside_quotes(parameter)
                    {
                        self.record_quoted_elements(spans, &value, Protection::Quoted, context)?;
                    } else {
                        self.record_expanded_value(spans, &value, context)?;
                    }
                    return Ok(value);
                }
                let values = self.positional().to_vec();
                self.record_value(spans, &Value::Array(values.clone()))?;
                Ok(Value::Array(values))
            }
            Parameter::Named { length: true, .. }
            | Parameter::Positional(_)
            | Parameter::PositionalCount
            | Parameter::LastStatus
            | Parameter::LastJob => {
                let value = self.parameter_value(parameter)?;
                self.record_expanded_value(spans, &value, context)?;
                Ok(value)
            }
        }
    }

    fn expand_selected_word(
        &mut self,
        word: &Word,
        split: Option<FieldExpansion<'_>>,
        output: ExpansionOutput,
    ) -> Result<Value, CommandFailure> {
        match split {
            Some(split) => self.expand_replacement(word, split.spans, split.context),
            None => self.expansion_value(word, output),
        }
    }

    fn finish_parameter(
        &mut self,
        value: Value,
        split: Option<FieldExpansion<'_>>,
    ) -> Result<Value, CommandFailure> {
        if let Some(split) = split {
            if split.quoted_elements {
                self.record_quoted_elements(
                    split.spans,
                    &value,
                    Protection::Quoted,
                    split.context,
                )?;
            } else {
                self.record_expanded_value(split.spans, &value, split.context)?;
            }
        }
        Ok(value)
    }

    fn expand_replacement(
        &mut self,
        word: &Word,
        spans: &mut Vec<ExpansionSpan>,
        context: FieldContext,
    ) -> Result<Value, CommandFailure> {
        if let [WordPart::CommandSubstitution(_)] = word.parts.as_slice() {
            let result = self.assignment_value(word)?;
            self.record_expanded_value(spans, &result.value, context)?;
            self.expansion_charges.extend(result.retained);
            return Ok(result.value);
        }
        let mut text = String::new();
        for part in &word.parts {
            let start = spans.len();
            let value = match part {
                WordPart::Literal(literal) => {
                    match context {
                        FieldContext::Fields | FieldContext::CasePattern => {
                            self.record_split_text(spans, literal)?
                        }
                        FieldContext::QuotedCasePattern => {
                            self.record_text(spans, literal, Protection::Quoted)?
                        }
                    }
                    self.expansion_text(literal, ExpansionOutput::Value)?
                }
                WordPart::Asset(_)
                | WordPart::SingleQuoted(_)
                | WordPart::DoubleQuoted(_)
                | WordPart::Parameter(_)
                | WordPart::CommandSubstitution(_)
                | WordPart::Arithmetic(_) => {
                    self.collect_field_part(part, spans, ExpansionOutput::Value, context)?
                }
            };
            if matches!(part, WordPart::SingleQuoted(_) | WordPart::DoubleQuoted(_)) {
                for span in &mut spans[start..] {
                    if let SpanContent::Text(_, protection) = &mut span.content {
                        *protection = Protection::Checked;
                    }
                }
            }
            if word.parts.len() == 1 {
                return Ok(value);
            }
            match &value {
                Value::String(part) => {
                    self.expansion_charges
                        .push(self.budget.charge_value_bytes(part.len() as u64)?);
                    text.push_str(part);
                }
                Value::Null
                | Value::Bool(_)
                | Value::Number(_)
                | Value::Array(_)
                | Value::Object(_) => {
                    let mut charge = self
                        .budget
                        .charge_value_bytes(value_bytes(&value).saturating_mul(12))?;
                    let rendered = quoted_text(&value);
                    let bytes = rendered.len() as u64;
                    charge.shrink(charge.bytes().saturating_sub(bytes.saturating_mul(2)));
                    text.push_str(&rendered);
                    charge.shrink(bytes);
                    self.expansion_charges.push(charge);
                }
            }
        }
        Ok(Value::String(text))
    }

    fn ifs(&self) -> Result<Ifs, LimitExceeded> {
        match self.lookup("IFS") {
            Some(value) => Ifs::new(&display(value), &self.budget),
            None => Ifs::new(DEFAULT_IFS, &self.budget),
        }
    }

    fn ifs_separator(&self) -> String {
        self.lookup("IFS").map_or_else(
            || " ".to_owned(),
            |value| {
                display(value)
                    .chars()
                    .next()
                    .map_or_else(String::new, |first| first.to_string())
            },
        )
    }

    fn ifs_join(&mut self, values: &[Value]) -> Result<String, LimitExceeded> {
        let separator = self.ifs_separator();
        let bytes = values
            .iter()
            .map(value_bytes)
            .sum::<u64>()
            .saturating_mul(12)
            + (values.len().saturating_sub(1) * separator.len()) as u64;
        let mut charge = self.budget.charge_value_bytes(bytes)?;
        let text = values
            .iter()
            .map(display)
            .collect::<Vec<_>>()
            .join(&separator);
        charge.shrink(charge.bytes().saturating_sub(text.len() as u64));
        self.expansion_charges.push(charge);
        Ok(text)
    }

    fn split_text_with(
        &mut self,
        fields: &mut Vec<String>,
        separated: &mut Boundary,
        text: &str,
        ifs: &Ifs,
    ) -> Result<(), CommandFailure> {
        self.budget.charge_step()?;
        for (kind, range) in ifs.scan(text, &[]) {
            if matches!(kind, Kind::Delimiter | Kind::LeadingByte)
                && *separated == Boundary::Whitespace
            {
                separated.accept(Kind::Delimiter);
                for byte in &text.as_bytes()[range.start + 1..range.end] {
                    match ifs.byte_kind(*byte) {
                        Kind::Delimiter => {
                            self.budget.charge_step()?;
                            self.expansion_charges
                                .push(self.budget.charge_value_bytes(size_of::<String>() as u64)?);
                            fields.push(String::new());
                        }
                        Kind::Text | Kind::LeadingByte | Kind::Whitespace => {
                            return Err(invalid_ifs_utf8());
                        }
                    }
                }
                continue;
            }
            match separated.accept(kind) {
                action @ (Action::Append | Action::Start | Action::Empty) => {
                    self.budget.charge_step()?;
                    let field = if kind == Kind::Delimiter {
                        ""
                    } else {
                        &text[range]
                    };
                    self.expansion_charges.push(
                        self.budget
                            .charge_value_bytes((field.len() + size_of::<String>()) as u64)?,
                    );
                    match action {
                        Action::Append => append(fields, field),
                        Action::Start | Action::Empty => fields.push(field.to_owned()),
                        Action::Separate => {}
                    }
                }
                Action::Separate => {}
            }
        }
        Ok(())
    }

    fn expand_quoted(&mut self, parts: &[WordPart]) -> Result<String, CommandFailure> {
        Ok(self.expand_quoted_fields(parts)?.join(" "))
    }

    fn expand_quoted_fields(&mut self, parts: &[WordPart]) -> Result<Vec<String>, CommandFailure> {
        self.expand_preserved_fields(parts, ExpansionOutput::Unsplit)
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
    fn expand_preserved_fields(
        &mut self,
        parts: &[WordPart],
        output: ExpansionOutput,
    ) -> Result<Vec<String>, CommandFailure> {
        let mut fields = vec![String::new()];
        for part in parts {
            match part {
                WordPart::Asset(number) => append(&mut fields, &number.to_string()),
                WordPart::Literal(literal) | WordPart::SingleQuoted(literal) => {
                    append(&mut fields, literal);
                }
                WordPart::DoubleQuoted(inner) => {
                    let inner = self.expand_quoted(inner)?;
                    append(&mut fields, &inner);
                }
                WordPart::Arithmetic(expression) => {
                    let number = self.evaluate_arithmetic(expression)?;
                    append(&mut fields, &render_number(number));
                }
                WordPart::Parameter(parameter) if splits_inside_quotes(parameter) => {
                    let value = self.parameter_value_with(parameter, output)?;
                    let elements = match value {
                        Value::Array(items) => items,
                        Value::Null => Vec::new(),
                        scalar => vec![scalar],
                    };
                    let mut elements = elements.iter().map(display);
                    let Some(first) = elements.next() else {
                        if parts.len() == 1 {
                            return Ok(Vec::new());
                        }
                        continue;
                    };
                    append(&mut fields, &first);
                    fields.extend(elements);
                }
                WordPart::Parameter(parameter) => {
                    let value = self.parameter_value_with(parameter, output)?;
                    append(&mut fields, &quoted_text(&value));
                }
                WordPart::CommandSubstitution(program) => {
                    let result = self.run_substitution(program)?;
                    append(&mut fields, &quoted_text(&result.value));
                    self.expansion_charges.extend(result.retained);
                }
            }
        }
        Ok(fields)
    }

    fn parameter_value(&mut self, parameter: &Parameter) -> Result<Value, CommandFailure> {
        self.parameter_value_with(parameter, ExpansionOutput::Legacy)
    }

    fn parameter_value_with(
        &mut self,
        parameter: &Parameter,
        output: ExpansionOutput,
    ) -> Result<Value, CommandFailure> {
        Ok(match parameter {
            Parameter::Named {
                name,
                indices,
                modifier,
                length,
            } => {
                let value = self.nested(|this| {
                    this.named_parameter_value(name, indices, modifier, None, output)
                })?;
                if *length {
                    parameter_length(&value)
                } else {
                    value
                }
            }
            Parameter::Positional(0) => Value::String("dekopon-shell".to_owned()),
            Parameter::Positional(position) => self
                .positional()
                .get(position - 1)
                .cloned()
                .unwrap_or(Value::Null),
            Parameter::AllPositional => Value::Array(self.positional().to_vec()),
            Parameter::AllPositionalJoined => {
                let values = self.positional().to_vec();
                Value::String(self.ifs_join(&values)?)
            }
            Parameter::PositionalCount => Value::from(self.positional().len()),
            Parameter::LastStatus => Value::from(self.last_status.get()),
            Parameter::LastJob => self
                .jobs
                .last
                .map_or_else(|| Value::String(String::new()), |id| Value::from(id.get())),
        })
    }

    fn named_parameter_value(
        &mut self,
        name: &str,
        indices: &[Index],
        modifier: &Modifier,
        mut split: Option<FieldExpansion<'_>>,
        output: ExpansionOutput,
    ) -> Result<Value, CommandFailure> {
        let (value, bound) = self.select_parameter(
            name,
            indices,
            if split
                .as_ref()
                .is_some_and(|split| split.context == FieldContext::Fields)
            {
                ExpansionOutput::Fields
            } else {
                output
            },
        )?;
        if self.options.nounset && !bound && *modifier == Modifier::None {
            return Err(CommandFailure::Fatal(FatalError::Assertion(format!(
                "{name}: unbound variable, and `set -u` is on"
            ))));
        }
        let absent = |this: &mut Self, colon: bool| -> Result<bool, LimitExceeded> {
            Ok(if colon {
                match &value {
                    Value::Null => true,
                    Value::Array(elements) => {
                        elements.len() <= 1 && this.ifs_join(elements)?.is_empty()
                    }
                    Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Object(_) => {
                        display(&value).is_empty()
                    }
                }
            } else {
                !bound
            })
        };
        let value = match modifier {
            Modifier::None => value,
            Modifier::Default { colon, word } => {
                if absent(self, *colon)? {
                    self.expand_selected_word(word, split.take(), output)?
                } else {
                    value
                }
            }
            Modifier::Assign { colon, word } => {
                if !absent(self, *colon)? {
                    return self.finish_parameter(value, split);
                }
                if !indices.is_empty() {
                    return Err(CommandFailure::usage(format!(
                        "${{{name}[...]:=word}} cannot assign through an index; assign to {name} itself"
                    )));
                }
                let substitute = self.expand_selected_word(word, None, output)?;
                self.assign(name, CommandResult::value(substitute.clone()))?;
                substitute
            }
            Modifier::Require { colon, word } => {
                if !absent(self, *colon)? {
                    return self.finish_parameter(value, split);
                }
                let message = match word {
                    Some(word) => self.expand_quoted(&word.parts)?,
                    None => "parameter is not set".to_owned(),
                };
                return Err(CommandFailure::Fatal(FatalError::Assertion(format!(
                    "{name}: {message}"
                ))));
            }
            Modifier::Alternate { colon, word } => {
                if absent(self, *colon)? {
                    Value::Null
                } else {
                    self.expand_selected_word(word, split.take(), output)?
                }
            }
            Modifier::StripPrefix(pattern) => {
                let pattern = self.literal_pattern(pattern, output)?;
                let text = display(&value);
                Value::String(
                    text.strip_prefix(&pattern)
                        .map_or(text.clone(), str::to_owned),
                )
            }
            Modifier::StripSuffix(pattern) => {
                let pattern = self.literal_pattern(pattern, output)?;
                let text = display(&value);
                Value::String(
                    text.strip_suffix(&pattern)
                        .map_or(text.clone(), str::to_owned),
                )
            }
            Modifier::Replace {
                all,
                pattern,
                replacement,
            } => {
                let pattern = self.literal_pattern(pattern, output)?;
                let replacement = self
                    .expand_preserved_fields(&replacement.parts, output)?
                    .join(" ");
                let text = display(&value);
                if pattern.is_empty() {
                    return Err(CommandFailure::usage(format!(
                        "${{{name}/...}} needs text to replace; an empty pattern matches everywhere"
                    )));
                }
                Value::String(if *all {
                    text.replace(&pattern, &replacement)
                } else {
                    text.replacen(&pattern, &replacement, 1)
                })
            }
        };
        self.finish_parameter(value, split)
    }

    fn select_parameter(
        &mut self,
        name: &str,
        indices: &[Index],
        output: ExpansionOutput,
    ) -> Result<(Value, bool), CommandFailure> {
        let mut bound = self.lookup(name).is_some();
        let mut value = self.lookup(name).cloned().unwrap_or(Value::Null);
        for index in indices {
            match index {
                Index::At(word) => {
                    let key = self
                        .expand_word_with(word, ExpansionOutput::Legacy)?
                        .join(" ");
                    value = value::index(&value, &key);
                    bound = !value.is_null();
                }
                Index::All => {}
                Index::AllJoined if matches!(output, ExpansionOutput::Fields) => {}
                Index::AllJoined => {
                    let values = match value {
                        Value::Array(items) => items,
                        Value::Null => Vec::new(),
                        scalar @ (Value::Bool(_)
                        | Value::Number(_)
                        | Value::String(_)
                        | Value::Object(_)) => vec![scalar],
                    };
                    value = Value::String(self.ifs_join(&values)?);
                }
            }
        }
        Ok((value, bound))
    }

    fn literal_pattern(
        &mut self,
        pattern: &Pattern,
        output: ExpansionOutput,
    ) -> Result<String, CommandFailure> {
        match pattern {
            Pattern::Literal(word) => {
                Ok(self.expand_preserved_fields(&word.parts, output)?.join(" "))
            }
            Pattern::Expanded(word) => {
                let text = self.expand_preserved_fields(&word.parts, output)?.join(" ");
                if let Some(character) = pattern_metacharacter(&text) {
                    return Err(CommandFailure::usage(expanded_pattern(character)));
                }
                Ok(text)
            }
        }
    }

    fn evaluate_conditional(
        &mut self,
        expression: &Conditional,
    ) -> Result<ExitCode, CommandFailure> {
        self.budget.charge_step()?;
        match expression {
            Conditional::Test(test) => self.evaluate_conditional_test(test),
            Conditional::Not(inner) => Ok(invert(self.evaluate_conditional(inner)?)),
            Conditional::And(terms) => self.evaluate_conditional_chain(terms, AndOr::And),
            Conditional::Or(terms) => self.evaluate_conditional_chain(terms, AndOr::Or),
        }
    }

    fn evaluate_conditional_chain(
        &mut self,
        terms: &[Conditional],
        operator: AndOr,
    ) -> Result<ExitCode, CommandFailure> {
        // Each operator must consume a step before the first operand, including skipped operators.
        for _ in 2..terms.len() {
            self.budget.charge_step()?;
        }
        let mut status = ExitCode::SUCCESS;
        for term in terms {
            status = self.evaluate_conditional(term)?;
            let stop = match operator {
                AndOr::And => status != ExitCode::SUCCESS,
                AndOr::Or => status == ExitCode::SUCCESS,
            };
            if stop {
                break;
            }
        }
        Ok(status)
    }

    fn evaluate_conditional_test(
        &mut self,
        test: &ConditionalTest,
    ) -> Result<ExitCode, CommandFailure> {
        let mut operands = Vec::with_capacity(test.words.len());
        for word in &test.words {
            operands.push(
                self.expand_preserved_fields(&word.parts, ExpansionOutput::Unsplit)?
                    .join(" "),
            );
        }
        if test.check_right_pattern
            && let [_, _, right] = operands.as_slice()
            && let Some(character) = pattern_metacharacter(right)
        {
            return Err(CommandFailure::usage(expanded_pattern(character)));
        }
        Ok(builtins::misc::evaluate_test("[[", &operands)?.status)
    }

    fn run_substitution(&mut self, program: &Program) -> Result<CommandResult, CommandFailure> {
        let (flow, mut captured) = self.nested(|this| {
            let frames = this.copy_frames()?;
            let saved_frames = std::mem::replace(&mut this.frames, frames);
            let globals = Arc::clone(&this.globals);
            let global_charges = std::mem::take(&mut this.global_charges);
            let shared_charges = std::mem::take(&mut this.shared_charges);
            let functions = this.functions.clone();
            let function_names = this.function_names.clone();
            let options = this.options;
            this.captures.push(Vec::new());
            let flow = this.execute_program(program);
            let captured = this.captures.pop().unwrap_or_default();
            let status = this.last_status;
            this.frames = saved_frames;
            this.globals = globals;
            this.global_charges = global_charges;
            this.shared_charges = shared_charges;
            this.functions = functions;
            this.function_names = function_names;
            this.options = options;
            this.last_status = status;
            this.last_substitution_status = status;
            Ok::<_, CommandFailure>((flow, captured))
        })?;
        let flow = flow.map_err(CommandFailure::Fatal)?;

        if let Flow::Exit(status) = flow {
            return Err(CommandFailure::Fatal(FatalError::Unsupported(format!(
                "exit {status} inside $( ) is not supported"
            ))));
        }

        let mut retained = capture_charges(&mut captured);
        let mut value = reduce_charged(&self.budget, captured, &mut retained)?;
        if let Value::String(text) = &mut value {
            text.truncate(text.trim_end_matches('\n').len());
        }
        let mut result = CommandResult {
            output: None,
            value,
            status: self.last_substitution_status,
            suppress_newline: false,
            retained,
        };
        retain_value(&self.budget, &result.value, &mut result.retained)?;
        self.last_substitution_status = result.status;
        Ok(result)
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
    fn evaluate_arithmetic(&mut self, expression: &ArithExpr) -> Result<Number, CommandFailure> {
        self.budget.charge_step()?;
        Ok(match expression.node() {
            crate::ast::ArithNode::Integer(value) => Number::Integer(*value),
            crate::ast::ArithNode::Float(value) => Number::Float(*value),
            crate::ast::ArithNode::Variable(name) => {
                let value = match name.parse::<usize>() {
                    Ok(position) => self.parameter_value(&Parameter::Positional(position))?,
                    Err(_) => self.lookup(name).cloned().unwrap_or(Value::Null),
                };
                to_number(&value)
            }
            crate::ast::ArithNode::Unary(operator, operand) => {
                let operand = self.evaluate_arithmetic(operand)?;
                match operator {
                    ArithUnaryOp::Negate => match operand {
                        Number::Integer(value) => Number::Integer(value.wrapping_neg()),
                        Number::Float(value) => Number::Float(-value),
                    },
                    ArithUnaryOp::Not => Number::Integer(i64::from(!operand.is_truthy())),
                }
            }
            crate::ast::ArithNode::Binary(operator, left, right) => {
                match operator {
                    ArithBinaryOp::And => {
                        let left = self.evaluate_arithmetic(left)?;
                        if !left.is_truthy() {
                            return Ok(Number::Integer(0));
                        }
                        let right = self.evaluate_arithmetic(right)?;
                        return Ok(Number::Integer(i64::from(right.is_truthy())));
                    }
                    ArithBinaryOp::Or => {
                        let left = self.evaluate_arithmetic(left)?;
                        if left.is_truthy() {
                            return Ok(Number::Integer(1));
                        }
                        let right = self.evaluate_arithmetic(right)?;
                        return Ok(Number::Integer(i64::from(right.is_truthy())));
                    }
                    _ => {}
                }

                let left = self.evaluate_arithmetic(left)?;
                let right = self.evaluate_arithmetic(right)?;
                arithmetic(*operator, left, right)?
            }
        })
    }
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
fn splits_inside_quotes(parameter: &Parameter) -> bool {
    match parameter {
        Parameter::AllPositional => true,
        Parameter::Named {
            indices, length, ..
        } => !*length && matches!(indices.last(), Some(Index::All)),
        _ => false,
    }
}

fn is_variable_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn invalid_ifs_utf8() -> CommandFailure {
    unsupported_option("IFS: Bash would produce a field that is not valid UTF-8")
}

fn unsupported_option(message: impl Into<String>) -> CommandFailure {
    CommandFailure::Fatal(FatalError::Unsupported(message.into()))
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
fn parameter_length(value: &Value) -> Value {
    match value {
        Value::Null => Value::from(0),
        Value::String(text) => Value::from(text.chars().count()),
        Value::Array(items) => Value::from(items.len()),
        Value::Object(fields) => Value::from(fields.len()),
        other => Value::from(display(other).chars().count()),
    }
}

fn merge_diagnostics(result: &mut CommandResult, diagnostics: Vec<Diagnostic>) {
    let mut diagnostics = diagnostics.into_iter();
    let Some(first) = diagnostics.next() else {
        return;
    };
    let first = match Diagnostic::from_result(result) {
        Some(output) => output.followed_by(first),
        None => first,
    };
    let merged = diagnostics
        .fold(first, Diagnostic::followed_by)
        .into_result();
    result.value = merged.value;
    result.suppress_newline = merged.suppress_newline;
}

fn write_display(stdout: &mut PipeWriter<StageObservation>, value: &Value) -> WriteOutcome {
    match value {
        Value::String(text) => stdout.write(text.as_bytes()),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            stdout.write(display(value).as_bytes())
        }
    }
}

fn invert(status: ExitCode) -> ExitCode {
    if status == ExitCode::SUCCESS {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

struct BufferStorage<'a> {
    buffers: &'a mut Arc<BTreeMap<String, Vec<u8>>>,
    charges: &'a mut BTreeMap<String, Vec<dekopon_process::RetainedBytes>>,
    shared_charges: &'a mut Vec<dekopon_process::RetainedBytes>,
}

fn append_buffer_bytes(
    budget: &Budget,
    storage: BufferStorage<'_>,
    name: &str,
    bytes: &[u8],
) -> Result<(), LimitExceeded> {
    if bytes.is_empty() {
        return Ok(());
    }
    let charge = budget.charge_value_bytes(bytes.len() as u64)?;
    append_charged_buffer(budget, storage, name, bytes, vec![charge])
}

fn append_charged_buffer(
    budget: &Budget,
    storage: BufferStorage<'_>,
    name: &str,
    bytes: &[u8],
    charges: Vec<dekopon_process::RetainedBytes>,
) -> Result<(), LimitExceeded> {
    if Arc::strong_count(storage.buffers) > 1 {
        let length: u64 = storage
            .buffers
            .values()
            .map(|value| value.len() as u64)
            .sum();
        storage
            .shared_charges
            .push(budget.charge_value_bytes(length)?);
    }
    Arc::make_mut(storage.buffers)
        .entry(name.to_owned())
        .or_default()
        .extend_from_slice(bytes);
    storage
        .charges
        .entry(name.to_owned())
        .or_default()
        .extend(charges);
    Ok(())
}

fn retain_value(
    budget: &Budget,
    value: &Value,
    charges: &mut Vec<dekopon_process::RetainedBytes>,
) -> Result<(), LimitExceeded> {
    let charged: u64 = charges
        .iter()
        .map(dekopon_process::RetainedBytes::bytes)
        .sum();
    let needed = value_bytes(value);
    if needed > charged {
        charges.push(budget.charge_value_bytes(needed - charged)?);
    } else {
        let mut excess = charged - needed;
        for charge in charges.iter_mut().rev() {
            let refund = excess.min(charge.bytes());
            charge.shrink(refund);
            excess -= refund;
        }
        charges.retain(|charge| charge.bytes() > 0);
    }
    Ok(())
}

fn capture_charges(captured: &mut [CommandResult]) -> Vec<dekopon_process::RetainedBytes> {
    captured
        .iter_mut()
        .flat_map(|result| std::mem::take(&mut result.retained))
        .collect()
}

fn push_diagnostic_bytes(bytes: &[u8], captures: &mut [StderrCapture], output: &mut OutputBuffer) {
    if let Some(capture) = captures.last_mut() {
        capture.push_bytes(bytes);
    } else {
        push_lossy_bytes(output, bytes);
    }
}

fn push_diagnostic_fragment(text: &str, captures: &mut [StderrCapture], output: &mut OutputBuffer) {
    if let Some(capture) = captures.last_mut() {
        capture.push_fragment(text);
    } else {
        output.push_fragment(text);
    }
}

fn push_lossy_bytes(output: &mut OutputBuffer, bytes: &[u8]) {
    let mut offset = 0;
    while offset < bytes.len() {
        let end = bytes.len().min(offset + 4096);
        let complete = lossy_complete_prefix(&bytes[offset..end]);
        let length = if complete == 0 {
            bytes.len().min(offset + 4) - offset
        } else {
            complete
        };
        output.push_fragment(&String::from_utf8_lossy(&bytes[offset..offset + length]));
        offset += length;
    }
}

fn lossy_complete_prefix(bytes: &[u8]) -> usize {
    let mut offset = 0;
    loop {
        match std::str::from_utf8(&bytes[offset..]) {
            Ok(_) => return bytes.len(),
            Err(error) => match error.error_len() {
                Some(invalid) => offset += error.valid_up_to() + invalid,
                None => return offset + error.valid_up_to(),
            },
        }
    }
}

struct RenderSize {
    bytes: u64,
    maximum: u64,
}

impl std::io::Write for RenderSize {
    fn write(&mut self, chunk: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(u64::try_from(chunk.len()).map_err(std::io::Error::other)?)
            .filter(|bytes| *bytes <= self.maximum)
            .ok_or_else(|| std::io::Error::other("rendered value exceeds retention limit"))?;
        Ok(chunk.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn rendered_bytes(value: &Value, maximum: u64) -> Result<u64, LimitExceeded> {
    match value {
        Value::Null => Ok(0),
        Value::String(text) => u64::try_from(text.len())
            .ok()
            .filter(|bytes| *bytes <= maximum)
            .ok_or(LimitExceeded::ValueBytes { maximum }),
        Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            let mut counter = RenderSize { bytes: 0, maximum };
            serde_json::to_writer(&mut counter, value)
                .map_err(|_oversized| LimitExceeded::ValueBytes { maximum })?;
            Ok(counter.bytes)
        }
    }
}

fn reduce_charged(
    budget: &Budget,
    captured: Vec<CommandResult>,
    retained: &mut Vec<dekopon_process::RetainedBytes>,
) -> Result<Value, LimitExceeded> {
    if captured.len() > 1 {
        let maximum = budget.max_value_bytes();
        let too_large = || LimitExceeded::ValueBytes { maximum };
        let mut bytes = 16u64;
        let mut previous_suppressed = true;
        for result in &captured {
            let length = rendered_bytes(&result.value, maximum)?;
            bytes = bytes
                .checked_add(length)
                .and_then(|total| total.checked_add(u64::from(!previous_suppressed)))
                .ok_or_else(too_large)?;
            previous_suppressed = result.suppress_newline;
        }
        retained.push(budget.charge_value_bytes(bytes)?);
    }
    Ok(reduce_captured(captured))
}

fn reduce_captured(captured: Vec<CommandResult>) -> Value {
    match captured.len() {
        0 => Value::String(String::new()),
        1 => captured
            .into_iter()
            .next()
            .map_or(Value::Null, |result| result.value),
        _ => {
            let mut text = String::new();
            let mut previous_suppressed = true;
            for result in &captured {
                if !previous_suppressed {
                    text.push('\n');
                }
                text.push_str(&display(&result.value));
                previous_suppressed = result.suppress_newline;
            }
            Value::String(text)
        }
    }
}

/// The byte cost charged against the memory budget is an approximation, not a measurement, and
/// includes a small fixed charge per node so a deeply nested structure built entirely of empty
/// pieces still counts against the limit.
fn value_bytes(value: &Value) -> u64 {
    const NODE_OVERHEAD: u64 = 16;
    NODE_OVERHEAD
        + match value {
            Value::Null | Value::Bool(_) | Value::Number(_) => 0,
            Value::String(text) => text.len() as u64,
            Value::Array(items) => items.iter().map(value_bytes).sum(),
            Value::Object(fields) => fields
                .iter()
                .map(|(key, field)| key.len() as u64 + value_bytes(field))
                .sum(),
        }
}

fn check_buffer_operand(command: &str, name: &str) -> Result<(), CommandFailure> {
    if dekopon_core::AssetNumber::parse(name).is_some() {
        return Err(CommandFailure::failed(format!(
            "{command}: {name} is an attachment, not a buffer; read it with `asset cat {name} | {command}`"
        )));
    }
    Ok(())
}

#[allow(
    clippy::map_err_ignore,
    reason = "ParseIntError separates only empty, non-digit, and overflow for a `shift` operand \
              the message quotes back in full; all three mean the same thing to the script author"
)]
fn parse_shift_count(arguments: &[String]) -> Result<usize, CommandFailure> {
    match arguments {
        [] => Ok(1),
        [count] => count.parse::<usize>().map_err(|_| {
            CommandFailure::usage(format!("shift: {count:?} is not a parameter count"))
        }),
        _ => Err(CommandFailure::usage(
            "shift: accepts at most one parameter count",
        )),
    }
}

fn unwind_break(level: u32) -> Flow {
    if level > 1 {
        Flow::Break(level - 1)
    } else {
        Flow::Normal
    }
}

fn parse_level(command: &str, arguments: &[String]) -> Result<u32, CommandFailure> {
    match arguments {
        [] => Ok(1),
        [level] => level
            .parse::<u32>()
            .ok()
            .filter(|level| *level > 0)
            .ok_or_else(|| {
                CommandFailure::usage(format!("{command}: {level:?} is not a positive loop level"))
            }),
        _ => Err(CommandFailure::usage(format!(
            "{command}: accepts at most one loop level"
        ))),
    }
}

#[allow(
    clippy::map_err_ignore,
    reason = "ParseIntError separates only empty, non-digit, and overflow for an `exit`/`return` \
              operand the message quotes back in full; an out-of-range status is not a different \
              user mistake from a non-numeric one here"
)]
fn parse_status(
    command: &str,
    arguments: &[String],
    fallback: ExitCode,
) -> Result<ExitCode, CommandFailure> {
    match arguments {
        [] => Ok(fallback),
        [status] => status
            .parse::<i64>()
            .map(ExitCode::from_script_exit)
            .map_err(|_| {
                CommandFailure::usage(format!("{command}: {status:?} is not a numeric status"))
            }),
        _ => Err(CommandFailure::usage(format!(
            "{command}: accepts at most one status"
        ))),
    }
}

fn append(fields: &mut Vec<String>, text: &str) {
    if let Some(last) = fields.last_mut() {
        last.push_str(text);
    } else {
        fields.push(text.to_owned());
    }
}

fn append_unsplit_value(fields: &mut Vec<String>, separated: &mut Boundary, value: &Value) {
    match value {
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    *separated = Boundary::Start;
                }
                append_field(fields, separated, &display(item));
            }
        }
        Value::Null => {}
        Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Object(_) => {
            let text = display(value);
            if !text.is_empty() {
                append_field(fields, separated, &text);
            }
        }
    }
}

fn append_field(fields: &mut Vec<String>, separated: &mut Boundary, text: &str) {
    match separated.accept(Kind::Text) {
        Action::Append => append(fields, text),
        Action::Start => fields.push(text.to_owned()),
        Action::Empty | Action::Separate => {}
    }
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
fn quoted_text(value: &Value) -> String {
    match value {
        Value::Array(items) => items.iter().map(display).collect::<Vec<_>>().join(" "),
        other => display(other),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Number {
    Integer(i64),
    Float(f64),
}

impl Number {
    fn is_truthy(self) -> bool {
        match self {
            Self::Integer(value) => value != 0,
            Self::Float(value) => value != 0.0,
        }
    }

    fn as_f64(self) -> f64 {
        match self {
            Self::Integer(value) => value as f64,
            Self::Float(value) => value,
        }
    }
}

fn render_number(number: Number) -> String {
    match number {
        Number::Integer(value) => value.to_string(),
        Number::Float(value) => {
            if value.fract() == 0.0 && value.is_finite() && value.abs() < 1e15 {
                format!("{}", value as i64)
            } else {
                value.to_string()
            }
        }
    }
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
fn to_number(value: &Value) -> Number {
    match value {
        Value::Number(number) => number.as_i64().map_or_else(
            || Number::Float(number.as_f64().unwrap_or_default()),
            Number::Integer,
        ),
        Value::Bool(flag) => Number::Integer(i64::from(*flag)),
        Value::String(text) => {
            let text = text.trim();
            text.parse::<i64>().map_or_else(
                |_| Number::Float(text.parse::<f64>().unwrap_or_default()),
                Number::Integer,
            )
        }
        _ => Number::Integer(0),
    }
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
fn arithmetic(
    operator: ArithBinaryOp,
    left: Number,
    right: Number,
) -> Result<Number, CommandFailure> {
    use ArithBinaryOp as Op;

    let comparison = |ordering: Option<Ordering>, expected: &[Ordering]| {
        Number::Integer(i64::from(
            ordering.is_some_and(|ordering| expected.contains(&ordering)),
        ))
    };
    let ordering = left.as_f64().partial_cmp(&right.as_f64());

    Ok(match operator {
        Op::Less => comparison(ordering, &[Ordering::Less]),
        Op::LessOrEqual => comparison(ordering, &[Ordering::Less, Ordering::Equal]),
        Op::Greater => comparison(ordering, &[Ordering::Greater]),
        Op::GreaterOrEqual => comparison(ordering, &[Ordering::Greater, Ordering::Equal]),
        Op::Equal => comparison(ordering, &[Ordering::Equal]),
        Op::NotEqual => comparison(ordering, &[Ordering::Less, Ordering::Greater]),
        Op::And | Op::Or => unreachable!("logical operators short-circuit before this point"),
        Op::Add | Op::Subtract | Op::Multiply | Op::Divide | Op::Remainder => match (left, right) {
            (Number::Integer(left), Number::Integer(right)) => match operator {
                Op::Add => Number::Integer(left.wrapping_add(right)),
                Op::Subtract => Number::Integer(left.wrapping_sub(right)),
                Op::Multiply => Number::Integer(left.wrapping_mul(right)),
                Op::Divide => {
                    if right == 0 {
                        return Err(CommandFailure::failed(
                            "dekopon-shell: arithmetic division by zero",
                        ));
                    }
                    Number::Integer(left.wrapping_div(right))
                }
                _ => {
                    if right == 0 {
                        return Err(CommandFailure::failed(
                            "dekopon-shell: arithmetic division by zero",
                        ));
                    }
                    Number::Integer(left.wrapping_rem(right))
                }
            },
            _ => {
                let (left, right) = (left.as_f64(), right.as_f64());
                match operator {
                    Op::Add => Number::Float(left + right),
                    Op::Subtract => Number::Float(left - right),
                    Op::Multiply => Number::Float(left * right),
                    Op::Divide => {
                        if right == 0.0 {
                            return Err(CommandFailure::failed(
                                "dekopon-shell: arithmetic division by zero",
                            ));
                        }
                        Number::Float(left / right)
                    }
                    _ => {
                        if right == 0.0 {
                            return Err(CommandFailure::failed(
                                "dekopon-shell: arithmetic division by zero",
                            ));
                        }
                        Number::Float(left % right)
                    }
                }
            }
        },
    })
}

#[cfg(test)]
mod nesting_tests;
#[cfg(test)]
mod thread_tests;
