use super::*;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum OutputMode {
    Value,
    Copy,
    Named,
    Lines,
    Bytes,
}

enum Destination {
    Discard,
    Diagnostics { deferred: bool, excerpt: bool },
    Buffer { name: String, deferred: bool },
    Pipe,
    Capture { returned: bool, whole: bool },
    Terminal,
}

pub(super) struct StageOut {
    destination: Destination,
    mode: OutputMode,
    pending: Vec<u8>,
    charges: Vec<dekopon_process::RetainedBytes>,
    collected: Vec<CommandResult>,
    truncated: bool,
}

impl StageOut {
    pub(super) fn new(
        evaluator: &Evaluator<'_>,
        sink: &Sink,
        mode: OutputMode,
        capture_output: bool,
    ) -> Self {
        let (whole, excerpt, named, value) = match mode {
            OutputMode::Value => (false, false, false, true),
            OutputMode::Copy => (false, true, false, false),
            OutputMode::Named => (false, false, true, false),
            OutputMode::Lines => (false, false, false, false),
            OutputMode::Bytes => (true, false, false, false),
        };
        let diagnostics = |deferred| Destination::Diagnostics { deferred, excerpt };
        let buffer = |name: &str, deferred| Destination::Buffer {
            name: name.to_owned(),
            deferred,
        };
        let capture = |returned| Destination::Capture { returned, whole };
        let destination = match sink {
            Sink::Discard => Destination::Discard,
            Sink::Diagnostics => diagnostics(!value && !named && !excerpt),
            Sink::Buffer { name, .. } => buffer(name, !value && !named && !excerpt),
            Sink::Value => {
                let active = evaluator.active_buffer_name();
                let discard = evaluator.output_discarded();
                if active.is_some() && (named || excerpt || !discard) {
                    buffer(active.unwrap_or_default(), false)
                } else if discard {
                    Destination::Discard
                } else if evaluator.diagnostics_redirected() {
                    diagnostics(false)
                } else if named && evaluator.stdout.is_some() {
                    Destination::Pipe
                } else if !evaluator.captures.is_empty() {
                    capture(whole)
                } else if evaluator.stdout.is_some() {
                    Destination::Pipe
                } else if (whole || excerpt) && capture_output {
                    capture(true)
                } else {
                    Destination::Terminal
                }
            }
        };
        Self {
            destination,
            mode,
            pending: Vec::new(),
            charges: Vec::new(),
            collected: Vec::new(),
            truncated: false,
        }
    }

    pub(super) fn deferred(&self) -> bool {
        match self.destination {
            Destination::Diagnostics { deferred, .. } => deferred,
            Destination::Discard
            | Destination::Buffer { .. }
            | Destination::Pipe
            | Destination::Capture { .. }
            | Destination::Terminal => false,
        }
    }

    fn collect(&mut self, evaluator: &Evaluator<'_>, bytes: &[u8]) -> Result<(), CommandFailure> {
        self.charges
            .push(evaluator.budget.charge_value_bytes(bytes.len() as u64)?);
        self.pending.extend_from_slice(bytes);
        Ok(())
    }

    pub(super) fn write(
        &mut self,
        evaluator: &mut Evaluator<'_>,
        bytes: &[u8],
    ) -> Result<bool, CommandFailure> {
        match &self.destination {
            Destination::Discard => {}
            Destination::Diagnostics { deferred: true, .. }
            | Destination::Buffer { deferred: true, .. }
            | Destination::Capture { whole: true, .. } => self.collect(evaluator, bytes)?,
            Destination::Diagnostics {
                deferred: false,
                excerpt: true,
            } => {
                let room = evaluator
                    .limits
                    .max_output_bytes
                    .saturating_sub(self.pending.len());
                self.truncated |= bytes.len() > room;
                self.pending
                    .extend_from_slice(&bytes[..bytes.len().min(room)]);
            }
            Destination::Diagnostics {
                deferred: false,
                excerpt: false,
            } => {
                push_diagnostic_bytes(bytes, &mut evaluator.stderr_capture, &mut evaluator.output);
            }
            Destination::Buffer {
                name,
                deferred: false,
            } => evaluator.append_buffer_bytes(name, bytes)?,
            Destination::Pipe => {
                if let Some(writer) = evaluator.stdout.as_mut()
                    && writer.write(bytes) == WriteOutcome::ReaderGone
                {
                    evaluator.reader_gone = true;
                    return Ok(false);
                }
            }
            Destination::Capture {
                returned,
                whole: false,
            } => {
                match self.mode {
                    OutputMode::Copy => telemetry::stage_write(bytes.len(), true, &None),
                    OutputMode::Lines => telemetry::stage_terminal_write(bytes.len(), false),
                    OutputMode::Value | OutputMode::Named | OutputMode::Bytes => {}
                }
                self.pending.extend_from_slice(bytes);
                let complete = match std::str::from_utf8(&self.pending) {
                    Ok(text) => text.len(),
                    Err(error) if error.error_len().is_none() => error.valid_up_to(),
                    Err(_invalid_utf8) => return Err(invalid_text()),
                };
                if complete > 0 {
                    let charge = evaluator.budget.charge_value_bytes(complete as u64)?;
                    let mut result = CommandResult::value(Value::String(
                        String::from_utf8_lossy(&self.pending[..complete]).into_owned(),
                    ))
                    .without_newline();
                    result.retained.push(charge);
                    if *returned {
                        self.collected.push(result);
                    } else if let Some(capture) = evaluator.captures.last_mut() {
                        capture.push(result);
                    }
                    self.pending.drain(..complete);
                }
            }
            Destination::Terminal => {
                telemetry::stage_write(bytes.len(), true, &None);
                self.pending.extend_from_slice(bytes);
                let complete = lossy_complete_prefix(&self.pending);
                evaluator
                    .output
                    .push_fragment(&String::from_utf8_lossy(&self.pending[..complete]));
                self.pending.drain(..complete);
            }
        }
        Ok(true)
    }

    pub(super) fn buffer(
        &mut self,
        evaluator: &mut Evaluator<'_>,
        name: &str,
    ) -> Result<bool, CommandFailure> {
        check_buffer_operand("cat", name)?;
        if name == DEV_NULL {
            return Ok(true);
        }
        if !evaluator.buffers.contains_key(name) {
            return Err(CommandFailure::failed(format!(
                "cat: {name}: no such buffer; buffers exist only after `> {name}` in this script"
            )));
        }
        match &self.destination {
            Destination::Buffer {
                name: destination, ..
            } => evaluator.append_named_buffer(name, destination)?,
            Destination::Discard => {}
            Destination::Diagnostics { .. }
            | Destination::Pipe
            | Destination::Capture { .. }
            | Destination::Terminal => {
                let buffers = Arc::clone(&evaluator.buffers);
                if let Some(bytes) = buffers.get(name) {
                    match self.destination {
                        Destination::Terminal => push_lossy_bytes(&mut evaluator.output, bytes),
                        Destination::Capture { .. } => {
                            let text = std::str::from_utf8(bytes)
                                .map_err(|_invalid_utf8| invalid_text())?;
                            let charge = evaluator.budget.charge_value_bytes(bytes.len() as u64)?;
                            let mut result = CommandResult::value(Value::String(text.to_owned()))
                                .without_newline();
                            result.retained.push(charge);
                            if let Some(capture) = evaluator.captures.last_mut() {
                                capture.push(result);
                            }
                        }
                        Destination::Discard
                        | Destination::Diagnostics { .. }
                        | Destination::Buffer { .. }
                        | Destination::Pipe => return self.write(evaluator, bytes),
                    }
                }
            }
        }
        Ok(true)
    }

    pub(super) fn finish(
        &mut self,
        evaluator: &mut Evaluator<'_>,
        status: ExitCode,
    ) -> Result<CommandResult, CommandFailure> {
        let successful = status == ExitCode::SUCCESS;
        match &self.destination {
            Destination::Terminal => {
                if self.mode != OutputMode::Lines || successful {
                    evaluator
                        .output
                        .push_fragment(&String::from_utf8_lossy(&self.pending));
                }
            }
            Destination::Diagnostics {
                excerpt: true,
                deferred: false,
            } => {
                push_diagnostic_bytes(
                    &self.pending,
                    &mut evaluator.stderr_capture,
                    &mut evaluator.output,
                );
                if self.truncated {
                    if let Some(capture) = evaluator.stderr_capture.last_mut() {
                        capture.truncated = true;
                    } else {
                        evaluator
                            .output
                            .push_block("... redirected diagnostics truncated ...");
                    }
                }
            }
            Destination::Buffer {
                name,
                deferred: true,
            } => {
                if !self.pending.is_empty() && (self.mode != OutputMode::Bytes || successful) {
                    evaluator.append_charged_buffer_bytes(
                        name,
                        &self.pending,
                        std::mem::take(&mut self.charges),
                    )?;
                }
            }
            Destination::Diagnostics { deferred: true, .. }
            | Destination::Capture { whole: true, .. } => {
                if !self.pending.is_empty() && (self.mode != OutputMode::Bytes || successful) {
                    let bytes = std::mem::take(&mut self.pending);
                    let text = String::from_utf8(bytes).map_err(|_invalid_utf8| {
                        if self.mode == OutputMode::Lines {
                            CommandFailure::Fatal(FatalError::Unsupported(
                                "standard input is not valid UTF-8 text".to_owned(),
                            ))
                        } else {
                            invalid_text()
                        }
                    })?;
                    return Ok(CommandResult {
                        value: Value::String(text),
                        status,
                        suppress_newline: true,
                        retained: std::mem::take(&mut self.charges),
                    });
                }
            }
            Destination::Capture {
                returned,
                whole: false,
            } => {
                if !self.pending.is_empty() && (self.mode != OutputMode::Lines || successful) {
                    return Err(invalid_text());
                }
                if *returned {
                    let mut retained = capture_charges(&mut self.collected);
                    let value = if self.collected.is_empty() {
                        Value::Null
                    } else {
                        reduce_charged(
                            &evaluator.budget,
                            std::mem::take(&mut self.collected),
                            &mut retained,
                        )?
                    };
                    retain_value(&evaluator.budget, &value, &mut retained)?;
                    return Ok(CommandResult {
                        value,
                        status,
                        suppress_newline: true,
                        retained,
                    });
                }
            }
            Destination::Discard
            | Destination::Diagnostics {
                deferred: false,
                excerpt: false,
            }
            | Destination::Buffer {
                deferred: false, ..
            }
            | Destination::Pipe => {}
        }
        Ok(CommandResult::status(status))
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
            Destination::Diagnostics { .. } => evaluator.write_redirected_diagnostics(&result),
            Destination::Buffer { name, .. } => evaluator.append_buffer(name, result)?,
            Destination::Capture { .. } => {
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
                    if written == WriteOutcome::ReaderGone {
                        evaluator.reader_gone = true;
                    }
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
}

fn invalid_text() -> CommandFailure {
    CommandFailure::failed("standard input is not valid UTF-8 text")
}
