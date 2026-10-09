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
        Capability, Clock, Code, DurableFiles, Failure, Header, Http, Monotonic, Proposal,
        Provider, Random, Request, Response, Settings, Stdout, Storage, Usage, endpoint::Base,
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

fn component_stdout(output: dekopon_provider_sdk_testkit::ComponentOutput) -> Value {
    assert_eq!(output.status, 0, "{}", output.stderr);
    assert!(output.stdout.ends_with(b"\n"));
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
fn real_component_captures_piped_bytes_and_status() {
    let input = json!({"text":"","piped":true});
    let result = Harness::<Cli>::get(cli_component())
        .stdin(b"piped bytes\n".to_vec())
        .call("cli-probe.upper", input.clone())
        .unwrap();
    assert_eq!(result.status, 0);
    assert_eq!(result.stdout, b"{\"text\":\"PIPED BYTES\\n\"}\n");
    assert_eq!(result.stderr, "");
    let empty = Harness::<Cli>::get(cli_component())
        .stdin(Vec::new())
        .call("cli-probe.upper", input)
        .unwrap();
    assert_eq!(empty.status, 2);
    assert!(empty.stdout.is_empty());
    assert_eq!(
        empty.stderr,
        "probe: piped input is empty, too large or unavailable\n"
    );
}

#[test]
fn closed_real_stdout_reader_terminates_producer_early() {
    let component = cli_component();
    let result = Harness::<Cli>::get(&component)
        .host_limits(BrokerHostLimits {
            max_timeout: Duration::from_secs(2),
            ..BrokerHostLimits::default()
        })
        .close_stdout_after(0)
        .call("cli-probe.upper", json!({"text":"hello"}))
        .unwrap();
    assert_eq!(result.status, 141, "{}", result.stderr);
    assert!(result.stdout.is_empty());
    assert_eq!(Harness::<Cli>::compiled_identities(), 1);
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
            component_stdout(
                Harness::<Cli>::get(&component)
                    .call("cli-probe.upper", input.clone())
                    .unwrap()
            ),
            json!({"text":"HELLO"})
        );
    }
    assert_eq!(Harness::<Cli>::compiled_identities(), 1);
    assert_eq!(
        component_stdout(
            Harness::<OtherCli>::get(&component)
                .call("cli-probe.upper", input)
                .unwrap()
        ),
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
        component_stdout(
            Harness::<Cli>::get(&renamed)
                .call("cli-probe.upper", json!({"text":"hello"}))
                .unwrap()
        ),
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

enum ClockNeedError {
    Output,
}
impl fmt::Display for ClockNeedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Output => f.write_str("stdout is closed"),
        }
    }
}
impl Failure for ClockNeedError {
    fn code(&self) -> Code {
        match self {
            Self::Output => Code::new("output-closed"),
        }
    }
}
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyInput {
    #[serde(default)]
    services: bool,
}
impl Provider for TypedClock {
    const ID: &'static str = "clock-probe";
    const COMMAND_WORDS: &'static [&'static str] = &["date"];
    const DESCRIPTION: &'static str = "Clock provider fixture";
    type Args = NoArgs;
    type Capabilities = (ClockNow,);
    fn propose(_: NoArgs, _: bool) -> Result<Proposal<Self>, Usage> {
        Ok(Proposal::to::<ClockNow>(EmptyInput { services: false }))
    }
}
impl Capability for ClockNow {
    type Provider = TypedClock;
    const NAME: &'static str = "now";
    const DESCRIPTION: &'static str = "Reads the broker wall clock";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = EmptyInput;
    type Needs = (Clock, Monotonic, Random);
    type Error = ClockNeedError;
    fn run(
        input: EmptyInput,
        (clock, monotonic, random): Self::Needs,
        out: &mut Stdout,
    ) -> Result<(), Self::Error> {
        let mut value = json!({"unixMillis":clock.now_unix_millis()});
        if input.services {
            value["monotonicNanos"] = json!(monotonic.now_nanos());
            let mut bytes = [0; 8];
            random.fill(&mut bytes);
            value["entropyBytes"] = json!(bytes.len());
            value["entropyChecksum"] =
                json!(bytes.iter().map(|byte| u64::from(*byte)).sum::<u64>());
        }
        writeln!(out, "{value}").map_err(|_closed| ClockNeedError::Output)
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
        component_stdout(output),
        json!({"unixMillis":951_782_400_123_u64,"rfc3339":"2000-02-29T00:00:00Z"})
    );
}

#[test]
fn typed_clock_component_reads_new_services_during_invoke() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/providers/clock-probe-provider.wasm");
    let output = Harness::<TypedClock>::get(path)
        .call("clock-probe.now", json!({"services":true}))
        .unwrap();
    let value = component_stdout(output);
    assert!(value["monotonicNanos"].as_u64().is_some());
    assert_eq!(value["entropyBytes"], 8);
    assert!(value.get("entropy").is_none());
}

#[test]
fn native_monotonic_and_entropy_are_injected_without_guest_mode() {
    let fixed = UNIX_EPOCH + Duration::from_millis(951_782_400_123);
    let output = Native::<TypedClock>::new()
        .clock(fixed)
        .monotonic(42)
        .entropy([7; 8])
        .call("clock-probe.now", r#"{"services":true}"#);
    let value = stdout_value(&output);
    assert_eq!(value["unixMillis"], 951_782_400_123_u64);
    assert_eq!(value["monotonicNanos"], 42);
    assert_eq!(value["entropyBytes"], 8);
    assert_eq!(value["entropyChecksum"], 56);
}

#[test]
#[should_panic(expected = "scripted entropy exhausted: read wants 8 bytes, 4 remain")]
fn native_entropy_exhaustion_panics_naming_the_shortfall() {
    let _output = Native::<TypedClock>::new()
        .entropy([7; 4])
        .call("clock-probe.now", r#"{"services":true}"#);
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
    let real = run.call("http-probe.fetch", input).unwrap();
    let real_output: serde_json::Value = serde_json::from_slice(&real.stdout).unwrap();
    assert_eq!(real.status, 0, "{}", real.stderr);
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

struct Vendor;
struct Search;
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Endpoint {
    base_url: Option<Base>,
}
const VENDOR_BASE: Base = Base::from_static("https://api.vendor.example");
impl Provider for Vendor {
    const ID: &'static str = "vendor";
    const COMMAND_WORDS: &'static [&'static str] = &["vendor"];
    const DESCRIPTION: &'static str = "a provider with an owner-overridable origin";
    type Args = NoArgs;
    type Capabilities = (Search,);
    fn propose(_: NoArgs, _: bool) -> Result<Proposal<Self>, Usage> {
        Ok(Proposal::to::<Search>(Text {
            text: "rust".to_owned(),
        }))
    }
}
impl Capability for Search {
    type Provider = Vendor;
    const NAME: &'static str = "search";
    const DESCRIPTION: &'static str = "Search the vendor";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = Text;
    type Needs = (Settings<Endpoint>, Http);
    type Error = Gone;
    fn run(
        input: Text,
        (settings, http): (Settings<Endpoint>, Http),
        out: &mut Stdout,
    ) -> Result<(), Self::Error> {
        let base = settings.into_inner().base_url.unwrap_or(VENDOR_BASE);
        let uri = base
            .join(&format!("/search?q={}", query_value(&input.text)))
            .unwrap();
        let response = http.send(Request::new("GET", uri).unwrap()).unwrap();
        emit(out, Ok(json!({"status": response.status})))
    }
}

fn query_value(text: &str) -> String {
    text.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

#[test]
fn a_base_url_setting_overrides_the_vendor_origin_and_keeps_its_prefix() {
    let ok = Response {
        status: 200,
        headers: Vec::new(),
        body: Vec::new(),
    };
    let vendor =
        Native::<Vendor>::new().http(HttpScript::new("api.vendor.example", "GET", ok.clone()));
    assert_eq!(
        stdout_value(&vendor.call("vendor.search", r#"{"text":"rust"}"#)),
        json!({"status": 200})
    );
    let fixture = Native::<Vendor>::new()
        .settings(json!({"baseUrl": "https://fixture.example.test/vendor/"}))
        .http(HttpScript::new("fixture.example.test", "GET", ok));
    assert_eq!(
        stdout_value(&fixture.call("vendor.search", r#"{"text":"rust & wasm=1/é"}"#)),
        json!({"status": 200})
    );
    let uris = |native: &Native<Vendor>| {
        native
            .requests()
            .into_iter()
            .map(|request| request.uri)
            .collect::<Vec<_>>()
    };
    assert_eq!(uris(&vendor), ["https://api.vendor.example/search?q=rust"]);
    assert_eq!(
        uris(&fixture),
        ["https://fixture.example.test/vendor/search?q=rust%20%26%20wasm%3D1%2F%C3%A9"]
    );
    let invalid = Native::<Vendor>::new()
        .settings(json!({"baseUrl": "https://fixture.example.test/vendor?x=1"}))
        .http(HttpScript::new(
            "api.vendor.example",
            "GET",
            Response {
                status: 200,
                headers: Vec::new(),
                body: Vec::new(),
            },
        ));
    let refused = invalid.call("vendor.search", r#"{"text":"rust"}"#);
    assert_eq!(refused.status, 1);
    assert!(refused.stderr.contains("settings"), "{}", refused.stderr);
    assert!(invalid.requests().is_empty());
}

fn streamed_http_run(body: &[u8]) -> dekopon_provider_sdk_testkit::Run<RawHttp> {
    Harness::<RawHttp>::get(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/providers/http-probe-provider.wasm"),
    )
    .http(HttpScript::new(
        "fixture.example.test",
        "POST",
        Response {
            status: 201,
            headers: Vec::new(),
            body: body.to_vec(),
        },
    ))
}

#[test]
fn real_streamed_http_returns_readable_isolated_assets() {
    use std::os::unix::fs::{FileExt as _, MetadataExt as _};

    use dekopon_provider_sdk_testkit::AssetConstraints;

    let run = || {
        let run = streamed_http_run(b"streamed asset bytes").assets(AssetConstraints {
            attach: true,
            ..Default::default()
        });
        let uri = format!("{}/images/generations", run.origin().unwrap());
        run.call(
            "http-probe.fetch",
            json!({"assetMode":"stream", "references":[], "uri":uri}),
        )
        .unwrap()
    };
    let first = run();
    let second = run();
    for result in [&first, &second] {
        assert_eq!(result.status, 0, "{}", result.stderr);
        assert_eq!(
            serde_json::from_slice::<Value>(&result.stdout).unwrap(),
            json!({"status":201})
        );
        assert_eq!(result.http_calls.len(), 1);
        assert_eq!(result.assets.attached.len(), 1);
        assert_eq!(result.assets.files.len(), 1);
        let asset = &result.assets.attached[0];
        assert_eq!(asset.content_type, "text/plain");
        assert_eq!(asset.bytes, 20);
        let file = result.assets.files[asset.descriptor as usize].file();
        let mut bytes = [0; 20];
        file.read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(&bytes, b"streamed asset bytes");
        assert_eq!(file.metadata().unwrap().nlink(), 0);
    }
    assert_ne!(
        first.assets.files[0].file().metadata().unwrap().ino(),
        second.assets.files[0].file().metadata().unwrap().ino()
    );
}

#[test]
fn streamed_http_without_assets_still_refuses_before_dispatch() {
    let run = streamed_http_run(b"streamed asset bytes");
    let uri = run.origin().unwrap().to_owned();
    let error = run
        .call(
            "http-probe.fetch",
            json!({"assetMode":"stream", "references":[], "uri":uri}),
        )
        .unwrap_err();
    let HarnessError::Invocation(failure) = error else {
        panic!("unexpected error: {error:?}");
    };
    assert!(failure.http_calls.is_empty());
    assert!(matches!(
        *failure.error,
        dekopon_broker_host::BrokerHostError::HostCallRejected {
            reason: "asset-call-rejected",
            ..
        }
    ));
}

#[test]
fn asset_storage_does_not_grant_attachment() {
    let run = streamed_http_run(b"streamed asset bytes").assets(Default::default());
    let uri = run.origin().unwrap().to_owned();
    let error = run
        .call(
            "http-probe.fetch",
            json!({"assetMode":"stream", "references":[], "uri":uri}),
        )
        .unwrap_err();
    let HarnessError::Invocation(failure) = error else {
        panic!("unexpected error: {error:?}");
    };
    assert_eq!(failure.http_calls.len(), 1);
    assert!(matches!(
        *failure.error,
        dekopon_broker_host::BrokerHostError::HostCallRejected {
            reason: "asset-call-rejected",
            ..
        }
    ));
}

#[test]
fn asset_storage_does_not_widen_http_methods() {
    let run = streamed_http_run(b"streamed asset bytes")
        .assets(dekopon_provider_sdk_testkit::AssetConstraints {
            attach: true,
            ..Default::default()
        })
        .http(HttpScript::new(
            "fixture.example.test",
            "GET",
            Response {
                status: 200,
                headers: Vec::new(),
                body: Vec::new(),
            },
        ));
    let uri = run.origin().unwrap().to_owned();
    let error = run
        .call(
            "http-probe.fetch",
            json!({"assetMode":"stream", "references":[], "uri":uri}),
        )
        .unwrap_err();
    let HarnessError::Invocation(failure) = error else {
        panic!("unexpected error: {error:?}");
    };
    assert!(matches!(
        *failure.error,
        dekopon_broker_host::BrokerHostError::HostCallRejected {
            reason: "denied",
            ..
        }
    ));
}

const RED_PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
const BLUE_PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYPj/HwADAgH/5ncLrgAAAABJRU5ErkJggg==";

#[test]
fn real_edit_requests_stream_fixture_images_and_return_output_assets() {
    use dekopon_core::base64::{Engine as _, STANDARD};
    use std::os::unix::fs::FileExt as _;

    let red = STANDARD.decode(RED_PIXEL).unwrap();
    let blue = STANDARD.decode(BLUE_PIXEL).unwrap();
    let run = streamed_http_run(&blue)
        .assets(dekopon_provider_sdk_testkit::AssetConstraints {
            attach: true,
            ..Default::default()
        })
        .asset(3, "image/png", blue.clone())
        .asset(7, "image/png", red);
    let uri = format!("{}/images/edits", run.origin().unwrap());
    let output = run.call(
        "http-probe.fetch",
        json!({"assetMode":"stream", "references":["chat-asset:7", "chat-asset:3", "chat-asset:7"], "uri":uri}),
    ).unwrap();
    assert_eq!(output.status, 0, "{}", output.stderr);
    let request = output.http_request.unwrap();
    assert_eq!(request.method, "POST");
    assert_eq!(request.uri, uri);
    assert_eq!(
        request.body,
        format!("{RED_PIXEL}{BLUE_PIXEL}{RED_PIXEL}").as_bytes()
    );
    assert_eq!(output.assets.attached.len(), 1);
    let asset = &output.assets.attached[0];
    assert_eq!(asset.bytes, blue.len() as u64);
    let mut bytes = vec![0; blue.len()];
    output.assets.files[asset.descriptor as usize]
        .file()
        .read_exact_at(&mut bytes, 0)
        .unwrap();
    assert_eq!(bytes, blue);
}

#[test]
fn input_asset_bytes_and_references_do_not_leak_between_calls() {
    use dekopon_core::base64::{Engine as _, STANDARD};

    for pixel in [RED_PIXEL, BLUE_PIXEL] {
        let run = streamed_http_run(b"edited")
            .assets(dekopon_provider_sdk_testkit::AssetConstraints {
                attach: true,
                ..Default::default()
            })
            .asset(7, "image/png", STANDARD.decode(pixel).unwrap());
        let uri = run.origin().unwrap().to_owned();
        let output = run
            .call(
                "http-probe.fetch",
                json!({"assetMode":"stream", "references":["chat-asset:7"], "uri":uri}),
            )
            .unwrap();
        assert_eq!(output.status, 0, "{}", output.stderr);
        assert_eq!(output.http_request.unwrap().body, pixel.as_bytes());
    }
    let run = streamed_http_run(b"edited").assets(dekopon_provider_sdk_testkit::AssetConstraints {
        attach: true,
        ..Default::default()
    });
    let uri = run.origin().unwrap().to_owned();
    let error = run
        .call(
            "http-probe.fetch",
            json!({"assetMode":"stream", "references":["chat-asset:7"], "uri":uri}),
        )
        .unwrap_err();
    let HarnessError::Invocation(failure) = error else {
        panic!("unexpected error: {error:?}");
    };
    assert!(failure.http_calls.is_empty());
    assert!(matches!(
        *failure.error,
        dekopon_broker_host::BrokerHostError::AssetInput {
            source: dekopon_broker_host::asset::AssetAdmissionError::DescriptorCount
        }
    ));
}

#[test]
fn an_input_fixture_can_be_read_without_an_attachment_grant() {
    use dekopon_core::base64::{Engine as _, STANDARD};

    let bytes = STANDARD.decode(RED_PIXEL).unwrap();
    let output = streamed_http_run(b"unused")
        .asset(1, "image/png", bytes.clone())
        .call(
            "http-probe.fetch",
            json!({"assetMode":"read", "reference":"chat-asset:1"}),
        )
        .unwrap();
    assert_eq!(output.status, 0, "{}", output.stderr);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"read":bytes.len()})
    );
    assert!(output.http_calls.is_empty());
    assert!(output.http_request.is_none());
    assert!(output.assets.attached.is_empty());
}

#[test]
fn duplicate_input_asset_ids_are_refused_by_broker_admission() {
    let error = streamed_http_run(b"unused")
        .asset(1, "image/png", vec![1])
        .asset(1, "image/png", vec![2])
        .call(
            "http-probe.fetch",
            json!({"assetMode":"read", "reference":"chat-asset:1"}),
        )
        .unwrap_err();
    let HarnessError::Invocation(failure) = error else {
        panic!("unexpected error: {error:?}");
    };
    assert!(failure.http_calls.is_empty());
    assert!(matches!(
        *failure.error,
        dekopon_broker_host::BrokerHostError::AssetInput {
            source: dekopon_broker_host::asset::AssetAdmissionError::DuplicateRow
        }
    ));
}

#[test]
fn input_images_do_not_grant_output_attachment() {
    use dekopon_core::base64::{Engine as _, STANDARD};

    let run =
        streamed_http_run(b"edited").asset(7, "image/png", STANDARD.decode(RED_PIXEL).unwrap());
    let uri = run.origin().unwrap().to_owned();
    let error = run
        .call(
            "http-probe.fetch",
            json!({"assetMode":"stream", "references":["chat-asset:7"], "uri":uri}),
        )
        .unwrap_err();
    let HarnessError::Invocation(failure) = error else {
        panic!("unexpected error: {error:?}");
    };
    assert_eq!(failure.http_calls.len(), 1);
    assert!(matches!(
        *failure.error,
        dekopon_broker_host::BrokerHostError::HostCallRejected {
            reason: "asset-call-rejected",
            ..
        }
    ));
}

#[test]
fn scripted_https_captures_the_complete_streamed_request_body() {
    use dekopon_core::base64::{Engine as _, STANDARD};

    let bytes = vec![0xa5; 8192];
    let expected = STANDARD.encode(&bytes);
    let run = streamed_http_run(b"done")
        .assets(dekopon_provider_sdk_testkit::AssetConstraints {
            attach: true,
            ..Default::default()
        })
        .asset(1, "application/octet-stream", bytes);
    let uri = run.origin().unwrap().to_owned();
    let output = run
        .call(
            "http-probe.fetch",
            json!({"assetMode":"stream", "references":["chat-asset:1"], "uri":uri}),
        )
        .unwrap();
    assert_eq!(output.status, 0, "{}", output.stderr);
    assert_eq!(output.http_request.unwrap().body, expected.as_bytes());
}
