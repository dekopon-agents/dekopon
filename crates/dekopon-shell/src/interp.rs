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
        AndOr, AndOrList, ArithBinaryOp, ArithExpr, ArithUnaryOp, ForLoop, IfStatement, Parameter,
        Pipeline, Program, SimpleCommand, Statement, WhileLoop, Word, WordPart,
    },
    builtins::{BuiltinContext, BuiltinKind, CommandFailure, CommandResult, FatalError, xargs},
    dispatch::{self, Resolution, arguments_to_input},
    limits::{Budget, LimitExceeded, Limits, OutputBuffer},
    parser::parse,
    value::{self, display},
};

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

    let mut interpreter = Interpreter {
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
        last_status: ExitCode::SUCCESS,
        last_substitution_status: ExitCode::SUCCESS,
    };

    let exit_code = match interpreter.execute_program(&program) {
        Ok(Flow::Exit(code)) => code,
        Ok(Flow::Return(code)) => code,
        Ok(_) => interpreter.last_status,
        Err(fatal) => interpreter.report_fatal(&fatal),
    };

    interpreter.output.finish();
    ScriptOutcome {
        output: interpreter.output.render(),
        exit_code,
        truncated: interpreter.output.is_truncated(),
        capability_calls: interpreter.budget.capability_calls(),
        steps: interpreter.budget.steps(),
    }
}

struct Interpreter<'a> {
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
    last_status: ExitCode,
    last_substitution_status: ExitCode,
}

impl Interpreter<'_> {
    // -----------------------------------------------------------------------
    // Diagnostics and output
    // -----------------------------------------------------------------------

    fn report_fatal(&mut self, fatal: &FatalError) -> ExitCode {
        let (message, code) = match fatal {
            FatalError::Limit(LimitExceeded::Steps { maximum }) => (
                format!(
                    "dekopon-shell: step budget exhausted after {maximum} steps; the script is doing too much work or looping without progress"
                ),
                ExitCode::SYNTAX,
            ),
            FatalError::Limit(LimitExceeded::RecursionDepth { maximum }) => (
                format!("dekopon-shell: shell functions nested deeper than {maximum} frames"),
                ExitCode::SYNTAX,
            ),
            FatalError::Limit(LimitExceeded::Deadline { timeout_ms }) => (
                format!("dekopon-shell: script exceeded its {timeout_ms}ms deadline"),
                ExitCode::TIMEOUT,
            ),
            FatalError::Limit(LimitExceeded::CapabilityCalls { maximum }) => (
                format!("dekopon-shell: script tried to make more than {maximum} capability calls"),
                ExitCode::SYNTAX,
            ),
            FatalError::Unsupported(reason) => {
                (format!("dekopon-shell: {reason}"), ExitCode::SYNTAX)
            }
        };
        self.write_line(&message);
        code
    }

    fn write_line(&mut self, line: &str) {
        if self.captures.is_empty() {
            self.output.push_block(line);
        }
    }

    fn emit(&mut self, result: CommandResult) {
        if let Some(capture) = self.captures.last_mut() {
            capture.push(result);
            return;
        }
        if result.value.is_null() {
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

    fn assign(&mut self, name: &str, value: Value) {
        for frame in self.frames.iter_mut().rev() {
            if let Some(slot) = frame.locals.get_mut(name) {
                *slot = value;
                return;
            }
        }
        self.globals.insert(name.to_owned(), value);
    }

    fn declare_local(&mut self, name: &str, value: Value) {
        if let Some(frame) = self.frames.last_mut() {
            frame.locals.insert(name.to_owned(), value);
            return;
        }
        self.globals.insert(name.to_owned(), value);
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
            self.assign(&statement.variable, Value::String(item));
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
        let mut input: Option<Value> = None;
        let mut last = CommandResult::status(ExitCode::SUCCESS);
        let commands = pipeline.commands.len();

        for (index, command) in pipeline.commands.iter().enumerate() {
            match self.execute_command(command, input.take())? {
                Executed::Flow(flow) => return Ok((self.last_status, Some(flow))),
                Executed::Result(result) => {
                    if index + 1 < commands {
                        input = Some(result.value.clone());
                    }
                    last = result;
                }
            }
        }

        let status = last.status;
        self.emit(last);
        Ok((status, None))
    }

    // -----------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------

    fn execute_command(
        &mut self,
        command: &SimpleCommand,
        input: Option<Value>,
    ) -> Result<Executed, FatalError> {
        self.budget.charge_step()?;

        let mut assignment_status = ExitCode::SUCCESS;
        for assignment in &command.assignments {
            let value = match self.assignment_value(&assignment.value) {
                Ok(value) => value,
                Err(failure) => {
                    let status = self.absorb(failure)?;
                    return Ok(Executed::Result(CommandResult::status(status)));
                }
            };
            assignment_status = self.last_substitution_status;
            self.assign(&assignment.name, value);
        }

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

        let executed = self.run_argv(&argv, input)?;
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
        self.write_buffer(name, redirect.append, result.value);
        Ok(Executed::Result(CommandResult::status(result.status)))
    }

    /// Stores a redirected value in the named in-memory buffer store.
    fn write_buffer(&mut self, name: &str, append: bool, value: Value) {
        if !append {
            self.buffers.insert(name.to_owned(), value);
            return;
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
    }

    fn run_argv(&mut self, argv: &[String], input: Option<Value>) -> Result<Executed, FatalError> {
        let command = argv[0].as_str();
        let arguments = &argv[1..];

        if let Some(executed) = self.run_control_word(command, arguments)? {
            return Ok(executed);
        }

        match dispatch::resolve(command, &self.function_names, self.invoker) {
            Resolution::Rejected(reason) => Err(FatalError::Unsupported((*reason).to_owned())),
            Resolution::Function => self.call_function(command, arguments),
            Resolution::Builtin(BuiltinKind::Simple(builtin)) => {
                let outcome = {
                    let mut context = BuiltinContext {
                        invoker: self.invoker,
                        budget: &mut self.budget,
                        buffers: &mut self.buffers,
                        curl_capability: self.curl_capability.as_deref(),
                    };
                    builtin.run(&mut context, arguments, input)
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
            Resolution::NotFound => {
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
                            self.declare_local(name, value::scalar_from_token(text));
                        }
                        None => self.declare_local(argument, Value::String(String::new())),
                    }
                }
                Executed::Result(CommandResult::status(ExitCode::SUCCESS))
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

    fn call_function(&mut self, name: &str, arguments: &[String]) -> Result<Executed, FatalError> {
        let Some(body) = self.functions.get(name).cloned() else {
            self.write_line(&format!("dekopon-shell: {name}: command not found"));
            return Ok(Executed::Result(CommandResult::status(ExitCode::NOT_FOUND)));
        };

        self.budget.charge_step()?;
        self.budget.enter_call()?;
        self.frames.push(Frame {
            locals: BTreeMap::new(),
            positional: arguments
                .iter()
                .map(|argument| Value::String(argument.clone()))
                .collect(),
        });

        let flow = self.execute_program(&body);
        self.frames.pop();
        self.budget.leave_call();

        Ok(match flow? {
            Flow::Return(status) => Executed::Result(CommandResult::status(status)),
            Flow::Exit(status) => Executed::Flow(Flow::Exit(status)),
            // `break`/`continue` that escape a function body do not unwind the caller's loop;
            // bash treats them as spent, and so does this evaluator.
            Flow::Normal | Flow::Break(_) | Flow::Continue(_) => {
                Executed::Result(CommandResult::status(self.last_status))
            }
        })
    }

    fn run_xargs(
        &mut self,
        arguments: &[String],
        input: Option<Value>,
    ) -> Result<Executed, FatalError> {
        let plan = match xargs::plan(arguments, input.as_ref()) {
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
            match self.run_argv(&invocation, None)? {
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

        Ok(Executed::Result(CommandResult {
            value: Value::Array(outputs),
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
        if let Some(WordPart::CommandSubstitution(program)) = word.parts.first() {
            if word.is_bare_command_substitution() {
                let (value, status) = self.run_substitution(program)?;
                self.last_substitution_status = status;
                return Ok(value);
            }
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
                    let text = self.expand_quoted(parts)?;
                    append(&mut fields, &text);
                    produced = true;
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
        let mut text = String::new();
        for part in parts {
            match part {
                WordPart::Literal(literal) | WordPart::SingleQuoted(literal) => {
                    text.push_str(literal);
                }
                WordPart::DoubleQuoted(inner) => text.push_str(&self.expand_quoted(inner)?),
                WordPart::Arithmetic(expression) => {
                    let number = self.evaluate_arithmetic(expression)?;
                    text.push_str(&render_number(number));
                }
                WordPart::Parameter(parameter) => {
                    let value = self.parameter_value(parameter)?;
                    text.push_str(&quoted_text(&value));
                }
                WordPart::CommandSubstitution(program) => {
                    let (value, _) = self.run_substitution(program)?;
                    text.push_str(&quoted_text(&value));
                }
            }
        }
        Ok(text)
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
        let value = match captured.len() {
            0 => Value::String(String::new()),
            1 => captured
                .into_iter()
                .next()
                .map_or(Value::Null, |result| result.value),
            _ => Value::String(
                captured
                    .iter()
                    .map(|result| display(&result.value))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        };
        Ok((value, status))
    }

    // -----------------------------------------------------------------------
    // Arithmetic
    // -----------------------------------------------------------------------

    fn evaluate_arithmetic(&mut self, expression: &ArithExpr) -> Result<Number, CommandFailure> {
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
