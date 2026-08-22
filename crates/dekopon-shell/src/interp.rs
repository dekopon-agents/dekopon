//! The tree-walking evaluator.
//!
//! # Namespace isolation
//!
//! The variable namespace is seeded **only** from the script's own `NAME=value` assignments. This
//! module never calls [`std::env`]. A script that reads `$PATH` or `$OPENAI_API_KEY` sees an unset
//! variable, not the host process's value, and
//! [`crate::interp::tests::the_process_environment_never_leaks_into_a_script`] proves it.
//!
//! # Scoping
//!
//! Variables are dynamically scoped exactly as bash scopes them: global by default, and `local`
//! shadows a name for the current function frame *and* every frame it calls into.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use serde_json::Value;

use crate::{
    CapabilityInvoker, ExitCode, ScriptOutcome,
    ast::{
        AndOr, AndOrList, ArithBinaryOp, ArithExpr, ArithUnaryOp, CasePattern, CaseStatement,
        ForLoop, IfStatement, Parameter, Pipeline, Program, SimpleCommand, Statement, WhileLoop,
        Word, WordPart,
    },
    builtins::{BuiltinContext, BuiltinKind, CommandFailure, CommandResult, FatalError, xargs},
    dispatch::{self, Resolution, arguments_to_input},
    limits::{Budget, LimitExceeded, Limits, OutputBuffer},
    parser::{expanded_case_pattern, parse, pattern_metacharacter},
    value::{self, display},
};

use telemetry::CommandKind;

pub(crate) mod telemetry;

/// Control-flow signals that unwind through the evaluator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Flow {
    Normal,
    Break(u32),
    Continue(u32),
    Return(ExitCode),
    Exit(ExitCode),
}

/// What executing one command produced.
enum Executed {
    Result(CommandResult),
    Flow(Flow),
}

/// One shell-function activation.
#[derive(Debug, Default)]
struct Frame {
    locals: BTreeMap<String, Value>,
    positional: Vec<Value>,
    /// The value piped into the call, offered to the first command of each pipeline in the body.
    ///
    /// Shared rather than owned: every pipeline in the body is offered it, so an owned value would
    /// be deep-copied once per statement — including for the statements that never read input —
    /// and all but the last copy dropped. See [`own`].
    stdin: Option<Rc<Value>>,
}

/// Parses and evaluates one script, returning its outcome.
pub(crate) fn run(
    script: &str,
    invoker: &dyn CapabilityInvoker,
    limits: Limits,
    curl_capability: Option<&str>,
) -> ScriptOutcome {
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
        budget: Budget::start(limits),
        output: OutputBuffer::new(&limits),
        globals: BTreeMap::new(),
        frames: Vec::new(),
        functions: BTreeMap::new(),
        function_names: BTreeSet::new(),
        buffers: BTreeMap::new(),
        captures: Vec::new(),
        curl_capability: curl_capability.map(str::to_owned),
        allow_clock: limits.allow_clock,
        counters: telemetry::ScriptCounters::default(),
        last_status: ExitCode::SUCCESS,
        last_substitution_status: ExitCode::SUCCESS,
    };

    // One span for the whole run, so the totals have a home that costs the same whether a script
    // ran three commands or thirty thousand. Every `shell.command` span nests inside it.
    let script = telemetry::script_span();
    let exit_code = {
        let _entered = script.enter();
        match evaluator.execute_program(&program) {
            Ok(Flow::Exit(code)) => code,
            Ok(Flow::Return(code)) => code,
            Ok(_) => evaluator.last_status,
            Err(fatal) => evaluator.report_fatal(&fatal),
        }
    };
    evaluator.counters.record_on(&script);

    evaluator.output.finish();
    ScriptOutcome {
        output: evaluator.output.render(),
        exit_code,
        truncated: evaluator.output.is_truncated(),
        capability_calls: evaluator.budget.capability_calls(),
        steps: evaluator.budget.steps(),
    }
}

/// The evaluator's mutable state for one script execution.
///
/// This is deliberately *not* named `Interpreter`: [`crate::Interpreter`] is the public,
/// immutable configuration handle a caller builds, while this is the private machine that runs
/// one script under it. Sharing a name would send a reader following `Interpreter::run` to the
/// wrong type.
struct Evaluator<'a> {
    invoker: &'a dyn CapabilityInvoker,
    budget: Budget,
    output: OutputBuffer,
    globals: BTreeMap<String, Value>,
    frames: Vec<Frame>,
    functions: BTreeMap<String, Rc<Program>>,
    function_names: BTreeSet<String>,
    buffers: BTreeMap<String, Value>,
    captures: Vec<Vec<CommandResult>>,
    curl_capability: Option<String>,
    /// Whether `date` may read the host wall clock; see [`crate::Limits::allow_clock`].
    allow_clock: bool,
    /// Per-script command totals, and the cap on how many command spans reach INFO.
    counters: telemetry::ScriptCounters,
    last_status: ExitCode,
    last_substitution_status: ExitCode,
}

impl Evaluator<'_> {
    // -----------------------------------------------------------------------
    // Diagnostics and output
    // -----------------------------------------------------------------------

    /// Reports a fatal error and returns the exit code the script ends with.
    ///
    /// The code half comes from [`telemetry::fatal_exit_code`] rather than from this match, so the
    /// code a `shell.command` span records for an aborted command is by construction the same one
    /// the script itself reports.
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
                format!("dekopon-shell: script tried to make more than {maximum} capability calls")
            }
            FatalError::Limit(LimitExceeded::ValueBytes { maximum }) => format!(
                "dekopon-shell: script tried to hold more than {maximum} bytes of values in variables, buffers, and substitutions"
            ),
            FatalError::Unsupported(reason) => format!("dekopon-shell: {reason}"),
        };
        self.write_line(&message);
        telemetry::fatal_exit_code(fatal)
    }

    /// Writes one diagnostic to the combined output.
    ///
    /// Diagnostics escape a `$( )` capture on purpose. Only the *value* of a substitution is being
    /// captured; suppressing its errors too would leave `v=$(nosuchcmd)` with an empty variable, no
    /// explanation, and only a numeric `$?` the script may never inspect. Real shells send command
    /// substitution stderr to the terminal for exactly this reason.
    fn write_line(&mut self, line: &str) {
        self.output.push_block(line);
    }

    /// Writes what one pipeline produced, to the capture that is collecting it or to the output.
    ///
    /// A command that produced no value writes nothing, and that has to be decided *before* the
    /// capture branch rather than after it. `$(true; echo a)` is `a` in bash; retaining `true`'s
    /// null in the capture made it a second element, and [`reduce_captured`] joins elements with a
    /// newline — so the substitution silently gained a leading blank line, or a trailing one for
    /// `$(echo a; true)`, depending on where the command that produced nothing happened to sit.
    /// The status such a command reported is unaffected: it travels through `last_status`, not
    /// through the capture.
    fn emit(&mut self, result: CommandResult) {
        if result.value.is_null() {
            return;
        }
        if let Some(capture) = self.captures.last_mut() {
            capture.push(result);
            return;
        }
        let text = display(&result.value);
        if result.suppress_newline {
            self.output.push_fragment(&text);
        } else {
            self.output.push_block(&text);
        }
    }

    /// Converts a recoverable failure into a status, or propagates a fatal one.
    fn absorb(&mut self, failure: CommandFailure) -> Result<ExitCode, FatalError> {
        match failure {
            CommandFailure::Status { message, status } => {
                self.write_line(&message);
                Ok(status)
            }
            CommandFailure::Fatal(fatal) => Err(fatal),
        }
    }

    // -----------------------------------------------------------------------
    // Scope
    // -----------------------------------------------------------------------

    fn lookup(&self, name: &str) -> Option<&Value> {
        for frame in self.frames.iter().rev() {
            if let Some(value) = frame.locals.get(name) {
                return Some(value);
            }
        }
        self.globals.get(name)
    }

    /// Binds a name, charging what the value costs against the value-byte ceiling.
    ///
    /// Every retention point funnels through here, [`Evaluator::declare_local`], and
    /// [`Evaluator::write_buffer`]: `x="$x$x"` is one cheap step but doubles the bytes held, so
    /// counting operations alone leaves memory unbounded.
    fn assign(&mut self, name: &str, value: Value) -> Result<(), LimitExceeded> {
        self.budget.charge_value_bytes(value_bytes(&value))?;
        for frame in self.frames.iter_mut().rev() {
            if let Some(slot) = frame.locals.get_mut(name) {
                *slot = value;
                return Ok(());
            }
        }
        self.globals.insert(name.to_owned(), value);
        Ok(())
    }

    /// Restores a binding captured before a transient prefix assignment.
    fn restore(&mut self, name: &str, previous: Option<Value>) {
        match previous {
            Some(value) => {
                for frame in self.frames.iter_mut().rev() {
                    if let Some(slot) = frame.locals.get_mut(name) {
                        *slot = value;
                        return;
                    }
                }
                self.globals.insert(name.to_owned(), value);
            }
            // The prefix assignment created the binding, so removing it is what "restore" means.
            // `assign` writes to `globals` exactly when no frame already held the name.
            None => {
                self.globals.remove(name);
            }
        }
    }

    fn declare_local(&mut self, name: &str, value: Value) -> Result<(), LimitExceeded> {
        self.budget.charge_value_bytes(value_bytes(&value))?;
        if let Some(frame) = self.frames.last_mut() {
            frame.locals.insert(name.to_owned(), value);
            return Ok(());
        }
        self.globals.insert(name.to_owned(), value);
        Ok(())
    }

    fn positional(&self) -> &[Value] {
        self.frames
            .last()
            .map_or(&[][..], |frame| frame.positional.as_slice())
    }

    // -----------------------------------------------------------------------
    // Statements
    // -----------------------------------------------------------------------

    fn execute_program(&mut self, program: &Program) -> Result<Flow, FatalError> {
        for statement in &program.statements {
            match self.execute_statement(statement)? {
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
            Statement::Function(definition) => {
                self.function_names.insert(definition.name.clone());
                self.functions
                    .insert(definition.name.clone(), Rc::new(definition.body.clone()));
                self.last_status = ExitCode::SUCCESS;
                Ok(Flow::Normal)
            }
        }
    }

    fn execute_if(&mut self, statement: &IfStatement) -> Result<Flow, FatalError> {
        for (condition, body) in &statement.branches {
            let (status, flow) = self.execute_list(condition)?;
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

    /// Runs the first `case` clause whose pattern matches, and no other.
    ///
    /// Every clause tested charges a step, the way a loop iteration does: a `case` with many
    /// alternatives inside a loop is real work, and a statement-level charge alone would let it
    /// run outside the step budget that bounds everything else.
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
                self.budget.charge_step()?;
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
                    // A pattern assembled at run time cannot be checked any earlier, so it is
                    // checked here: `p='*.json'; case $f in $p)` must not quietly compare the
                    // subject against the four literal characters `*`, `.`, `j`...
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

        // No clause matched. bash reports success for that, and so does this: `case` asked a
        // question, and "none of the above" is an answer rather than a failure.
        self.last_status = ExitCode::SUCCESS;
        Ok(Flow::Normal)
    }

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

        // A loop reports the status of the last command its body ran, or success when the body
        // never ran. The condition that ended the loop must not leak into `$?`, exactly as in bash.
        let mut body_status = ExitCode::SUCCESS;
        for item in items {
            self.budget.charge_step()?;
            self.assign(&statement.variable, Value::String(item))?;
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

    fn execute_while(&mut self, statement: &WhileLoop) -> Result<Flow, FatalError> {
        let mut body_status = ExitCode::SUCCESS;
        loop {
            // Every iteration charges a step. This is the only thing standing between
            // `while true; do :; done` and an unbounded loop.
            self.budget.charge_step()?;
            let (status, flow) = self.execute_list(&statement.condition)?;
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
        let (mut status, flow) = self.execute_pipeline(&list.first)?;
        self.last_status = status;
        if flow.is_some() {
            return Ok((status, flow));
        }

        for (operator, pipeline) in &list.rest {
            let should_run = match operator {
                AndOr::And => status == ExitCode::SUCCESS,
                AndOr::Or => status != ExitCode::SUCCESS,
            };
            if !should_run {
                continue;
            }
            let (next, flow) = self.execute_pipeline(pipeline)?;
            status = next;
            self.last_status = status;
            if flow.is_some() {
                return Ok((status, flow));
            }
        }
        Ok((status, None))
    }

    fn execute_pipeline(
        &mut self,
        pipeline: &Pipeline,
    ) -> Result<(ExitCode, Option<Flow>), FatalError> {
        self.budget.charge_step()?;
        // A function body inherits the value piped into the call, offered to the first command of
        // each pipeline in the body. It is shared rather than consumed: consuming it would let a
        // condition that never reads input (`if [ -n "$1" ]; then cat; fi`) swallow the value
        // before `cat` could see it, which is the same class of silent data loss as dropping it.
        let mut input: Option<Rc<Value>> = self.frames.last().and_then(|frame| frame.stdin.clone());
        let mut last = CommandResult::status(ExitCode::SUCCESS);
        let commands = pipeline.commands.len();

        for (index, command) in pipeline.commands.iter().enumerate() {
            let piped = index + 1 < commands;
            match self.execute_command(command, input.take(), piped)? {
                Executed::Flow(flow) => return Ok((self.last_status, Some(flow))),
                Executed::Result(result) => {
                    if piped {
                        // Only the terminal command's result is ever read, and the terminal
                        // command is by construction never piped — so an intermediate value moves
                        // into the next stage instead of being copied into it and then dropped.
                        // Without this `cap big | jq . | grep x` deep-copies the payload per stage.
                        last = CommandResult::status(result.status);
                        input = Some(Rc::new(result.value));
                    } else {
                        last = result;
                    }
                }
            }
        }

        let status = if pipeline.negated {
            invert(last.status)
        } else {
            last.status
        };
        self.emit(last);
        Ok((status, None))
    }

    // -----------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------

    fn execute_command(
        &mut self,
        command: &SimpleCommand,
        input: Option<Rc<Value>>,
        capture_output: bool,
    ) -> Result<Executed, FatalError> {
        self.budget.charge_step()?;

        // Command words are expanded *before* any prefix assignment is applied, so `x=new echo $x`
        // prints the old value, and the binding is restored afterwards so it does not leak into the
        // rest of the script. Both halves are what `DEBUG=1 some.capability` means in bash.
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
                restore.push((
                    assignment.name.clone(),
                    self.lookup(&assignment.name).cloned(),
                ));
            }
            if let Err(limit) = self.assign(&assignment.name, value) {
                self.restore_all(restore);
                return Err(limit.into());
            }
        }

        if argv.is_empty() {
            // An assignment-only command reports the status of its last command substitution,
            // so `x=$(false); echo $?` behaves the way bash trains a model to expect.
            let status = if command.assignments.is_empty() {
                ExitCode::SUCCESS
            } else {
                assignment_status
            };
            return Ok(Executed::Result(CommandResult::status(status)));
        }

        // A here-document supplies this command's input in place of anything piped into it: `<<EOF`
        // redirects the same stdin a pipe would have filled, and the redirection is what bash
        // applies last. The body arrives as one JSON string, because a block of literal text is
        // exactly what a string is in this value model — no byte stream is involved anywhere here,
        // so a command that wants structure still pipes the body through `jq`.
        let input = match &command.here_doc {
            None => input,
            Some(body) => match self.expand_quoted(&body.parts) {
                Ok(text) => {
                    let value = Value::String(text);
                    if let Err(limit) = self.budget.charge_value_bytes(value_bytes(&value)) {
                        self.restore_all(restore);
                        return Err(limit.into());
                    }
                    Some(Rc::new(value))
                }
                Err(failure) => {
                    self.restore_all(restore);
                    let status = self.absorb(failure)?;
                    return Ok(Executed::Result(CommandResult::status(status)));
                }
            },
        };

        let executed = self.run_argv(&argv, input, capture_output);
        self.restore_all(restore);
        let executed = executed?;
        let Executed::Result(result) = executed else {
            return Ok(executed);
        };

        let Some(redirect) = &command.redirect else {
            return Ok(Executed::Result(result));
        };

        let target = match self.expand_word(&redirect.target) {
            Ok(expanded) => expanded,
            Err(failure) => {
                let status = self.absorb(failure)?;
                return Ok(Executed::Result(CommandResult::status(status)));
            }
        };
        let [name] = target.as_slice() else {
            self.write_line(
                "dekopon-shell: a redirection target must expand to exactly one buffer name",
            );
            return Ok(Executed::Result(CommandResult::status(ExitCode::SYNTAX)));
        };
        self.write_buffer(name, redirect.append, result.value)?;
        Ok(Executed::Result(CommandResult::status(result.status)))
    }

    /// Restores every binding a transient prefix assignment shadowed, in reverse order.
    fn restore_all(&mut self, restore: Vec<(String, Option<Value>)>) {
        for (name, previous) in restore.into_iter().rev() {
            self.restore(&name, previous);
        }
    }

    /// Stores a redirected value in the named in-memory buffer store.
    fn write_buffer(
        &mut self,
        name: &str,
        append: bool,
        value: Value,
    ) -> Result<(), LimitExceeded> {
        self.budget.charge_value_bytes(value_bytes(&value))?;
        if !append {
            self.buffers.insert(name.to_owned(), value);
            return Ok(());
        }
        match self.buffers.remove(name) {
            None => {
                self.buffers.insert(name.to_owned(), value);
            }
            Some(Value::Array(mut existing)) => {
                existing.push(value);
                self.buffers.insert(name.to_owned(), Value::Array(existing));
            }
            Some(existing) => {
                self.buffers
                    .insert(name.to_owned(), Value::Array(vec![existing, value]));
            }
        }
        Ok(())
    }

    /// Executes one command word, wrapped in the span every command in a script produces.
    ///
    /// This is the single place a command word actually runs, so it is the single place worth
    /// instrumenting: one span here covers builtins, capability calls, shell functions, refused
    /// words, and unknown words alike, and covers builtins added later without another edit. The
    /// recursion `xargs` drives back into this function is deliberately *not* special-cased — one
    /// script word that maps a command over ten items really did run ten commands, and each of
    /// them gets its own span nested inside the `xargs` one, which is exactly the syscall-by-
    /// syscall reading this instrumentation exists to give.
    ///
    /// See [`telemetry`] for what these spans may and may not carry, and for the per-script cap
    /// that decides which of them are emitted at INFO.
    fn run_argv(
        &mut self,
        argv: &[String],
        input: Option<Rc<Value>>,
        capture_output: bool,
    ) -> Result<Executed, FatalError> {
        let command = argv[0].as_str();
        let arguments = &argv[1..];

        // Classification happens before execution so that the span and its opening event both
        // carry the resolution kind, and so a command that aborts the script is still described by
        // more than "something failed".
        let (kind, resolution) = if telemetry::is_control_word(command) {
            (CommandKind::Control, None)
        } else {
            let resolution = dispatch::resolve(command, &self.function_names, self.invoker);
            (CommandKind::of(&resolution), Some(resolution))
        };
        let name = telemetry::traceable_name(kind, command);

        let level = self.counters.charge(kind);
        let span = telemetry::command_span(level, name, kind, arguments.len());
        // Recorded only for `not-granted`, where it comes from the session's own granted set rather
        // than from the script. See `telemetry::name_is_fixed_vocabulary`.
        if let Some(Resolution::NotGranted { namespace }) = resolution.as_ref() {
            span.record("capability.namespace", namespace.as_str());
        }
        let _entered = span.enter();

        let executed = self.dispatch_command(command, arguments, resolution, input, capture_output);
        let (status, outcome) = match &executed {
            Ok(Executed::Result(result)) => {
                (result.status, telemetry::outcome_label(result.status))
            }
            // `break`, `return 1`, and `exit 3` are commands that succeeded at doing what they
            // were asked; the status they carry belongs to the script, and is reported as theirs.
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
        self.counters.record_status(status);
        executed
    }

    /// Runs one already-classified command word.
    fn dispatch_command(
        &mut self,
        command: &str,
        arguments: &[String],
        resolution: Option<Resolution>,
        input: Option<Rc<Value>>,
        capture_output: bool,
    ) -> Result<Executed, FatalError> {
        let Some(resolution) = resolution else {
            if let Some(executed) = self.run_control_word(command, arguments)? {
                return Ok(executed);
            }
            // Unreachable while [`telemetry::CONTROL_WORDS`] and `run_control_word` agree, which
            // `control_words_and_their_dispatcher_agree` pins. Reporting "command not found" is
            // what a word added to only one of the two should do: fail closed and visibly, rather
            // than silently succeed with no effect.
            self.write_line(&format!("dekopon-shell: {command}: command not found"));
            return Ok(Executed::Result(CommandResult::status(ExitCode::NOT_FOUND)));
        };

        match resolution {
            Resolution::Rejected(reason) => Err(FatalError::Unsupported(reason.to_owned())),
            Resolution::Function => self.call_function(command, arguments, input, capture_output),
            Resolution::Builtin(BuiltinKind::Simple(builtin)) => {
                let outcome = {
                    let mut context = BuiltinContext {
                        invoker: self.invoker,
                        budget: &mut self.budget,
                        buffers: &mut self.buffers,
                        curl_capability: self.curl_capability.as_deref(),
                        allow_clock: self.allow_clock,
                    };
                    // The one place a piped value has to become owned. A pipeline stage's own
                    // output is held by nobody else and moves straight through; a function frame's
                    // stdin is shared with the frame, so only that case copies — and only for the
                    // commands that actually reach for input.
                    builtin.run(&mut context, arguments, own(input))
                };
                match outcome {
                    Ok(result) => Ok(Executed::Result(result)),
                    Err(failure) => {
                        let status = self.absorb(failure)?;
                        Ok(Executed::Result(CommandResult::status(status)))
                    }
                }
            }
            Resolution::Builtin(BuiltinKind::Xargs) => self.run_xargs(arguments, input),
            // The provider rewrites its own argv, then the result travels the identical path a
            // direct capability word takes: same budget, same denial, same telemetry. The rewrite
            // proposes; it does not grant.
            Resolution::ProviderCommand => {
                let (capability, input) = match self.invoker.resolve_command(command, arguments) {
                    Some(Ok(resolved)) => resolved,
                    Some(Err(message)) => {
                        let status = self.absorb(CommandFailure::usage(message))?;
                        return Ok(Executed::Result(CommandResult::status(status)));
                    }
                    // Resolution said a provider owned this word, so nothing owning it now means
                    // the registry changed underneath the session. Fail closed and visibly.
                    None => {
                        self.write_line(&format!("dekopon-shell: {command}: command not found"));
                        return Ok(Executed::Result(CommandResult::status(ExitCode::NOT_FOUND)));
                    }
                };
                let outcome = {
                    let mut context = BuiltinContext {
                        invoker: self.invoker,
                        budget: &mut self.budget,
                        buffers: &mut self.buffers,
                        curl_capability: self.curl_capability.as_deref(),
                        allow_clock: self.allow_clock,
                    };
                    context.invoke_capability(&capability, input)
                };
                match outcome {
                    Ok(result) => Ok(Executed::Result(result)),
                    Err(failure) => {
                        let status = self.absorb(failure)?;
                        Ok(Executed::Result(CommandResult::status(status)))
                    }
                }
            }
            Resolution::Capability => {
                let input = match arguments_to_input(command, arguments) {
                    Ok(input) => input,
                    Err(failure) => {
                        let status = self.absorb(failure)?;
                        return Ok(Executed::Result(CommandResult::status(status)));
                    }
                };
                let outcome = {
                    let mut context = BuiltinContext {
                        invoker: self.invoker,
                        budget: &mut self.budget,
                        buffers: &mut self.buffers,
                        curl_capability: self.curl_capability.as_deref(),
                        allow_clock: self.allow_clock,
                    };
                    context.invoke_capability(command, input)
                };
                match outcome {
                    Ok(result) => Ok(Executed::Result(result)),
                    Err(failure) => {
                        let status = self.absorb(failure)?;
                        Ok(Executed::Result(CommandResult::status(status)))
                    }
                }
            }
            // Byte for byte what `NotFound` reports, and that is the point: a model that could
            // tell "no such command" from "you were not granted that" would have an oracle for
            // enumerating the deployment's capabilities one guess at a time. The difference is
            // recorded in the span and nowhere the script can read.
            Resolution::NotFound | Resolution::NotGranted { .. } => {
                self.write_line(&format!("dekopon-shell: {command}: command not found"));
                Ok(Executed::Result(CommandResult::status(ExitCode::NOT_FOUND)))
            }
        }
    }

    /// Handles the control words the evaluator owns rather than the builtin table.
    fn run_control_word(
        &mut self,
        command: &str,
        arguments: &[String],
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
            // `shift` belongs beside `local`: this shell already models `$1`, `$@`, and `$#`, so
            // its absence broke `while [ $# -gt 0 ]; do ...; shift; done` while looking fine.
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
                    // Shifting past the end is a failed `shift` in bash, not a truncation.
                    Executed::Result(CommandResult::status(ExitCode::FAILURE))
                } else {
                    frame.positional.drain(..count);
                    Executed::Result(CommandResult::status(ExitCode::SUCCESS))
                }
            }
            "unset" => {
                for name in arguments {
                    self.globals.remove(name);
                    for frame in &mut self.frames {
                        frame.locals.remove(name);
                    }
                }
                Executed::Result(CommandResult::status(ExitCode::SUCCESS))
            }
            ":" => Executed::Result(CommandResult::status(ExitCode::SUCCESS)),
            _ => return Ok(None),
        };
        Ok(Some(executed))
    }

    /// Calls one shell function.
    ///
    /// `input` is the value piped into the call, and `capture_output` says whether the caller
    /// consumes what the function produced. Both matter: without them a function is silently
    /// broken in a pipeline, leaking its output past the pipe and handing the next command a null.
    /// Output is captured only when it is consumed, so a function in terminal position keeps
    /// streaming into the bounded output buffer rather than accumulating in memory.
    fn call_function(
        &mut self,
        name: &str,
        arguments: &[String],
        input: Option<Rc<Value>>,
        capture_output: bool,
    ) -> Result<Executed, FatalError> {
        let Some(body) = self.functions.get(name).cloned() else {
            self.write_line(&format!("dekopon-shell: {name}: command not found"));
            return Ok(Executed::Result(CommandResult::status(ExitCode::NOT_FOUND)));
        };

        self.budget.charge_step()?;
        self.budget.enter_call()?;
        let positional = arguments
            .iter()
            .map(|argument| Value::String(argument.clone()))
            .collect::<Vec<_>>();
        for argument in &positional {
            self.budget.charge_value_bytes(value_bytes(argument))?;
        }
        self.frames.push(Frame {
            locals: BTreeMap::new(),
            positional,
            stdin: input,
        });
        if capture_output {
            self.captures.push(Vec::new());
        }

        let flow = self.execute_program(&body);
        let captured = if capture_output {
            self.captures.pop().unwrap_or_default()
        } else {
            Vec::new()
        };
        self.frames.pop();
        self.budget.leave_call();

        let value = if capture_output {
            reduce_captured(captured)
        } else {
            Value::Null
        };
        Ok(match flow? {
            Flow::Return(status) => Executed::Result(CommandResult {
                value,
                status,
                suppress_newline: false,
            }),
            Flow::Exit(status) => Executed::Flow(Flow::Exit(status)),
            // `break`/`continue` that escape a function body do not unwind the caller's loop;
            // bash treats them as spent, and so does this evaluator.
            Flow::Normal | Flow::Break(_) | Flow::Continue(_) => Executed::Result(CommandResult {
                value,
                status: self.last_status,
                suppress_newline: false,
            }),
        })
    }

    fn run_xargs(
        &mut self,
        arguments: &[String],
        input: Option<Rc<Value>>,
    ) -> Result<Executed, FatalError> {
        let plan = match xargs::plan(arguments, input.as_deref()) {
            Ok(plan) => plan,
            Err(failure) => {
                let status = self.absorb(failure)?;
                return Ok(Executed::Result(CommandResult::status(status)));
            }
        };

        let mut outputs = Vec::new();
        let mut status = ExitCode::SUCCESS;
        for invocation in plan.invocations {
            self.budget.charge_step()?;
            match self.run_argv(&invocation, None, true)? {
                Executed::Flow(flow) => return Ok(Executed::Flow(flow)),
                Executed::Result(result) => {
                    if result.status != ExitCode::SUCCESS {
                        status = result.status;
                    }
                    if !result.value.is_null() {
                        outputs.push(result.value);
                    }
                }
            }
        }

        // Nothing produced is nothing emitted; an empty `[]` would be a phantom line of output.
        let value = if outputs.is_empty() {
            Value::Null
        } else {
            Value::Array(outputs)
        };
        Ok(Executed::Result(CommandResult {
            value,
            status,
            suppress_newline: false,
        }))
    }

    // -----------------------------------------------------------------------
    // Expansion
    // -----------------------------------------------------------------------

    /// Evaluates an assignment right-hand side.
    ///
    /// A whole-RHS `$(cmd)` keeps its structured value instead of collapsing to text. This is the
    /// documented deviation from bash that makes `ip=$(curl ...)` followed by `${ip[origin]}` work.
    fn assignment_value(&mut self, word: &Word) -> Result<Value, CommandFailure> {
        self.last_substitution_status = ExitCode::SUCCESS;
        if word.parts.is_empty() {
            return Ok(Value::String(String::new()));
        }
        if let Some(WordPart::CommandSubstitution(program)) = word.parts.first()
            && word.is_bare_command_substitution()
        {
            let (value, status) = self.run_substitution(program)?;
            self.last_substitution_status = status;
            return Ok(value);
        }
        let expanded = self.expand_word(word)?;
        Ok(Value::String(expanded.join(" ")))
    }

    /// Expands one word into zero or more argv words.
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
                    // `"$@"` is the one place double quotes produce more than one word. It is the
                    // most-trained idiom in shell (`for x in "$@"`), so it follows bash exactly
                    // rather than collapsing to a space-joined string; `"$*"` is the joined form.
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
                    let (value, _) = self.run_substitution(program)?;
                    produced |= spread(&mut fields, &value);
                }
            }
        }

        // An unquoted expansion that produced nothing yields no argv word at all, so
        // `echo $undefined` passes zero arguments rather than one empty one.
        if !produced && fields.len() == 1 && fields[0].is_empty() {
            return Ok(Vec::new());
        }
        Ok(fields)
    }

    /// Expands the interior of a double-quoted string into exactly one field.
    fn expand_quoted(&mut self, parts: &[WordPart]) -> Result<String, CommandFailure> {
        Ok(self.expand_quoted_fields(parts)?.join(" "))
    }

    /// Expands the interior of a double-quoted string, splitting only where `"$@"` demands it.
    ///
    /// Every part appends to the current field. `"$@"` is the sole exception: each parameter after
    /// the first opens a new field, so `"$@"` yields one word per parameter, `"a$@b"` glues the
    /// literals onto the outer two, and zero parameters yield zero words.
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
                WordPart::Parameter(Parameter::AllPositional) => {
                    let mut positional = self.positional().iter().map(display);
                    let Some(first) = positional.next() else {
                        // A bare `"$@"` with no parameters is no word at all, so `f "$@"` with
                        // nothing to forward calls `f` with zero arguments rather than one empty
                        // one. Glued to other text it contributes nothing instead.
                        if parts.len() == 1 {
                            return Ok(Vec::new());
                        }
                        continue;
                    };
                    append(&mut fields, &first);
                    fields.extend(positional);
                }
                WordPart::Parameter(parameter) => {
                    let value = self.parameter_value(parameter)?;
                    append(&mut fields, &quoted_text(&value));
                }
                WordPart::CommandSubstitution(program) => {
                    let (value, _) = self.run_substitution(program)?;
                    append(&mut fields, &quoted_text(&value));
                }
            }
        }
        Ok(fields)
    }

    fn parameter_value(&mut self, parameter: &Parameter) -> Result<Value, CommandFailure> {
        Ok(match parameter {
            Parameter::Named { name, indices } => {
                let mut value = self.lookup(name).cloned().unwrap_or(Value::Null);
                for index in indices {
                    let expanded = self.expand_word(index)?;
                    let key = expanded.join(" ");
                    value = value::index(&value, &key);
                }
                value
            }
            Parameter::Positional(0) => Value::String("dekopon-shell".to_owned()),
            Parameter::Positional(position) => self
                .positional()
                .get(position - 1)
                .cloned()
                .unwrap_or(Value::Null),
            Parameter::AllPositional => Value::Array(self.positional().to_vec()),
            // `$*` is the always-joined counterpart of `$@`: one word, whatever the quoting.
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

    /// Runs a command substitution, capturing what its pipelines produced.
    fn run_substitution(&mut self, program: &Program) -> Result<(Value, ExitCode), CommandFailure> {
        self.captures.push(Vec::new());
        let flow = self.execute_program(program);
        let captured = self.captures.pop().unwrap_or_default();
        let flow = flow.map_err(CommandFailure::Fatal)?;

        if let Flow::Exit(status) = flow {
            // There is no subshell to confine an `exit` to, so it ends the script.
            return Err(CommandFailure::Fatal(FatalError::Unsupported(format!(
                "exit {status} inside $( ) ends the whole script; this shell has no subshells"
            ))));
        }

        let status = self.last_status;
        let value = reduce_captured(captured);
        self.budget
            .charge_value_bytes(value_bytes(&value))
            .map_err(CommandFailure::from)?;
        // Every substitution records its status, not only a whole-RHS one: `x=a$(false)` must
        // still leave `$?` at 1, exactly as `url="https://$(get_host)/x"` must report a failed
        // lookup rather than reading as a success.
        self.last_substitution_status = status;
        Ok((value, status))
    }

    // -----------------------------------------------------------------------
    // Arithmetic
    // -----------------------------------------------------------------------

    /// Evaluates one arithmetic node.
    ///
    /// Every node charges a step. The parser caps how deep and how large an expression may be, so
    /// this recursion is bounded before it starts; the step charge is what keeps a large-but-legal
    /// expression inside the same budget the rest of the evaluator answers to.
    fn evaluate_arithmetic(&mut self, expression: &ArithExpr) -> Result<Number, CommandFailure> {
        self.budget.charge_step()?;
        Ok(match expression {
            ArithExpr::Integer(value) => Number::Integer(*value),
            ArithExpr::Float(value) => Number::Float(*value),
            ArithExpr::Variable(name) => {
                // `$(( $1 + 1 ))` reads a positional parameter; only `$N` can produce a
                // digit-only name here, because a bare digit lexes as a literal.
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
                // `&&` and `||` short-circuit, so `$(( x != 0 && 10 / x > 1 ))` is safe.
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

/// Materializes a piped value for a command that consumes it by value.
///
/// Piped values travel as [`Rc`] so that offering one costs a refcount rather than a deep copy of
/// whatever a capability returned. This is where that ends: a stage's own output is held by nobody
/// else and moves through untouched, while a function frame's stdin is shared with the frame and
/// every later pipeline in its body, so only that case copies — and only for a command that
/// actually reaches for input.
fn own(input: Option<Rc<Value>>) -> Option<Value> {
    input.map(Rc::unwrap_or_clone)
}

/// Inverts a pipeline status for a leading `!`, collapsing every failure to plain success.
fn invert(status: ExitCode) -> ExitCode {
    if status == ExitCode::SUCCESS {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Reduces what a captured block emitted into one value.
///
/// One result keeps its structure so `x=$(cap ... )` stays JSON; several are joined as text
/// because that is what a caller reading multiple lines expects.
///
/// The join honors [`CommandResult::suppress_newline`] the same way [`Evaluator::emit`] does. A
/// capture is still a stream of writes: `v=$(printf '%s' a; printf '%s' b)` is `ab` in bash, and
/// inserting the newline the script explicitly suppressed silently corrupted every value a model
/// assembled piecewise — a URL, a JSON fragment — with no diagnostic anywhere.
fn reduce_captured(captured: Vec<CommandResult>) -> Value {
    match captured.len() {
        0 => Value::String(String::new()),
        1 => captured
            .into_iter()
            .next()
            .map_or(Value::Null, |result| result.value),
        _ => {
            let mut text = String::new();
            // Seeded as if the (absent) result before the first one suppressed its terminator, so
            // nothing is prefixed to the capture.
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

/// Estimates the bytes one value occupies, for the value-byte ceiling.
///
/// This is an approximation of heap cost, not a measurement: it counts payload bytes plus a small
/// fixed charge per node so that a deeply nested structure of empty pieces still costs something.
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

/// A `break N` that escapes the outermost loop it can reach becomes an ordinary exit from it.
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

/// Appends text to the last field.
fn append(fields: &mut Vec<String>, text: &str) {
    if let Some(last) = fields.last_mut() {
        last.push_str(text);
    } else {
        fields.push(text.to_owned());
    }
}

/// Spreads one expanded value across argv fields.
///
/// POSIX IFS word splitting is dropped. In its place: an unquoted expansion holding a JSON array
/// expands element by element into separate argv words, and any scalar expands to exactly one word.
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

/// Renders a value inside double quotes, where arrays never split into separate words.
///
/// `"$@"` therefore joins with a space rather than producing one word per parameter. That is a
/// documented deviation from bash, chosen so that double quotes always mean "exactly one word".
fn quoted_text(value: &Value) -> String {
    match value {
        Value::Array(items) => items.iter().map(display).collect::<Vec<_>>().join(" "),
        other => display(other),
    }
}

/// An arithmetic value.
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

/// Coerces a value into an arithmetic operand, defaulting to zero like bash does.
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
mod tests;
