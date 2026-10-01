use dekopon_provider_sdk::clap::{self, Args, Parser, Subcommand};
use dekopon_provider_sdk::provider::{
    Bounded, Capability, Code, Failure, Proposal, Provider, Usage,
};
use dekopon_provider_sdk::schemars::JsonSchema;
use dekopon_provider_sdk::{EffectKind, RiskLevel};
use serde::{Deserialize, Serialize};

struct CliProbe;
const MAX_TEXT_BYTES: usize = 16 * 1024;

#[derive(Parser)]
#[command(name = "probe", version = "0.1.0")]
struct Probe {
    #[command(subcommand)]
    transform: Transform,
}

#[derive(Subcommand)]
enum Transform {
    /// Upper-case the text
    Upper(TextSource),
    /// Count the characters in the text
    Count(TextSource),
    /// Reverse the text
    Reverse(TextSource),
}

#[derive(Args)]
struct TextSource {
    /// The text to transform
    #[arg(
        long,
        value_name = "TEXT",
        conflicts_with = "piped",
        required_unless_present = "piped"
    )]
    text: Option<Bounded<MAX_TEXT_BYTES>>,
    /// Read the text piped into the word instead
    #[arg(value_name = "-", value_parser = ["-"])]
    piped: Option<String>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TextInput {
    text: Bounded<MAX_TEXT_BYTES>,
}

#[derive(Serialize)]
struct TextOutput {
    text: String,
}
#[derive(Serialize)]
struct CountOutput {
    characters: usize,
}

#[derive(Debug)]
struct Never;
impl std::fmt::Display for Never {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unreachable")
    }
}
impl Failure for Never {
    fn code(&self) -> Code {
        Code::new("unreachable")
    }
}

struct Upper;
struct Count;
struct Reverse;

macro_rules! capability {
    ($kind:ident, $name:literal, $desc:literal, $output:ty, $body:expr) => {
        impl Capability for $kind {
            type Provider = CliProbe;
            const NAME: &'static str = $name;
            const DESCRIPTION: &'static str = $desc;
            const EFFECT: EffectKind = EffectKind::ReadOnly;
            const RISK: RiskLevel = RiskLevel::Low;
            type Input = TextInput;
            type Needs = ();
            type Output = $output;
            type Error = Never;
            fn run(input: TextInput, (): ()) -> Result<Self::Output, Self::Error> {
                Ok(($body)(input.text.as_str()))
            }
        }
    };
}
capability!(
    Upper,
    "upper",
    "Upper-case the text",
    TextOutput,
    |text: &str| TextOutput {
        text: text.to_uppercase()
    }
);
capability!(
    Count,
    "count",
    "Count the characters in the text",
    CountOutput,
    |text: &str| CountOutput {
        characters: text.chars().count()
    }
);
capability!(
    Reverse,
    "reverse",
    "Reverse the text",
    TextOutput,
    |text: &str| TextOutput {
        text: text.chars().rev().collect()
    }
);

impl Provider for CliProbe {
    const ID: &'static str = "cli-probe";
    const DESCRIPTION: &'static str =
        "Command-line provider fixture: help, usage errors, stdin, proposals";
    const COMMAND_WORDS: &'static [&'static str] = &["probe"];
    type Args = Probe;
    type Capabilities = (Upper, Count, Reverse);

    fn propose(args: Probe, stdin: Option<&str>) -> Result<Proposal<Self>, Usage> {
        let (source, name) = match args.transform {
            Transform::Upper(source) => (source, "upper"),
            Transform::Count(source) => (source, "count"),
            Transform::Reverse(source) => (source, "reverse"),
        };
        let text = match (source.text, source.piped) {
            (Some(text), _) => text,
            (None, Some(_)) => Bounded::new(
                stdin.ok_or_else(|| Usage::new(format!("probe {name} -: nothing was piped in")))?,
            )
            .map_err(|error| Usage::new(error.to_string()))?,
            (None, None) => {
                return Err(Usage::new(format!(
                    "probe {name} takes `--text <TEXT>` or `-`"
                )));
            }
        };
        let input = TextInput { text };
        Ok(match name {
            "upper" => Proposal::to::<Upper>(input),
            "count" => Proposal::to::<Count>(input),
            "reverse" => Proposal::to::<Reverse>(input),
            _ => unreachable!(),
        })
    }
}

dekopon_provider_sdk::export!(CliProbe);

#[cfg(test)]
mod tests {
    use super::*;
    use dekopon_provider_sdk::provider;
    use dekopon_provider_sdk::{CommandRunOutcome, ComponentResponse};
    use serde_json::json;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn dispatch_and_manifest_share_the_declared_capabilities() {
        let manifest = provider::manifest::<CliProbe>().unwrap();
        assert_eq!(manifest.command_words, ["probe"]);
        assert_eq!(manifest.capabilities.len(), 3);
        for (name, expected) in [
            ("upper", json!({"text":"HELLO"})),
            ("count", json!({"characters":5})),
            ("reverse", json!({"text":"olleh"})),
        ] {
            let id = format!("cli-probe.{name}");
            assert!(
                manifest
                    .capabilities
                    .iter()
                    .any(|cap| cap.id.as_str() == id)
            );
            assert_eq!(
                provider::call::<CliProbe>(&id, r#"{"text":"hello"}"#),
                ComponentResponse::Succeeded { output: expected }
            );
            let proposal = provider::command::<CliProbe>(&argv(&[name, "--text", "hello"]), None);
            assert!(
                matches!(proposal, CommandRunOutcome::Proposed { capability, .. } if capability.as_str() == id)
            );
        }
    }

    #[test]
    fn help_usage_stdin_and_bounded_input() {
        for flag in ["--help", "-h", "--version"] {
            assert!(
                matches!(provider::command::<CliProbe>(&argv(&[flag]), None), CommandRunOutcome::Rendered { stdout, stderr, status: 0 } if !stdout.contains('\u{1b}') && stderr.is_empty())
            );
        }
        assert!(
            matches!(provider::command::<CliProbe>(&argv(&["bogus"]), None), CommandRunOutcome::Rendered { stdout, stderr, status: 2 } if stdout.is_empty() && stderr.contains("unrecognized subcommand") && !stderr.contains('\u{1b}'))
        );
        assert!(
            matches!(provider::command::<CliProbe>(&argv(&["upper", "-"]), Some("hello")), CommandRunOutcome::Proposed { input, .. } if input == json!({"text":"hello"}))
        );
        assert!(
            matches!(provider::command::<CliProbe>(&argv(&["upper", "-"]), None), CommandRunOutcome::Failed { error } if error.code == "usage" && error.message.contains("nothing was piped"))
        );
        let too_long = "x".repeat(MAX_TEXT_BYTES + 1);
        assert!(matches!(
            provider::command::<CliProbe>(&argv(&["upper", "--text", &too_long]), None),
            CommandRunOutcome::Rendered { status: 2, .. }
        ));
        assert!(
            matches!(provider::call::<CliProbe>("cli-probe.upper", &json!({"text":too_long}).to_string()), ComponentResponse::Failed { error } if error.code == "invalid-input")
        );
        for input in [
            json!({}),
            json!({"text":1}),
            json!({"text":"a","extra":true}),
        ] {
            assert!(
                matches!(provider::call::<CliProbe>("cli-probe.upper", &input.to_string()), ComponentResponse::Failed { error } if error.code == "invalid-input")
            );
        }
    }
}
