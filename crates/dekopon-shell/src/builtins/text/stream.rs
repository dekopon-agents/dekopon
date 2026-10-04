use crate::{
    CapabilityInvoker, ExitCode, RetainedBytes,
    builtins::{
        CommandFailure,
        text::{grep::GrepConfig, sed::Substitution},
    },
    limits::Budget,
    pipe::{PipeReader, ReadOutcome},
};

#[derive(Clone, Copy)]
pub(crate) enum TextStream {
    Grep,
    Sed,
}

enum Operation {
    Grep(GrepConfig),
    Sed(Substitution),
}

#[derive(Default)]
struct LineProgress {
    index: usize,
    matches: usize,
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
            Self::Grep => "-v -i -c -n -E",
            Self::Sed => "-e -E",
        }
    }

    pub(crate) fn run(
        self,
        arguments: &[String],
        reader: &mut PipeReader,
        budget: &mut Budget,
        invoker: &dyn CapabilityInvoker,
        mut emit: impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
    ) -> Result<ExitCode, CommandFailure> {
        let operation = match self {
            Self::Grep => Operation::Grep(GrepConfig::parse(arguments)?),
            Self::Sed => Operation::Sed(super::sed::parse_arguments(arguments)?),
        };
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
                        &pending,
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
                &pending,
                &mut progress,
                budget,
                invoker,
                &mut emit,
            )? {
                return Ok(ExitCode::SUCCESS);
            }
        }
        if let Operation::Grep(config) = &operation {
            if config.count_only()
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
    bytes: &[u8],
    progress: &mut LineProgress,
    budget: &mut Budget,
    invoker: &dyn CapabilityInvoker,
    emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
) -> Result<bool, CommandFailure> {
    let terminated = bytes.ends_with(b"\n");
    let line = std::str::from_utf8(bytes.strip_suffix(b"\n").unwrap_or(bytes)).map_err(
        |_invalid_utf8| CommandFailure::failed("standard input is not valid UTF-8 text"),
    )?;
    let emitted = match operation {
        Operation::Grep(config) => {
            if !config.selects(line, budget)? {
                return Ok(true);
            }
            progress.matches += 1;
            if config.count_only() {
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
    let status = command.run(
        &arguments
            .iter()
            .map(|arg| (*arg).to_owned())
            .collect::<Vec<_>>(),
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
