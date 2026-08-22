//! `echo`, `printf`, `test`/`[`, `true`, `false`, `sleep`, and `cat`.

use std::{thread, time::Duration};

use serde_json::Value;

use super::{Builtin, BuiltinContext, CommandFailure, CommandResult, unsupported_flag};
use crate::{ExitCode, ast::DEV_NULL};

/// `echo [-neE] ARGS...`.
pub(crate) struct Echo;

impl Builtin for Echo {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn run(
        &self,
        _context: &mut BuiltinContext<'_>,
        arguments: &[String],
        _input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let mut suppress_newline = false;
        let mut escapes = false;
        let mut words = arguments;
        // `-n`, `-e`, `-E`, and bundles of them are consumed as flags, exactly as busybox echo
        // does. Leaving `-e` unconsumed printed it as data, silently corrupting the value the
        // script produced — and `echo -e` is how a bash-fluent model writes a multi-line string.
        while let Some(first) = words.first() {
            if !is_echo_flags(first) {
                break;
            }
            for flag in first.chars().skip(1) {
                match flag {
                    'n' => suppress_newline = true,
                    'e' => escapes = true,
                    _ => escapes = false,
                }
            }
            words = &words[1..];
        }
        let joined = words.join(" ");
        let text = if escapes {
            interpret_escapes(&joined)
        } else {
            joined
        };
        let result = CommandResult::value(Value::String(text));
        Ok(if suppress_newline {
            result.without_newline()
        } else {
            result
        })
    }
}

/// Reports whether one argument is an `echo` flag bundle such as `-n`, `-e`, or `-ne`.
fn is_echo_flags(argument: &str) -> bool {
    argument.len() > 1
        && argument.starts_with('-')
        && argument
            .chars()
            .skip(1)
            .all(|flag| matches!(flag, 'n' | 'e' | 'E'))
}

/// Expands the escape sequences `echo -e` interprets.
///
/// The set matches [`format_text`]: an unrecognized escape stays literal rather than being dropped,
/// so nothing disappears from the text a script meant to emit.
fn interpret_escapes(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        match characters.next() {
            Some('n') => output.push('\n'),
            Some('t') => output.push('\t'),
            Some('r') => output.push('\r'),
            Some('\\') => output.push('\\'),
            Some(other) => {
                output.push('\\');
                output.push(other);
            }
            None => output.push('\\'),
        }
    }
    output
}

/// `printf FORMAT [ARGS...]`.
pub(crate) struct Printf;

impl Builtin for Printf {
    fn name(&self) -> &'static str {
        "printf"
    }

    fn run(
        &self,
        _context: &mut BuiltinContext<'_>,
        arguments: &[String],
        _input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let Some((format, values)) = arguments.split_first() else {
            return Err(CommandFailure::usage(
                "printf: a format argument is required",
            ));
        };
        let rendered = format_text(format, values)?;
        // printf never appends a newline of its own; `\n` in the format is the only source.
        Ok(CommandResult::value(Value::String(rendered)).without_newline())
    }
}

/// Renders a curated printf format: `%s`, `%d`, `%f`, `%%`, plus `\n`, `\t`, and `\\`.
#[allow(
    clippy::map_err_ignore,
    reason = "both discarded values are ParseFloatError over a `%d` or `%f` argument the message \
              quotes back in full; \"invalid float literal\" adds nothing to naming the operand \
              that was not a number"
)]
fn format_text(format: &str, values: &[String]) -> Result<String, CommandFailure> {
    let mut output = String::new();
    let mut next = values.iter();
    let mut characters = format.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            '\\' => match characters.next() {
                Some('n') => output.push('\n'),
                Some('t') => output.push('\t'),
                Some('r') => output.push('\r'),
                Some('\\') => output.push('\\'),
                Some(other) => {
                    output.push('\\');
                    output.push(other);
                }
                None => output.push('\\'),
            },
            '%' => match characters.next() {
                Some('%') => output.push('%'),
                Some('s') => output.push_str(next.next().map_or("", String::as_str)),
                Some('d') => {
                    let argument = next.next().map_or("0", String::as_str);
                    let number = argument.trim().parse::<f64>().map_err(|_| {
                        CommandFailure::usage(format!("printf: {argument:?} is not a number"))
                    })?;
                    output.push_str(&format!("{}", number.trunc() as i64));
                }
                Some('f') => {
                    let argument = next.next().map_or("0", String::as_str);
                    let number = argument.trim().parse::<f64>().map_err(|_| {
                        CommandFailure::usage(format!("printf: {argument:?} is not a number"))
                    })?;
                    output.push_str(&format!("{number:.6}"));
                }
                Some(other) => {
                    return Err(CommandFailure::usage(format!(
                        "printf: conversion %{other} is not supported; use %s, %d, %f, or %%"
                    )));
                }
                None => output.push('%'),
            },
            other => output.push(other),
        }
    }
    Ok(output)
}

/// `test EXPRESSION`.
pub(crate) struct Test;

impl Builtin for Test {
    fn name(&self) -> &'static str {
        "test"
    }

    fn run(
        &self,
        _context: &mut BuiltinContext<'_>,
        arguments: &[String],
        _input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        evaluate_test("test", arguments)
    }
}

/// `[ EXPRESSION ]`.
pub(crate) struct TestBracket;

impl Builtin for TestBracket {
    fn name(&self) -> &'static str {
        "["
    }

    fn run(
        &self,
        _context: &mut BuiltinContext<'_>,
        arguments: &[String],
        _input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let Some((last, rest)) = arguments.split_last() else {
            return Err(CommandFailure::usage("[: missing closing `]`"));
        };
        if last != "]" {
            return Err(CommandFailure::usage("[: missing closing `]`"));
        }
        evaluate_test("[", rest)
    }
}

/// Evaluates the curated `test` expression grammar.
///
/// File tests (`-f`, `-d`, `-e`, `-r`, `-w`, `-x`) are absent: there is no filesystem, so accepting
/// them would answer a question this shell cannot ask.
pub(crate) fn evaluate_test(command: &str, arguments: &[String]) -> Result<CommandResult, CommandFailure> {
    // Leading `!`s are counted in a loop rather than peeled off by recursion. argv length here is
    // attacker-controlled — an unquoted expansion of a JSON array spreads element by element — so
    // one frame per `!` let a three-line script build a 20,000-deep stack and abort the process.
    let mut negations = 0usize;
    let mut arguments = arguments;
    while arguments.first().is_some_and(|first| first == "!") {
        negations += 1;
        arguments = &arguments[1..];
    }
    let negated = negations % 2 == 1;

    let truth = match arguments {
        [] => false,
        [single] => !single.is_empty(),
        [flag, operand] => match flag.as_str() {
            "-z" => operand.is_empty(),
            "-n" => !operand.is_empty(),
            "-f" | "-d" | "-e" | "-r" | "-w" | "-x" | "-s" => {
                return Err(CommandFailure::usage(format!(
                    "{command}: file test {flag} is not supported: this shell has no filesystem"
                )));
            }
            other => {
                return Err(CommandFailure::usage(format!(
                    "{command}: unary operator {other:?} is not supported"
                )));
            }
        },
        [left, operator, right] => match operator.as_str() {
            "=" | "==" => left == right,
            "!=" => left != right,
            "<" => left < right,
            ">" => left > right,
            "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" => {
                let left = parse_number(command, left)?;
                let right = parse_number(command, right)?;
                match operator.as_str() {
                    "-eq" => (left - right).abs() < f64::EPSILON,
                    "-ne" => (left - right).abs() >= f64::EPSILON,
                    "-lt" => left < right,
                    "-le" => left <= right,
                    "-gt" => left > right,
                    _ => left >= right,
                }
            }
            other => {
                return Err(CommandFailure::usage(format!(
                    "{command}: binary operator {other:?} is not supported"
                )));
            }
        },
        _ => {
            return Err(CommandFailure::usage(format!(
                "{command}: expected at most three operands; combine conditions with `&&` or `||`"
            )));
        }
    };

    Ok(CommandResult::status(if truth != negated {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }))
}

#[allow(
    clippy::map_err_ignore,
    reason = "ParseFloatError has one meaning, \"not a float literal\", over an operand the \
              message quotes back in full"
)]
fn parse_number(command: &str, text: &str) -> Result<f64, CommandFailure> {
    text.trim()
        .parse::<f64>()
        .map_err(|_| CommandFailure::usage(format!("{command}: {text:?} is not a number")))
}

/// `true`.
pub(crate) struct True;

impl Builtin for True {
    fn name(&self) -> &'static str {
        "true"
    }

    fn run(
        &self,
        _context: &mut BuiltinContext<'_>,
        _arguments: &[String],
        _input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        Ok(CommandResult::status(ExitCode::SUCCESS))
    }
}

/// `false`.
pub(crate) struct False;

impl Builtin for False {
    fn name(&self) -> &'static str {
        "false"
    }

    fn run(
        &self,
        _context: &mut BuiltinContext<'_>,
        _arguments: &[String],
        _input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        Ok(CommandResult::status(ExitCode::FAILURE))
    }
}

/// `sleep SECONDS`.
pub(crate) struct Sleep;

impl Builtin for Sleep {
    fn name(&self) -> &'static str {
        "sleep"
    }

    fn run(
        &self,
        context: &mut BuiltinContext<'_>,
        arguments: &[String],
        _input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let [seconds] = arguments else {
            return Err(CommandFailure::usage(
                "sleep: exactly one duration in seconds is required",
            ));
        };
        #[allow(
            clippy::map_err_ignore,
            reason = "ParseFloatError has one meaning, \"not a float literal\", over an operand \
                      the message quotes back in full; the range and finiteness checks that \
                      follow are what carry the interesting rejections"
        )]
        let seconds = seconds.trim().parse::<f64>().map_err(|_| {
            CommandFailure::usage(format!("sleep: {seconds:?} is not a number of seconds"))
        })?;
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(CommandFailure::usage(
                "sleep: the duration must be a finite, non-negative number of seconds",
            ));
        }

        // Sleeping is capped by whatever remains of the script deadline, so `sleep 3600` cannot
        // park the interpreter past its own wall clock. Overshooting the deadline then trips it.
        //
        // The conversion is fallible on purpose: `Duration::from_secs_f64` *panics* above roughly
        // 1.8e19 seconds, so `sleep 1e30` would abort the whole process rather than being clamped
        // to a deadline it was always going to exceed.
        let requested = Duration::try_from_secs_f64(seconds).unwrap_or(Duration::MAX);
        let remaining = context.budget.remaining();
        thread::sleep(requested.min(remaining));
        context.budget.check_deadline()?;
        Ok(CommandResult::status(ExitCode::SUCCESS))
    }
}

/// `cat BUFFER...`.
pub(crate) struct Cat;

impl Builtin for Cat {
    fn name(&self) -> &'static str {
        "cat"
    }

    fn run(
        &self,
        context: &mut BuiltinContext<'_>,
        arguments: &[String],
        input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        for argument in arguments {
            if argument.starts_with('-') && argument.len() > 1 {
                return Err(unsupported_flag("cat", argument));
            }
        }

        // `cat` reads only the named in-memory buffer store written by `>` and `>>`. It resolves no
        // path, touches no filesystem, and reaches nothing outside this one script execution.
        if arguments.is_empty() {
            return Ok(CommandResult::value(input.unwrap_or(Value::Null)));
        }

        let mut values = Vec::new();
        for name in arguments {
            // `/dev/null` is the one name that never needs a prior write: it discards on the way in
            // and reads empty on the way out, which is what makes `cmd > /dev/null` and
            // `cat /dev/null` mean here what they mean everywhere else.
            if name == DEV_NULL {
                values.push(Value::Null);
                continue;
            }
            let Some(value) = context.buffers.get(name) else {
                return Err(CommandFailure::failed(format!(
                    "cat: {name}: no such buffer; buffers exist only after `> {name}` in this script"
                )));
            };
            values.push(value.clone());
        }

        Ok(CommandResult::value(match values.len() {
            1 => values.into_iter().next().unwrap_or(Value::Null),
            _ => Value::Array(values),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{Value, json};

    use crate::{
        ExitCode,
        builtins::{
            CommandFailure,
            test_support::{run_builtin, run_builtin_with},
        },
        limits::Limits,
    };

    use super::{Cat, Echo, False, Printf, Sleep, Test, TestBracket, True};

    #[test]
    fn echo_joins_arguments_with_spaces() {
        let result = run_builtin(&Echo, &["a", "b"], None).expect("echo runs");
        assert_eq!(result.value, json!("a b"));
        assert!(!result.suppress_newline);
        assert!(
            run_builtin(&Echo, &["-n", "a"], None)
                .expect("echo runs")
                .suppress_newline
        );
    }

    #[test]
    fn echo_consumes_its_flag_bundles_instead_of_printing_them() {
        // An unconsumed `-e` used to be printed as data, so the value the script produced silently
        // gained a flag it never meant to emit.
        assert_eq!(
            run_builtin(&Echo, &["-e", "a\\nb"], None)
                .expect("echo runs")
                .value,
            json!("a\nb")
        );
        assert_eq!(
            run_builtin(&Echo, &["-E", "a\\nb"], None)
                .expect("echo runs")
                .value,
            json!("a\\nb")
        );
        let bundled = run_builtin(&Echo, &["-ne", "a\\tb"], None).expect("echo runs");
        assert_eq!(bundled.value, json!("a\tb"));
        assert!(bundled.suppress_newline);
        // A word that merely starts with a dash is still ordinary text, as in real echo.
        assert_eq!(
            run_builtin(&Echo, &["-x", "a"], None)
                .expect("echo runs")
                .value,
            json!("-x a")
        );
    }

    #[test]
    fn printf_renders_the_curated_conversions() {
        assert_eq!(
            run_builtin(&Printf, &["%s=%d\\n", "count", "7"], None)
                .expect("printf runs")
                .value,
            json!("count=7\n")
        );
        assert_eq!(
            run_builtin(&Printf, &["%f", "1.5"], None)
                .expect("printf runs")
                .value,
            json!("1.500000")
        );
        assert_eq!(
            run_builtin(&Printf, &["100%%"], None)
                .expect("printf runs")
                .value,
            json!("100%")
        );
        assert!(run_builtin(&Printf, &["%q", "x"], None).is_err());
    }

    #[test]
    fn test_evaluates_string_and_numeric_comparisons() {
        let status = |arguments: &[&str]| {
            run_builtin(&Test, arguments, None)
                .expect("test runs")
                .status
        };
        assert_eq!(status(&["-z", ""]), ExitCode::SUCCESS);
        assert_eq!(status(&["-n", ""]), ExitCode::FAILURE);
        assert_eq!(status(&["a", "=", "a"]), ExitCode::SUCCESS);
        assert_eq!(status(&["a", "!=", "a"]), ExitCode::FAILURE);
        assert_eq!(status(&["2", "-gt", "1"]), ExitCode::SUCCESS);
        assert_eq!(status(&["2", "-le", "1"]), ExitCode::FAILURE);
        assert_eq!(status(&["!", "-z", "x"]), ExitCode::SUCCESS);
    }

    #[test]
    fn stacked_negations_are_counted_not_recursed() {
        let status = |arguments: &[&str]| {
            run_builtin(&Test, arguments, None)
                .expect("test runs")
                .status
        };
        assert_eq!(status(&["!", "!", "-n", "x"]), ExitCode::SUCCESS);
        assert_eq!(status(&["!", "!", "!", "-n", "x"]), ExitCode::FAILURE);

        // argv length is attacker-controlled: an unquoted expansion of a JSON array spreads one
        // word per element, so a runtime-generated pile of `!`s must not build a stack frame each.
        let bangs = vec!["!"; 50_000];
        let mut arguments = bangs.clone();
        arguments.extend(["-n", "x"]);
        assert_eq!(status(&arguments), ExitCode::SUCCESS);
    }

    #[test]
    fn bracket_requires_its_closing_bracket() {
        assert_eq!(
            run_builtin(&TestBracket, &["a", "=", "a", "]"], None)
                .expect("[ runs")
                .status,
            ExitCode::SUCCESS
        );
        assert!(run_builtin(&TestBracket, &["a", "=", "a"], None).is_err());
    }

    #[test]
    fn file_tests_are_rejected_because_there_is_no_filesystem() {
        let failure = run_builtin(&Test, &["-f", "/etc/passwd"], None)
            .expect_err("file tests are not supported");
        let CommandFailure::Status { message, .. } = failure else {
            panic!("a file test must stay recoverable");
        };
        assert!(message.contains("no filesystem"), "{message}");
    }

    #[test]
    fn true_and_false_carry_their_conventional_statuses() {
        assert_eq!(
            run_builtin(&True, &[], None).expect("true runs").status,
            ExitCode::SUCCESS
        );
        assert_eq!(
            run_builtin(&False, &[], None).expect("false runs").status,
            ExitCode::FAILURE
        );
    }

    #[test]
    fn sleep_is_capped_by_the_remaining_deadline() {
        let limits = Limits {
            timeout: Duration::from_millis(30),
            ..Limits::default()
        };
        let started = std::time::Instant::now();
        let failure =
            run_builtin_with(&Sleep, &["30"], None, limits, None, &mut Default::default())
                .expect_err("an oversized sleep trips the deadline");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "sleep was not capped"
        );
        assert!(matches!(failure, CommandFailure::Fatal(_)));
    }

    #[test]
    fn sleep_rejects_malformed_durations() {
        assert!(run_builtin(&Sleep, &["soon"], None).is_err());
        assert!(run_builtin(&Sleep, &["-1"], None).is_err());
        assert!(run_builtin(&Sleep, &[], None).is_err());
    }

    #[test]
    fn an_astronomical_sleep_is_clamped_rather_than_panicking() {
        // `Duration::from_secs_f64` panics past its u64-second ceiling, so `sleep 1e30` used to
        // abort the process instead of being capped by the deadline like any other long sleep.
        let limits = Limits {
            timeout: Duration::from_millis(20),
            ..Limits::default()
        };
        for duration in ["1e30", "99999999999999999999", "1e300"] {
            let started = std::time::Instant::now();
            let failure = run_builtin_with(
                &Sleep,
                &[duration],
                None,
                limits,
                None,
                &mut Default::default(),
            )
            .expect_err("an oversized sleep trips the deadline");
            assert!(matches!(failure, CommandFailure::Fatal(_)), "{duration}");
            assert!(started.elapsed() < Duration::from_secs(5), "{duration}");
        }
    }

    #[test]
    fn cat_reads_only_named_in_memory_buffers() {
        let mut buffers = std::collections::BTreeMap::new();
        buffers.insert("buf".to_owned(), json!("hi"));
        let result = run_builtin_with(&Cat, &["buf"], None, Limits::default(), None, &mut buffers)
            .expect("cat runs");
        assert_eq!(result.value, json!("hi"));

        let failure = run_builtin_with(
            &Cat,
            &["/etc/passwd"],
            None,
            Limits::default(),
            None,
            &mut buffers,
        )
        .expect_err("real paths are not buffers");
        let CommandFailure::Status { message, .. } = failure else {
            panic!("a missing buffer must stay recoverable");
        };
        assert!(message.contains("no such buffer"), "{message}");
    }

    #[test]
    fn cat_without_arguments_passes_its_input_through() {
        assert_eq!(
            run_builtin(&Cat, &[], Some(json!({"a": 1})))
                .expect("cat runs")
                .value,
            json!({"a": 1})
        );
        assert_eq!(
            run_builtin(&Cat, &[], None).expect("cat runs").value,
            Value::Null
        );
    }
}
