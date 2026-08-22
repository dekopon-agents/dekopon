//! The `date` builtin, and the opt-in that decides whether it exists at all.
//!
//! Reading the wall clock is ambient authority. Nothing else in this interpreter can observe
//! anything outside the script's own value space without going through a capability the operator
//! granted, and the clock has no capability to go through: there is no provider for "what time is
//! it", and inventing one would be a fiction — a capability identifier nobody granted, authorized
//! by nobody, audited nowhere.
//!
//! So it is gated the way `curl`'s target is gated instead: by an explicit, off-by-default setting
//! the embedder threads in, [`crate::Limits::allow_clock`]. With it off, `date` reports "command
//! not found" exactly as an ungranted capability does, rather than returning a fixed or fabricated
//! time — a script that is told it is midnight on the epoch will believe it.
//!
//! The surface is deliberately small. This shell is busybox-curated rather than bash-complete, and
//! a full `strftime` is a large surface whose unimplemented corners would each be a silently wrong
//! answer. What is here is the epoch second and an ISO-8601 instant; every other format is
//! rejected by name.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::{Builtin, BuiltinContext, CommandFailure, CommandResult};
use crate::ExitCode;

/// `date [-u] [-I[seconds]] [+%s]`.
pub(crate) struct Date;

impl Builtin for Date {
    fn name(&self) -> &'static str {
        "date"
    }

    fn run(
        &self,
        context: &mut BuiltinContext<'_>,
        arguments: &[String],
        _input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        // Parsed before the gate is checked, so a malformed `date` is a usage error whether or not
        // the clock is available. The alternative teaches an operator that enabling the flag is
        // what fixes a typo.
        let format = parse(arguments)?;

        if !context.allow_clock {
            return Err(CommandFailure::Status {
                message:
                    "date: command not found: this session cannot read the clock; an operator enables it with --shell-allow-clock"
                        .to_owned(),
                status: ExitCode::NOT_FOUND,
            });
        }

        // A clock before 1970 is not a time this shell can render, and it is not a time any host
        // running this is plausibly reporting; saying so is better than rendering a negative epoch.
        #[allow(
            clippy::map_err_ignore,
            reason = "SystemTimeError carries only how far the clock sits before the epoch, and \
                      the message already states that fact; the exact offset would also be a \
                      host clock reading this session was not granted"
        )]
        let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| {
            CommandFailure::failed("date: the host clock is set before 1970 and cannot be rendered")
        })?;
        let seconds = i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX);

        Ok(CommandResult::value(match format {
            // A JSON number rather than text, so it stays a number through a pipe into `jq` and
            // through an assignment. Arithmetic wants it in a variable first — `t=$(date +%s)`,
            // then `$(( t + 60 ))` — because `$(( ... ))` here takes names and literals, not a
            // nested command substitution.
            Format::EpochSeconds => Value::from(seconds),
            Format::Iso8601 => Value::String(render_iso8601(seconds)),
        }))
    }
}

/// The formats `date` can render.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Format {
    /// `+%s`: whole seconds since the Unix epoch.
    EpochSeconds,
    /// The default and `-I`: `2026-08-05T12:34:56Z`.
    Iso8601,
}

/// Parses `date`'s curated flag set.
///
/// `-u` is accepted as a documented no-op: this shell has no other time zone to convert from, so
/// honoring it claims nothing that did not happen. Every other flag is rejected, because each one
/// would change what the answer means.
fn parse(arguments: &[String]) -> Result<Format, CommandFailure> {
    let mut format = None;
    for argument in arguments {
        let selected = match argument.as_str() {
            "-u" | "--utc" | "--universal" => continue,
            "-I" | "-Is" | "--iso-8601" | "--iso-8601=seconds" => Format::Iso8601,
            "+%s" => Format::EpochSeconds,
            other if other.starts_with('+') => {
                return Err(CommandFailure::usage(format!(
                    "date: format {other:?} is not supported; this shell renders `+%s` for the epoch second or `-I` for an ISO-8601 instant, and has no `strftime`"
                )));
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(CommandFailure::usage(format!(
                    "date: option not yet supported: {other}; this shell reads the current UTC time only, and cannot set the clock or convert another one"
                )));
            }
            other => {
                return Err(CommandFailure::usage(format!(
                    "date: unexpected argument {other:?}; pass `+%s`, `-I`, or no argument at all"
                )));
            }
        };
        if format.is_some_and(|existing| existing != selected) {
            return Err(CommandFailure::usage(
                "date: pass at most one output format",
            ));
        }
        format = Some(selected);
    }
    Ok(format.unwrap_or(Format::Iso8601))
}

/// Renders one epoch second as `YYYY-MM-DDTHH:MM:SSZ`.
fn render_iso8601(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (time / 3_600, (time % 3_600) / 60, time % 60);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Converts days since 1970-01-01 into a proleptic Gregorian year, month, and day.
///
/// This is Howard Hinnant's `civil_from_days`, reproduced rather than depended on: it is exact for
/// every day a host clock can report, and a calendar crate would be a new dependency and a new
/// license entry for twenty lines of integer arithmetic.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    // Shift the epoch to 0000-03-01 so that a leap day lands at the end of a four-century era.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use crate::{
        ExitCode,
        builtins::{
            CommandFailure,
            test_support::{run_builtin, run_builtin_with_clock},
        },
    };

    use super::{Date, render_iso8601};

    #[test]
    fn the_clock_is_unreachable_until_an_operator_enables_it() {
        // The default must look exactly like any other command this session cannot reach, so a
        // script cannot tell "not permitted" from "not a command" and go looking for a way around.
        let failure = run_builtin(&Date, &[], None).expect_err("the clock is off by default");
        let CommandFailure::Status { message, status } = failure else {
            panic!("an unavailable clock must stay recoverable");
        };
        assert_eq!(status, ExitCode::NOT_FOUND);
        assert!(message.contains("command not found"), "{message}");
        assert!(message.contains("--shell-allow-clock"), "{message}");
    }

    #[test]
    fn an_enabled_clock_renders_the_two_supported_formats() {
        let iso = run_builtin_with_clock(&Date, &[]).expect("date runs");
        let Value::String(rendered) = &iso.value else {
            panic!("the default format is a string, found {:?}", iso.value);
        };
        assert_eq!(rendered.len(), 20, "{rendered}");
        assert!(rendered.ends_with('Z'), "{rendered}");

        let epoch = run_builtin_with_clock(&Date, &["+%s"]).expect("date runs");
        let seconds = epoch.value.as_i64().expect("an epoch second is a number");
        // Any clock this test can run under is past 2020 and short of 2100.
        assert!(
            (1_577_836_800..4_102_444_800).contains(&seconds),
            "{seconds}"
        );
    }

    #[test]
    fn utc_is_accepted_and_other_flags_are_rejected_by_name() {
        assert!(run_builtin_with_clock(&Date, &["-u"]).is_ok());
        assert!(run_builtin_with_clock(&Date, &["-u", "-I"]).is_ok());

        let failure = run_builtin_with_clock(&Date, &["+%Y-%m-%d"]).expect_err("strftime is out");
        let CommandFailure::Status { message, .. } = failure else {
            panic!("an unsupported format must stay recoverable");
        };
        assert!(message.contains("strftime"), "{message}");

        assert!(run_builtin_with_clock(&Date, &["-d", "yesterday"]).is_err());
        assert!(run_builtin_with_clock(&Date, &["-s", "2020-01-01"]).is_err());
        assert!(run_builtin_with_clock(&Date, &["tomorrow"]).is_err());
        assert!(run_builtin_with_clock(&Date, &["+%s", "-I"]).is_err());
    }

    #[test]
    fn a_malformed_date_is_a_usage_error_even_with_the_clock_off() {
        // Otherwise an operator learns that enabling the clock is what fixes a typo.
        let failure =
            run_builtin(&Date, &["+%Y"], None).expect_err("a bad format is a usage error");
        let CommandFailure::Status { status, .. } = failure else {
            panic!("a usage error must stay recoverable");
        };
        assert_eq!(status, ExitCode::SYNTAX);
    }

    #[test]
    fn the_calendar_matches_known_instants() {
        // Epoch, a leap day, a century that is not a leap year, and one that is.
        assert_eq!(render_iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(render_iso8601(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(render_iso8601(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(render_iso8601(1_754_395_496), "2025-08-05T12:04:56Z");
        assert_eq!(render_iso8601(4_102_444_799), "2099-12-31T23:59:59Z");
    }
}
