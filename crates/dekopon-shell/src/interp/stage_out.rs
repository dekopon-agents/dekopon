use super::*;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum OutputMode {
    Value,
    Copy,
    Named,
    Lines,
    Bytes,
}

enum Diagnostics {
    Chunks,
    Excerpt,
    Deferred,
}
enum Buffering {
    Immediate,
    Deferred,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Capture {
    Chunks,
    Bytes,
}

enum Destination {
    Discard,
    Diagnostics(Diagnostics),
    Buffer(String, Buffering),
    Pipe,
    Capture(Capture),
    Terminal,
}

pub(super) struct StageOut {
    destination: Destination,
    mode: OutputMode,
    pending: Vec<u8>,
    charges: Vec<dekopon_process::RetainedBytes>,
    truncated: bool,
}

impl StageOut {
    pub(super) fn new(evaluator: &Evaluator<'_>, sink: &Sink, mode: OutputMode) -> Self {
        let (whole, excerpt, named, value) = match mode {
            OutputMode::Value => (false, false, false, true),
            OutputMode::Copy => (false, true, false, false),
            OutputMode::Named => (false, false, true, false),
            OutputMode::Lines => (false, false, false, false),
            OutputMode::Bytes => (true, false, false, false),
        };
        let diagnostics = |deferred| {
            Destination::Diagnostics(if deferred {
                Diagnostics::Deferred
            } else if excerpt {
                Diagnostics::Excerpt
            } else {
                Diagnostics::Chunks
            })
        };
        let buffer = |name: &str, deferred| {
            Destination::Buffer(
                name.to_owned(),
                if deferred {
                    Buffering::Deferred
                } else {
                    Buffering::Immediate
                },
            )
        };
        let capture = || {
            Destination::Capture(if whole {
                Capture::Bytes
            } else {
                Capture::Chunks
            })
        };
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
                    capture()
                } else if evaluator.stdout.is_some() {
                    Destination::Pipe
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
            truncated: false,
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
            Destination::Diagnostics(Diagnostics::Deferred)
            | Destination::Buffer(_, Buffering::Deferred)
            | Destination::Capture(Capture::Bytes) => self.collect(evaluator, bytes)?,
            Destination::Diagnostics(Diagnostics::Excerpt) => {
                let room = evaluator
                    .limits
                    .max_output_bytes
                    .saturating_sub(self.pending.len());
                self.truncated |= bytes.len() > room;
                self.pending
                    .extend_from_slice(&bytes[..bytes.len().min(room)]);
            }
            Destination::Diagnostics(Diagnostics::Chunks) => {
                push_diagnostic_bytes(bytes, &mut evaluator.stderr_capture, &mut evaluator.output);
            }
            Destination::Buffer(name, Buffering::Immediate) => {
                evaluator.append_buffer_bytes(name, bytes)?
            }
            Destination::Pipe => {
                if let Some(writer) = evaluator.stdout.as_mut()
                    && writer.write(bytes) == WriteOutcome::ReaderGone
                {
                    evaluator.reader_gone = true;
                    return Ok(false);
                }
            }
            Destination::Capture(Capture::Chunks) if self.mode == OutputMode::Named => {
                let text = std::str::from_utf8(bytes).map_err(|_invalid_utf8| invalid_text())?;
                capture_text(evaluator, text)?;
            }
            Destination::Capture(Capture::Chunks) => {
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
                    capture_text(
                        evaluator,
                        &String::from_utf8_lossy(&self.pending[..complete]),
                    )?;
                    self.pending.drain(..complete);
                }
            }
            Destination::Terminal if self.mode == OutputMode::Named => {
                push_lossy_bytes(&mut evaluator.output, bytes)
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
            Destination::Buffer(destination, _) => {
                evaluator.append_named_buffer(name, destination)?
            }
            Destination::Discard => {}
            Destination::Diagnostics(_)
            | Destination::Pipe
            | Destination::Capture(_)
            | Destination::Terminal => {
                let buffers = Arc::clone(&evaluator.buffers);
                if let Some(bytes) = buffers.get(name) {
                    return self.write(evaluator, bytes);
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
            Destination::Diagnostics(Diagnostics::Excerpt) => {
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
            Destination::Buffer(name, Buffering::Deferred) => {
                if !self.pending.is_empty() && (self.mode != OutputMode::Bytes || successful) {
                    evaluator.append_charged_buffer_bytes(
                        name,
                        &self.pending,
                        std::mem::take(&mut self.charges),
                    )?;
                }
            }
            Destination::Diagnostics(Diagnostics::Deferred)
            | Destination::Capture(Capture::Bytes) => {
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
            Destination::Capture(Capture::Chunks) => {
                if !self.pending.is_empty() && (self.mode == OutputMode::Copy || successful) {
                    return Err(invalid_text());
                }
            }
            Destination::Discard
            | Destination::Diagnostics(Diagnostics::Chunks)
            | Destination::Buffer(_, Buffering::Immediate)
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
            Destination::Diagnostics(_) => evaluator.write_redirected_diagnostics(&result),
            Destination::Buffer(name, _) => evaluator.append_buffer(name, result)?,
            Destination::Capture(_) => {
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

fn capture_text(evaluator: &mut Evaluator<'_>, text: &str) -> Result<(), CommandFailure> {
    let charge = evaluator.budget.charge_value_bytes(text.len() as u64)?;
    let mut result = CommandResult::value(Value::String(text.to_owned())).without_newline();
    result.retained.push(charge);
    if let Some(capture) = evaluator.captures.last_mut() {
        capture.push(result);
    }
    Ok(())
}

fn invalid_text() -> CommandFailure {
    CommandFailure::failed("standard input is not valid UTF-8 text")
}
