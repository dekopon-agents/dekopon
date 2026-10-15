use crate::{
    CapabilityInvoker, ExitCode, RetainedBytes,
    builtins::{
        CommandFailure,
        text::{
            grep::{GrepConfig, GrepMode},
            sed::Substitution,
        },
    },
    limits::Budget,
    pipe::{PipeReader, ReadOutcome},
};

#[derive(Clone, Copy)]
pub(crate) enum TextStream {
    Grep,
    Sed,
}

pub(crate) enum Operation {
    Grep(GrepConfig),
    Sed(Substitution),
}

#[derive(Default)]
struct LineProgress {
    index: usize,
    matches: usize,
    last_emitted: usize,
    after_until: usize,
    before: std::collections::VecDeque<(usize, Vec<u8>, Vec<RetainedBytes>)>,
}

impl TextStream {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Grep => "grep",
            Self::Sed => "sed",
        }
    }

    pub(crate) fn help(self) -> &'static str {
        match self {
            Self::Grep => "-v -i -c -n -E -q -A N -B N -C N",
            Self::Sed => "-e -E",
        }
    }

    pub(crate) fn parse(self, arguments: &[String]) -> Result<Operation, CommandFailure> {
        match self {
            Self::Grep => Ok(Operation::Grep(GrepConfig::parse(arguments)?)),
            Self::Sed => Ok(Operation::Sed(super::sed::parse_arguments(arguments)?)),
        }
    }

    #[cfg(test)]
    pub(crate) fn run(
        self,
        arguments: &[String],
        reader: &mut PipeReader,
        budget: &mut Budget,
        invoker: &dyn CapabilityInvoker,
        emit: impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
    ) -> Result<ExitCode, CommandFailure> {
        self.run_parsed(self.parse(arguments)?, reader, budget, invoker, emit)
    }

    pub(crate) fn run_parsed(
        self,
        operation: Operation,
        reader: &mut PipeReader,
        budget: &mut Budget,
        invoker: &dyn CapabilityInvoker,
        mut emit: impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
    ) -> Result<ExitCode, CommandFailure> {
        let mut pending = Vec::new();
        let mut charges: Vec<RetainedBytes> = Vec::new();
        let mut progress = LineProgress::default();
        while let ReadOutcome::Bytes(chunk) = reader.read(budget, invoker)? {
            budget.charge_step_with(invoker)?;
            let mut rest = chunk.as_slice();
            while !rest.is_empty() {
                let end = rest.iter().position(|byte| *byte == b'\n').map(|at| at + 1);
                let part = &rest[..end.unwrap_or(rest.len())];
                charges.push(budget.charge_value_bytes(part.len() as u64)?);
                pending.extend_from_slice(part);
                rest = &rest[part.len()..];
                if end.is_some() {
                    progress.index += 1;
                    if !process_line(
                        &operation,
                        (&pending, &mut charges),
                        &mut progress,
                        budget,
                        invoker,
                        &mut emit,
                    )? {
                        return Ok(ExitCode::SUCCESS);
                    }
                    pending.clear();
                    charges.clear();
                }
            }
        }
        if !pending.is_empty() {
            progress.index += 1;
            if !process_line(
                &operation,
                (&pending, &mut charges),
                &mut progress,
                budget,
                invoker,
                &mut emit,
            )? {
                return Ok(ExitCode::SUCCESS);
            }
        }
        if let Operation::Grep(config) = &operation {
            if let GrepMode::Count = config.mode
                && !super::sed::emit_chunks(
                    format!("{}\n", progress.matches).as_bytes(),
                    budget,
                    invoker,
                    &mut emit,
                )?
            {
                return Ok(ExitCode::SUCCESS);
            }
            return Ok(if progress.matches == 0 {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            });
        }
        Ok(ExitCode::SUCCESS)
    }
}

fn process_line(
    operation: &Operation,
    line_bytes: (&[u8], &mut Vec<RetainedBytes>),
    progress: &mut LineProgress,
    budget: &mut Budget,
    invoker: &dyn CapabilityInvoker,
    emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
) -> Result<bool, CommandFailure> {
    let (bytes, charges) = line_bytes;
    let terminated = bytes.ends_with(b"\n");
    let line = std::str::from_utf8(bytes.strip_suffix(b"\n").unwrap_or(bytes)).map_err(
        |_invalid_utf8| CommandFailure::failed("standard input is not valid UTF-8 text"),
    )?;
    let emitted = match operation {
        Operation::Grep(config) => {
            let selected = config.selects(line, budget)?;
            if selected {
                progress.matches += 1;
                if let GrepMode::Quiet = config.mode {
                    return Ok(false);
                }
            }
            if let GrepMode::Count = config.mode {
                return Ok(true);
            }
            if matches!(config.mode, GrepMode::Lines) && (config.before > 0 || config.after > 0) {
                if selected {
                    for (index, saved, _charges) in std::mem::take(&mut progress.before) {
                        if !emit_grep(
                            config,
                            progress,
                            (index, &saved, false),
                            budget,
                            invoker,
                            emit,
                        )? {
                            return Ok(false);
                        }
                    }
                    progress.after_until = progress.index.saturating_add(config.after);
                }
                if selected || progress.index <= progress.after_until {
                    if !emit_grep(
                        config,
                        progress,
                        (progress.index, bytes, selected),
                        budget,
                        invoker,
                        emit,
                    )? {
                        return Ok(false);
                    }
                } else if config.before > 0 {
                    progress.before.push_back((
                        progress.index,
                        bytes.to_vec(),
                        std::mem::take(charges),
                    ));
                    if progress.before.len() > config.before {
                        progress.before.pop_front();
                    }
                }
                return Ok(true);
            }
            if !selected {
                return Ok(true);
            }
            if config.number()
                && !super::sed::emit_chunks(
                    format!("{}:", progress.index).as_bytes(),
                    budget,
                    invoker,
                    emit,
                )?
            {
                return Ok(false);
            }
            super::sed::emit_chunks(line.as_bytes(), budget, invoker, emit)?
        }
        Operation::Sed(substitution) => substitution.emit(line, budget, invoker, emit)?,
    };
    if !emitted {
        return Ok(false);
    }
    if terminated {
        super::sed::emit_chunks(b"\n", budget, invoker, emit)
    } else {
        Ok(true)
    }
}

fn emit_grep(
    config: &GrepConfig,
    progress: &mut LineProgress,
    line: (usize, &[u8], bool),
    budget: &mut Budget,
    invoker: &dyn CapabilityInvoker,
    emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
) -> Result<bool, CommandFailure> {
    let (index, bytes, selected) = line;
    if progress.last_emitted > 0
        && index > progress.last_emitted.saturating_add(1)
        && !super::sed::emit_chunks(b"--\n", budget, invoker, emit)?
    {
        return Ok(false);
    }
    progress.last_emitted = index;
    if config.number() {
        let marker = if selected { ':' } else { '-' };
        if !super::sed::emit_chunks(format!("{index}{marker}").as_bytes(), budget, invoker, emit)? {
            return Ok(false);
        }
    }
    super::sed::emit_chunks(bytes, budget, invoker, emit)
}

#[cfg(test)]
pub(crate) fn run_test(
    command: TextStream,
    arguments: &[&str],
    input: serde_json::Value,
) -> Result<crate::builtins::CommandResult, CommandFailure> {
    use crate::{
        builtins::{CommandResult, test_support::NoCapabilities},
        limits::Limits,
        value::to_lines,
    };
    let lines = to_lines(&input);
    let bytes = if lines.is_empty() {
        Vec::new()
    } else {
        format!("{}\n", lines.join("\n")).into_bytes()
    };
    let mut reader = PipeReader::from_bytes(bytes);
    let mut budget = Budget::start(Limits::default());
    let mut output = Vec::new();
    let operation = command.parse(
        &arguments
            .iter()
            .map(|arg| (*arg).to_owned())
            .collect::<Vec<_>>(),
    )?;
    let status = command.run_parsed(
        operation,
        &mut reader,
        &mut budget,
        &NoCapabilities,
        |bytes| {
            output.extend_from_slice(bytes);
            Ok(true)
        },
    )?;
    let output = String::from_utf8(output).expect("text stream emits UTF-8");
    let mut result = if matches!(command, TextStream::Grep) && arguments.contains(&"-c") {
        CommandResult::value(serde_json::Value::from(
            output.trim().parse::<usize>().expect("grep count"),
        ))
    } else {
        CommandResult::lines(output.lines().map(str::to_owned).collect())
    };
    result.status = status;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::{
        CapabilityCallResult, CommandProposal,
        limits::{LimitExceeded, Limits},
        pipe,
    };

    struct Stub {
        cancelled: bool,
    }

    impl CapabilityInvoker for Stub {
        fn granted(&self) -> Vec<String> {
            Vec::new()
        }
        fn invoke(
            &self,
            _: CommandProposal,
            _streams: crate::Streams,
            _tree: &crate::TreeContext,
        ) -> CapabilityCallResult {
            unreachable!()
        }
        fn cancelled(&self) -> bool {
            self.cancelled
        }
    }

    #[test]
    fn case_insensitive_sed_charges_unicode_fold_before_allocation() {
        let arguments = ["s/i/y/i".to_owned()];
        let input = "İ\n".as_bytes().to_vec();
        let mut reader = PipeReader::from_bytes(input.clone());
        let mut budget = Budget::start(Limits {
            max_value_bytes: 5,
            ..Limits::default()
        });
        let error = TextStream::Sed
            .run(
                &arguments,
                &mut reader,
                &mut budget,
                &Stub { cancelled: false },
                |_| Ok(true),
            )
            .expect_err("folded scratch exceeds retention after pending input");
        assert!(matches!(
            error,
            CommandFailure::Fatal(crate::builtins::FatalError::Limit(
                LimitExceeded::ValueBytes { maximum: 5 }
            ))
        ));

        let mut reader = PipeReader::from_bytes(input);
        let mut budget = Budget::start(Limits {
            max_value_bytes: 6,
            ..Limits::default()
        });
        let mut output = Vec::new();
        let status = TextStream::Sed
            .run(
                &arguments,
                &mut reader,
                &mut budget,
                &Stub { cancelled: false },
                |chunk| {
                    output.extend_from_slice(chunk);
                    Ok(true)
                },
            )
            .expect("pending input and folded scratch fit");
        assert_eq!(status, ExitCode::SUCCESS);
        assert_eq!(output, "İ\n".as_bytes());
    }

    #[test]
    fn a_text_builtin_notices_cancellation_between_fixed_input_chunks() {
        struct CancelAfterFirstChunk(AtomicUsize);
        impl CapabilityInvoker for CancelAfterFirstChunk {
            fn granted(&self) -> Vec<String> {
                Vec::new()
            }
            fn invoke(
                &self,
                _: CommandProposal,
                _streams: crate::Streams,
                _tree: &crate::TreeContext,
            ) -> CapabilityCallResult {
                unreachable!()
            }
            fn cancelled(&self) -> bool {
                self.0.fetch_add(1, Ordering::SeqCst) > 0
            }
        }
        for command in [TextStream::Grep, TextStream::Sed] {
            let args = match command {
                TextStream::Grep => vec!["x".to_owned()],
                TextStream::Sed => vec!["s/x/y/".to_owned()],
            };
            let mut reader = PipeReader::from_bytes(vec![b'x'; 2 * pipe::CHUNK_BYTES]);
            let invoker = CancelAfterFirstChunk(AtomicUsize::new(0));
            let mut budget = Budget::start(Limits::default());
            let error = command
                .run(&args, &mut reader, &mut budget, &invoker, |_| Ok(true))
                .expect_err("cancellation turns on after the first charged chunk");
            assert!(matches!(
                error,
                CommandFailure::Fatal(crate::builtins::FatalError::Limit(LimitExceeded::Cancelled))
            ));
            assert_eq!(invoker.0.load(Ordering::SeqCst), 2);
        }
    }

    #[test]
    fn context_across_chunks_emits_one_numbered_unterminated_tail() {
        let mut input = b"x\n".repeat(pipe::CHUNK_BYTES / 2 - 1);
        input.extend_from_slice(b"P\nx\nP\ntail");
        let mut reader = PipeReader::from_bytes(input);
        let mut budget = Budget::start(Limits::default());
        let mut output = Vec::new();
        let status = TextStream::Grep
            .run(
                &["-nB1".to_owned(), "-A1".to_owned(), "P".to_owned()],
                &mut reader,
                &mut budget,
                &Stub { cancelled: false },
                |bytes| {
                    output.extend_from_slice(bytes);
                    Ok(true)
                },
            )
            .expect("context across the chunk boundary");
        assert_eq!(status, ExitCode::SUCCESS);
        assert_eq!(
            output,
            format!(
                "{}-x\n{}:P\n{}-x\n{}:P\n{}-tail",
                pipe::CHUNK_BYTES / 2 - 1,
                pipe::CHUNK_BYTES / 2,
                pipe::CHUNK_BYTES / 2 + 1,
                pipe::CHUNK_BYTES / 2 + 2,
                pipe::CHUNK_BYTES / 2 + 3
            )
            .as_bytes()
        );
    }

    #[test]
    fn quiet_grep_does_not_retain_before_context() {
        let mut reader = PipeReader::from_bytes(b"abc\nabc\nmatch\n".to_vec());
        let mut budget = Budget::start(Limits {
            max_value_bytes: 7,
            ..Limits::default()
        });
        let status = TextStream::Grep
            .run(
                &["-q".to_owned(), "-B2".to_owned(), "match".to_owned()],
                &mut reader,
                &mut budget,
                &Stub { cancelled: false },
                |_| panic!("quiet grep must not emit context"),
            )
            .expect("quiet grep needs no retained before window");
        assert_eq!(status, ExitCode::SUCCESS);
    }

    #[test]
    fn grep_before_window_retains_charges_and_context_stays_step_bounded() {
        let mut reader = PipeReader::from_bytes(b"abc\nabc\nabc\nmatch\n".to_vec());
        let mut budget = Budget::start(Limits {
            max_value_bytes: 7,
            ..Limits::default()
        });
        let failure = TextStream::Grep
            .run(
                &["-B2".to_owned(), "match".to_owned()],
                &mut reader,
                &mut budget,
                &Stub { cancelled: false },
                |_| Ok(true),
            )
            .expect_err("two retained context lines exceed the budget");
        assert!(matches!(
            failure,
            CommandFailure::Fatal(crate::builtins::FatalError::Limit(
                LimitExceeded::ValueBytes { maximum: 7 }
            ))
        ));
    }

    #[test]
    fn repeated_replacements_emit_bounded_chunks_without_retaining_output() {
        let replacement = "y".repeat(8 * 1024);
        let script = format!("s/x/{replacement}/g");
        let mut reader = PipeReader::from_bytes(b"xxxxx".to_vec());
        let mut budget = Budget::start(Limits {
            max_value_bytes: 9 * 1024,
            ..Limits::default()
        });
        let mut total = 0;
        let status = TextStream::Sed
            .run(
                &[script],
                &mut reader,
                &mut budget,
                &Stub { cancelled: false },
                |bytes| {
                    assert!(bytes.len() <= pipe::CHUNK_BYTES);
                    total += bytes.len();
                    Ok(true)
                },
            )
            .expect("only the pending input is retained");
        assert_eq!(status, ExitCode::SUCCESS);
        assert_eq!(total, 40 * 1024);
    }

    #[test]
    fn a_matching_line_is_emitted_before_stdin_closes() {
        let (mut writer, mut reader) = pipe::pipe();
        let (sent, received) = std::sync::mpsc::sync_channel(1);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let mut budget = Budget::start(Limits::default());
                TextStream::Grep.run(
                    &["x".to_owned()],
                    &mut reader,
                    &mut budget,
                    &Stub { cancelled: false },
                    |bytes| {
                        sent.send(bytes.to_vec()).expect("the consumer is alive");
                        Ok(true)
                    },
                )
            });
            assert_eq!(writer.write(b"x\n"), pipe::WriteOutcome::Accepted);
            assert_eq!(
                received
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .expect("line arrives before close"),
                b"x"
            );
            assert_eq!(
                received
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .expect("newline arrives before close"),
                b"\n"
            );
            drop(writer);
            assert_eq!(
                worker.join().expect("stage joins").expect("grep completes"),
                ExitCode::SUCCESS
            );
        });
    }

    #[test]
    fn text_builtins_release_completed_lines_before_reading_more() {
        for command in [TextStream::Grep, TextStream::Sed] {
            let args = match command {
                TextStream::Grep => vec!["x".to_owned()],
                TextStream::Sed => vec!["s/x/y/".to_owned()],
            };
            let mut reader = PipeReader::from_bytes(b"x\nx\nx\nx\nx\n".to_vec());
            let mut budget = Budget::start(Limits {
                max_value_bytes: 3,
                ..Limits::default()
            });
            let mut output = Vec::new();
            let status = command
                .run(
                    &args,
                    &mut reader,
                    &mut budget,
                    &Stub { cancelled: false },
                    |bytes| {
                        output.extend_from_slice(bytes);
                        Ok(true)
                    },
                )
                .expect("completed lines refund their retained bytes");
            assert_eq!(status, ExitCode::SUCCESS);
            assert_eq!(output.len(), 10);
        }
    }

    #[test]
    fn text_builtins_charge_each_chunk_and_observe_cancel() {
        for command in [TextStream::Grep, TextStream::Sed] {
            let args = match command {
                TextStream::Grep => vec!["x".to_owned()],
                TextStream::Sed => vec!["s/x/y/".to_owned()],
            };
            let (mut writer, mut reader) = pipe::pipe();
            for _ in 0..5 {
                assert_eq!(writer.write(b"x"), pipe::WriteOutcome::Accepted);
            }
            drop(writer);
            let mut budget = Budget::start(Limits {
                max_steps: 2,
                ..Limits::default()
            });
            let error = command
                .run(
                    &args,
                    &mut reader,
                    &mut budget,
                    &Stub { cancelled: false },
                    |_| Ok(true),
                )
                .expect_err("the third chunk exceeds the budget");
            assert!(matches!(
                error,
                CommandFailure::Fatal(crate::builtins::FatalError::Limit(LimitExceeded::Steps {
                    maximum: 2
                }))
            ));

            let mut reader = PipeReader::from_bytes(b"x".to_vec());
            let mut budget = Budget::start(Limits::default());
            let error = command
                .run(
                    &args,
                    &mut reader,
                    &mut budget,
                    &Stub { cancelled: true },
                    |_| Ok(true),
                )
                .expect_err("cancel is checked for each chunk");
            assert!(matches!(
                error,
                CommandFailure::Fatal(crate::builtins::FatalError::Limit(LimitExceeded::Cancelled))
            ));
        }
    }
}
