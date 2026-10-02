use dekopon_provider_sdk::clap::Parser;
use dekopon_provider_sdk::provider::{
    Capability, Clock, Code, Failure, Proposal, Provider, Stdout, Usage,
};
use dekopon_provider_sdk::{EffectKind, RiskLevel};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Write;

struct ClockProbe;
struct Now;

#[derive(Parser)]
#[command(name = "date")]
struct Date {}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Debug)]
struct ClockError(u64);
impl std::fmt::Display for ClockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the host clock reads {} ms, past 9999-12-31T23:59:59.999Z",
            self.0
        )
    }
}
impl Failure for ClockError {
    fn code(&self) -> Code {
        Code::new("clock-out-of-range")
    }
}

impl Provider for ClockProbe {
    const ID: &'static str = "clock-probe";
    const COMMAND_WORDS: &'static [&'static str] = &["date"];
    const DESCRIPTION: &'static str =
        "Clock provider fixture: the date word and the host wall clock";
    type Args = Date;
    type Capabilities = (Now,);
    fn propose(_: Date, _: bool) -> Result<Proposal<Self>, Usage> {
        Ok(Proposal::to::<Now>(Empty {}))
    }
}

impl Capability for Now {
    type Provider = ClockProbe;
    const NAME: &'static str = "now";
    const DESCRIPTION: &'static str = "Reads the broker host's wall clock in UTC";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = Empty;
    type Needs = Clock;
    type Error = ClockError;
    fn run(_: Empty, clock: Clock, out: &mut Stdout) -> Result<(), ClockError> {
        writeln!(out, "{}", reading(clock.now_unix_millis())?)
            .map_err(|_| ClockError(clock.now_unix_millis()))
    }
}

const MAX_RFC3339_UNIX_MILLIS: u64 = 253_402_300_799_999;

fn reading(unix_millis: u64) -> Result<Value, ClockError> {
    Ok(json!({"unixMillis": unix_millis, "rfc3339": rfc3339(unix_millis)?}))
}

fn rfc3339(unix_millis: u64) -> Result<String, ClockError> {
    if unix_millis > MAX_RFC3339_UNIX_MILLIS {
        return Err(ClockError(unix_millis));
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

dekopon_provider_sdk::export!(ClockProbe);

#[cfg(test)]
mod tests {
    use super::*;
    use dekopon_provider_sdk::CommandRunOutcome;
    use dekopon_provider_sdk::provider;

    #[test]
    fn date_proposes_clock_now_and_refuses_other_arguments() {
        assert!(
            matches!(provider::command::<ClockProbe>(&[], true), CommandRunOutcome::Proposed { capability, input, .. } if capability.as_str() == "clock-probe.now" && input == json!({}))
        );
        assert!(
            matches!(provider::command::<ClockProbe>(&["--help".into()], false), CommandRunOutcome::Rendered { status: 0, stdout, .. } if !stdout.contains('\u{1b}'))
        );
        assert!(matches!(
            provider::command::<ClockProbe>(&["-u".into()], false),
            CommandRunOutcome::Rendered { status: 2, .. }
        ));
        let exit = provider::invoke_native::<ClockProbe>(
            "clock-probe.now",
            r#"{"zone":"UTC"}"#,
            provider::NativeStdio {
                stdin: None,
                stdout: Box::new(std::io::sink()),
            },
        );
        assert_eq!(exit.status, 2);
    }

    #[test]
    fn rfc3339_renders_utc_to_the_second_across_leap_days() {
        for (millis, expected) in [
            (0, "1970-01-01T00:00:00Z"),
            (951_782_400_000, "2000-02-29T00:00:00Z"),
            (1_789_000_000_123, "2026-09-10T00:26:40Z"),
            (MAX_RFC3339_UNIX_MILLIS, "9999-12-31T23:59:59Z"),
        ] {
            assert_eq!(rfc3339(millis).unwrap(), expected);
        }
        let error = rfc3339(MAX_RFC3339_UNIX_MILLIS + 1).unwrap_err();
        assert_eq!(error.code().as_str(), "clock-out-of-range");
        assert!(matches!(error, ClockError(millis) if millis == MAX_RFC3339_UNIX_MILLIS + 1));
        assert_eq!(
            reading(951_782_400_000).unwrap(),
            json!({"unixMillis": 951_782_400_000_u64, "rfc3339": "2000-02-29T00:00:00Z"})
        );
    }
}
