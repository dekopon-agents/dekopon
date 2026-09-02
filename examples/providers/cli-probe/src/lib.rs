//! Import-free fixture whose `probe` command word behaves like a small command-line program.
//!
//! It is the checked-in `run-command` guest built on the SDK's `clap` layer: `probe --help` and
//! `probe --version` render on stdout at status 0, `probe bogus` and `probe count` (missing its
//! argument) render clap's usage error on stderr at status 2, `probe upper --text hi` proposes
//! `cli-probe.upper`, `probe upper -` reads the value piped into the word, and `probe upper -`
//! with nothing piped is declined with a `usage` error. The command tree is declared once through
//! `#[derive(Parser)]` against the clap the SDK re-exports; the hand-rolled baseline the SDK
//! documents lives in `memory-reservation-probe`.

use dekopon_provider_sdk::clap::{self, Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use dekopon_provider_sdk::{
    CapabilityId, CommandInvocation, CommandRun, EffectKind, Idempotency, Provider,
    ProviderApiVersion, ProviderCapability, ProviderError, ProviderManifest, RiskLevel, cli,
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

/// The largest `text` the fixture transforms; anything longer is refused with its length.
const MAX_TEXT_BYTES: usize = 16 * 1024;

// The `probe` command tree, declared once and rendered by clap. A plain comment rather than a doc
// comment: clap would render a doc comment as the `about` line above `Usage:`.
#[derive(Parser)]
#[command(name = "probe", version = "0.1.0")]
struct Probe {
    #[command(subcommand)]
    transform: Transform,
}

// Every subcommand; each proposes exactly one capability, named by the `const` `manifest()`
// declares, so a renamed capability is a compile error rather than an exit code. A plain comment
// for the same reason as on `Probe`: clap renders an enum's doc comment as the parent's `about`.
#[derive(Subcommand)]
enum Transform {
    /// Upper-case the text
    Upper(TextSource),
    /// Count the characters in the text
    Count(TextSource),
    /// Reverse the text
    Reverse(TextSource),
}

impl Transform {
    /// The capability this subcommand proposes and the description the manifest declares for it.
    const fn capability(&self) -> (&'static str, &'static str) {
        match self {
            Self::Upper(_) => (UPPER, "Upper-case the text"),
            Self::Count(_) => (COUNT, "Count the characters in the text"),
            Self::Reverse(_) => (REVERSE, "Reverse the text"),
        }
    }

    /// The argument every subcommand takes.
    const fn source(&self) -> &TextSource {
        match self {
            Self::Upper(source) | Self::Count(source) | Self::Reverse(source) => source,
        }
    }
}

/// Where a subcommand's text comes from: `--text <TEXT>`, or `-` for the value piped into the word.
#[derive(Args)]
struct TextSource {
    /// The text to transform
    #[arg(
        long,
        value_name = "TEXT",
        conflicts_with = "piped",
        required_unless_present = "piped"
    )]
    text: Option<String>,
    /// Read the text piped into the word instead
    #[arg(value_name = "-", value_parser = ["-"])]
    piped: Option<String>,
}

/// Every subcommand the tree declares, in help order; the manifest is derived from it so a
/// subcommand cannot exist without its capability.
const SUBCOMMANDS: [Transform; 3] = [
    Transform::Upper(TextSource::NONE),
    Transform::Count(TextSource::NONE),
    Transform::Reverse(TextSource::NONE),
];

impl TextSource {
    /// A source with neither argument, for the manifest table only; clap never produces it.
    const NONE: Self = Self {
        text: None,
        piped: None,
    };
}

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
                .map(|transform| {
                    let (capability, description) = transform.capability();
                    ProviderCapability {
                        id: capability.parse().expect("static capability ID"),
                        description: description.to_owned(),
                        effect: EffectKind::ReadOnly,
                        risk: RiskLevel::Low,
                        idempotency: Idempotency::Idempotent,
                        input_schema: json!({
                            "type": "object",
                            "required": ["text"],
                            "properties": {"text": {"type": "string"}},
                            "additionalProperties": false
                        }),
                    }
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
        cli::run_command(Probe::command(), argv, stdin, dispatch)
    }
}

/// Turns clap's matches into the proposal for the selected subcommand.
///
/// Runs only after clap accepted the argv, so the subcommand and one of its two sources are
/// present; what remains to check is what clap cannot know — whether anything was piped, and the
/// text bound — and each refusal is a decline naming its cause.
fn dispatch(
    matches: clap::ArgMatches,
    stdin: Option<&str>,
) -> Result<CommandInvocation, ProviderError> {
    let probe = Probe::from_arg_matches(&matches)
        .map_err(|error| ProviderError::new("usage", error.to_string()))?;
    let (capability, _) = probe.transform.capability();
    let source = probe.transform.source();
    let text = match (&source.text, &source.piped) {
        (Some(text), _) => text.as_str(),
        (None, Some(_)) => stdin.ok_or_else(|| {
            ProviderError::new(
                "usage",
                format!(
                    "probe {} -: nothing was piped in",
                    subcommand_word(&probe.transform)
                ),
            )
        })?,
        (None, None) => {
            return Err(ProviderError::new(
                "usage",
                format!(
                    "probe {} takes `--text <TEXT>` or `-`",
                    subcommand_word(&probe.transform)
                ),
            ));
        }
    };
    check_text(text)?;
    Ok(CommandInvocation {
        capability: capability.parse().expect("static capability ID"),
        input: json!({"text": text}),
    })
}

/// The word a subcommand is typed as, for messages.
const fn subcommand_word(transform: &Transform) -> &'static str {
    match transform {
        Transform::Upper(_) => "upper",
        Transform::Count(_) => "count",
        Transform::Reverse(_) => "reverse",
    }
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

    use super::{CliProbe, MAX_TEXT_BYTES, SUBCOMMANDS, subcommand_word};

    /// clap's rendering of the tree, pinned byte for byte; the fixture's lockfile pins clap.
    const HELP: &str = "Usage: probe <COMMAND>\n\
\n\
Commands:\n\
\x20 upper    Upper-case the text\n\
\x20 count    Count the characters in the text\n\
\x20 reverse  Reverse the text\n\
\x20 help     Print this message or the help of the given subcommand(s)\n\
\n\
Options:\n\
\x20 -h, --help     Print help\n\
\x20 -V, --version  Print version\n";

    const VERSION: &str = "probe 0.1.0\n";

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    fn rendered(words: &[&str], stdin: Option<&str>) -> (String, String, u8) {
        let run = CliProbe::run_command(&argv(words), stdin).expect("rendered, not declined");
        let CommandRun::Rendered {
            stdout,
            stderr,
            status,
        } = run
        else {
            panic!("expected rendered text for {words:?}, got {run:?}");
        };
        (stdout, stderr, status)
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
        for transform in &SUBCOMMANDS {
            let (capability, _) = transform.capability();
            assert!(
                declared.iter().any(|id| id == capability),
                "{capability} is dispatched to but not declared"
            );
            let run =
                CliProbe::run_command(&argv(&[subcommand_word(transform), "--text", "x"]), None)
                    .expect("every subcommand proposes");
            assert!(
                matches!(run, CommandRun::Proposal(ref invocation) if invocation.capability.as_str() == capability),
                "{capability}: {run:?}"
            );
        }
        assert_eq!(declared.len(), SUBCOMMANDS.len());
    }

    #[test]
    fn help_and_version_render_on_stdout_at_status_zero() {
        for (flag, expected) in [("--help", HELP), ("-h", HELP), ("--version", VERSION)] {
            let (stdout, stderr, status) = rendered(&[flag], None);
            assert_eq!(status, 0, "{flag}");
            assert_eq!(stdout, expected, "{flag}");
            assert!(stderr.is_empty(), "{flag}: {stderr:?}");
            assert!(!stdout.contains('\u{1b}'), "{flag}: plain, never coloured");
        }
    }

    #[test]
    fn an_unknown_subcommand_is_a_usage_error_on_stderr_at_status_two() {
        let (stdout, stderr, status) = rendered(&["bogus"], None);
        assert_eq!(status, 2);
        assert!(stdout.is_empty(), "{stdout:?}");
        assert!(
            stderr.starts_with("error: unrecognized subcommand 'bogus'"),
            "{stderr:?}"
        );
        assert!(stderr.contains("\nUsage: probe <COMMAND>\n"), "{stderr:?}");
        assert!(!stderr.contains('\u{1b}'), "plain, never coloured");
    }

    /// clap answers a bare `probe` with the help page on stderr at status 2, the way a tool with
    /// a required subcommand does; every other malformed argv gets clap's `error:` line, and the
    /// ones clap gives a usage line carry the word the model typed, not the bare subcommand.
    #[test]
    fn a_missing_or_malformed_argument_is_a_usage_error() {
        let (stdout, stderr, status) = rendered(&[], None);
        assert_eq!(status, 2);
        assert!(stdout.is_empty(), "{stdout:?}");
        assert_eq!(stderr, HELP);
        for words in [
            &["count"][..],
            &["count", "--text"][..],
            &["count", "--text", "a", "-"][..],
            &["count", "+"][..],
        ] {
            let (stdout, stderr, status) = rendered(words, None);
            assert_eq!(status, 2, "{words:?}");
            assert!(stdout.is_empty(), "{words:?}: {stdout:?}");
            assert!(stderr.starts_with("error: "), "{words:?}: {stderr:?}");
            assert!(
                !stderr.contains('\u{1b}'),
                "{words:?}: plain, never coloured"
            );
        }
        let (_, stderr, _) = rendered(&["count"], None);
        assert!(
            stderr.contains("\nUsage: probe count --text <TEXT> [-]\n"),
            "{stderr:?}"
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
    fn a_dash_without_a_piped_value_is_declined_naming_the_cause() {
        let error = CliProbe::run_command(&argv(&["upper", "-"]), None)
            .expect_err("nothing piped is a decline, not a proposal");
        assert_eq!(error.code(), "usage");
        assert_eq!(error.message(), "probe upper -: nothing was piped in");
    }

    #[test]
    fn an_oversized_text_is_refused_by_both_paths_naming_its_length() {
        let text = "x".repeat(MAX_TEXT_BYTES + 1);
        let error = CliProbe::run_command(&argv(&["upper", "--text", &text]), None)
            .expect_err("the bound is a decline");
        assert_eq!(error.code(), "invalid-input");
        assert!(error.message().contains("16385 bytes"), "{error:?}");
        let error = CliProbe::invoke(
            &"cli-probe.upper".parse().expect("static capability"),
            json!({"text": text}),
        )
        .expect_err("invoke enforces the same bound");
        assert_eq!(error.code(), "invalid-input");
        assert!(error.message().contains("16385 bytes"), "{error:?}");
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
