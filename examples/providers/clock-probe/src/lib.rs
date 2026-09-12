//! Clock conformance fixture: its `date` word proposes `clock.now`, and `invoke` reads the broker
//! host's wall clock through `dekopon:clock/wall@1.0.0`.
//!
//! `run-command` is pure, as the provider contract requires: `date` proposes `clock.now` with an
//! empty input, `date --help` renders a hand-written page on stdout at status 0, and anything else
//! is a usage error on stderr at status 2. The clock is read only inside `invoke`, which answers
//! `{"unixMillis": n, "rfc3339": "YYYY-MM-DDTHH:MM:SSZ"}` in UTC. The test-only
//! `date --clock-in-run-command` reads the clock from `run-command` instead, which is the call the
//! broker host must trap.

use dekopon_provider_sdk::{
    CapabilityId, CommandRun, EffectKind, Provider, ProviderApiVersion, ProviderCapability,
    ProviderError, ProviderManifest, RiskLevel,
};
use serde_json::{Value, json};

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "provider",
        generate_all,
        pub_export_macro: true,
    });
}

struct ClockProbe;

/// The one capability: read the host's wall clock.
const NOW: &str = "clock.now";

/// Test-only argument that reads the clock from `run-command`, where the host must refuse it.
const CLOCK_IN_RUN_COMMAND: &str = "--clock-in-run-command";

/// The hand-written help page; there is no parser to render one.
const HELP: &str = "Usage: date\n\
\n\
Prints the broker host's current time in UTC by proposing `clock.now`.\n\
\n\
Options:\n\
\x20     --help  Print help\n";

/// 9999-12-31T23:59:59.999Z, the last instant a four-digit RFC 3339 year can name.
const MAX_RFC3339_UNIX_MILLIS: u64 = 253_402_300_799_999;

impl Provider for ClockProbe {
    fn manifest() -> ProviderManifest {
        ProviderManifest {
            api_version: ProviderApiVersion::V1Alpha1,
            id: "clock-probe".parse().expect("static provider ID"),
            description: "Clock provider fixture: the date word and the host wall clock".to_owned(),
            command_words: vec!["date".to_owned()],
            capabilities: vec![ProviderCapability {
                id: NOW.parse().expect("static capability ID"),
                description: "Reads the broker host's wall clock in UTC".to_owned(),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                input_schema: json!({"type": "object", "additionalProperties": false}),
            }],
        }
    }

    fn invoke(capability: &CapabilityId, input: Value) -> Result<Value, ProviderError> {
        if capability.as_str() != NOW {
            return Err(ProviderError::new(
                "unsupported",
                format!("clock-probe does not implement {}", capability.as_str()),
            ));
        }
        match input.as_object() {
            Some(object) if object.is_empty() => {}
            Some(object) => {
                let field = object.keys().next().map_or("", String::as_str);
                return Err(ProviderError::new(
                    "invalid-input",
                    format!("unexpected input field `{field}`"),
                ));
            }
            None => {
                return Err(ProviderError::new(
                    "invalid-input",
                    "input must be an empty object",
                ));
            }
        }
        reading(dekopon_provider_clock::now_unix_millis())
    }

    fn run_command(argv: &[String], _stdin: Option<&str>) -> Result<CommandRun, ProviderError> {
        match argv {
            [] => Ok(CommandRun::proposal(
                NOW.parse().expect("static capability ID"),
                json!({}),
            )),
            [flag] if flag == "--help" => Ok(CommandRun::rendered(HELP, 0)),
            [flag] if flag == CLOCK_IN_RUN_COMMAND => Ok(CommandRun::rendered(
                format!("{}\n", dekopon_provider_clock::now_unix_millis()),
                0,
            )),
            [first, rest @ ..] => {
                // `--help` alone renders; followed by anything, the follower is what was unexpected.
                let unexpected = rest.first().filter(|_| first == "--help").unwrap_or(first);
                Ok(CommandRun::rendered_error(
                    format!("date: unexpected argument '{unexpected}'\n\n{HELP}"),
                    2,
                ))
            }
        }
    }
}

/// The invocation's answer for one clock reading.
fn reading(unix_millis: u64) -> Result<Value, ProviderError> {
    Ok(json!({"unixMillis": unix_millis, "rfc3339": rfc3339(unix_millis)?}))
}

/// Renders `unix_millis` as the RFC 3339 UTC timestamp `YYYY-MM-DDTHH:MM:SSZ`, truncated to the
/// second. A reading past year 9999 has no four-digit year, so it is refused naming the reading.
fn rfc3339(unix_millis: u64) -> Result<String, ProviderError> {
    if unix_millis > MAX_RFC3339_UNIX_MILLIS {
        return Err(ProviderError::new(
            "clock-out-of-range",
            format!("the host clock reads {unix_millis} ms, past 9999-12-31T23:59:59.999Z"),
        ));
    }
    let seconds = unix_millis / 1_000;
    let (year, month, day) = civil_from_days(seconds / 86_400);
    let second_of_day = seconds % 86_400;
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3_600,
        second_of_day % 3_600 / 60,
        second_of_day % 60
    ))
}

/// The proleptic Gregorian `(year, month, day)` of a day count since 1970-01-01.
///
/// Howard Hinnant's `civil_from_days`, restricted to non-negative day counts: shift the epoch to
/// 0000-03-01 so the leap day ends each 400-year era, then read the year, the day of the
/// March-based year, and the month from it.
const fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let march_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * march_month + 2) / 5 + 1;
    let month = if march_month < 10 {
        march_month + 3
    } else {
        march_month - 9
    };
    let year = year_of_era + era * 400;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

dekopon_provider_sdk::export_provider_with_cli!(ClockProbe, bindings);

#[cfg(test)]
mod tests {
    use dekopon_provider_sdk::{CommandRun, Provider};
    use serde_json::json;

    use super::{ClockProbe, HELP, MAX_RFC3339_UNIX_MILLIS, NOW, reading, rfc3339};

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn manifest_declares_the_date_word_and_one_read_only_capability() {
        let manifest = ClockProbe::manifest();
        assert_eq!(manifest.id.as_str(), "clock-probe");
        assert_eq!(manifest.command_words, ["date"]);
        assert_eq!(
            manifest
                .capabilities
                .iter()
                .map(|capability| capability.id.as_str())
                .collect::<Vec<_>>(),
            [NOW]
        );
    }

    #[test]
    fn the_bare_word_proposes_clock_now_with_an_empty_input() {
        let run = ClockProbe::run_command(&[], Some("piped")).expect("the word proposes");
        assert_eq!(
            run,
            CommandRun::proposal(NOW.parse().expect("static capability"), json!({}))
        );
    }

    #[test]
    fn help_renders_on_stdout_at_status_zero() {
        let run = ClockProbe::run_command(&argv(&["--help"]), None).expect("help renders");
        assert_eq!(run, CommandRun::rendered(HELP, 0));
    }

    #[test]
    fn any_other_argument_is_a_usage_error_on_stderr_at_status_two() {
        for (words, unexpected) in [
            (&["-u"][..], "-u"),
            (&["+%s"][..], "+%s"),
            (&["--help", "extra"][..], "extra"),
        ] {
            let run = ClockProbe::run_command(&argv(words), None).expect("a usage error renders");
            let CommandRun::Rendered {
                stdout,
                stderr,
                status,
            } = run
            else {
                panic!("expected a rendered usage error for {words:?}, got {run:?}");
            };
            assert_eq!(status, 2, "{words:?}");
            assert!(stdout.is_empty(), "{stdout:?}");
            assert!(
                stderr.starts_with(&format!("date: unexpected argument '{unexpected}'")),
                "{stderr:?}"
            );
        }
    }

    #[test]
    fn rfc3339_renders_utc_to_the_second_across_leap_days() {
        for (unix_millis, expected) in [
            (0, "1970-01-01T00:00:00Z"),
            (951_782_400_000, "2000-02-29T00:00:00Z"),
            (1_789_000_000_123, "2026-09-10T00:26:40Z"),
            (MAX_RFC3339_UNIX_MILLIS, "9999-12-31T23:59:59Z"),
        ] {
            assert_eq!(rfc3339(unix_millis).expect("in range"), expected);
        }
    }

    #[test]
    fn a_reading_past_year_9999_is_refused_naming_it() {
        let error = rfc3339(MAX_RFC3339_UNIX_MILLIS + 1).expect_err("no four-digit year");
        assert_eq!(error.code(), "clock-out-of-range");
        assert!(error.message().contains("253402300800000"), "{error:?}");
    }

    #[test]
    fn a_reading_carries_both_the_millis_and_the_timestamp() {
        assert_eq!(
            reading(951_782_400_000).expect("in range"),
            json!({"unixMillis": 951_782_400_000_u64, "rfc3339": "2000-02-29T00:00:00Z"})
        );
    }

    #[test]
    fn invoke_refuses_other_capabilities_and_non_empty_input_before_reading_the_clock() {
        let other = "clock.later".parse().expect("capability");
        let error = ClockProbe::invoke(&other, json!({})).expect_err("unsupported");
        assert_eq!(error.code(), "unsupported");

        let now = NOW.parse().expect("capability");
        let error = ClockProbe::invoke(&now, json!({"zone": "UTC"})).expect_err("extra field");
        assert_eq!(error.code(), "invalid-input");
        assert!(error.message().contains("`zone`"), "{error:?}");

        let error = ClockProbe::invoke(&now, json!("now")).expect_err("not an object");
        assert_eq!(error.code(), "invalid-input");
    }
}
