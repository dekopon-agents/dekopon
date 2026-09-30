//! The evaluator never reads the host process environment, so a script variable comes only from the
//! script's own assignments, and reading something like an API key by name sees it as unset, not
//! the host's real value.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    thread::{self, Scope, ScopedJoinHandle},
};

use serde_json::Value;

use crate::{
    CallBudget, CapabilityInvoker, CommandRun, ExitCode, ScriptOutcome, TreeContext,
    ast::{
        AndOr, AndOrList, ArithBinaryOp, ArithExpr, ArithUnaryOp, CasePattern, CaseStatement,
        Command, Conditional, ConditionalTest, DEV_NULL, ForLoop, IfStatement, Index, Modifier,
        Parameter, Pattern, Pipeline, Program, Redirect, RedirectTarget, SimpleCommand, Statement,
        Stream, WhileLoop, Word, WordPart,
    },
    builtins::{
        self, BuiltinContext, BuiltinKind, CommandFailure, CommandResult, FatalError, xargs,
    },
    dispatch::{self, Resolution},
    limits::{Budget, LimitExceeded, Limits, OutputBuffer},
    parser::{
        expanded_case_pattern, expanded_conditional_pattern, expanded_parameter_pattern, parse,
        pattern_metacharacter,
    },
    pipe::{self, PipeReader, PipeWriter, ReadOutcome, WriteOutcome},
    value::{self, display},
};

use telemetry::CommandKind;

pub(crate) mod telemetry;

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
    Piped(PipeReader),
}

enum StreamCommand {
    Lines(builtins::text::lines::LineCommand),
    Text(builtins::text::stream::TextStream),
    Extra(builtins::text::extra::ExtraStream),
}

struct BuiltinInput {
    value: Value,
    charges: Vec<crate::RetainedBytes>,
}

/// Each non-final stage thread reserves this much stack, charged against retained bytes until its
/// join, so the default budget admits sixteen concurrent producers.
const STAGE_STACK_BYTES: usize = 2 * 1024 * 1024;

struct StageOutcome {
    status: ExitCode,
    diagnostics: Vec<String>,
    enclosing: Option<PipeReader>,
}

enum Stage<'scope> {
    Running {
        handle: ScopedJoinHandle<'scope, Result<StageOutcome, FatalError>>,
        _stack: crate::RetainedBytes,
    },
    Refused {
        message: String,
    },
}

enum StageFailure {
    Refused(String),
    Fatal(FatalError),
}

impl Stage<'_> {
    fn join(self) -> Result<StageOutcome, StageFailure> {
        match self {
            Self::Refused { message } => Err(StageFailure::Refused(message)),
            Self::Running { handle, _stack } => match handle.join() {
                Ok(outcome) => outcome.map_err(StageFailure::Fatal),
                Err(panic) => std::panic::resume_unwind(panic),
            },
        }
    }
}

/// A redirected stderr capture must stay bounded by the same output ceilings, since a function can
/// run thousands of commands into it.
#[derive(Debug)]
struct StderrCapture {
    lines: Vec<String>,
    bytes: usize,
    max_lines: usize,
    max_bytes: usize,
    truncated: bool,
}

impl StderrCapture {
    fn new(limits: &Limits) -> Self {
        Self {
            lines: Vec::new(),
            bytes: 0,
            max_lines: limits.max_output_lines,
            max_bytes: limits.max_output_bytes,
            truncated: false,
        }
    }

    fn push(&mut self, line: &str) {
        if self.lines.len() >= self.max_lines || self.bytes + line.len() > self.max_bytes {
            self.truncated = true;
            return;
        }
        self.bytes += line.len();
        self.lines.push(line.to_owned());
    }

    fn finish(mut self) -> Vec<String> {
        if self.truncated {
            self.lines
                .push("... redirected diagnostics truncated ...".to_owned());
        }
        self.lines
    }
}

#[derive(Debug, Default)]
struct Frame {
    locals: BTreeMap<String, Value>,
    positional: Vec<Value>,
    local_charges: BTreeMap<String, Vec<crate::RetainedBytes>>,
    _positional_charges: Vec<crate::RetainedBytes>,
}

struct SavedVariable {
    name: String,
    value: Option<Value>,
    charge: Option<Vec<crate::RetainedBytes>>,
}

pub(crate) fn run(
    script: &str,
    prev: Option<&str>,
    invoker: &dyn CapabilityInvoker,
    limits: Limits,
) -> ScriptOutcome {
    run_with_tree(
        script,
        prev,
        invoker,
        limits,
        &TreeContext::new(limits, CallBudget::new(limits.max_capability_calls)),
    )
}

pub(crate) fn run_with_tree(
    script: &str,
    prev: Option<&str>,
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

    let mut evaluator = Evaluator {
        invoker,
        budget: Budget::start_tree(limits, tree.clone()),
        limits,
        output: OutputBuffer::new(&limits),
        globals: Arc::new(
            prev.map(|prev| ("PREV".to_owned(), Value::String(prev.to_owned())))
                .into_iter()
                .collect(),
        ),
        global_charges: BTreeMap::new(),
        buffer_charges: BTreeMap::new(),
        frames: Vec::new(),
        functions: BTreeMap::new(),
        function_names: BTreeSet::new(),
        buffers: Arc::new(BTreeMap::new()),
        captures: Vec::new(),
        discard_capture_depth: None,
        expansion_charges: Vec::new(),
        options: ShellOptions::default(),
        testing_status: 0,
        stdin: Vec::new(),
        stdout: None,
        reader_gone: false,
        stdout_redirected: false,
        shared_charges: Vec::new(),
        stderr_capture: Vec::new(),
        counters: telemetry::ScriptCounters::default(),
        last_status: ExitCode::SUCCESS,
        last_substitution_status: ExitCode::SUCCESS,
    };

    let script = telemetry::script_span();
    let exit_code = {
        let _entered = script.enter();
        match evaluator.execute_program(&program) {
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

struct Evaluator<'a> {
    invoker: &'a dyn CapabilityInvoker,
    budget: Budget,
    limits: Limits,
    output: OutputBuffer,
    globals: Arc<BTreeMap<String, Value>>,
    global_charges: BTreeMap<String, Vec<crate::RetainedBytes>>,
    buffer_charges: BTreeMap<String, Vec<crate::RetainedBytes>>,
    frames: Vec<Frame>,
    functions: BTreeMap<String, Arc<Program>>,
    function_names: BTreeSet<String>,
    buffers: Arc<BTreeMap<String, Value>>,
    captures: Vec<Vec<CommandResult>>,
    discard_capture_depth: Option<usize>,
    expansion_charges: Vec<crate::RetainedBytes>,
    stdin: Vec<PipeReader>,
    stdout: Option<PipeWriter>,
    reader_gone: bool,
    stdout_redirected: bool,
    shared_charges: Vec<crate::RetainedBytes>,
    options: ShellOptions,
    testing_status: u32,
    stderr_capture: Vec<StderrCapture>,
    counters: telemetry::ScriptCounters,
    last_status: ExitCode,
    last_substitution_status: ExitCode,
}

impl<'a> Evaluator<'a> {
    fn report_fatal(&mut self, fatal: &FatalError) -> ExitCode {
        let message = match fatal {
            FatalError::Limit(LimitExceeded::Steps { maximum }) => format!(
                "dekopon-shell: step budget exhausted after {maximum} steps; the script is doing too much work or looping without progress"
            ),
            FatalError::Limit(LimitExceeded::RecursionDepth { maximum }) => {
                format!("dekopon-shell: shell functions nested deeper than {maximum} frames")
            }
            FatalError::Limit(LimitExceeded::Deadline { timeout_ms }) => {
                format!("dekopon-shell: script exceeded its {timeout_ms}ms deadline")
            }
            FatalError::Limit(LimitExceeded::CapabilityCalls { maximum }) => {
                format!("dekopon-shell: the turn's capability-call budget of {maximum} is spent")
            }
            FatalError::Limit(LimitExceeded::ValueBytes { maximum }) => format!(
                "dekopon-shell: script tried to hold more than {maximum} bytes of values in variables, buffers, and substitutions"
            ),
            FatalError::Limit(LimitExceeded::Cancelled) => {
                "dekopon-shell: script cancelled".to_owned()
            }
            FatalError::Unsupported(reason) | FatalError::Assertion(reason) => {
                format!("dekopon-shell: {reason}")
            }
        };
        self.output.push_block(&message);
        telemetry::fatal_exit_code(fatal)
    }

    fn write_redirected_diagnostics(&mut self, result: &CommandResult) {
        if result.value.is_null() {
            return;
        }
        let text = display(&result.value);
        if result.suppress_newline {
            if let Some(capture) = self.stderr_capture.last_mut() {
                capture.push(text.strip_suffix('\n').unwrap_or(&text));
            } else {
                self.output.push_fragment(&text);
            }
        } else {
            self.write_line(&text);
        }
    }

    fn write_line(&mut self, line: &str) {
        if let Some(capture) = self.stderr_capture.last_mut() {
            capture.push(line);
            return;
        }
        self.output.push_block(line);
    }

    fn output_discarded(&self) -> bool {
        self.discard_capture_depth
            .is_some_and(|depth| self.captures.len() <= depth)
    }

    fn emit(&mut self, result: CommandResult) {
        if result.value.is_null() || self.output_discarded() {
            return;
        }
        if let Some(capture) = self.captures.last_mut() {
            capture.push(result);
            return;
        }
        if let Some(stdout) = self.stdout.as_mut() {
            let written = match write_display(stdout, &result.value) {
                WriteOutcome::Accepted if !result.suppress_newline => stdout.write(b"\n"),
                written @ (WriteOutcome::Accepted | WriteOutcome::ReaderGone) => written,
            };
            match written {
                WriteOutcome::Accepted => {}
                WriteOutcome::ReaderGone => self.reader_gone = true,
            }
            return;
        }
        let text = display(&result.value);
        if result.suppress_newline {
            self.output.push_fragment(&text);
        } else {
            self.output.push_block(&text);
        }
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
        let result = match stdout.strip_suffix('\n') {
            Some(terminated) => CommandResult {
                value: Value::String(terminated.to_owned()),
                status,
                suppress_newline: false,
                retained: vec![retained],
            },
            None => CommandResult {
                value: Value::String(stdout),
                status,
                suppress_newline: true,
                retained: vec![retained],
            },
        };
        Ok(Executed::Result(result))
    }

    fn lookup(&self, name: &str) -> Option<&Value> {
        for frame in self.frames.iter().rev() {
            if let Some(value) = frame.locals.get(name) {
                return Some(value);
            }
        }
        self.globals.get(name)
    }

    fn assign(&mut self, name: &str, mut result: CommandResult) -> Result<(), LimitExceeded> {
        if !self
            .frames
            .iter()
            .any(|frame| frame.locals.contains_key(name))
        {
            self.unshare_globals()?;
        }
        let (values, charges) = self
            .frames
            .iter_mut()
            .rev()
            .find(|frame| frame.locals.contains_key(name))
            .map_or(
                (Arc::make_mut(&mut self.globals), &mut self.global_charges),
                |frame| (&mut frame.locals, &mut frame.local_charges),
            );
        values.remove(name);
        charges.remove(name);
        retain_value(&self.budget, &result.value, &mut result.retained)?;
        values.insert(name.to_owned(), result.value);
        charges.insert(name.to_owned(), result.retained);
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
            let bytes = self.buffers.values().map(value_bytes).sum();
            self.shared_charges
                .push(self.budget.charge_value_bytes(bytes)?);
        }
        Arc::make_mut(&mut self.buffers);
        Ok(())
    }

    fn save_variable(&mut self, name: &str) -> SavedVariable {
        let value = self.lookup(name).cloned();
        let charge = self
            .frames
            .iter_mut()
            .rev()
            .find(|frame| frame.locals.contains_key(name))
            .map_or_else(
                || self.global_charges.remove(name),
                |frame| frame.local_charges.remove(name),
            );
        SavedVariable {
            name: name.to_owned(),
            value,
            charge,
        }
    }

    fn restore(&mut self, saved: SavedVariable) {
        let SavedVariable {
            name,
            value,
            charge,
        } = saved;
        let in_frame = self
            .frames
            .iter()
            .any(|frame| frame.locals.contains_key(&name));
        if !in_frame
            && Arc::strong_count(&self.globals) > 1
            && self.globals.get(&name) == value.as_ref()
        {
            if let Some(charge) = charge {
                self.global_charges.insert(name, charge);
            }
            return;
        }
        let (values, charges) = self
            .frames
            .iter_mut()
            .rev()
            .find(|frame| frame.locals.contains_key(&name))
            .map_or(
                (Arc::make_mut(&mut self.globals), &mut self.global_charges),
                |frame| (&mut frame.locals, &mut frame.local_charges),
            );
        values.remove(&name);
        charges.remove(&name);
        if let Some(value) = value {
            values.insert(name.clone(), value);
        }
        if let Some(charge) = charge {
            charges.insert(name, charge);
        }
    }

    fn declare_local(&mut self, name: &str, value: Value) -> Result<(), LimitExceeded> {
        if let Some(frame) = self.frames.last_mut() {
            frame.locals.remove(name);
            frame.local_charges.remove(name);
            let charge = self.budget.charge_value_bytes(value_bytes(&value))?;
            frame.locals.insert(name.to_owned(), value);
            frame.local_charges.insert(name.to_owned(), vec![charge]);
            return Ok(());
        }
        self.assign(name, CommandResult::value(value))
    }

    fn positional(&self) -> &[Value] {
        self.frames
            .last()
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
        self.budget.charge_step_with(self.invoker)?;
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
        let subject = match self.expand_word(&statement.subject) {
            Ok(expanded) => expanded.join(" "),
            Err(failure) => {
                let status = self.absorb(failure)?;
                self.last_status = status;
                return Ok(Flow::Normal);
            }
        };

        for clause in &statement.clauses {
            for pattern in &clause.patterns {
                self.budget.charge_step_with(self.invoker)?;
                let matched = match pattern {
                    CasePattern::Any => true,
                    CasePattern::Literal(word) => match self.expand_word(word) {
                        Ok(expanded) => expanded.join(" ") == subject,
                        Err(failure) => {
                            let status = self.absorb(failure)?;
                            self.last_status = status;
                            return Ok(Flow::Normal);
                        }
                    },
                    CasePattern::Expanded(word) => {
                        let expanded = match self.expand_word(word) {
                            Ok(expanded) => expanded.join(" "),
                            Err(failure) => {
                                let status = self.absorb(failure)?;
                                self.last_status = status;
                                return Ok(Flow::Normal);
                            }
                        };
                        if let Some((character, meaning)) = pattern_metacharacter(&expanded) {
                            self.write_line(&format!(
                                "dekopon-shell: {}",
                                expanded_case_pattern(character, meaning)
                            ));
                            self.last_status = ExitCode::SYNTAX;
                            return Ok(Flow::Normal);
                        }
                        expanded == subject
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
            self.budget.charge_step_with(self.invoker)?;
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
            self.budget.charge_step_with(self.invoker)?;
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
        if names.first().is_some_and(|first| first == "-r") {
            names = &names[1..];
        }
        if let Some(flag) = names.first().filter(|first| first.starts_with('-')) {
            return Err(CommandFailure::usage(format!(
                "read: option {flag:?} is not supported; this shell has only `read [-r] NAME...`"
            )));
        }
        if names.is_empty() {
            return Err(CommandFailure::usage(
                "read: needs at least one variable name to bind",
            ));
        }
        for name in names {
            if !is_variable_name(name) {
                return Err(CommandFailure::usage(format!(
                    "read: {name:?} is not a valid variable name"
                )));
            }
        }

        let line = match input {
            StageInput::Piped(mut reader) => reader.read_line(&self.budget, self.invoker)?,
            StageInput::Inherited => match self.stdin.last_mut() {
                Some(reader) => reader.read_line(&self.budget, self.invoker)?,
                None => None,
            },
        };
        let Some(line) = line else {
            return Ok(CommandResult::status(ExitCode::FAILURE));
        };
        let line = String::from_utf8(line.bytes)
            .map_err(|_not_utf8| CommandFailure::failed("read: input is not valid UTF-8 text"))?;

        let fields = split_read_fields(&line, names.len());
        for (index, name) in names.iter().enumerate() {
            let field = fields.get(index).copied().unwrap_or_default();
            self.assign(name, CommandResult::value(Value::String(field.to_owned())))?;
        }
        Ok(CommandResult::status(ExitCode::SUCCESS))
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
        Arc::make_mut(&mut self.globals).insert(
            "PIPESTATUS".to_owned(),
            Value::Array(
                stages
                    .into_iter()
                    .map(|stage| Value::from(stage.get()))
                    .collect(),
            ),
        );
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
        self.budget.charge_step_with(self.invoker)?;
        if self.reader_gone && self.captures.is_empty() {
            return Ok((self.last_status, Some(Flow::Exit(self.last_status))));
        }
        let Some((last, producers)) = pipeline.commands.split_last() else {
            return Ok((ExitCode::SUCCESS, None));
        };

        let (outcomes, executed) = thread::scope(|scope| {
            let mut input = StageInput::Inherited;
            let mut running = Vec::with_capacity(producers.len());
            for (index, command) in producers.iter().enumerate() {
                let (writer, reader) = pipe::pipe();
                let stage_input = std::mem::replace(&mut input, StageInput::Piped(reader));
                running.push(self.spawn_stage(scope, index, command, stage_input, writer));
            }
            let executed = self.tested(pipeline.negated, |evaluator| {
                evaluator.execute_command(last, input, false)
            });
            let outcomes = running.into_iter().map(Stage::join).collect::<Vec<_>>();
            (outcomes, executed)
        });
        self.shared_charges.clear();

        let mut stages = Vec::with_capacity(pipeline.commands.len());
        let mut fatal = None;
        for (index, outcome) in outcomes.into_iter().enumerate() {
            match outcome {
                Ok(outcome) => {
                    for line in outcome.diagnostics {
                        self.write_line(&line);
                    }
                    if index == 0
                        && let (Some(slot), Some(reader)) =
                            (self.stdin.last_mut(), outcome.enclosing)
                    {
                        *slot = reader;
                    }
                    stages.push(outcome.status);
                }
                Err(StageFailure::Refused(message)) => {
                    self.write_line(&message);
                    stages.push(ExitCode::FAILURE);
                }
                Err(StageFailure::Fatal(error)) => {
                    fatal.get_or_insert(error);
                    stages.push(ExitCode::FAILURE);
                }
            }
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
        self.emit(last);
        Ok((status, None))
    }

    /// The enclosing compound's stream moves into the first stage for the stage's lifetime and
    /// comes back at its join, so whatever it leaves unread is still there for the next command.
    fn spawn_stage<'scope, 'env>(
        &mut self,
        scope: &'scope Scope<'scope, 'env>,
        index: usize,
        command: &'env Command,
        input: StageInput,
        writer: PipeWriter,
    ) -> Stage<'scope>
    where
        'a: 'env,
    {
        let refused = |reason: &dyn std::fmt::Display| Stage::Refused {
            message: format!("dekopon-shell: pipeline stage {index}: {reason}"),
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
        let enclosing = match input {
            StageInput::Inherited => self.stdin.last_mut().map(std::mem::take),
            StageInput::Piped(_) => None,
        };
        let span = tracing::Span::current();
        let dispatcher = tracing::dispatcher::get_default(Clone::clone);
        let spawned = thread::Builder::new()
            .name(format!("dekopon-shell-stage-{index}"))
            .stack_size(STAGE_STACK_BYTES)
            .spawn_scoped(scope, move || {
                tracing::dispatcher::with_default(&dispatcher, || {
                    let _entered = span.enter();
                    let inherited = enclosing.is_some();
                    if let Some(reader) = enclosing {
                        stage.stdin.push(reader);
                    }
                    let executed =
                        stage.tested(true, |stage| stage.execute_command(command, input, false))?;
                    let status = match executed {
                        Executed::Result(result) => {
                            let status = result.status;
                            stage.emit(result);
                            status
                        }
                        Executed::Flow(Flow::Exit(status) | Flow::Return(status)) => status,
                        Executed::Flow(Flow::Normal | Flow::Break(_) | Flow::Continue(_)) => {
                            stage.last_status
                        }
                    };
                    stage.stdout = None;
                    let enclosing = if inherited { stage.stdin.pop() } else { None };
                    let diagnostics = stage
                        .stderr_capture
                        .pop()
                        .map(StderrCapture::finish)
                        .unwrap_or_default();
                    Ok(StageOutcome {
                        status,
                        diagnostics,
                        enclosing,
                    })
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

    fn snapshot(&mut self, writer: PipeWriter) -> Result<Evaluator<'a>, LimitExceeded> {
        let mut frames = Vec::with_capacity(self.frames.len());
        for frame in &self.frames {
            let mut local_charges = BTreeMap::new();
            for (name, value) in &frame.locals {
                local_charges.insert(
                    name.clone(),
                    vec![self.budget.charge_value_bytes(value_bytes(value))?],
                );
            }
            let positional_charges = frame
                .positional
                .iter()
                .map(|value| self.budget.charge_value_bytes(value_bytes(value)))
                .collect::<Result<Vec<_>, _>>()?;
            frames.push(Frame {
                locals: frame.locals.clone(),
                positional: frame.positional.clone(),
                local_charges,
                _positional_charges: positional_charges,
            });
        }
        Ok(Evaluator {
            invoker: self.invoker,
            budget: self.budget.fork(),
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
            discard_capture_depth: None,
            expansion_charges: Vec::new(),
            options: self.options,
            testing_status: 0,
            stdin: Vec::new(),
            stdout: Some(writer),
            reader_gone: false,
            stdout_redirected: false,
            shared_charges: Vec::new(),
            stderr_capture: vec![StderrCapture::new(&self.limits)],
            counters: telemetry::ScriptCounters::default(),
            last_status: self.last_status,
            last_substitution_status: ExitCode::SUCCESS,
        })
    }

    fn copy_stdin(&mut self, input: StageInput) -> Result<CommandResult, CommandFailure> {
        let Self {
            stdin,
            stdout,
            output,
            budget,
            invoker,
            reader_gone,
            ..
        } = self;
        let mut piped;
        let reader = match input {
            StageInput::Piped(reader) => {
                piped = reader;
                &mut piped
            }
            StageInput::Inherited => match stdin.last_mut() {
                Some(reader) => reader,
                None => return Ok(CommandResult::status(ExitCode::SUCCESS)),
            },
        };
        let mut partial = Vec::new();
        while let ReadOutcome::Bytes(chunk) = reader.read(budget, *invoker)? {
            budget.charge_step_with(*invoker)?;
            let Some(writer) = stdout.as_mut() else {
                partial.extend_from_slice(&chunk);
                let complete = lossy_complete_prefix(&partial);
                output.push_fragment(&String::from_utf8_lossy(&partial[..complete]));
                partial.drain(..complete);
                continue;
            };
            match writer.write(&chunk) {
                WriteOutcome::Accepted => {}
                WriteOutcome::ReaderGone => {
                    *reader_gone = true;
                    break;
                }
            }
        }
        if !partial.is_empty() {
            output.push_fragment(&String::from_utf8_lossy(&partial));
        }
        Ok(CommandResult::status(ExitCode::SUCCESS))
    }

    fn builtin_input(
        &mut self,
        reads_stdin: bool,
        input: StageInput,
    ) -> Result<Option<BuiltinInput>, CommandFailure> {
        let bytes = match input {
            StageInput::Piped(mut reader) if reads_stdin => {
                reader.drain(&self.budget, self.invoker)?
            }
            StageInput::Inherited if reads_stdin => match self.stdin.last_mut() {
                Some(reader) => reader.drain(&self.budget, self.invoker)?,
                None => return Ok(None),
            },
            StageInput::Piped(_) | StageInput::Inherited => return Ok(None),
        };
        if bytes.bytes.is_empty() {
            return Ok(None);
        }
        let (bytes, charges) = bytes.into_parts();
        let text = String::from_utf8(bytes).map_err(|_not_utf8| {
            CommandFailure::failed("standard input is not valid UTF-8 text")
        })?;
        let value = Value::String(text);
        telemetry::record_stdin(&tracing::Span::current(), &value);
        Ok(Some(BuiltinInput { value, charges }))
    }

    fn execute_command(
        &mut self,
        command: &Command,
        input: StageInput,
        capture_output: bool,
    ) -> Result<Executed, FatalError> {
        let parent = std::mem::take(&mut self.expansion_charges);
        let result = match command {
            Command::Simple(command) => self.execute_simple_command(command, input, capture_output),
            Command::Compound {
                statement,
                redirects,
            } => self.execute_compound_command(statement, redirects, input, capture_output),
        };
        self.expansion_charges = parent;
        result
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
    fn execute_compound_command(
        &mut self,
        statement: &Statement,
        redirects: &[Redirect],
        input: StageInput,
        capture_output: bool,
    ) -> Result<Executed, FatalError> {
        let (stdout, stderr) = match self.resolve_redirects(redirects) {
            Ok(sinks) => sinks,
            Err(failure) => {
                let status = self.absorb(failure)?;
                return Ok(Executed::Result(CommandResult::status(status)));
            }
        };
        let collect = capture_output || stdout != Sink::Value;

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
        if collect {
            self.captures.push(Vec::new());
        }

        let flow = self.execute_statement(statement);

        let mut captured = if collect {
            self.captures.pop().unwrap_or_default()
        } else {
            Vec::new()
        };
        if piped {
            self.stdin.pop();
        }
        let mut diagnostics = capturing
            .then(|| self.stderr_capture.pop())
            .flatten()
            .map(StderrCapture::finish)
            .unwrap_or_default();

        let flow = flow?;
        let retained = capture_charges(&mut captured);
        let value = if collect {
            reduce_captured(captured)
        } else {
            Value::Null
        };
        let status = match flow {
            Flow::Normal => self.last_status,
            _ => {
                self.route_diagnostics(diagnostics, &stderr)?;
                return Ok(Executed::Flow(flow));
            }
        };

        let mut result = CommandResult {
            value,
            status,
            suppress_newline: false,
            retained,
        };
        if stderr == Sink::Value {
            result.value = merge_diagnostics(result.value, diagnostics);
            diagnostics = Vec::new();
        }
        self.open_buffers(&[&stdout, &stderr])?;
        self.route_diagnostics(diagnostics, &stderr)?;

        match stdout {
            Sink::Value => Ok(Executed::Result(result)),
            Sink::Discard => Ok(Executed::Result(CommandResult::status(result.status))),
            Sink::Diagnostics => {
                self.write_redirected_diagnostics(&result);
                Ok(Executed::Result(CommandResult::status(result.status)))
            }
            Sink::Buffer { name, .. } => {
                let status = result.status;
                self.append_buffer(&name, result)?;
                Ok(Executed::Result(CommandResult::status(status)))
            }
        }
    }

    fn execute_simple_command(
        &mut self,
        command: &SimpleCommand,
        input: StageInput,
        capture_output: bool,
    ) -> Result<Executed, FatalError> {
        self.budget.charge_step_with(self.invoker)?;

        let mut argv = Vec::new();
        for word in &command.words {
            match self.expand_word(word) {
                Ok(expanded) => argv.extend(expanded),
                Err(failure) => {
                    let status = self.absorb(failure)?;
                    return Ok(Executed::Result(CommandResult::status(status)));
                }
            }
        }

        // `--help` wins only when the script wrote it as the command's one bare, unquoted word.
        let literal_help = argv.len() == 2
            && matches!(
                command.words.as_slice(),
                [_, Word { parts }] if matches!(parts.as_slice(), [WordPart::Literal(only)] if only == "--help")
            );

        let transient = !argv.is_empty();
        let mut restore = Vec::new();
        let mut assignment_status = ExitCode::SUCCESS;
        for assignment in &command.assignments {
            let value = match self.assignment_value(&assignment.value) {
                Ok(value) => value,
                Err(failure) => {
                    self.restore_all(restore);
                    let status = self.absorb(failure)?;
                    return Ok(Executed::Result(CommandResult::status(status)));
                }
            };
            assignment_status = self.last_substitution_status;
            if transient {
                restore.push(self.save_variable(&assignment.name));
            }
            if let Err(limit) = self.assign(&assignment.name, value) {
                self.restore_all(restore);
                return Err(limit.into());
            }
        }

        if argv.is_empty() {
            let status = if command.assignments.is_empty() {
                ExitCode::SUCCESS
            } else {
                assignment_status
            };
            return Ok(Executed::Result(CommandResult::status(status)));
        }

        let mut here_doc_charges = Vec::new();
        let start = self.expansion_charges.len();
        let input = match &command.here_doc {
            None => input,
            Some(body) => match self.expand_quoted(&body.parts) {
                Ok(text) => {
                    here_doc_charges = self.expansion_charges.split_off(start);
                    match self.budget.charge_value_bytes(text.len() as u64) {
                        Ok(charge) => here_doc_charges.push(charge),
                        Err(limit) => {
                            self.restore_all(restore);
                            return Err(limit.into());
                        }
                    }
                    StageInput::Piped(PipeReader::from_bytes(text.into_bytes()))
                }
                Err(failure) => {
                    self.restore_all(restore);
                    let status = self.absorb(failure)?;
                    return Ok(Executed::Result(CommandResult::status(status)));
                }
            },
        };

        let (stdout, stderr) = match self.resolve_redirects(&command.redirects) {
            Ok(sinks) => sinks,
            Err(failure) => {
                self.restore_all(restore);
                let status = self.absorb(failure)?;
                return Ok(Executed::Result(CommandResult::status(status)));
            }
        };

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
        let executed = self.run_argv(&argv, input, capture_output, literal_help, &stdout);
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
            result.value = merge_diagnostics(result.value, diagnostics);
            diagnostics = Vec::new();
        }

        // A redirect truncates its target once, when it is set up, not on every write, so combining
        // a value redirect with a merged stderr redirect cannot let one silently overwrite the
        // other depending on order.
        self.open_buffers(&[&stdout, &stderr])?;
        self.route_diagnostics(diagnostics, &stderr)?;

        match stdout {
            Sink::Value => Ok(Executed::Result(result)),
            Sink::Discard => Ok(Executed::Result(CommandResult::status(result.status))),
            Sink::Diagnostics => {
                self.write_redirected_diagnostics(&result);
                Ok(Executed::Result(CommandResult::status(result.status)))
            }
            Sink::Buffer { name, .. } => {
                let status = result.status;
                self.append_buffer(&name, result)?;
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
                Arc::make_mut(&mut self.buffers).insert(name.clone(), Value::Null);
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
        diagnostics: Vec<String>,
        sink: &Sink,
    ) -> Result<(), FatalError> {
        if diagnostics.is_empty() {
            return Ok(());
        }
        match sink {
            Sink::Discard => {}
            Sink::Diagnostics | Sink::Value => {
                for line in diagnostics {
                    self.write_line(&line);
                }
            }
            Sink::Buffer { name, .. } => {
                let name = name.clone();
                self.append_buffer(&name, CommandResult::value(value::from_lines(diagnostics)))?;
            }
        }
        Ok(())
    }

    fn restore_all(&mut self, restore: Vec<SavedVariable>) {
        for saved in restore.into_iter().rev() {
            self.restore(saved);
        }
    }

    fn append_buffer(
        &mut self,
        name: &str,
        mut result: CommandResult,
    ) -> Result<(), LimitExceeded> {
        if result.value.is_null() {
            return Ok(());
        }
        retain_value(&self.budget, &result.value, &mut result.retained)?;
        self.buffer_charges
            .entry(name.to_owned())
            .or_default()
            .extend(result.retained);
        let value = result.value;
        self.unshare_buffers()?;
        let buffers = Arc::make_mut(&mut self.buffers);
        match buffers.remove(name) {
            None | Some(Value::Null) => {
                buffers.insert(name.to_owned(), value);
            }
            Some(Value::Array(mut existing)) => {
                existing.push(value);
                buffers.insert(name.to_owned(), Value::Array(existing));
            }
            Some(existing) => {
                buffers.insert(name.to_owned(), Value::Array(vec![existing, value]));
            }
        }
        Ok(())
    }

    fn run_argv(
        &mut self,
        argv: &[String],
        input: StageInput,
        capture_output: bool,
        literal_help: bool,
        stdout_sink: &Sink,
    ) -> Result<Executed, FatalError> {
        let command = argv[0].as_str();
        let arguments = &argv[1..];

        let (kind, resolution) = if telemetry::is_control_word(command) {
            (CommandKind::Control, None)
        } else {
            let resolution = dispatch::resolve(command, &self.function_names, self.invoker);
            (CommandKind::of(&resolution), Some(resolution))
        };
        self.counters.charge(kind);
        let span = telemetry::command_span(command, kind, arguments.len());
        let _entered = span.enter();
        let traced = !span.is_disabled();
        if traced {
            telemetry::record_arguments(&span, arguments);
        }

        let executed = self.dispatch_command(
            command,
            arguments,
            resolution,
            input,
            capture_output,
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
        self.counters.record_status(status);
        executed
    }

    fn run_base64(
        &mut self,
        arguments: &[String],
        input: StageInput,
        capture_output: bool,
        stdout_sink: &Sink,
        literal_help: bool,
    ) -> Result<Executed, FatalError> {
        if literal_help {
            return Ok(Executed::Result(builtins::help_result(
                "base64",
                crate::builtins::encode::HELP,
            )));
        }
        let mut reader = match input {
            StageInput::Piped(reader) => reader,
            StageInput::Inherited => self
                .stdin
                .last_mut()
                .map(std::mem::take)
                .unwrap_or_default(),
        };
        let mut bytes = Vec::new();
        let mut charges = Vec::new();
        let mut utf8_pending = Vec::new();
        let retention_budget = self.budget.fork();
        let discard = self.output_discarded();
        let outcome = crate::builtins::encode::stream(
            arguments,
            &mut reader,
            &mut self.budget,
            self.invoker,
            |chunk| {
                if matches!(stdout_sink, Sink::Discard) {
                    return Ok(true);
                }
                if let Some(writer) = self.stdout.as_mut() {
                    return Ok(match writer.write(chunk) {
                        WriteOutcome::Accepted => true,
                        WriteOutcome::ReaderGone => {
                            self.reader_gone = true;
                            false
                        }
                    });
                }
                if matches!(stdout_sink, Sink::Value) && discard {
                    return Ok(true);
                }
                if matches!(stdout_sink, Sink::Value) && !capture_output && self.captures.is_empty()
                {
                    utf8_pending.extend_from_slice(chunk);
                    let complete = lossy_complete_prefix(&utf8_pending);
                    self.output
                        .push_fragment(&String::from_utf8_lossy(&utf8_pending[..complete]));
                    utf8_pending.drain(..complete);
                } else {
                    charges.push(retention_budget.charge_value_bytes(chunk.len() as u64)?);
                    bytes.extend_from_slice(chunk);
                }
                Ok(true)
            },
        );
        let status = match outcome {
            Ok(()) => ExitCode::SUCCESS,
            Err(failure) => self.absorb(failure)?,
        };
        if !utf8_pending.is_empty() {
            self.output
                .push_fragment(&String::from_utf8_lossy(&utf8_pending));
        }
        if bytes.is_empty() || status != ExitCode::SUCCESS {
            return Ok(Executed::Result(CommandResult::status(status)));
        }
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_invalid_utf8) => {
                let status = self.absorb(CommandFailure::failed(
                    "standard input is not valid UTF-8 text",
                ))?;
                return Ok(Executed::Result(CommandResult::status(status)));
            }
        };
        Ok(Executed::Result(CommandResult {
            value: Value::String(text),
            status,
            suppress_newline: true,
            retained: charges,
        }))
    }

    fn run_lines(
        &mut self,
        command: StreamCommand,
        arguments: &[String],
        input: StageInput,
        literal_help: bool,
        stdout_sink: &Sink,
    ) -> Result<Executed, FatalError> {
        if literal_help {
            let (name, help) = match command {
                StreamCommand::Lines(command) => (command.name(), command.help()),
                StreamCommand::Text(command) => (command.name(), command.help()),
                StreamCommand::Extra(command) => (command.name(), command.help()),
            };
            return Ok(Executed::Result(builtins::help_result(name, help)));
        }
        let retention_budget = self.budget.fork();
        let mut redirected_bytes = Vec::new();
        let mut redirect_charges = Vec::new();
        let mut utf8_pending = Vec::new();
        let mut reader = match input {
            StageInput::Piped(reader) => reader,
            StageInput::Inherited => self
                .stdin
                .last_mut()
                .map(std::mem::take)
                .unwrap_or_default(),
        };
        let discard = self.output_discarded();
        let mut emit = |line: &[u8]| {
            if matches!(stdout_sink, Sink::Discard) {
                return Ok(true);
            }
            if let Some(writer) = self.stdout.as_mut() {
                match writer.write(line) {
                    WriteOutcome::Accepted => return Ok(true),
                    WriteOutcome::ReaderGone => {
                        self.reader_gone = true;
                        return Ok(false);
                    }
                }
            }
            if matches!(stdout_sink, Sink::Value) && discard {
                return Ok(true);
            }
            if !matches!(stdout_sink, Sink::Value) {
                let charge = retention_budget.charge_value_bytes(line.len() as u64)?;
                redirected_bytes.extend_from_slice(line);
                redirect_charges.push(charge);
                return Ok(true);
            }
            utf8_pending.extend_from_slice(line);
            let complete = match std::str::from_utf8(&utf8_pending) {
                Ok(text) => text.len(),
                Err(error) if error.error_len().is_none() => error.valid_up_to(),
                Err(_invalid_utf8) if self.captures.is_empty() => {
                    lossy_complete_prefix(&utf8_pending)
                }
                Err(_invalid_utf8) => {
                    return Err(CommandFailure::failed(
                        "standard input is not valid UTF-8 text",
                    ));
                }
            };
            if complete > 0 {
                let text = String::from_utf8_lossy(&utf8_pending[..complete]);
                if let Some(capture) = self.captures.last_mut() {
                    let charge = retention_budget.charge_value_bytes(complete as u64)?;
                    let mut result =
                        CommandResult::value(Value::String(text.into_owned())).without_newline();
                    result.retained.push(charge);
                    capture.push(result);
                } else {
                    self.output.push_fragment(&text);
                }
                utf8_pending.drain(..complete);
            }
            Ok(true)
        };
        let outcome = match command {
            StreamCommand::Lines(command) => command
                .run(
                    arguments,
                    &mut reader,
                    &mut self.budget,
                    self.invoker,
                    &mut emit,
                )
                .map(|()| ExitCode::SUCCESS),
            StreamCommand::Extra(command) => command.run(
                arguments,
                &mut reader,
                &mut self.budget,
                self.invoker,
                &mut emit,
            ),
            StreamCommand::Text(command) => command.run(
                arguments,
                &mut reader,
                &mut self.budget,
                self.invoker,
                &mut emit,
            ),
        };
        let mut status = match outcome {
            Ok(status) => status,
            Err(failure) => self.absorb(failure)?,
        };
        if status == ExitCode::SUCCESS && !utf8_pending.is_empty() {
            if self.captures.is_empty() {
                self.output
                    .push_fragment(&String::from_utf8_lossy(&utf8_pending));
            } else {
                status = self.absorb(CommandFailure::failed(
                    "standard input is not valid UTF-8 text",
                ))?;
            }
        }
        if !redirected_bytes.is_empty() {
            let text = String::from_utf8(redirected_bytes).map_err(|_invalid_utf8| {
                FatalError::Unsupported("standard input is not valid UTF-8 text".to_owned())
            })?;
            let mut result = CommandResult::value(Value::String(text)).without_newline();
            result.status = status;
            result.retained = redirect_charges;
            return Ok(Executed::Result(result));
        }
        Ok(Executed::Result(CommandResult::status(status)))
    }

    fn run_simple_builtin(
        &mut self,
        builtin: &dyn builtins::Builtin,
        arguments: &[String],
        input: StageInput,
        capture_output: bool,
        literal_help: bool,
    ) -> Result<Executed, FatalError> {
        if literal_help {
            return Ok(Executed::Result(builtins::help_result(
                builtin.name(),
                builtin.help(),
            )));
        }
        if builtin.copies_stdin()
            && arguments.is_empty()
            && !capture_output
            && self.captures.is_empty()
            && (self.stdout.is_some() || !self.stdout_redirected)
        {
            return match self.copy_stdin(input) {
                Ok(result) => Ok(Executed::Result(result)),
                Err(failure) => {
                    let status = self.absorb(failure)?;
                    Ok(Executed::Result(CommandResult::status(status)))
                }
            };
        }
        let input = match self.builtin_input(builtin.reads_stdin(), input) {
            Ok(input) => input,
            Err(failure) => {
                let status = self.absorb(failure)?;
                return Ok(Executed::Result(CommandResult::status(status)));
            }
        };
        let (value, charges) = input.map(|input| (input.value, input.charges)).unzip();
        let outcome = {
            let mut context = BuiltinContext {
                invoker: self.invoker,
                budget: &mut self.budget,
                buffers: &self.buffers,
            };
            builtin.run(&mut context, arguments, value)
        };
        drop(charges);
        match outcome {
            Ok(result) => Ok(Executed::Result(result)),
            Err(failure) => {
                let status = self.absorb(failure)?;
                Ok(Executed::Result(CommandResult::status(status)))
            }
        }
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
        input: StageInput,
        capture_output: bool,
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
            Resolution::Function => {
                self.call_function(command, arguments, input, capture_output, stdout_sink)
            }
            Resolution::Builtin(BuiltinKind::Simple(builtin)) => {
                self.run_simple_builtin(builtin, arguments, input, capture_output, literal_help)
            }
            Resolution::Builtin(BuiltinKind::Base64) => {
                self.run_base64(arguments, input, capture_output, stdout_sink, literal_help)
            }
            Resolution::Builtin(BuiltinKind::Lines(command)) => self.run_lines(
                StreamCommand::Lines(command),
                arguments,
                input,
                literal_help,
                stdout_sink,
            ),
            Resolution::Builtin(BuiltinKind::TextStream(command)) => self.run_lines(
                StreamCommand::Text(command),
                arguments,
                input,
                literal_help,
                stdout_sink,
            ),
            Resolution::Builtin(BuiltinKind::Extra(command)) => self.run_lines(
                StreamCommand::Extra(command),
                arguments,
                input,
                literal_help,
                stdout_sink,
            ),
            Resolution::Builtin(BuiltinKind::Xargs) => {
                if literal_help {
                    return Ok(Executed::Result(builtins::help_result(
                        xargs::NAME,
                        xargs::HELP,
                    )));
                }
                self.run_xargs(arguments, input)
            }
            Resolution::ProviderCommand => {
                let stdin = match input {
                    StageInput::Piped(_) => match self.builtin_input(true, input) {
                        Ok(stdin) => stdin,
                        Err(failure) => {
                            let status = self.absorb(failure)?;
                            return Ok(Executed::Result(CommandResult::status(status)));
                        }
                    },
                    StageInput::Inherited => None,
                };
                self.budget.check_deadline()?;
                let stdin_text = stdin.as_ref().map(|input| display(&input.value));
                let run = self
                    .invoker
                    .run_command(command, arguments, stdin_text.as_deref());
                self.budget.check_deadline()?;
                let proposal = match run {
                    Some(CommandRun::Proposed {
                        capability,
                        input,
                        secret_use,
                        report,
                    }) => {
                        let mut proposal =
                            crate::CommandProposal::new(capability, input, secret_use);
                        proposal.report = report;
                        proposal
                    }
                    Some(CommandRun::Failed { message }) => {
                        let status = self.absorb(CommandFailure::usage(message))?;
                        return Ok(Executed::Result(CommandResult::status(status)));
                    }
                    Some(CommandRun::Errored { message }) => {
                        let status = self.absorb(CommandFailure::failed(format!(
                            "{command}: failed: {message}"
                        )))?;
                        return Ok(Executed::Result(CommandResult::status(status)));
                    }
                    Some(CommandRun::Denied { reason }) => {
                        let status = self.absorb(CommandFailure::Status {
                            message: format!("{command}: denied: {reason}"),
                            status: ExitCode::DENIED,
                        })?;
                        return Ok(Executed::Result(CommandResult::status(status)));
                    }
                    Some(CommandRun::Rendered {
                        stdout,
                        stderr,
                        status,
                    }) => return self.emit_rendered(stdout, stderr, status),
                    None => {
                        self.write_line(&format!("dekopon-shell: {command}: command not found"));
                        return Ok(Executed::Result(CommandResult::status(ExitCode::NOT_FOUND)));
                    }
                };
                if !self.invoker.is_granted(&proposal.capability) {
                    self.write_line(&format!(
                        "dekopon-shell: {command}: requires capability {}, which is not \
                         granted in this session",
                        proposal.capability
                    ));
                    return Ok(Executed::Result(CommandResult::status(ExitCode::NOT_FOUND)));
                }
                let outcome = {
                    let mut context = BuiltinContext {
                        invoker: self.invoker,
                        budget: &mut self.budget,
                        buffers: &self.buffers,
                    };
                    context.invoke_proposal(proposal)
                };
                match outcome {
                    Ok(result) => Ok(Executed::Result(self.provider_value(result))),
                    Err(failure) => {
                        let status = self.absorb(failure)?;
                        Ok(Executed::Result(CommandResult::status(status)))
                    }
                }
            }
            Resolution::NotFound => {
                self.write_line(&format!("dekopon-shell: {command}: command not found"));
                Ok(Executed::Result(CommandResult::status(ExitCode::NOT_FOUND)))
            }
        }
    }

    fn provider_value(&self, mut result: CommandResult) -> CommandResult {
        if result.value.is_null() && self.stdout.is_some() && self.captures.is_empty() {
            result.value = Value::String(String::new());
        }
        result
    }

    fn run_control_word(
        &mut self,
        command: &str,
        arguments: &[String],
        input: StageInput,
    ) -> Result<Option<Executed>, FatalError> {
        let executed = match command {
            "break" | "continue" => {
                let level = match parse_level(command, arguments) {
                    Ok(level) => level,
                    Err(failure) => {
                        let status = self.absorb(failure)?;
                        return Ok(Some(Executed::Result(CommandResult::status(status))));
                    }
                };
                Executed::Flow(if command == "break" {
                    Flow::Break(level)
                } else {
                    Flow::Continue(level)
                })
            }
            "set" => match self.apply_set_options(arguments) {
                Ok(()) => Executed::Result(CommandResult::status(ExitCode::SUCCESS)),
                Err(failure) => {
                    let status = self.absorb(failure)?;
                    Executed::Result(CommandResult::status(status))
                }
            },
            "return" => {
                if self.frames.is_empty() {
                    self.write_line("dekopon-shell: return: only valid inside a function");
                    return Ok(Some(Executed::Result(CommandResult::status(
                        ExitCode::SYNTAX,
                    ))));
                }
                let status = match parse_status(command, arguments, self.last_status) {
                    Ok(status) => status,
                    Err(failure) => {
                        let status = self.absorb(failure)?;
                        return Ok(Some(Executed::Result(CommandResult::status(status))));
                    }
                };
                Executed::Flow(Flow::Return(status))
            }
            "exit" => {
                let status = match parse_status(command, arguments, self.last_status) {
                    Ok(status) => status,
                    Err(failure) => {
                        let status = self.absorb(failure)?;
                        return Ok(Some(Executed::Result(CommandResult::status(status))));
                    }
                };
                Executed::Flow(Flow::Exit(status))
            }
            "read" => match self.run_read(arguments, input) {
                Ok(result) => Executed::Result(result),
                Err(failure) => {
                    let status = self.absorb(failure)?;
                    Executed::Result(CommandResult::status(status))
                }
            },
            "local" => {
                if self.frames.is_empty() {
                    self.write_line("dekopon-shell: local: only valid inside a function");
                    return Ok(Some(Executed::Result(CommandResult::status(
                        ExitCode::SYNTAX,
                    ))));
                }
                for argument in arguments {
                    match argument.split_once('=') {
                        Some((name, text)) => {
                            self.declare_local(name, value::scalar_from_token(text))?;
                        }
                        None => self.declare_local(argument, Value::String(String::new()))?,
                    }
                }
                Executed::Result(CommandResult::status(ExitCode::SUCCESS))
            }
            "shift" => {
                let Some(frame) = self.frames.last_mut() else {
                    self.write_line("dekopon-shell: shift: only valid inside a function");
                    return Ok(Some(Executed::Result(CommandResult::status(
                        ExitCode::SYNTAX,
                    ))));
                };
                let count = match parse_shift_count(arguments) {
                    Ok(count) => count,
                    Err(failure) => {
                        let status = self.absorb(failure)?;
                        return Ok(Some(Executed::Result(CommandResult::status(status))));
                    }
                };
                if count > frame.positional.len() {
                    Executed::Result(CommandResult::status(ExitCode::FAILURE))
                } else {
                    frame.positional.drain(..count);
                    frame._positional_charges.drain(..count);
                    Executed::Result(CommandResult::status(ExitCode::SUCCESS))
                }
            }
            "unset" => {
                self.unshare_globals()?;
                for name in arguments {
                    Arc::make_mut(&mut self.globals).remove(name);
                    self.global_charges.remove(name);
                    for frame in &mut self.frames {
                        frame.locals.remove(name);
                        frame.local_charges.remove(name);
                    }
                }
                Executed::Result(CommandResult::status(ExitCode::SUCCESS))
            }
            ":" => Executed::Result(CommandResult::status(ExitCode::SUCCESS)),
            _ => return Ok(None),
        };
        Ok(Some(executed))
    }

    fn call_function(
        &mut self,
        name: &str,
        arguments: &[String],
        input: StageInput,
        capture_output: bool,
        stdout_sink: &Sink,
    ) -> Result<Executed, FatalError> {
        let capture_output =
            *stdout_sink != Sink::Discard && (capture_output || *stdout_sink != Sink::Value);
        let Some(body) = self.functions.get(name).cloned() else {
            self.write_line(&format!("dekopon-shell: {name}: command not found"));
            return Ok(Executed::Result(CommandResult::status(ExitCode::NOT_FOUND)));
        };

        self.budget.charge_step_with(self.invoker)?;
        self.budget.enter_call()?;
        let positional = arguments
            .iter()
            .map(|argument| Value::String(argument.clone()))
            .collect::<Vec<_>>();
        let positional_charges = positional
            .iter()
            .map(|argument| self.budget.charge_value_bytes(value_bytes(argument)))
            .collect::<Result<Vec<_>, _>>()?;
        self.frames.push(Frame {
            locals: BTreeMap::new(),
            positional,
            local_charges: BTreeMap::new(),
            _positional_charges: positional_charges,
        });
        let piped = match input {
            StageInput::Piped(reader) => {
                self.stdin.push(reader);
                true
            }
            StageInput::Inherited => false,
        };
        if capture_output {
            self.captures.push(Vec::new());
        }

        let previous_discard = self.discard_capture_depth;
        if *stdout_sink == Sink::Discard {
            self.discard_capture_depth = Some(self.captures.len());
        }
        let flow = self.execute_program(&body);
        self.discard_capture_depth = previous_discard;
        let mut captured = if capture_output {
            self.captures.pop().unwrap_or_default()
        } else {
            Vec::new()
        };
        if piped {
            self.stdin.pop();
        }
        self.frames.pop();
        self.budget.leave_call();

        let mut retained = capture_charges(&mut captured);
        let redirected = *stdout_sink != Sink::Value;
        let value = if redirected {
            let mut output = String::new();
            for result in &captured {
                let text = display(&result.value);
                let newline = usize::from(!result.suppress_newline);
                retained.push(
                    self.budget
                        .charge_value_bytes((text.len() + newline) as u64)?,
                );
                output.push_str(&text);
                if newline != 0 {
                    output.push('\n');
                }
            }
            Value::String(output)
        } else if capture_output {
            reduce_captured(captured)
        } else {
            Value::Null
        };
        Ok(match flow? {
            Flow::Return(status) => Executed::Result(CommandResult {
                value,
                status,
                suppress_newline: redirected,
                retained,
            }),
            Flow::Exit(status) => Executed::Flow(Flow::Exit(status)),
            Flow::Normal | Flow::Break(_) | Flow::Continue(_) => Executed::Result(CommandResult {
                value,
                status: self.last_status,
                suppress_newline: redirected,
                retained,
            }),
        })
    }

    fn run_xargs(
        &mut self,
        arguments: &[String],
        input: StageInput,
    ) -> Result<Executed, FatalError> {
        let plan = match self
            .builtin_input(true, input)
            .and_then(|input| xargs::plan(arguments, input.as_ref().map(|input| &input.value)))
        {
            Ok(plan) => plan,
            Err(failure) => {
                let status = self.absorb(failure)?;
                return Ok(Executed::Result(CommandResult::status(status)));
            }
        };

        let mut outputs = Vec::new();
        let mut retained = Vec::new();
        let mut status = ExitCode::SUCCESS;
        for invocation in plan.invocations {
            self.budget.charge_step_with(self.invoker)?;
            match self.run_argv(
                &invocation,
                StageInput::Piped(PipeReader::default()),
                true,
                false,
                &Sink::Value,
            )? {
                Executed::Flow(flow) => return Ok(Executed::Flow(flow)),
                Executed::Result(result) => {
                    if result.status != ExitCode::SUCCESS {
                        status = result.status;
                    }
                    if !result.value.is_null() {
                        outputs.push(result.value);
                        retained.extend(result.retained);
                    }
                }
            }
        }

        let value = if outputs.is_empty() {
            Value::Null
        } else {
            Value::Array(outputs)
        };
        Ok(Executed::Result(CommandResult {
            value,
            status,
            suppress_newline: false,
            retained,
        }))
    }

    /// Assigning a variable from a whole command substitution or another whole variable keeps its
    /// structured value instead of flattening it to text, a deliberate deviation from real shells
    /// that lets a script later index into what it captured.
    fn assignment_value(&mut self, word: &Word) -> Result<CommandResult, CommandFailure> {
        self.last_substitution_status = ExitCode::SUCCESS;
        if let [WordPart::CommandSubstitution(program)] = word.parts.as_slice() {
            return self.run_substitution(program);
        }
        let start = self.expansion_charges.len();
        let value = match word.parts.as_slice() {
            [WordPart::Parameter(parameter)] => self.parameter_value(parameter)?,
            _ => Value::String(self.expand_word(word)?.join(" ")),
        };
        let mut result = CommandResult::value(value);
        result.retained = self.expansion_charges.split_off(start);
        Ok(result)
    }

    fn expansion_value(&mut self, word: &Word) -> Result<Value, CommandFailure> {
        let result = self.assignment_value(word)?;
        self.expansion_charges.extend(result.retained);
        Ok(result.value)
    }

    fn expand_word(&mut self, word: &Word) -> Result<Vec<String>, CommandFailure> {
        let mut fields = vec![String::new()];
        let mut produced = false;

        for part in &word.parts {
            match part {
                WordPart::Literal(text) | WordPart::SingleQuoted(text) => {
                    append(&mut fields, text);
                    produced = true;
                }
                WordPart::DoubleQuoted(parts) => {
                    let expanded = self.expand_quoted_fields(parts)?;
                    let mut expanded = expanded.into_iter();
                    if let Some(first) = expanded.next() {
                        append(&mut fields, &first);
                        produced = true;
                    }
                    for extra in expanded {
                        fields.push(extra);
                    }
                }
                WordPart::Arithmetic(expression) => {
                    let number = self.evaluate_arithmetic(expression)?;
                    append(&mut fields, &render_number(number));
                    produced = true;
                }
                WordPart::Parameter(parameter) => {
                    let value = self.parameter_value(parameter)?;
                    produced |= spread(&mut fields, &value);
                }
                WordPart::CommandSubstitution(program) => {
                    let result = self.run_substitution(program)?;
                    produced |= spread(&mut fields, &result.value);
                    self.expansion_charges.extend(result.retained);
                }
            }
        }

        if !produced && fields.len() == 1 && fields[0].is_empty() {
            return Ok(Vec::new());
        }
        Ok(fields)
    }

    fn expand_quoted(&mut self, parts: &[WordPart]) -> Result<String, CommandFailure> {
        Ok(self.expand_quoted_fields(parts)?.join(" "))
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
    fn expand_quoted_fields(&mut self, parts: &[WordPart]) -> Result<Vec<String>, CommandFailure> {
        let mut fields = vec![String::new()];
        for part in parts {
            match part {
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
                    let value = self.parameter_value(parameter)?;
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
                    let value = self.parameter_value(parameter)?;
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
        Ok(match parameter {
            Parameter::Named {
                name,
                indices,
                modifier,
                length,
            } => {
                let (value, bound) = self.select_parameter(name, indices)?;
                if self.options.nounset && !bound && *modifier == Modifier::None {
                    return Err(CommandFailure::Fatal(FatalError::Assertion(format!(
                        "{name}: unbound variable, and `set -u` is on"
                    ))));
                }
                let value = self.apply_modifier(name, indices, modifier, value, bound)?;
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
            Parameter::AllPositionalJoined => Value::String(
                self.positional()
                    .iter()
                    .map(display)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            Parameter::PositionalCount => Value::from(self.positional().len()),
            Parameter::LastStatus => Value::from(self.last_status.get()),
        })
    }

    fn select_parameter(
        &mut self,
        name: &str,
        indices: &[Index],
    ) -> Result<(Value, bool), CommandFailure> {
        let mut bound = self.lookup(name).is_some();
        let mut value = self.lookup(name).cloned().unwrap_or(Value::Null);
        for index in indices {
            match index {
                Index::At(word) => {
                    let expanded = self.expand_word(word)?;
                    let key = expanded.join(" ");
                    value = value::index(&value, &key);
                    bound = !value.is_null();
                }
                Index::All => {}
                Index::AllJoined => {
                    value = Value::String(value::to_lines(&value).join(" "));
                }
            }
        }
        Ok((value, bound))
    }

    fn apply_modifier(
        &mut self,
        name: &str,
        indices: &[Index],
        modifier: &Modifier,
        value: Value,
        bound: bool,
    ) -> Result<Value, CommandFailure> {
        let absent = |colon: bool| {
            if colon {
                value.is_null() || display(&value).is_empty()
            } else {
                !bound
            }
        };
        Ok(match modifier {
            Modifier::None => value,
            Modifier::Default { colon, word } => {
                if absent(*colon) {
                    self.expansion_value(word)?
                } else {
                    value
                }
            }
            Modifier::Assign { colon, word } => {
                if !absent(*colon) {
                    return Ok(value);
                }
                if !indices.is_empty() {
                    return Err(CommandFailure::usage(format!(
                        "${{{name}[...]:=word}} cannot assign through an index; assign to {name} itself"
                    )));
                }
                let substitute = self.expansion_value(word)?;
                self.assign(name, CommandResult::value(substitute.clone()))?;
                substitute
            }
            Modifier::Require { colon, word } => {
                if !absent(*colon) {
                    return Ok(value);
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
                if absent(*colon) {
                    Value::Null
                } else {
                    self.expansion_value(word)?
                }
            }
            Modifier::StripPrefix(pattern) => {
                let pattern = self.literal_pattern(pattern)?;
                let text = display(&value);
                Value::String(
                    text.strip_prefix(&pattern)
                        .map_or(text.clone(), str::to_owned),
                )
            }
            Modifier::StripSuffix(pattern) => {
                let pattern = self.literal_pattern(pattern)?;
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
                let pattern = self.literal_pattern(pattern)?;
                let replacement = self.expand_quoted(&replacement.parts)?;
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
        })
    }

    fn literal_pattern(&mut self, pattern: &Pattern) -> Result<String, CommandFailure> {
        match pattern {
            Pattern::Literal(word) => self.expand_quoted(&word.parts),
            Pattern::Expanded(word) => {
                let text = self.expand_quoted(&word.parts)?;
                if let Some((character, meaning)) = pattern_metacharacter(&text) {
                    return Err(CommandFailure::usage(expanded_parameter_pattern(
                        character, meaning,
                    )));
                }
                Ok(text)
            }
        }
    }

    fn evaluate_conditional(
        &mut self,
        expression: &Conditional,
    ) -> Result<ExitCode, CommandFailure> {
        self.budget.charge_step_with(self.invoker)?;
        match expression {
            Conditional::Test(test) => self.evaluate_conditional_test(test),
            Conditional::Not(inner) => Ok(invert(self.evaluate_conditional(inner)?)),
            Conditional::And(left, right) => {
                let status = self.evaluate_conditional(left)?;
                if status == ExitCode::SUCCESS {
                    return self.evaluate_conditional(right);
                }
                Ok(status)
            }
            Conditional::Or(left, right) => {
                let status = self.evaluate_conditional(left)?;
                if status == ExitCode::SUCCESS {
                    return Ok(status);
                }
                self.evaluate_conditional(right)
            }
        }
    }

    fn evaluate_conditional_test(
        &mut self,
        test: &ConditionalTest,
    ) -> Result<ExitCode, CommandFailure> {
        let mut operands = Vec::with_capacity(test.words.len());
        for word in &test.words {
            operands.push(self.expand_quoted(&word.parts)?);
        }
        if test.check_right_pattern
            && let [_, _, right] = operands.as_slice()
            && let Some((character, meaning)) = pattern_metacharacter(right)
        {
            return Err(CommandFailure::usage(expanded_conditional_pattern(
                character, meaning,
            )));
        }
        Ok(builtins::misc::evaluate_test("[[", &operands)?.status)
    }

    fn run_substitution(&mut self, program: &Program) -> Result<CommandResult, CommandFailure> {
        self.captures.push(Vec::new());
        let flow = self.execute_program(program);
        let mut captured = self.captures.pop().unwrap_or_default();
        let flow = flow.map_err(CommandFailure::Fatal)?;

        if let Flow::Exit(status) = flow {
            // This shell has no subshells, so calling exit inside a captured command substitution
            // ends the entire script rather than only the substitution, unlike a real shell.
            return Err(CommandFailure::Fatal(FatalError::Unsupported(format!(
                "exit {status} inside $( ) ends the whole script; this shell has no subshells"
            ))));
        }

        let retained = capture_charges(&mut captured);
        let value = reduce_captured(captured);
        let mut result = CommandResult {
            value,
            status: self.last_status,
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
        self.budget.charge_step_with(self.invoker)?;
        Ok(match expression {
            ArithExpr::Integer(value) => Number::Integer(*value),
            ArithExpr::Float(value) => Number::Float(*value),
            ArithExpr::Variable(name) => {
                let value = match name.parse::<usize>() {
                    Ok(position) => self.parameter_value(&Parameter::Positional(position))?,
                    Err(_) => self.lookup(name).cloned().unwrap_or(Value::Null),
                };
                to_number(&value)
            }
            ArithExpr::Unary(operator, operand) => {
                let operand = self.evaluate_arithmetic(operand)?;
                match operator {
                    ArithUnaryOp::Negate => match operand {
                        Number::Integer(value) => Number::Integer(value.wrapping_neg()),
                        Number::Float(value) => Number::Float(-value),
                    },
                    ArithUnaryOp::Not => Number::Integer(i64::from(!operand.is_truthy())),
                }
            }
            ArithExpr::Binary(operator, left, right) => {
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

fn split_read_fields(line: &str, count: usize) -> Vec<&str> {
    let mut fields = Vec::with_capacity(count);
    let mut rest = line.trim_start();
    while fields.len() + 1 < count && !rest.is_empty() {
        match rest.find(char::is_whitespace) {
            Some(offset) => {
                fields.push(&rest[..offset]);
                rest = rest[offset..].trim_start();
            }
            None => break,
        }
    }
    fields.push(rest);
    fields
}

fn is_variable_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
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

fn merge_diagnostics(value: Value, diagnostics: Vec<String>) -> Value {
    if diagnostics.is_empty() {
        return value;
    }
    let mut lines = value::to_lines(&value);
    lines.extend(diagnostics);
    value::from_lines(lines)
}

fn write_display(stdout: &mut PipeWriter, value: &Value) -> WriteOutcome {
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

fn retain_value(
    budget: &Budget,
    value: &Value,
    charges: &mut Vec<crate::RetainedBytes>,
) -> Result<(), LimitExceeded> {
    let charged: u64 = charges.iter().map(crate::RetainedBytes::bytes).sum();
    let missing = value_bytes(value).saturating_sub(charged);
    if missing > 0 {
        charges.push(budget.charge_value_bytes(missing)?);
    }
    Ok(())
}

fn capture_charges(captured: &mut [CommandResult]) -> Vec<crate::RetainedBytes> {
    captured
        .iter_mut()
        .flat_map(|result| std::mem::take(&mut result.retained))
        .collect()
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

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
fn spread(fields: &mut Vec<String>, value: &Value) -> bool {
    match value {
        Value::Array(items) => {
            if items.is_empty() {
                return false;
            }
            let mut items = items.iter();
            if let Some(first) = items.next() {
                append(fields, &display(first));
            }
            for item in items {
                fields.push(display(item));
            }
            true
        }
        Value::Null => false,
        scalar => {
            let text = display(scalar);
            let produced = !text.is_empty();
            append(fields, &text);
            produced
        }
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
