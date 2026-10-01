#![allow(clippy::unwrap_used)]
use std::time::{Duration, UNIX_EPOCH};

use dekopon_provider_sdk::{
    ComponentResponse, EffectKind, RiskLevel,
    clap::Parser,
    provider::{Capability, Clock, Http, Proposal, Provider, Request, Usage},
};
use dekopon_provider_sdk_testkit::{Harness, HttpScript, Native};
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
    for _ in 0..2 {
        assert_eq!(
            Harness::<Cli>::get()
                .call("cli-probe.upper", input.clone())
                .unwrap(),
            json!({"text":"HELLO"})
        );
    }
    assert_eq!(Harness::<Cli>::compiled_identities(), 1);
    assert_eq!(
        Harness::<OtherCli>::get()
            .call("cli-probe.upper", input)
            .unwrap(),
        json!({"text":"HELLO"})
    );
    assert_eq!(Harness::<OtherCli>::compiled_identities(), 1);
    assert_eq!(Harness::<Cli>::compiled_identities(), 1);
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
