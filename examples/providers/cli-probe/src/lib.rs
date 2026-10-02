use dekopon_provider_sdk::clap::{self, Args, Parser, Subcommand};
use dekopon_provider_sdk::provider::{
    Bounded, Capability, Code, Failure, Proposal, Provider, Stdout, Usage, stdin,
};
use dekopon_provider_sdk::{EffectKind, RiskLevel};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    piped: bool,
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
        f.write_str("probe: piped input is empty, too large or unavailable")
    }
}
impl Failure for Never {
    fn code(&self) -> Code {
        Code::USAGE
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
            type Error = Never;
            fn run(input: TextInput, (): (), out: &mut Stdout) -> Result<(), Self::Error> {
                let mut text = input.text.as_str().to_owned();
                if input.piped {
                    let mut reader = stdin().ok_or(Never)?;
                    reader
                        .take((MAX_TEXT_BYTES + 1) as u64)
                        .read_to_string(&mut text)
                        .map_err(|_| Never)?;
                    if text.is_empty() || text.len() > MAX_TEXT_BYTES {
                        return Err(Never);
                    }
                }
                let value: $output = ($body)(&text);
                writeln!(
                    out,
                    "{}",
                    serde_json::to_string(&value).expect("serializable probe output")
                )
                .map_err(|_| Never)
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

    fn propose(args: Probe, stdin_piped: bool) -> Result<Proposal<Self>, Usage> {
        match args.transform {
            Transform::Upper(source) => propose_text::<Upper>(source, stdin_piped, "upper"),
            Transform::Count(source) => propose_text::<Count>(source, stdin_piped, "count"),
            Transform::Reverse(source) => propose_text::<Reverse>(source, stdin_piped, "reverse"),
        }
    }
}

fn propose_text<C: Capability<Provider = CliProbe, Input = TextInput>>(
    source: TextSource,
    stdin_piped: bool,
    name: &str,
) -> Result<Proposal<CliProbe>, Usage> {
    let text = match (source.text, source.piped) {
        (Some(text), _) => text,
        (None, Some(_)) if stdin_piped => {
            return Ok(Proposal::to::<C>(TextInput {
                text: Bounded::new("").expect("empty text is bounded"),
                piped: true,
            }));
        }
        (None, Some(_)) => return Err(Usage::new(format!("probe {name} -: nothing was piped in"))),
        (None, None) => {
            return Err(Usage::new(format!(
                "probe {name} takes `--text <TEXT>` or `-`"
            )));
        }
    };
    Ok(Proposal::to::<C>(TextInput { text, piped: false }))
}

dekopon_provider_sdk::export!(CliProbe);

#[cfg(test)]
mod tests {
    use super::*;
    use dekopon_provider_sdk::CommandRunOutcome;
    use dekopon_provider_sdk::provider;
    use serde_json::json;

    #[derive(Clone, Default)]
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn invoke(id: &str, input: &str) -> (provider::NativeExit, Vec<u8>) {
        let capture = Capture::default();
        let result = provider::invoke_native::<CliProbe>(
            id,
            input,
            provider::NativeStdio {
                stdin: None,
                stdout: Box::new(capture.clone()),
            },
        );
        let bytes = capture.0.lock().unwrap().clone();
        (result, bytes)
    }

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
            let (result, stdout) = invoke(&id, r#"{"text":"hello"}"#);
            assert_eq!(result.status, 0, "{}", result.stderr);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&stdout).unwrap(),
                expected
            );
            let proposal = provider::command::<CliProbe>(&argv(&[name, "--text", "hello"]), false);
            assert!(
                matches!(proposal, CommandRunOutcome::Proposed { capability, .. } if capability.as_str() == id)
            );
        }
    }

    #[test]
    fn help_usage_stdin_and_bounded_input() {
        for flag in ["--help", "-h", "--version"] {
            assert!(
                matches!(provider::command::<CliProbe>(&argv(&[flag]), false), CommandRunOutcome::Rendered { stdout, stderr, status: 0 } if !stdout.contains('\u{1b}') && stderr.is_empty())
            );
        }
        assert!(
            matches!(provider::command::<CliProbe>(&argv(&["bogus"]), false), CommandRunOutcome::Rendered { stdout, stderr, status: 2 } if stdout.is_empty() && stderr.contains("unrecognized subcommand") && !stderr.contains('\u{1b}'))
        );
        assert!(
            matches!(provider::command::<CliProbe>(&argv(&["upper", "-"]), true), CommandRunOutcome::Proposed { input, .. } if input == json!({"text":"","piped":true}))
        );
        assert!(
            matches!(provider::command::<CliProbe>(&argv(&["upper", "-"]), false), CommandRunOutcome::Failed { error } if error.code == "usage" && error.message.contains("nothing was piped"))
        );
        let too_long = "x".repeat(MAX_TEXT_BYTES + 1);
        assert!(matches!(
            provider::command::<CliProbe>(&argv(&["upper", "--text", &too_long]), false),
            CommandRunOutcome::Rendered { status: 2, .. }
        ));
        assert_eq!(
            invoke("cli-probe.upper", &json!({"text":too_long}).to_string())
                .0
                .status,
            2
        );
        for input in [
            json!({}),
            json!({"text":1}),
            json!({"text":"a","extra":true}),
        ] {
            assert_eq!(invoke("cli-probe.upper", &input.to_string()).0.status, 2);
        }
    }
}
