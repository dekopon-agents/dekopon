use std::fmt;

use clap::{Parser, Subcommand};
use dekopon_provider_sdk::provider::{
    Capability, Code, Failure, Proposal, Provider, SdkFailure, Usage, call, command, manifest,
};
use dekopon_provider_sdk::{
    CommandRunOutcome, ComponentFailure, ComponentResponse, EffectKind, RiskLevel,
    SecretUseProposal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const DRN: &str = "drn:com.example:secret:prod:api/token";

struct Fixture;

#[derive(Parser)]
#[command(name = "fixture", version = "0.1.0", about = "Transforms text")]
struct Args {
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Subcommand)]
enum Verb {
    /// Upper-case the text
    Upper {
        /// The text; omit it to read piped input
        #[arg(long)]
        text: Option<String>,
    },
    /// Count the characters of the text
    Count {
        /// The text to count
        text: String,
        /// A secret to present as a bearer token
        #[arg(long)]
        bearer: Option<String>,
    },
    /// Propose a capability the provider does not list
    Stray,
}

impl Provider for Fixture {
    const ID: &'static str = "fixture";
    const COMMAND_WORDS: &'static [&'static str] = &["fixture"];
    const DESCRIPTION: &'static str = "Transforms text";
    type Args = Args;
    type Capabilities = (Upper, Count);

    fn propose(args: Args, stdin: Option<&str>) -> Result<Proposal<Self>, Usage> {
        match args.verb {
            Verb::Upper { text } => {
                let text = text
                    .or_else(|| stdin.map(str::to_owned))
                    .ok_or_else(|| Usage::new("fixture upper: pass --text or pipe input"))?;
                Ok(Proposal::to::<Upper>(TextInput { text }))
            }
            Verb::Count { text, bearer } => {
                let proposal = Proposal::to::<Count>(CountInput { text, limit: 8 });
                match bearer {
                    None => Ok(proposal),
                    Some(drn) => {
                        let Ok(secret) = drn.parse() else {
                            return Err(Usage::new("fixture count: --bearer takes a DRN"));
                        };
                        Ok(proposal.with_secret_use(SecretUseProposal::HttpBearer { secret }))
                    }
                }
            }
            Verb::Stray => Ok(Proposal::to::<Stray>(TextInput {
                text: String::new(),
            })),
        }
    }
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TextInput {
    /// The text to transform
    text: String,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CountInput {
    /// The text to count
    text: String,
    /// The largest count that succeeds
    #[serde(default)]
    limit: u32,
}

#[derive(Serialize)]
struct TextOutput {
    text: String,
}

enum CountFailure {
    TooLong,
}

impl fmt::Display for CountFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong => formatter.write_str("the text is longer than the limit"),
        }
    }
}

const TOO_LONG: Code = Code::new("too-long");

impl Failure for CountFailure {
    fn code(&self) -> Code {
        match self {
            Self::TooLong => TOO_LONG,
        }
    }
}

struct Upper;

impl Capability for Upper {
    type Provider = Fixture;
    const NAME: &'static str = "upper";
    const DESCRIPTION: &'static str = "Upper-cases text";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = TextInput;
    type Needs = ();
    type Output = TextOutput;
    type Error = CountFailure;

    fn run(input: TextInput, (): ()) -> Result<TextOutput, CountFailure> {
        Ok(TextOutput {
            text: input.text.to_uppercase(),
        })
    }
}

struct Count;

impl Capability for Count {
    type Provider = Fixture;
    const NAME: &'static str = "count";
    const DESCRIPTION: &'static str = "Counts characters";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = CountInput;
    type Needs = ();
    type Output = u32;
    type Error = CountFailure;

    fn run(input: CountInput, (): ()) -> Result<u32, CountFailure> {
        u32::try_from(input.text.chars().count())
            .ok()
            .filter(|count| *count <= input.limit)
            .ok_or(CountFailure::TooLong)
    }
}

struct Stray;

impl Capability for Stray {
    type Provider = Fixture;
    const NAME: &'static str = "stray";
    const DESCRIPTION: &'static str = "Is not listed by its provider";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = TextInput;
    type Needs = ();
    type Output = TextOutput;
    type Error = CountFailure;

    fn run(input: TextInput, (): ()) -> Result<TextOutput, CountFailure> {
        Ok(TextOutput { text: input.text })
    }
}

fn run(words: &[&str], stdin: Option<&str>) -> CommandRunOutcome {
    let argv: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
    command::<Fixture>(&argv, stdin)
}

fn failed(response: ComponentResponse) -> ComponentFailure {
    match response {
        ComponentResponse::Failed { error } => error,
        ComponentResponse::Succeeded { output } => panic!("expected a failure, got {output}"),
    }
}

fn walk(schema: &Value, visit: &mut impl FnMut(&serde_json::Map<String, Value>)) {
    match schema {
        Value::Object(object) => {
            visit(object);
            object.values().for_each(|value| walk(value, visit));
        }
        Value::Array(items) => items.iter().for_each(|value| walk(value, visit)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

#[test]
fn the_manifest_derives_capability_ids_from_the_provider_and_capability_names() {
    let manifest = manifest::<Fixture>().expect("the fixture identifiers are valid");
    assert_eq!(manifest.id.as_str(), "fixture");
    assert_eq!(manifest.description, "Transforms text");
    assert_eq!(manifest.command_words, ["fixture"]);
    let ids: Vec<&str> = manifest
        .capabilities
        .iter()
        .map(|capability| capability.id.as_str())
        .collect();
    assert_eq!(ids, ["fixture.upper", "fixture.count"]);
    assert_eq!(manifest.capabilities[1].description, "Counts characters");
}

#[test]
fn every_input_schema_is_closed_and_inline() {
    let manifest = manifest::<Fixture>().expect("the fixture identifiers are valid");
    for capability in &manifest.capabilities {
        let schema = &capability.input_schema;
        assert_eq!(schema["type"], "object", "{schema}");
        walk(schema, &mut |object| {
            for key in ["$ref", "$defs", "$schema", "definitions", "title"] {
                assert!(!object.contains_key(key), "{key} in {schema}");
            }
            if object.get("type") == Some(&json!("object")) {
                assert_eq!(object.get("additionalProperties"), Some(&json!(false)));
            }
        });
    }
    let count = &manifest.capabilities[1].input_schema;
    assert_eq!(count["required"], json!(["text"]));
    assert_eq!(
        count["properties"]["limit"]["description"],
        "The largest count that succeeds"
    );
}

#[test]
fn a_call_dispatches_to_the_named_capability_and_serializes_its_output() {
    assert_eq!(
        call::<Fixture>("fixture.upper", r#"{"text":"hello"}"#),
        ComponentResponse::Succeeded {
            output: json!({"text": "HELLO"})
        }
    );
    assert_eq!(
        call::<Fixture>("fixture.count", r#"{"text":"hello","limit":5}"#),
        ComponentResponse::Succeeded { output: json!(5) }
    );
}

#[test]
fn a_capability_failure_carries_its_typed_code_and_display_message() {
    let error = failed(call::<Fixture>("fixture.count", r#"{"text":"hello"}"#));
    assert_eq!(error.code, TOO_LONG.as_str());
    assert_eq!(error.message, CountFailure::TooLong.to_string());
}

#[test]
fn an_unknown_capability_fails_with_the_sdk_code_and_static_message() {
    for capability in ["fixture.stray", "other.upper", "fixture", "Not Valid"] {
        let error = failed(call::<Fixture>(capability, r#"{"text":"x"}"#));
        assert_eq!(error.code, SdkFailure::UnknownCapability.code().as_str());
        assert_eq!(error.message, SdkFailure::UnknownCapability.to_string());
    }
}

#[test]
fn invalid_input_never_echoes_parser_text() {
    for input in [
        r#"{"text":5}"#,
        r#"{"text":"x","extra":"secret-looking"}"#,
        r#"{"limit":1}"#,
        "{",
    ] {
        let error = failed(call::<Fixture>("fixture.count", input));
        assert_eq!(error.code, SdkFailure::InvalidInput.code().as_str());
        assert_eq!(error.message, SdkFailure::InvalidInput.to_string());
    }
}

#[test]
fn the_sdk_owns_distinct_codes_for_its_own_failures() {
    let codes = [
        SdkFailure::UnknownCapability,
        SdkFailure::InvalidInput,
        SdkFailure::InvalidSettings,
        SdkFailure::SerializationFailed,
    ]
    .map(|failure| failure.code().as_str());
    assert_eq!(
        codes,
        [
            "unknown-capability",
            "invalid-input",
            "invalid-settings",
            "serialization-failed"
        ]
    );
}

#[test]
fn help_and_usage_render_from_the_args_with_no_escape_byte() {
    let CommandRunOutcome::Rendered {
        stdout,
        stderr,
        status,
    } = run(&["--help"], None)
    else {
        panic!("help renders");
    };
    assert_eq!(status, 0);
    assert!(stderr.is_empty(), "{stderr:?}");
    assert!(stdout.contains("Usage: fixture <COMMAND>"), "{stdout:?}");
    assert!(stdout.contains("upper"), "{stdout:?}");

    for words in [
        &["--help"][..],
        &["upper", "--help"],
        &["bogus"],
        &["count"],
    ] {
        let CommandRunOutcome::Rendered { stdout, stderr, .. } = run(words, None) else {
            panic!("{words:?} renders");
        };
        assert!(!stdout.contains('\u{1b}'), "{words:?}: {stdout:?}");
        assert!(!stderr.contains('\u{1b}'), "{words:?}: {stderr:?}");
    }

    let CommandRunOutcome::Rendered {
        stdout,
        stderr,
        status,
    } = run(&["count"], None)
    else {
        panic!("a missing argument renders a usage error");
    };
    assert_eq!(status, 2);
    assert!(stdout.is_empty(), "{stdout:?}");
    assert!(stderr.contains("Usage: fixture count"), "{stderr:?}");
}

#[test]
fn a_command_proposes_the_capability_with_its_typed_input() {
    assert_eq!(
        run(&["upper", "--text", "hi"], None),
        CommandRunOutcome::Proposed {
            capability: "fixture.upper".parse().expect("valid id"),
            input: json!({"text": "hi"}),
            secret_use: None,
        }
    );
    assert_eq!(
        run(&["upper"], Some("piped")),
        CommandRunOutcome::Proposed {
            capability: "fixture.upper".parse().expect("valid id"),
            input: json!({"text": "piped"}),
            secret_use: None,
        }
    );
}

#[test]
fn a_proposal_carries_its_secret_use() {
    assert_eq!(
        run(&["count", "abc", "--bearer", DRN], None),
        CommandRunOutcome::Proposed {
            capability: "fixture.count".parse().expect("valid id"),
            input: json!({"text": "abc", "limit": 8}),
            secret_use: Some(SecretUseProposal::HttpBearer {
                secret: DRN.parse().expect("canonical DRN"),
            }),
        }
    );
}

#[test]
fn a_provider_usage_error_is_a_failed_run_with_the_usage_code() {
    assert_eq!(
        run(&["upper"], None),
        CommandRunOutcome::Failed {
            error: ComponentFailure {
                code: Code::USAGE.as_str().to_owned(),
                message: "fixture upper: pass --text or pipe input".to_owned(),
            },
        }
    );
}

#[test]
fn a_proposal_for_an_unlisted_capability_is_refused() {
    let CommandRunOutcome::Failed { error } = run(&["stray"], None) else {
        panic!("an unlisted capability cannot be proposed");
    };
    assert_eq!(error.code, SdkFailure::UnknownCapability.code().as_str());
}
