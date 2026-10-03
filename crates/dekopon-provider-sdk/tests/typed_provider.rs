use std::cell::RefCell;
use std::convert::Infallible;
use std::fmt;
use std::io::{self, Write};
use std::marker::PhantomData;
use std::rc::Rc;

use clap::{Parser, Subcommand};
use dekopon_provider_sdk::provider::{
    Capability, Code, Failure, ManifestError, NativeExit, NativeStdio, Proposal, Provider,
    SchemaFault, SdkFailure, Stdout, Usage, command, invoke_native, manifest, stdin,
};
use dekopon_provider_sdk::{
    CommandRunOutcome, ComponentFailure, EffectKind, RiskLevel, SecretUseProposal,
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

    fn propose(args: Args, stdin_piped: bool) -> Result<Proposal<Self>, Usage> {
        match args.verb {
            Verb::Upper { text } => {
                if text.is_none() && !stdin_piped {
                    return Err(Usage::new("fixture upper: pass --text or pipe input"));
                }
                Ok(Proposal::to::<Upper>(UpperInput { text }))
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
            Verb::Stray => Ok(Proposal::to::<Stray>(UpperInput { text: None })),
        }
    }
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpperInput {
    /// The text to transform; absent to transform piped lines
    #[serde(default)]
    text: Option<String>,
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

enum CountFailure {
    TooLong,
    EmptyLine,
    NothingPiped,
    Gone,
}

impl fmt::Display for CountFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TooLong => "the text is longer than the limit",
            Self::EmptyLine => "fixture upper: a piped line is empty",
            Self::NothingPiped => "fixture upper: nothing was piped in",
            Self::Gone => "fixture upper: stdout is closed",
        })
    }
}

impl From<io::Error> for CountFailure {
    fn from(_: io::Error) -> Self {
        Self::Gone
    }
}

const TOO_LONG: Code = Code::new("too-long").exiting(3);

impl Failure for CountFailure {
    fn code(&self) -> Code {
        match self {
            Self::TooLong => TOO_LONG,
            Self::EmptyLine | Self::NothingPiped => Code::USAGE,
            Self::Gone => Code::new("gone"),
        }
    }
}

struct Gone;

impl fmt::Display for Gone {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("stdout is closed")
    }
}

impl Failure for Gone {
    fn code(&self) -> Code {
        Code::new("gone")
    }
}

impl From<io::Error> for Gone {
    fn from(_: io::Error) -> Self {
        Self
    }
}

struct Upper;

impl Capability for Upper {
    type Provider = Fixture;
    const NAME: &'static str = "upper";
    const DESCRIPTION: &'static str = "Upper-cases text";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = UpperInput;
    type Needs = ();
    type Error = CountFailure;

    fn run(input: UpperInput, (): (), out: &mut Stdout) -> Result<(), CountFailure> {
        if let Some(text) = input.text {
            writeln!(out, "{}", text.to_uppercase())?;
            return Ok(());
        }
        let mut lines = 0;
        for line in stdin().ok_or(CountFailure::NothingPiped)?.lines() {
            let line = line?;
            if line.is_empty() {
                return Err(CountFailure::EmptyLine);
            }
            writeln!(out, "{}", line.to_uppercase())?;
            lines += 1;
        }
        if lines == 0 {
            return Err(CountFailure::NothingPiped);
        }
        Ok(())
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
    type Error = CountFailure;

    fn run(input: CountInput, (): (), out: &mut Stdout) -> Result<(), CountFailure> {
        let count = u32::try_from(input.text.chars().count())
            .ok()
            .filter(|count| *count <= input.limit)
            .ok_or(CountFailure::TooLong)?;
        writeln!(out, "{count}")?;
        Ok(())
    }
}

struct Stray;

impl Capability for Stray {
    type Provider = Fixture;
    const NAME: &'static str = "stray";
    const DESCRIPTION: &'static str = "Is not listed by its provider";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = UpperInput;
    type Needs = ();
    type Error = CountFailure;

    fn run(_: UpperInput, (): (), _: &mut Stdout) -> Result<(), CountFailure> {
        Ok(())
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
    fn propose(_: ClockArgs, _: bool) -> Result<Proposal<Self>, Usage> {
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
    type Error = Gone;
    fn run(_: ClockInput, clock: Self::Needs, out: &mut Stdout) -> Result<(), Gone> {
        writeln!(out, "{}", clock.now_unix_millis())?;
        Ok(())
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
        fn now_nanos(&mut self) -> u64 {
            0
        }
        fn fill_random(
            &mut self,
            out: &mut [u8],
        ) -> Result<(), dekopon_provider_sdk::random::RandomError> {
            out.fill(0xa5);
            Ok(())
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
        fn open(
            &mut self,
            _: dekopon_provider_sdk::provider::Request,
        ) -> Result<
            dekopon_provider_sdk::provider::OpenedResponse,
            dekopon_provider_sdk::provider::HttpError,
        > {
            unreachable!()
        }
    }
    assert_eq!(
        with_port(Fake, || call::<ClockFixture>("clock-fixture.read", "{}")).stdout,
        "123\n"
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
            fn propose(_: ClockArgs, _: bool) -> Result<Proposal<Self>, Usage> {
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
            type Error = Infallible;
            fn run(_: ClockInput, _: Self::Needs, _: &mut Stdout) -> Result<(), Infallible> {
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
            fn propose(_: ClockArgs, _: bool) -> Result<Proposal<Self>, Usage> {
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
            type Error = Gone;
            fn run(_: ClockInput, needs: Self::Needs, out: &mut Stdout) -> Result<(), Gone> {
                writeln!(out, "{}", ($body)(needs))?;
                Ok(())
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
        fn now_nanos(&mut self) -> u64 {
            0
        }
        fn fill_random(
            &mut self,
            out: &mut [u8],
        ) -> Result<(), dekopon_provider_sdk::random::RandomError> {
            out.fill(0xa5);
            Ok(())
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
        fn open(
            &mut self,
            _: dekopon_provider_sdk::provider::Request,
        ) -> Result<
            dekopon_provider_sdk::provider::OpenedResponse,
            dekopon_provider_sdk::provider::HttpError,
        > {
            unreachable!()
        }
    }
    assert_eq!(
        with_port(Fake, || call::<HttpFixture>("native-http.send", "{}")).stdout,
        "202\n"
    );
    assert_eq!(
        with_port(Fake, || call::<HttpClockFixture>("native-http.send", "{}")).stdout,
        "325\n"
    );
}

#[test]
fn native_storage_and_assets_require_the_real_component_harness_even_in_a_tuple() {
    fn requires_harness<P: Provider>() {
        let exit = call::<P>("native-import.read", "{}");
        assert_eq!(exit.status, 1);
        assert_eq!(
            exit.stderr,
            "this capability needs the component harness (Harness<P>)\n"
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
        fn now_nanos(&mut self) -> u64 {
            unreachable!()
        }
        fn fill_random(
            &mut self,
            _: &mut [u8],
        ) -> Result<(), dekopon_provider_sdk::random::RandomError> {
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
        fn open(
            &mut self,
            _: dekopon_provider_sdk::provider::Request,
        ) -> Result<
            dekopon_provider_sdk::provider::OpenedResponse,
            dekopon_provider_sdk::provider::HttpError,
        > {
            unreachable!()
        }
    }
    with_port(SettingsFake, requires_harness::<TupleFixture>);
}

fn run(words: &[&str], stdin_piped: bool) -> CommandRunOutcome {
    let argv: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
    command::<Fixture>(&argv, stdin_piped)
}

#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Closed;

impl Write for Closed {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::BrokenPipe.into())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct Exit {
    status: u8,
    stdout: String,
    stderr: String,
}

fn call_with<P: Provider>(capability: &str, input: &str, piped: Option<&'static str>) -> Exit {
    let captured = Captured::default();
    let NativeExit { status, stderr } = invoke_native::<P>(
        capability,
        input,
        NativeStdio {
            stdin: piped.map(|text| Box::new(text.as_bytes()) as Box<dyn io::Read>),
            stdout: Box::new(captured.clone()),
        },
    );
    let stdout = String::from_utf8(captured.0.take()).expect("UTF-8 stdout");
    Exit {
        status,
        stdout,
        stderr,
    }
}

fn call<P: Provider>(capability: &str, input: &str) -> Exit {
    call_with::<P>(capability, input, None)
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
fn a_call_dispatches_to_the_named_capability_and_writes_its_stdout() {
    let exit = call::<Fixture>("fixture.upper", r#"{"text":"hello"}"#);
    assert_eq!(
        (exit.status, exit.stdout.as_str(), exit.stderr.as_str()),
        (0, "HELLO\n", "")
    );
    let exit = call::<Fixture>("fixture.count", r#"{"text":"hello","limit":5}"#);
    assert_eq!((exit.status, exit.stdout.as_str()), (0, "5\n"));
}

#[test]
fn piped_lines_stream_to_stdout_and_a_usage_failure_exits_2_with_its_message_on_stderr() {
    let exit = call_with::<Fixture>("fixture.upper", "{}", Some("a\nb\n"));
    assert_eq!(
        (exit.status, exit.stdout.as_str(), exit.stderr.as_str()),
        (0, "A\nB\n", "")
    );

    let exit = call_with::<Fixture>("fixture.upper", "{}", Some("a\n\nb\n"));
    assert_eq!(exit.status, 2);
    assert_eq!(exit.stdout, "A\n");
    assert_eq!(exit.stderr, "fixture upper: a piped line is empty\n");
}

#[test]
fn zero_byte_piped_input_differs_from_nothing_piped_and_both_are_usage_errors() {
    let empty = call_with::<Fixture>("fixture.upper", "{}", Some(""));
    let absent = call::<Fixture>("fixture.upper", "{}");
    assert_eq!(
        (empty.status, empty.stderr.as_str()),
        (2, "fixture upper: nothing was piped in\n")
    );
    assert_eq!(
        (absent.status, absent.stderr.as_str()),
        (2, "fixture upper: nothing was piped in\n")
    );
    assert!(
        invoke_native::<Fixture>(
            "fixture.stray",
            "{}",
            NativeStdio {
                stdin: None,
                stdout: Box::new(io::sink()),
            },
        )
        .status
            != 0
    );
}

#[test]
fn a_write_to_a_closed_stdout_exits_141_whatever_the_capability_returns() {
    let exit = invoke_native::<Fixture>(
        "fixture.upper",
        "{}",
        NativeStdio {
            stdin: Some(Box::new(&b"a\nb\n"[..])),
            stdout: Box::new(Closed),
        },
    );
    assert_eq!(
        exit,
        NativeExit {
            status: 141,
            stderr: String::new()
        }
    );
}

#[test]
fn a_capability_failure_exits_with_its_code_s_status_and_display_message() {
    let exit = call::<Fixture>("fixture.count", r#"{"text":"hello"}"#);
    assert_eq!(exit.status, TOO_LONG.status().get());
    assert_eq!(exit.status, 3);
    assert_eq!(exit.stdout, "");
    assert_eq!(exit.stderr, format!("{}\n", CountFailure::TooLong));
    assert_eq!(Code::new("default").status().get(), 1);
    assert_eq!(Code::USAGE.status().get(), 2);
}

#[test]
fn an_unknown_capability_fails_with_the_sdk_status_and_static_message() {
    for capability in ["fixture.stray", "other.upper", "fixture", "Not Valid"] {
        let exit = call::<Fixture>(capability, r#"{"text":"x"}"#);
        assert_eq!(
            exit.status,
            SdkFailure::UnknownCapability.code().status().get()
        );
        assert_eq!(exit.stderr, format!("{}\n", SdkFailure::UnknownCapability));
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
        let exit = call::<Fixture>("fixture.count", input);
        assert_eq!(exit.status, 2);
        assert_eq!(exit.stderr, format!("{}\n", SdkFailure::InvalidInput));
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
    } = run(&["--help"], false)
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
        let CommandRunOutcome::Rendered { stdout, stderr, .. } = run(words, false) else {
            panic!("{words:?} renders");
        };
        assert!(!stdout.contains('\u{1b}'), "{words:?}: {stdout:?}");
        assert!(!stderr.contains('\u{1b}'), "{words:?}: {stderr:?}");
    }

    let CommandRunOutcome::Rendered {
        stdout,
        stderr,
        status,
    } = run(&["count"], false)
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
        run(&["upper", "--text", "hi"], false),
        CommandRunOutcome::Proposed {
            capability: "fixture.upper".parse().expect("valid id"),
            input: json!({"text": "hi"}),
            secret_use: None,
        }
    );
    assert_eq!(
        run(&["upper"], true),
        CommandRunOutcome::Proposed {
            capability: "fixture.upper".parse().expect("valid id"),
            input: json!({"text": null}),
            secret_use: None,
        }
    );
}

#[test]
fn a_proposal_carries_its_secret_use() {
    assert_eq!(
        run(&["count", "abc", "--bearer", DRN], false),
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
        run(&["upper"], false),
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
    let CommandRunOutcome::Failed { error } = run(&["stray"], false) else {
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

    fn propose(_: NoArgs, _: bool) -> Result<Proposal<Self>, Usage> {
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
    type Error = Infallible;

    fn run(_: I, (): (), _: &mut Stdout) -> Result<(), Infallible> {
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
