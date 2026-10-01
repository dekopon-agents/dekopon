use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomData;

use clap::{Parser, Subcommand};
use dekopon_provider_sdk::provider::{
    Capability, Code, Failure, ManifestError, Proposal, Provider, SchemaFault, SdkFailure, Usage,
    call, command, manifest,
};
use dekopon_provider_sdk::{
    CommandRunOutcome, ComponentFailure, ComponentResponse, EffectKind, RiskLevel,
    SecretUseProposal,
};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
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

struct ClockFixture;

#[derive(Parser)]
struct ClockArgs {}

impl Provider for ClockFixture {
    const ID: &'static str = "clock-fixture";
    const COMMAND_WORDS: &'static [&'static str] = &["clock-fixture"];
    const DESCRIPTION: &'static str = "Clock fixture";
    type Args = ClockArgs;
    type Capabilities = (ClockRead,);
    fn propose(_: ClockArgs, _: Option<&str>) -> Result<Proposal<Self>, Usage> {
        Ok(Proposal::to::<ClockRead>(ClockInput {}))
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ClockInput {}

struct ClockRead;
impl Capability for ClockRead {
    type Provider = ClockFixture;
    const NAME: &'static str = "read";
    const DESCRIPTION: &'static str = "Reads the clock";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = ClockInput;
    type Needs = dekopon_provider_sdk::provider::Clock;
    type Output = u64;
    type Error = Infallible;
    fn run(_: ClockInput, clock: Self::Needs) -> Result<u64, Infallible> {
        Ok(clock.now_unix_millis())
    }
}

#[test]
fn native_fake_port_reaches_typed_dispatch_and_restores_after_return() {
    use dekopon_provider_sdk::provider::{Port, with_port};
    struct Fake;
    impl Port for Fake {
        fn now_unix_millis(&mut self) -> u64 {
            123
        }
        fn settings(&mut self) -> Option<String> {
            None
        }
        fn send(
            &mut self,
            _: dekopon_provider_sdk::provider::Request,
        ) -> Result<
            dekopon_provider_sdk::provider::Response,
            dekopon_provider_sdk::provider::HttpError,
        > {
            unreachable!()
        }
        fn stream(
            &mut self,
            _: dekopon_provider_sdk::provider::StreamedRequest<'_>,
        ) -> Result<
            dekopon_provider_sdk::provider::StreamedResponse,
            dekopon_provider_sdk::provider::HttpError,
        > {
            unreachable!()
        }
    }
    assert_eq!(
        with_port(Fake, || call::<ClockFixture>("clock-fixture.read", "{}")),
        ComponentResponse::Succeeded { output: json!(123) }
    );
}

macro_rules! refused_native_import {
    ($provider:ident, $capability:ident, $needs:ty) => {
        struct $provider;
        struct $capability;
        impl Provider for $provider {
            const ID: &'static str = "native-import";
            const COMMAND_WORDS: &'static [&'static str] = &["native-import"];
            const DESCRIPTION: &'static str = "Native import fixture";
            type Args = ClockArgs;
            type Capabilities = ($capability,);
            fn propose(_: ClockArgs, _: Option<&str>) -> Result<Proposal<Self>, Usage> {
                Ok(Proposal::to::<$capability>(ClockInput {}))
            }
        }
        impl Capability for $capability {
            type Provider = $provider;
            const NAME: &'static str = "read";
            const DESCRIPTION: &'static str = "Import request";
            const EFFECT: EffectKind = EffectKind::ReadOnly;
            const RISK: RiskLevel = RiskLevel::Low;
            type Input = ClockInput;
            type Needs = $needs;
            type Output = ();
            type Error = Infallible;
            fn run(_: ClockInput, _: Self::Needs) -> Result<(), Infallible> {
                panic!("the unsupported native import must be rejected before run")
            }
        }
    };
}
refused_native_import!(
    JsonlFixture,
    JsonlCall,
    dekopon_provider_sdk::provider::Storage<dekopon_provider_sdk::provider::Jsonl>
);
refused_native_import!(
    DurableFixture,
    DurableCall,
    dekopon_provider_sdk::provider::Storage<dekopon_provider_sdk::provider::DurableFiles>
);
refused_native_import!(
    AssetsFixture,
    AssetsCall,
    dekopon_provider_sdk::provider::Assets
);
refused_native_import!(
    HttpAssetsFixture,
    HttpAssetsCall,
    (
        dekopon_provider_sdk::provider::Http,
        dekopon_provider_sdk::provider::Assets
    )
);
refused_native_import!(
    TupleFixture,
    TupleCall,
    (
        dekopon_provider_sdk::provider::Settings<String>,
        dekopon_provider_sdk::provider::Assets
    )
);

struct HttpFixture;
struct HttpCall;
struct HttpClockFixture;
struct HttpClockCall;

macro_rules! http_provider {
    ($provider:ident, $capability:ident, $needs:ty, $body:expr) => {
        impl Provider for $provider {
            const ID: &'static str = "native-http";
            const COMMAND_WORDS: &'static [&'static str] = &["native-http"];
            const DESCRIPTION: &'static str = "Native HTTP fixture";
            type Args = ClockArgs;
            type Capabilities = ($capability,);
            fn propose(_: ClockArgs, _: Option<&str>) -> Result<Proposal<Self>, Usage> {
                Ok(Proposal::to::<$capability>(ClockInput {}))
            }
        }
        impl Capability for $capability {
            type Provider = $provider;
            const NAME: &'static str = "send";
            const DESCRIPTION: &'static str = "Sends a request";
            const EFFECT: EffectKind = EffectKind::ReadOnly;
            const RISK: RiskLevel = RiskLevel::Low;
            type Input = ClockInput;
            type Needs = $needs;
            type Output = u64;
            type Error = Infallible;
            fn run(_: ClockInput, needs: Self::Needs) -> Result<u64, Infallible> {
                Ok(($body)(needs))
            }
        }
    };
}

fn fake_http_send(http: dekopon_provider_sdk::provider::Http) -> u64 {
    let request = dekopon_provider_sdk::provider::Request::new("GET", "https://example.test/")
        .expect("valid request");
    u64::from(http.send(request).expect("fake port responds").status)
}
http_provider!(
    HttpFixture,
    HttpCall,
    dekopon_provider_sdk::provider::Http,
    fake_http_send
);
http_provider!(
    HttpClockFixture,
    HttpClockCall,
    (
        dekopon_provider_sdk::provider::Http,
        dekopon_provider_sdk::provider::Clock
    ),
    |(http, clock): (
        dekopon_provider_sdk::provider::Http,
        dekopon_provider_sdk::provider::Clock
    )| { fake_http_send(http) + clock.now_unix_millis() }
);

#[test]
fn native_http_and_http_clock_tuple_reach_the_fake_port() {
    use dekopon_provider_sdk::provider::{Port, with_port};
    struct Fake;
    impl Port for Fake {
        fn now_unix_millis(&mut self) -> u64 {
            123
        }
        fn settings(&mut self) -> Option<String> {
            None
        }
        fn send(
            &mut self,
            request: dekopon_provider_sdk::provider::Request,
        ) -> Result<
            dekopon_provider_sdk::provider::Response,
            dekopon_provider_sdk::provider::HttpError,
        > {
            assert_eq!(request.uri, "https://example.test/");
            Ok(dekopon_provider_sdk::provider::Response {
                status: 202,
                headers: vec![],
                body: vec![],
            })
        }
        fn stream(
            &mut self,
            _: dekopon_provider_sdk::provider::StreamedRequest<'_>,
        ) -> Result<
            dekopon_provider_sdk::provider::StreamedResponse,
            dekopon_provider_sdk::provider::HttpError,
        > {
            unreachable!()
        }
    }
    assert_eq!(
        with_port(Fake, || call::<HttpFixture>("native-http.send", "{}")),
        ComponentResponse::Succeeded { output: json!(202) }
    );
    assert_eq!(
        with_port(Fake, || call::<HttpClockFixture>("native-http.send", "{}")),
        ComponentResponse::Succeeded { output: json!(325) }
    );
}

#[test]
fn native_storage_and_assets_require_the_real_component_harness_even_in_a_tuple() {
    fn requires_harness<P: Provider>() {
        let error = failed(call::<P>("native-import.read", "{}"));
        assert_eq!(
            error.code,
            SdkFailure::ComponentHarnessRequired.code().as_str()
        );
        assert_eq!(
            error.message,
            "this capability needs the component harness (Harness<P>)"
        );
    }
    requires_harness::<JsonlFixture>();
    requires_harness::<DurableFixture>();
    requires_harness::<AssetsFixture>();
    requires_harness::<HttpAssetsFixture>();
    use dekopon_provider_sdk::provider::{Port, with_port};
    struct SettingsFake;
    impl Port for SettingsFake {
        fn now_unix_millis(&mut self) -> u64 {
            unreachable!()
        }
        fn settings(&mut self) -> Option<String> {
            Some("\"valid\"".to_owned())
        }
        fn send(
            &mut self,
            _: dekopon_provider_sdk::provider::Request,
        ) -> Result<
            dekopon_provider_sdk::provider::Response,
            dekopon_provider_sdk::provider::HttpError,
        > {
            unreachable!()
        }
        fn stream(
            &mut self,
            _: dekopon_provider_sdk::provider::StreamedRequest<'_>,
        ) -> Result<
            dekopon_provider_sdk::provider::StreamedResponse,
            dekopon_provider_sdk::provider::HttpError,
        > {
            unreachable!()
        }
    }
    with_port(SettingsFake, requires_harness::<TupleFixture>);
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

#[derive(Parser)]
#[command(name = "faulty")]
struct NoArgs;

struct Faulty<I>(PhantomData<I>);

impl<I: Faulted> Provider for Faulty<I> {
    const ID: &'static str = "faulty";
    const COMMAND_WORDS: &'static [&'static str] = &["faulty"];
    const DESCRIPTION: &'static str = "Declares an input schema the SDK refuses";
    type Args = NoArgs;
    type Capabilities = (Take<I>,);

    fn propose(_: NoArgs, _: Option<&str>) -> Result<Proposal<Self>, Usage> {
        Err(Usage::new("faulty takes no proposal"))
    }
}

trait Faulted: DeserializeOwned + Serialize + JsonSchema + 'static {}

#[derive(Deserialize, Serialize, JsonSchema)]
struct Open {
    text: String,
}

impl Faulted for Open {}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Tree {
    children: Vec<Tree>,
}

impl Faulted for Tree {}

struct Take<I>(PhantomData<I>);

impl<I: Faulted> Capability for Take<I> {
    type Provider = Faulty<I>;
    const NAME: &'static str = "take";
    const DESCRIPTION: &'static str = "Takes the input";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = I;
    type Needs = ();
    type Output = ();
    type Error = Infallible;

    fn run(_: I, (): ()) -> Result<(), Infallible> {
        Ok(())
    }
}

#[test]
fn a_manifest_refuses_an_input_schema_that_is_open_or_holds_a_reference() {
    assert!(matches!(
        manifest::<Faulty<Open>>(),
        Err(ManifestError::Schema {
            capability: "take",
            fault: SchemaFault::Open
        })
    ));
    assert!(matches!(
        manifest::<Faulty<Tree>>(),
        Err(ManifestError::Schema {
            capability: "take",
            fault: SchemaFault::Reference
        })
    ));
}

#[derive(Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Link {
    #[serde(rename = "$ref")]
    target: String,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Linked {
    #[serde(default)]
    link: Link,
}

impl Faulted for Linked {}

#[test]
fn a_ref_named_property_or_default_is_data_not_a_reference() {
    let manifest = manifest::<Faulty<Linked>>().expect("a closed inline schema is published");
    let link = &manifest.capabilities[0].input_schema["properties"]["link"];
    assert_eq!(link["default"], json!({"$ref": ""}));
    assert_eq!(link["properties"]["$ref"]["type"], "string");
}
