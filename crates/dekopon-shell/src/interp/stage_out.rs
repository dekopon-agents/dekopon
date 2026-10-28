use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum OutputMode {
    Value,
    Copy,
    Named,
    Lines,
    Base64,
}

enum Destination {
    Discard,
    Diagnostics,
    Buffer(String),
    Pipe,
    Capture,
    Terminal,
}

pub(super) struct StageOut {
    destination: Destination,
    mode: OutputMode,
    deferred: bool,
    collect: bool,
    bytes: Vec<u8>,
    charges: Vec<dekopon_process::RetainedBytes>,
    collected: Vec<CommandResult>,
    pending: Vec<u8>,
    truncated: bool,
}

impl StageOut {
    pub(super) fn new(
        evaluator: &Evaluator<'_>,
        sink: &Sink,
        mode: OutputMode,
        capture_output: bool,
    ) -> Self {
        let active = evaluator.active_buffer_name();
        let discarded = evaluator.output_discarded();
        let diagnostics = evaluator.diagnostics_redirected();
        let captured = !evaluator.captures.is_empty();
        let destination = match sink {
            Sink::Discard => Destination::Discard,
            Sink::Diagnostics => Destination::Diagnostics,
            Sink::Buffer { name, .. } => Destination::Buffer(name.clone()),
            Sink::Value => {
                let buffer_first = match mode {
                    OutputMode::Copy | OutputMode::Named => true,
                    OutputMode::Value | OutputMode::Lines | OutputMode::Base64 => false,
                };
                if discarded && !(buffer_first && active.is_some()) {
                    Destination::Discard
                } else if let Some(name) = active {
                    Destination::Buffer(name.to_owned())
                } else if diagnostics {
                    Destination::Diagnostics
                } else if evaluator.stdout.is_some() && (!captured || mode == OutputMode::Named) {
                    Destination::Pipe
                } else if captured || capture_output {
                    Destination::Capture
                } else {
                    Destination::Terminal
                }
            }
        };
        let explicit = match sink {
            Sink::Diagnostics | Sink::Buffer { .. } => true,
            Sink::Discard | Sink::Value => false,
        };
        let deferred = match mode {
            OutputMode::Lines => explicit,
            OutputMode::Base64 => {
                explicit
                    || match destination {
                        Destination::Capture => true,
                        Destination::Discard
                        | Destination::Diagnostics
                        | Destination::Buffer(_)
                        | Destination::Pipe
                        | Destination::Terminal => false,
                    }
            }
            OutputMode::Value | OutputMode::Copy | OutputMode::Named => false,
        };
        Self {
            destination,
            mode,
            deferred,
            collect: capture_output && !captured,
            bytes: Vec::new(),
            charges: Vec::new(),
            collected: Vec::new(),
            pending: Vec::new(),
            truncated: false,
        }
    }

    pub(super) fn value(
        &mut self,
        evaluator: &mut Evaluator<'_>,
        mut result: CommandResult,
    ) -> Result<(), LimitExceeded> {
        if result.value.is_null() {
            return Ok(());
        }
        match &self.destination {
            Destination::Discard => {}
            Destination::Diagnostics => evaluator.write_redirected_diagnostics(&result),
            Destination::Buffer(name) => evaluator.append_buffer(name, result)?,
            Destination::Capture => {
                retain_value(&evaluator.budget, &result.value, &mut result.retained)?;
                if let Some(capture) = evaluator.captures.last_mut() {
                    capture.push(result);
                }
            }
            Destination::Pipe => {
                if let Some(stdout) = evaluator.stdout.as_mut() {
                    let written = match write_display(stdout, &result.value) {
                        WriteOutcome::Accepted if !result.suppress_newline => stdout.write(b"\n"),
                        written @ (WriteOutcome::Accepted | WriteOutcome::ReaderGone) => written,
                    };
                    evaluator.reader_gone |= written == WriteOutcome::ReaderGone;
                }
            }
            Destination::Terminal => {
                let text = display(&result.value);
                telemetry::stage_write(
                    text.len() + usize::from(!result.suppress_newline),
                    true,
                    &None,
                );
                if result.suppress_newline {
                    evaluator.output.push_fragment(&text);
                } else {
                    evaluator.output.push_block(&text);
                }
            }
        }
        Ok(())
    }

    pub(super) fn write(
        &mut self,
        evaluator: &mut Evaluator<'_>,
        bytes: &[u8],
    ) -> Result<bool, CommandFailure> {
        match &self.destination {
            Destination::Discard => return Ok(true),
            Destination::Buffer(name) if !self.deferred => {
                evaluator.append_buffer_bytes(name, bytes)?;
                return Ok(true);
            }
            Destination::Diagnostics if !self.deferred => {
                if self.mode == OutputMode::Copy {
                    let room = evaluator
                        .limits
                        .max_output_bytes
                        .saturating_sub(self.pending.len());
                    self.truncated |= bytes.len() > room;
                    self.pending
                        .extend_from_slice(&bytes[..bytes.len().min(room)]);
                } else {
                    push_diagnostic_bytes(
                        bytes,
                        &mut evaluator.stderr_capture,
                        &mut evaluator.output,
                    );
                }
                return Ok(true);
            }
            Destination::Pipe => {
                if let Some(writer) = evaluator.stdout.as_mut() {
                    if writer.write(bytes) == WriteOutcome::ReaderGone {
                        evaluator.reader_gone = true;
                        return Ok(false);
                    }
                }
                return Ok(true);
            }
            Destination::Buffer(_)
            | Destination::Diagnostics
            | Destination::Capture
            | Destination::Terminal => {}
        }
        if self.deferred {
            self.charges
                .push(evaluator.budget.charge_value_bytes(bytes.len() as u64)?);
            self.bytes.extend_from_slice(bytes);
            return Ok(true);
        }
        let captured = match self.destination {
            Destination::Capture => true,
            Destination::Terminal => false,
            Destination::Discard
            | Destination::Diagnostics
            | Destination::Buffer(_)
            | Destination::Pipe => return Ok(true),
        };
        match self.mode {
            OutputMode::Lines => telemetry::stage_terminal_write(bytes.len(), !captured),
            OutputMode::Copy | OutputMode::Base64 => {
                telemetry::stage_write(bytes.len(), true, &None)
            }
            OutputMode::Value | OutputMode::Named => {}
        }
        self.pending.extend_from_slice(bytes);
        let complete = if self.mode == OutputMode::Lines && captured {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => text.len(),
                Err(error) if error.error_len().is_none() => error.valid_up_to(),
                Err(_invalid_utf8) => return Err(invalid_utf8()),
            }
        } else {
            lossy_complete_prefix(&self.pending)
        };
        if captured && complete > 0 {
            let text = std::str::from_utf8(&self.pending[..complete])
                .map_err(|_invalid_utf8| invalid_utf8())?;
            let charge = evaluator.budget.charge_value_bytes(complete as u64)?;
            let mut result = CommandResult::value(Value::String(text.to_owned())).without_newline();
            result.retained.push(charge);
            if self.collect {
                self.collected.push(result);
            } else if let Some(capture) = evaluator.captures.last_mut() {
                capture.push(result);
            }
        } else if !captured {
            evaluator
                .output
                .push_fragment(&String::from_utf8_lossy(&self.pending[..complete]));
        }
        self.pending.drain(..complete);
        Ok(true)
    }

    pub(super) fn named(
        &mut self,
        evaluator: &mut Evaluator<'_>,
        name: &str,
    ) -> Result<bool, CommandFailure> {
        check_buffer_operand("cat", name)?;
        if name == DEV_NULL {
            return Ok(true);
        }
        let bytes = evaluator.buffers.get(name).ok_or_else(|| {
            CommandFailure::failed(format!(
                "cat: {name}: no such buffer; buffers exist only after `> {name}` in this script"
            ))
        })?;
        match &self.destination {
            Destination::Discard => {}
            Destination::Buffer(destination) => evaluator.append_named_buffer(name, destination)?,
            Destination::Diagnostics => {
                push_diagnostic_bytes(bytes, &mut evaluator.stderr_capture, &mut evaluator.output)
            }
            Destination::Pipe => {
                if let Some(writer) = evaluator.stdout.as_mut() {
                    if writer.write(bytes) == WriteOutcome::ReaderGone {
                        evaluator.reader_gone = true;
                        return Ok(false);
                    }
                }
            }
            Destination::Capture => {
                let text = std::str::from_utf8(bytes).map_err(|_invalid_utf8| invalid_utf8())?;
                let charge = evaluator.budget.charge_value_bytes(bytes.len() as u64)?;
                let mut result =
                    CommandResult::value(Value::String(text.to_owned())).without_newline();
                result.retained.push(charge);
                if let Some(capture) = evaluator.captures.last_mut() {
                    capture.push(result);
                }
            }
            Destination::Terminal => match std::str::from_utf8(bytes) {
                Ok(text) => evaluator.output.push_fragment(text),
                Err(_invalid_utf8) => push_lossy_bytes(&mut evaluator.output, bytes),
            },
        }
        Ok(true)
    }

    pub(super) fn finish(
        mut self,
        evaluator: &mut Evaluator<'_>,
        status: ExitCode,
    ) -> Result<CommandResult, CommandFailure> {
        if self.mode == OutputMode::Copy {
            match self.destination {
                Destination::Diagnostics => {
                    if let Some(capture) = evaluator.stderr_capture.last_mut() {
                        capture.push_bytes(&self.pending);
                        capture.truncated |= self.truncated;
                    } else {
                        push_lossy_bytes(&mut evaluator.output, &self.pending);
                        if self.truncated {
                            evaluator
                                .output
                                .push_block("... redirected diagnostics truncated ...");
                        }
                    }
                    return Ok(CommandResult::status(status));
                }
                Destination::Buffer(_) => return Ok(CommandResult::status(status)),
                Destination::Discard
                | Destination::Pipe
                | Destination::Capture
                | Destination::Terminal => {}
            }
        }
        if !self.pending.is_empty()
            && (self.mode != OutputMode::Lines || status == ExitCode::SUCCESS)
        {
            match self.destination {
                Destination::Capture => return Err(invalid_utf8()),
                Destination::Terminal => evaluator
                    .output
                    .push_fragment(&String::from_utf8_lossy(&self.pending)),
                Destination::Discard
                | Destination::Diagnostics
                | Destination::Buffer(_)
                | Destination::Pipe => {}
            }
        }
        if self.mode == OutputMode::Copy {
            let mut retained = capture_charges(&mut self.collected);
            let value = if self.collected.is_empty() {
                Value::Null
            } else {
                reduce_charged(&evaluator.budget, self.collected, &mut retained)?
            };
            retain_value(&evaluator.budget, &value, &mut retained)?;
            return Ok(CommandResult {
                value,
                status,
                suppress_newline: true,
                retained,
            });
        }
        if self.bytes.is_empty() || (self.mode == OutputMode::Base64 && status != ExitCode::SUCCESS)
        {
            return Ok(CommandResult::status(status));
        }
        match &self.destination {
            Destination::Buffer(name) => {
                evaluator.append_charged_buffer_bytes(name, &self.bytes, self.charges)?;
                Ok(CommandResult::status(status))
            }
            Destination::Diagnostics | Destination::Capture => {
                let text = String::from_utf8(self.bytes).map_err(|_invalid_utf8| {
                    if self.mode == OutputMode::Lines {
                        CommandFailure::Fatal(FatalError::Unsupported(
                            "standard input is not valid UTF-8 text".to_owned(),
                        ))
                    } else {
                        invalid_utf8()
                    }
                })?;
                Ok(CommandResult {
                    value: Value::String(text),
                    status,
                    suppress_newline: true,
                    retained: self.charges,
                })
            }
            Destination::Discard | Destination::Pipe | Destination::Terminal => {
                Ok(CommandResult::status(status))
            }
        }
    }
}

fn invalid_utf8() -> CommandFailure {
    CommandFailure::failed("standard input is not valid UTF-8 text")
}
