#![allow(clippy::unwrap_used)]
use std::{
    path::PathBuf,
    time::{Duration, UNIX_EPOCH},
};

fn cli_component() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/providers/cli-probe-provider.wasm")
}

use dekopon_provider_sdk::{
    ComponentResponse, EffectKind, RiskLevel,
    clap::Parser,
    provider::{Capability, Clock, Header, Http, Proposal, Provider, Request, Response, Usage},
};
use dekopon_provider_sdk_testkit::{BrokerHostLimits, Harness, HarnessError, HttpScript, Native};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Parser)]
struct NoArgs {}

struct Cli;
struct Upper;
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Text {
    text: String,
}
impl Provider for Cli {
    const ID: &'static str = "cli-probe";
    const COMMAND_WORDS: &'static [&'static str] = &["probe"];
    const DESCRIPTION: &'static str = "typed test of checked cli-probe";
    type Args = NoArgs;
    type Capabilities = (Upper,);
    fn propose(_: NoArgs, _: Option<&str>) -> Result<Proposal<Self>, Usage> {
        Ok(Proposal::to::<Upper>(Text {
            text: "abc".to_owned(),
        }))
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
    type Output = Value;
    type Error = std::convert::Infallible;
    fn run(input: Text, (): ()) -> Result<Value, Self::Error> {
        Ok(json!({"text": input.text.to_uppercase()}))
    }
}

#[test]
fn cached_real_component_and_native_dispatch_agree() {
    let input = json!({"text":"hello"});
    let native = Native::<Cli>::new();
    assert_eq!(
        native.call("cli-probe.upper", &input.to_string()),
        ComponentResponse::Succeeded {
            output: json!({"text":"HELLO"})
        }
    );
    let component = cli_component();
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
    fn propose(_: NoArgs, _: Option<&str>) -> Result<Proposal<Self>, Usage> {
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
    type Output = Value;
    type Error = std::convert::Infallible;
    fn run(input: Text, (): ()) -> Result<Value, Self::Error> {
        Ok(json!({"text": input.text.to_uppercase()}))
    }
}

struct RawHttp;
struct Fetch;
impl Provider for RawHttp {
    const ID: &'static str = "http-probe";
    const COMMAND_WORDS: &'static [&'static str] = &["httpprobe"];
    const DESCRIPTION: &'static str = "native counterpart for the raw HTTP fixture";
    type Args = NoArgs;
    type Capabilities = (Fetch,);
    fn propose(_: NoArgs, _: Option<&str>) -> Result<Proposal<Self>, Usage> {
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
    type Output = Value;
    type Error = std::convert::Infallible;
    fn run(input: UrlInput, http: Http) -> Result<Value, Self::Error> {
        let response = http.send(Request::new("GET", input.uri).unwrap()).unwrap();
        Ok(json!({"headerCount": response.headers.len()}))
    }
}

#[test]
fn scripted_response_headers_match_native_and_raw_component() {
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
    let ComponentResponse::Succeeded { output } =
        native.call("http-probe.fetch", &input.to_string())
    else {
        panic!("native fake must return the scripted headers");
    };
    assert_eq!(output["headerCount"], 2);
    let real = run.call_full("http-probe.fetch", input).unwrap();
    assert_eq!(real.output["headerCount"], output["headerCount"]);
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
    fn propose(_: NoArgs, _: Option<&str>) -> Result<Proposal<Self>, Usage> {
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
    type Output = Value;
    type Error = std::convert::Infallible;
    fn run(input: UrlInput, (clock, http): (Clock, Http)) -> Result<Value, Self::Error> {
        let request = Request::new("GET", input.uri).unwrap();
        let response = http.send(request).unwrap();
        Ok(
            json!({"clock":clock.now_unix_millis(),"status":response.status,"body":String::from_utf8(response.body).unwrap()}),
        )
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
        native.call(
            "fake-imports.read",
            r#"{"uri":"https://fixture.example.test/path"}"#
        ),
        ComponentResponse::Succeeded {
            output: json!({"clock":951_782_400_123_u64,"status":201,"body":"ok"})
        }
    );
    assert_eq!(native.requests().len(), 1);
    assert_eq!(
        native.requests()[0].uri,
        "https://fixture.example.test/path"
    );
}
