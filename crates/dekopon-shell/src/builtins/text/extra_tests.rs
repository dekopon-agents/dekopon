use super::extra::ExtraStream;
use crate::{
    builtins::{CommandFailure, FatalError},
    limits::Budget,
};
use dekopon_process::pipe::{self, PipeReader, WriteOutcome};
use dekopon_process::{ExitCode, LimitExceeded, Limits};

fn arguments(command: ExtraStream) -> Vec<String> {
    match command {
        ExtraStream::Cut => vec!["-c".into(), "1".into()],
        ExtraStream::Uniq | ExtraStream::Wc | ExtraStream::Sort => Vec::new(),
    }
}

#[test]
fn four_text_commands_charge_each_small_pipe_chunk() {
    for command in [
        ExtraStream::Cut,
        ExtraStream::Uniq,
        ExtraStream::Wc,
        ExtraStream::Sort,
    ] {
        let (mut writer, mut reader) = pipe::pipe(
            None,
            None,
            crate::interp::telemetry::stage_read,
            crate::interp::telemetry::stage_write,
        );
        for _ in 0..5 {
            assert_eq!(writer.write(b"x\n"), WriteOutcome::Accepted);
        }
        drop(writer);
        let mut budget = Budget::start(Limits {
            max_steps: 2,
            ..Limits::default()
        });
        let failure = command
            .run(&arguments(command), &mut reader, &mut budget, |_| Ok(true))
            .expect_err("the third small chunk exhausts the step budget");
        assert!(matches!(
            failure,
            CommandFailure::Fatal(FatalError::Limit(LimitExceeded::Steps { maximum: 2 }))
        ));
    }
}

#[test]
fn four_text_commands_notice_cancellation_between_chunks() {
    for command in [
        ExtraStream::Cut,
        ExtraStream::Uniq,
        ExtraStream::Wc,
        ExtraStream::Sort,
    ] {
        let failure = crate::builtins::test_support::cancel_after_first_step(|reader, budget| {
            command.run(&arguments(command), reader, budget, |_| Ok(true))
        })
        .expect_err("a later chunk notices cancellation");
        assert!(matches!(
            failure,
            CommandFailure::Fatal(FatalError::Limit(LimitExceeded::Cancelled))
        ));
    }
}

#[test]
fn cut_selects_characters_from_a_near_limit_line_without_retaining_copies() {
    let text = "é".repeat(2048);
    let mut reader = PipeReader::from_bytes(
        text.as_bytes().to_vec(),
        None,
        crate::interp::telemetry::stage_read,
    );
    let mut budget = Budget::start(Limits {
        max_value_bytes: text.len() as u64,
        ..Limits::default()
    });
    let mut output = Vec::new();
    let result = ExtraStream::Cut.run(
        &["-c".into(), "2-3".into()],
        &mut reader,
        &mut budget,
        |part| {
            output.extend_from_slice(part);
            Ok(true)
        },
    );
    assert_eq!(
        result.expect("only the pending line needs retention"),
        ExitCode::SUCCESS
    );
    assert_eq!(output, "éé".as_bytes());
    assert!(budget.charge_value_bytes(text.len() as u64).is_ok());
}

#[test]
fn streaming_cut_selects_unsorted_overlapping_fields_and_characters() {
    for (arguments, input, expected) in [
        (vec!["-d", ":", "-f", "3,1-2,2"], "a:b:c:d\n", "a:b:c\n"),
        (vec!["-c", "3,1-2,2"], "éßγδ\n", "éßγ\n"),
    ] {
        let mut reader = PipeReader::from_bytes(
            input.as_bytes().to_vec(),
            None,
            crate::interp::telemetry::stage_read,
        );
        let mut budget = Budget::start(Limits::default());
        let mut output = Vec::new();
        ExtraStream::Cut
            .run(
                &arguments
                    .iter()
                    .map(|arg| (*arg).to_owned())
                    .collect::<Vec<_>>(),
                &mut reader,
                &mut budget,
                |part| {
                    output.extend_from_slice(part);
                    Ok(true)
                },
            )
            .expect("selected spans stream");
        assert_eq!(output, expected.as_bytes());
    }
}

fn sorted(arguments: &[&str], input: &str) -> Result<String, CommandFailure> {
    let mut reader = PipeReader::from_bytes(
        input.as_bytes().to_vec(),
        None,
        crate::interp::telemetry::stage_read,
    );
    let mut budget = Budget::start(Limits::default());
    let mut output = Vec::new();
    ExtraStream::Sort.run(
        &arguments
            .iter()
            .map(|arg| (*arg).to_owned())
            .collect::<Vec<_>>(),
        &mut reader,
        &mut budget,
        |part| {
            output.extend_from_slice(part);
            Ok(true)
        },
    )?;
    Ok(String::from_utf8(output).expect("sort emits text"))
}

#[test]
fn streaming_sort_orders_text_lexicographically() {
    assert_eq!(
        sorted(&[], "pear\napple\nfig\n").expect("sort"),
        "apple\nfig\npear\n"
    );
    assert_eq!(sorted(&[], "b\na").expect("sort"), "a\nb");
}

#[test]
fn streaming_sort_orders_numeric_keys_before_reversing() {
    assert_eq!(
        sorted(&["-n"], "10\n9\n100\n").expect("sort"),
        "9\n10\n100\n"
    );
    assert_eq!(sorted(&[], "10\n9\n100\n").expect("sort"), "10\n100\n9\n");
}

#[test]
fn streaming_sort_reverse_and_unique_compose() {
    assert_eq!(sorted(&["-r"], "a\nc\nb\n").expect("sort"), "c\nb\na\n");
    assert_eq!(sorted(&["-u"], "b\na\nb\n").expect("sort"), "a\nb\n");
    assert_eq!(
        sorted(&["-n", "-r", "-u"], "2\n1\n2\n").expect("sort"),
        "2\n1\n"
    );
}

#[test]
fn streaming_sort_orders_nan_keys_as_text_before_numbers() {
    assert_eq!(
        sorted(&["-n"], "5\nnan\n3\nNaN\n1\n").expect("sort"),
        "NaN\nnan\n1\n3\n5\n"
    );
}

#[test]
fn streaming_sort_refuses_unsupported_flags() {
    let failure = sorted(&["-k", "2"], "a\n").expect_err("key sorting is unsupported");
    assert!(matches!(failure, CommandFailure::Status { message, .. } if message.contains("-k")));
}

#[test]
fn sorted_lines_hold_their_charges_until_the_command_finishes() {
    let mut budget = Budget::start(Limits {
        max_value_bytes: 3,
        ..Limits::default()
    });
    let mut reader = PipeReader::from_bytes(
        b"a\nb\n".to_vec(),
        None,
        crate::interp::telemetry::stage_read,
    );
    let failure = ExtraStream::Sort
        .run(&[], &mut reader, &mut budget, |_| Ok(true))
        .expect_err("two retained two-byte lines do not fit in three bytes");
    assert!(matches!(
        failure,
        CommandFailure::Fatal(FatalError::Limit(LimitExceeded::ValueBytes { .. }))
    ));
    let mut reader = PipeReader::from_bytes(
        b"a\nb\n".to_vec(),
        None,
        crate::interp::telemetry::stage_read,
    );
    let mut budget = Budget::start(Limits {
        max_value_bytes: 4,
        ..Limits::default()
    });
    for _ in 0..2 {
        assert_eq!(
            ExtraStream::Sort
                .run(&[], &mut reader, &mut budget, |_| Ok(true))
                .expect("retained bytes are refunded"),
            ExitCode::SUCCESS
        );
        reader = PipeReader::from_bytes(
            b"a\nb\n".to_vec(),
            None,
            crate::interp::telemetry::stage_read,
        );
    }
}
