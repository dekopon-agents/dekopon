//! Import-free fixture whose `probe` command word behaves like a small command-line program.
//!
//! It is the checked-in `run-command` guest: `probe --help` renders a help page on stdout at
//! status 0, `probe upper --text hi` proposes `cli-probe.upper`, `probe upper -` reads the value
//! piped into the word, `probe upper -` with nothing piped is a usage error on stderr at status 2,
//! and an unknown subcommand is a decline. The argument handling is hand-rolled on purpose: it is
//! the clap-free baseline the SDK's `Provider::run_command` contract promises.

use std::fmt::Display;

use dekopon_provider_sdk::{
    CapabilityId, CommandRun, EffectKind, Idempotency, Provider, ProviderApiVersion,
    ProviderCapability, ProviderError, ProviderManifest, RiskLevel,
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

/// The fixture: three pure text transformations reachable as `probe` subcommands.
struct CliProbe;

/// Upper-cases the text.
const UPPER: &str = "cli-probe.upper";
/// Counts the characters in the text.
const COUNT: &str = "cli-probe.count";
/// Reverses the text.
const REVERSE: &str = "cli-probe.reverse";

/// Every subcommand and the capability it proposes, the one table `manifest()` and
/// `run_command()` both read from so a renamed capability is a failing test, never an exit code.
const SUBCOMMANDS: [(&str, &str, &str); 3] = [
    ("upper", UPPER, "Upper-case the text"),
    ("count", COUNT, "Count the characters in the text"),
    ("reverse", REVERSE, "Reverse the text"),
];

/// The largest `text` the fixture transforms; anything longer is refused with its length.
const MAX_TEXT_BYTES: usize = 16 * 1024;

const VERSION: &str = "probe 0.1.0\n";

const HELP: &str = "Usage: probe <COMMAND>\n\
\n\
Commands:\n\
\x20 upper    Upper-case the text\n\
\x20 count    Count the characters in the text\n\
\x20 reverse  Reverse the text\n\
\n\
Each command takes `--text <TEXT>` or `-` to read the value piped into the word.\n\
\n\
Options:\n\
\x20 -h, --help     Print help\n\
\x20 -V, --version  Print version\n";

impl Provider for CliProbe {
    fn manifest() -> ProviderManifest {
        ProviderManifest {
            api_version: ProviderApiVersion::V1Alpha1,
            id: "cli-probe".parse().expect("static provider ID"),
            description: "Command-line provider fixture: help, usage errors, stdin, proposals"
                .to_owned(),
            command_words: vec!["probe".to_owned()],
            capabilities: SUBCOMMANDS
                .iter()
                .map(|(_, capability, description)| ProviderCapability {
                    id: capability.parse().expect("static capability ID"),
                    description: (*description).to_owned(),
                    effect: EffectKind::ReadOnly,
                    risk: RiskLevel::Low,
                    idempotency: Idempotency::Idempotent,
                    input_schema: json!({
                        "type": "object",
                        "required": ["text"],
                        "properties": {"text": {"type": "string"}},
                        "additionalProperties": false
                    }),
                })
                .collect(),
        }
    }

    fn invoke(capability: &CapabilityId, input: Value) -> Result<Value, ProviderError> {
        let text = text_argument(&input)?;
        match capability.as_str() {
            UPPER => Ok(json!({"text": text.to_uppercase()})),
            COUNT => Ok(json!({"characters": text.chars().count()})),
            REVERSE => Ok(json!({"text": text.chars().rev().collect::<String>()})),
            other => Err(ProviderError::new(
                "unsupported",
                format!("cli-probe does not implement {other}"),
            )),
        }
    }

    fn run_command(argv: &[String], stdin: Option<&str>) -> Result<CommandRun, ProviderError> {
        let Some((subcommand, rest)) = argv.split_first() else {
            return Ok(usage("a subcommand is required"));
        };
        match subcommand.as_str() {
            "--help" | "-h" => return Ok(CommandRun::rendered(HELP, 0)),
            "--version" | "-V" => return Ok(CommandRun::rendered(VERSION, 0)),
            _ => {}
        }
        let Some(capability) = capability_for(subcommand) else {
            return Err(ProviderError::new(
                "usage",
                format!("unrecognized subcommand '{subcommand}'"),
            ));
        };
        let text = match rest {
            [flag, text] if flag == "--text" => text.as_str(),
            [dash] if dash == "-" => match stdin {
                Some(text) => text,
                None => return Ok(usage(format!("probe {subcommand} -: nothing was piped in"))),
            },
            _ => {
                return Ok(usage(format!(
                    "probe {subcommand} takes `--text <TEXT>` or `-`"
                )));
            }
        };
        if let Err(error) = check_text(text) {
            return Ok(usage(error.message()));
        }
        Ok(CommandRun::proposal(
            capability.parse().expect("static capability ID"),
            json!({"text": text}),
        ))
    }
}

/// The capability a subcommand proposes, `None` for a word the table does not know.
fn capability_for(subcommand: &str) -> Option<&'static str> {
    SUBCOMMANDS
        .iter()
        .find(|(word, _, _)| *word == subcommand)
        .map(|(_, capability, _)| *capability)
}

/// A usage error as a command-line program prints one: on stderr, at status 2, naming the fix.
fn usage(message: impl Display) -> CommandRun {
    CommandRun::rendered_error(
        format!(
            "error: {message}\n\nUsage: probe <COMMAND>\n\nFor more information, try '--help'.\n"
        ),
        2,
    )
}

/// Refuses a `text` beyond the fixture's bound, naming both lengths.
fn check_text(text: &str) -> Result<(), ProviderError> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(ProviderError::new(
            "invalid-input",
            format!(
                "text is {} bytes; the limit is {MAX_TEXT_BYTES}",
                text.len()
            ),
        ));
    }
    Ok(())
}

/// The one `text` string an input object may carry; every other shape is refused by name.
fn text_argument(input: &Value) -> Result<&str, ProviderError> {
    let Some(object) = input.as_object() else {
        return Err(ProviderError::new(
            "invalid-input",
            "input must be an object with a string `text` field",
        ));
    };
    if let Some(extra) = object.keys().find(|key| key.as_str() != "text") {
        return Err(ProviderError::new(
            "invalid-input",
            format!("unexpected input field `{extra}`"),
        ));
    }
    let Some(text) = object.get("text").and_then(Value::as_str) else {
        return Err(ProviderError::new(
            "invalid-input",
            "input must carry a string `text` field",
        ));
    };
    check_text(text)?;
    Ok(text)
}

dekopon_provider_sdk::export_provider_with_cli!(CliProbe, bindings);

#[cfg(test)]
mod tests {
    use dekopon_provider_sdk::{CommandInvocation, CommandRun, Provider};
    use serde_json::json;

    use super::{CliProbe, HELP, MAX_TEXT_BYTES, SUBCOMMANDS, VERSION};

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn every_dispatch_target_is_declared_in_the_manifest() {
        let manifest = CliProbe::manifest();
        assert_eq!(manifest.id.as_str(), "cli-probe");
        assert_eq!(manifest.command_words, ["probe"]);
        let declared = manifest
            .capabilities
            .iter()
            .map(|capability| capability.id.as_str().to_owned())
            .collect::<Vec<_>>();
        for (_, capability, _) in SUBCOMMANDS {
            assert!(
                declared.iter().any(|id| id == capability),
                "{capability} is dispatched to but not declared"
            );
        }
        assert_eq!(declared.len(), SUBCOMMANDS.len());
    }

    #[test]
    fn help_and_version_render_on_stdout_at_status_zero() {
        for (flag, expected) in [("--help", HELP), ("-h", HELP), ("--version", VERSION)] {
            let run = CliProbe::run_command(&argv(&[flag]), None).expect("help is not a decline");
            assert_eq!(
                run,
                CommandRun::Rendered {
                    stdout: expected.to_owned(),
                    stderr: String::new(),
                    status: 0,
                },
                "{flag}"
            );
        }
        assert!(
            !HELP.contains('\u{1b}'),
            "help text is plain, never coloured"
        );
    }

    #[test]
    fn a_text_flag_proposes_the_subcommands_capability() {
        let run = CliProbe::run_command(&argv(&["reverse", "--text", "abc"]), None)
            .expect("a well-formed subcommand proposes");
        assert_eq!(
            run,
            CommandRun::Proposal(CommandInvocation {
                capability: "cli-probe.reverse".parse().expect("static capability"),
                input: json!({"text": "abc"}),
            })
        );
    }

    #[test]
    fn a_dash_reads_the_piped_value_into_the_proposal() {
        let run = CliProbe::run_command(&argv(&["upper", "-"]), Some("hello"))
            .expect("a piped value proposes");
        assert_eq!(
            run,
            CommandRun::proposal(
                "cli-probe.upper".parse().expect("static capability"),
                json!({"text": "hello"})
            )
        );
    }

    #[test]
    fn a_dash_without_a_piped_value_is_a_usage_error_on_stderr() {
        let run = CliProbe::run_command(&argv(&["upper", "-"]), None)
            .expect("a usage error is rendered, not declined");
        let CommandRun::Rendered {
            stdout,
            stderr,
            status,
        } = run
        else {
            panic!("expected rendered text, got {run:?}");
        };
        assert_eq!(status, 2);
        assert!(stdout.is_empty(), "{stdout:?}");
        assert!(stderr.contains("nothing was piped in"), "{stderr:?}");
    }

    #[test]
    fn a_missing_or_malformed_argument_is_a_usage_error() {
        for words in [&[][..], &["count"][..], &["count", "--text"][..]] {
            let run = CliProbe::run_command(&argv(words), None).expect("rendered, not declined");
            assert!(
                matches!(run, CommandRun::Rendered { status: 2, ref stderr, .. } if stderr.starts_with("error: ")),
                "{words:?}: {run:?}"
            );
        }
    }

    #[test]
    fn an_oversized_text_is_refused_by_both_paths_naming_its_length() {
        let text = "x".repeat(MAX_TEXT_BYTES + 1);
        let run = CliProbe::run_command(&argv(&["upper", "--text", &text]), None)
            .expect("rendered, not declined");
        assert!(
            matches!(run, CommandRun::Rendered { status: 2, ref stderr, .. } if stderr.contains("16385 bytes")),
            "{run:?}"
        );
        let error = CliProbe::invoke(
            &"cli-probe.upper".parse().expect("static capability"),
            json!({"text": text}),
        )
        .expect_err("invoke enforces the same bound");
        assert_eq!(error.code(), "invalid-input");
        assert!(error.message().contains("16385 bytes"), "{error:?}");
    }

    #[test]
    fn an_unknown_subcommand_is_a_decline() {
        let error = CliProbe::run_command(&argv(&["bogus"]), None)
            .expect_err("an unknown word is declined");
        assert_eq!(error.code(), "usage");
        assert!(error.message().contains("'bogus'"), "{error:?}");
    }

    #[test]
    fn invoke_transforms_the_text_and_refuses_other_shapes() {
        let cases = [
            ("cli-probe.upper", json!({"text": "HELLO"})),
            ("cli-probe.count", json!({"characters": 5})),
            ("cli-probe.reverse", json!({"text": "olleh"})),
        ];
        for (capability, expected) in cases {
            let output = CliProbe::invoke(
                &capability.parse().expect("static capability"),
                json!({"text": "hello"}),
            )
            .expect("a string text transforms");
            assert_eq!(output, expected, "{capability}");
        }
        let capability = "cli-probe.upper".parse().expect("static capability");
        for (input, cause) in [
            (json!("hello"), "must be an object"),
            (json!({}), "string `text`"),
            (json!({"text": 1}), "string `text`"),
            (json!({"text": "a", "extra": true}), "`extra`"),
        ] {
            let error = CliProbe::invoke(&capability, input.clone()).expect_err("refused");
            assert_eq!(error.code(), "invalid-input", "{input}");
            assert!(error.message().contains(cause), "{input}: {error:?}");
        }
    }
}
