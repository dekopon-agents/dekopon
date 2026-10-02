#![allow(clippy::unwrap_used)]
use std::{
    path::PathBuf,
    time::{Duration, UNIX_EPOCH},
};

fn cli_component() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/providers/cli-probe-provider.wasm")
}

use std::{fmt, io::Write};

use dekopon_provider_sdk::{
    EffectKind, RiskLevel,
    clap::{Parser, Subcommand},
    provider::{
        Capability, Clock, Code, DurableFiles, Failure, Header, Http, Proposal, Provider, Request,
        Response, Stdout, Storage, Usage,
    },
};
use dekopon_provider_sdk_testkit::{
    BrokerHostLimits, Harness, HarnessError, HttpScript, Native, conformance,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Parser)]
struct NoArgs {}

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

fn emit(out: &mut Stdout, value: Result<Value, std::convert::Infallible>) -> Result<(), Gone> {
    let Ok(value) = value;
    writeln!(out, "{value}").map_err(|_closed| Gone)
}

fn stdout_value(output: &dekopon_provider_sdk_testkit::NativeOutput) -> Value {
    assert_eq!(output.status, 0, "{}", output.stderr);
    serde_json::from_slice(&output.stdout).unwrap()
}

struct Cli;
struct Upper;
struct Count;
struct Reverse;
#[derive(Parser)]
#[command(name = "probe")]
struct CliArgs {
    #[command(subcommand)]
    transform: CliTransform,
}
#[derive(Subcommand)]
enum CliTransform {
    Upper,
    Count,
    Reverse,
}
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Text {
    text: String,
}
impl Provider for Cli {
    const ID: &'static str = "cli-probe";
    const COMMAND_WORDS: &'static [&'static str] = &["probe"];
    const DESCRIPTION: &'static str = "typed test of checked cli-probe";
    type Args = CliArgs;
    type Capabilities = (Upper, Count, Reverse);
    fn propose(args: CliArgs, _: bool) -> Result<Proposal<Self>, Usage> {
        let text = Text {
            text: "abc".to_owned(),
        };
        Ok(match args.transform {
            CliTransform::Upper => Proposal::to::<Upper>(text),
            CliTransform::Count => Proposal::to::<Count>(text),
            CliTransform::Reverse => Proposal::to::<Reverse>(text),
        })
    }
}
impl Capability for Upper {
    type Provider = Cli;
    const NAME: &'static str = "upper";
    const DESCRIPTION: &'static str = "Upper-case text";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = Text;
    type Needs = ();
    type Error = Gone;
    fn run(input: Text, (): (), out: &mut Stdout) -> Result<(), Self::Error> {
        emit(out, Ok(json!({"text": input.text.to_uppercase()})))
    }
}

impl Capability for Count {
    type Provider = Cli;
    const NAME: &'static str = "count";
    const DESCRIPTION: &'static str = "Count characters";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = Text;
    type Needs = ();
    type Error = Gone;
    fn run(input: Text, (): (), out: &mut Stdout) -> Result<(), Self::Error> {
        let value = Ok(json!({"characters":input.text.chars().count()}));
        emit(out, value)
    }
}
impl Capability for Reverse {
    type Provider = Cli;
    const NAME: &'static str = "reverse";
    const DESCRIPTION: &'static str = "Reverse text";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = Text;
    type Needs = ();
    type Error = Gone;
    fn run(input: Text, (): (), out: &mut Stdout) -> Result<(), Self::Error> {
        let value = Ok(json!({"text":input.text.chars().rev().collect::<String>()}));
        emit(out, value)
    }
}

#[test]
fn conformance_accepts_checked_cli_probe() {
    conformance::<Cli>(&cli_component()).unwrap();
}

#[test]
fn conformance_refuses_an_inconsistent_component_declaration() {
    let error =
        conformance::<OtherCli>(&cli_component()).expect_err("partial capability list is refused");
    assert!(
        matches!(
            error,
            dekopon_provider_sdk_testkit::ConformanceError::CapabilityIds { .. }
        ),
        "{error:?}"
    );
}

struct MissingWord;
struct MissingUpper;
struct MissingCount;
struct MissingReverse;
impl Provider for MissingWord {
    const ID: &'static str = "cli-probe";
    const COMMAND_WORDS: &'static [&'static str] = &["missing"];
    const DESCRIPTION: &'static str = "word mismatch fixture";
    type Args = CliArgs;
    type Capabilities = (MissingUpper, MissingCount, MissingReverse);
    fn propose(_: CliArgs, _: bool) -> Result<Proposal<Self>, Usage> {
        Ok(Proposal::to::<MissingUpper>(Text {
            text: "abc".to_owned(),
        }))
    }
}
macro_rules! missing_capability {
    ($capability:ident, $name:literal) => {
        impl Capability for $capability {
            type Provider = MissingWord;
            const NAME: &'static str = $name;
            const DESCRIPTION: &'static str = "command mismatch fixture";
            const EFFECT: EffectKind = EffectKind::ReadOnly;
            const RISK: RiskLevel = RiskLevel::Low;
            type Input = Text;
            type Needs = ();
            type Error = Gone;
            fn run(_: Text, (): (), out: &mut Stdout) -> Result<(), Self::Error> {
                emit(out, Ok(json!({})))
            }
        }
    };
}
missing_capability!(MissingUpper, "upper");
missing_capability!(MissingCount, "count");
missing_capability!(MissingReverse, "reverse");

#[test]
fn conformance_refuses_a_declared_word_the_component_does_not_advertise() {
    let error = conformance::<MissingWord>(&cli_component()).expect_err("missing word refused");
    assert!(
        matches!(error, dekopon_provider_sdk_testkit::ConformanceError::HelpUsage { ref word } if word == "missing"),
        "{error:?}"
    );
}

#[test]
fn cached_real_component_and_native_dispatch_agree() {
    let input = json!({"text":"hello"});
    let native = Native::<Cli>::new();
    assert_eq!(
        stdout_value(&native.call("cli-probe.upper", &input.to_string())),
        json!({"text":"HELLO"})
    );
    let component = cli_component();
    conformance::<Cli>(&component).unwrap();
    for _ in 0..2 {
        assert_eq!(
            Harness::<Cli>::get(&component)
                .call("cli-probe.upper", input.clone())
                .unwrap(),
            json!({"text":"HELLO"})
        );
    }
    assert_eq!(Harness::<Cli>::compiled_identities(), 1);
    assert_eq!(
        Harness::<OtherCli>::get(&component)
            .call("cli-probe.upper", input)
            .unwrap(),
        json!({"text":"HELLO"})
    );
    assert_eq!(Harness::<OtherCli>::compiled_identities(), 1);
    assert_eq!(Harness::<Cli>::compiled_identities(), 1);
    let error = Harness::<Cli>::get(&component)
        .host_limits(BrokerHostLimits {
            fuel: 1,
            ..BrokerHostLimits::default()
        })
        .call("cli-probe.upper", json!({"text":"hello"}))
        .expect_err("a cached default-limit registry must not widen a narrowed fuel budget");
    assert!(matches!(error, HarnessError::Host(_)), "{error:?}");
    assert_eq!(Harness::<Cli>::compiled_identities(), 1);

    let temporary = tempfile::tempdir().unwrap();
    let renamed = temporary.path().join("not-cli-probe.wasm");
    std::fs::copy(component, &renamed).unwrap();
    assert_eq!(
        Harness::<Cli>::get(&renamed)
            .call("cli-probe.upper", json!({"text":"hello"}))
            .unwrap(),
        json!({"text":"HELLO"})
    );
    assert_eq!(Harness::<Cli>::compiled_identities(), 2);
}

struct OtherCli;
struct OtherUpper;
impl Provider for OtherCli {
    const ID: &'static str = "cli-probe";
    const COMMAND_WORDS: &'static [&'static str] = &["probe"];
    const DESCRIPTION: &'static str = "a distinct test provider type for the same artifact";
    type Args = NoArgs;
    type Capabilities = (OtherUpper,);
    fn propose(_: NoArgs, _: bool) -> Result<Proposal<Self>, Usage> {
        Ok(Proposal::to::<OtherUpper>(Text {
            text: "abc".to_owned(),
        }))
    }
}
impl Capability for OtherUpper {
    type Provider = OtherCli;
    const NAME: &'static str = "upper";
    const DESCRIPTION: &'static str = "Upper-case text";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = Text;
    type Needs = ();
    type Error = Gone;
    fn run(input: Text, (): (), out: &mut Stdout) -> Result<(), Self::Error> {
        emit(out, Ok(json!({"text": input.text.to_uppercase()})))
    }
}

struct TypedStorage;
struct StorageRun;
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StorageInput {
    mode: Option<String>,
}
impl Provider for TypedStorage {
    const ID: &'static str = "storage-probe";
    const COMMAND_WORDS: &'static [&'static str] = &["storageprobe"];
    const DESCRIPTION: &'static str = "Durable files conformance fixture";
    type Args = NoArgs;
    type Capabilities = (StorageRun,);
    fn propose(_: NoArgs, _: bool) -> Result<Proposal<Self>, Usage> {
        Ok(Proposal::to::<StorageRun>(StorageInput { mode: None }))
    }
}
impl Capability for StorageRun {
    type Provider = TypedStorage;
    const NAME: &'static str = "run";
    const DESCRIPTION: &'static str = "Durable files";
    const EFFECT: EffectKind = EffectKind::LocalWrite;
    const RISK: RiskLevel = RiskLevel::Medium;
    type Input = StorageInput;
    type Needs = Storage<DurableFiles>;
    type Error = Gone;
    fn run(_: StorageInput, _: Storage<DurableFiles>, out: &mut Stdout) -> Result<(), Self::Error> {
        let value = Ok(json!({}));
        emit(out, value)
    }
}
#[test]
fn typed_storage_component_matches_durable_files_imports() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/providers/storage-probe-provider.wasm");
    conformance::<TypedStorage>(path).unwrap();
}

struct TypedClock;
struct ClockNow;
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyInput {}
impl Provider for TypedClock {
    const ID: &'static str = "clock-probe";
    const COMMAND_WORDS: &'static [&'static str] = &["date"];
    const DESCRIPTION: &'static str = "Clock provider fixture";
    type Args = NoArgs;
    type Capabilities = (ClockNow,);
    fn propose(_: NoArgs, _: bool) -> Result<Proposal<Self>, Usage> {
        Ok(Proposal::to::<ClockNow>(EmptyInput {}))
    }
}
impl Capability for ClockNow {
    type Provider = TypedClock;
    const NAME: &'static str = "now";
    const DESCRIPTION: &'static str = "Reads the broker wall clock";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = EmptyInput;
    type Needs = Clock;
    type Error = Gone;
    fn run(_: EmptyInput, clock: Clock, out: &mut Stdout) -> Result<(), Self::Error> {
        let value = Ok(json!({"unixMillis":clock.now_unix_millis()}));
        emit(out, value)
    }
}

#[test]
fn typed_clock_component_matches_imports_and_uses_injected_clock() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/providers/clock-probe-provider.wasm");
    conformance::<TypedClock>(&path).unwrap();
    let instant = UNIX_EPOCH + Duration::from_millis(951_782_400_123);
    let output = Harness::<TypedClock>::get(path)
        .clock(instant)
        .call("clock-probe.now", json!({}))
        .unwrap();
    assert_eq!(
        output,
        json!({"unixMillis":951_782_400_123_u64,"rfc3339":"2000-02-29T00:00:00Z"})
    );
}

struct RawHttp;
struct Fetch;
impl Provider for RawHttp {
    const ID: &'static str = "http-probe";
    const COMMAND_WORDS: &'static [&'static str] = &["httpprobe"];
    const DESCRIPTION: &'static str = "native counterpart for the typed HTTP fixture";
    type Args = NoArgs;
    type Capabilities = (Fetch, ConditionalWrite, Purge);
    fn propose(_: NoArgs, _: bool) -> Result<Proposal<Self>, Usage> {
        Ok(Proposal::to::<Fetch>(UrlInput {
            uri: "https://fixture.example.test/path".to_owned(),
        }))
    }
}
impl Capability for Fetch {
    type Provider = RawHttp;
    const NAME: &'static str = "fetch";
    const DESCRIPTION: &'static str = "Read a scripted response";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = UrlInput;
    type Needs = Http;
    type Error = Gone;
    fn run(input: UrlInput, http: Http, out: &mut Stdout) -> Result<(), Self::Error> {
        let response = http.send(Request::new("GET", input.uri).unwrap()).unwrap();
        let value = Ok(json!({"headerCount": response.headers.len()}));
        emit(out, value)
    }
}

struct ConditionalWrite;
struct Purge;
macro_rules! http_route {
    ($type:ident, $name:literal) => {
        impl Capability for $type {
            type Provider = RawHttp;
            const NAME: &'static str = $name;
            const DESCRIPTION: &'static str = $name;
            const EFFECT: EffectKind = EffectKind::ExternalWrite;
            const RISK: RiskLevel = RiskLevel::High;
            type Input = UrlInput;
            type Needs = Http;
            type Error = Gone;
            fn run(_: UrlInput, _: Http, out: &mut Stdout) -> Result<(), Self::Error> {
                emit(out, Ok(json!({})))
            }
        }
    };
}
http_route!(ConditionalWrite, "conditional-write");
http_route!(Purge, "purge");

#[test]
fn typed_http_component_matches_http_and_asset_imports() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/providers/http-probe-provider.wasm");
    conformance::<RawHttp>(path).unwrap();
}

#[test]
fn scripted_response_headers_match_native_and_typed_component() {
    let response = Response {
        status: 200,
        headers: vec![
            Header::text("content-length", "2").unwrap(),
            Header::text("x-script", "present").unwrap(),
        ],
        body: b"ok".to_vec(),
    };
    let script = HttpScript::new("fixture.example.test", "GET", response.clone());
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/providers/http-probe-provider.wasm");
    let run = Harness::<RawHttp>::get(path).http(script);
    let origin = run.origin().unwrap();
    let authority = origin.trim_start_matches("https://").to_owned();
    let input = json!({"uri": format!("{origin}/resource")});
    let native_script = HttpScript::new(&authority, "GET", response);
    let native = Native::<RawHttp>::new().http(native_script);
    let output = stdout_value(&native.call("http-probe.fetch", &input.to_string()));
    assert_eq!(output["headerCount"], 2);
    let (real, stdout) = run.call_full("http-probe.fetch", input).unwrap();
    let real_output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(real_output["headerCount"], output["headerCount"]);
    assert_eq!(real.http_calls.len(), 1);
    assert_eq!(real.http_calls[0].authority, authority);
}

#[test]
fn scripted_headers_cannot_inject_response_delimiters_or_framing() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/providers/http-probe-provider.wasm");
    for header in [
        Header {
            name: "x-script".to_owned(),
            value: b"ok\r\nInjected: yes".to_vec(),
        },
        Header::text("content-length", "999").unwrap(),
        Header::text("transfer-encoding", "chunked").unwrap(),
    ] {
        let run = Harness::<RawHttp>::get(&path).http(HttpScript::new(
            "fixture.example.test",
            "GET",
            Response {
                status: 200,
                headers: vec![header],
                body: b"ok".to_vec(),
            },
        ));
        assert!(run.origin().is_none());
        assert!(matches!(
            run.call(
                "http-probe.fetch",
                json!({"uri":"https://fixture.example.test/path"})
            ),
            Err(HarnessError::Fixture(_))
        ));
    }
}

struct Fake;
struct Read;
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UrlInput {
    uri: String,
}
impl Provider for Fake {
    const ID: &'static str = "fake-imports";
    const COMMAND_WORDS: &'static [&'static str] = &["fake"];
    const DESCRIPTION: &'static str = "native fake imports";
    type Args = NoArgs;
    type Capabilities = (Read,);
    fn propose(_: NoArgs, _: bool) -> Result<Proposal<Self>, Usage> {
        Ok(Proposal::to::<Read>(UrlInput {
            uri: "https://fixture.example.test/path".to_owned(),
        }))
    }
}
impl Capability for Read {
    type Provider = Fake;
    const NAME: &'static str = "read";
    const DESCRIPTION: &'static str = "Read clock and HTTP";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = UrlInput;
    type Needs = (Clock, Http);
    type Error = Gone;
    fn run(
        input: UrlInput,
        (clock, http): (Clock, Http),
        out: &mut Stdout,
    ) -> Result<(), Self::Error> {
        let request = Request::new("GET", input.uri).unwrap();
        let response = http.send(request).unwrap();
        let value = Ok(
            json!({"clock":clock.now_unix_millis(),"status":response.status,"body":String::from_utf8(response.body).unwrap()}),
        );
        emit(out, value)
    }
}

#[test]
fn native_fake_records_http_and_fixes_guest_clock() {
    let instant = UNIX_EPOCH + Duration::from_millis(951_782_400_123);
    let native = Native::<Fake>::new().clock(instant).http(HttpScript::new(
        "fixture.example.test",
        "GET",
        dekopon_provider_sdk::provider::Response {
            status: 201,
            headers: Vec::new(),
            body: b"ok".to_vec(),
        },
    ));
    assert_eq!(
        stdout_value(&native.call(
            "fake-imports.read",
            r#"{"uri":"https://fixture.example.test/path"}"#
        )),
        json!({"clock":951_782_400_123_u64,"status":201,"body":"ok"})
    );
    assert_eq!(native.requests().len(), 1);
    assert_eq!(
        native.requests()[0].uri,
        "https://fixture.example.test/path"
    );
}
