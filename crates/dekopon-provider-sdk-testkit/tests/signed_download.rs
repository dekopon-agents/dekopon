#![allow(clippy::unwrap_used)]
use std::{fmt, io::Write as _, path::PathBuf};

use dekopon_provider_sdk::{
    EffectKind, RiskLevel,
    clap::Parser,
    provider::{Capability, Code, Failure, Http, Proposal, Provider, Response, Stdout, Usage},
    signed_download as signed,
};
use dekopon_provider_sdk_testkit::{Harness, HarnessError, HttpScript, Native, ObservedRequest};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

const API: &str = "https://api.github.test/repos/owner/repo/actions/jobs/1/logs";
const STORAGE: &str =
    "https://productionresultssa1.blob.core.windows.net/logs/job.txt?sig=synthetic-query";
const BODY: &[u8] = b"signed log bytes";

#[derive(Parser)]
struct Args {}
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Input {
    uri: String,
    asset_mode: String,
    bytes: u32,
}
struct Probe;
struct Fetch;
#[derive(Debug)]
enum Failed {
    Download(signed::DownloadError),
    Asset(dekopon_provider_sdk::asset::AssetError),
    Output(std::io::Error),
}
impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Download(error) => write!(f, "signed download failed: {:?}", error.code),
            Self::Asset(error) => error.fmt(f),
            Self::Output(error) => error.fmt(f),
        }
    }
}
impl Failure for Failed {
    fn code(&self) -> Code {
        Code::new("signed-download-failed")
    }
}
impl Provider for Probe {
    const ID: &'static str = "http-probe";
    const COMMAND_WORDS: &'static [&'static str] = &["httpprobe"];
    const DESCRIPTION: &'static str = "signed download test";
    type Args = Args;
    type Capabilities = (Fetch,);
    fn propose(_: Args, _: bool) -> Result<Proposal<Self>, Usage> {
        Ok(Proposal::to::<Fetch>(input()))
    }
}
impl Capability for Fetch {
    type Provider = Probe;
    const NAME: &'static str = "fetch";
    const DESCRIPTION: &'static str = "download a signed log window";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = Input;
    type Needs = Http;
    type Error = Failed;
    fn run(input: Input, _: Http, out: &mut Stdout) -> Result<(), Failed> {
        match signed::download(&signed::Request {
            api_uri: input.uri,
            window: signed::Window::Tail(input.bytes),
        })
        .map_err(Failed::Download)?
        {
            signed::Response::Storage { status, body, .. } => {
                let body = body
                    .map(|body| body.read_all())
                    .transpose()
                    .map_err(Failed::Asset)?;
                writeln!(out, "{}", json!({"status": status, "bytes": body})).map_err(Failed::Output)
            }
            signed::Response::Api(_) => Err(Failed::Download(signed::DownloadError {
                code: signed::ErrorCode::Incomplete,
            })),
        }
    }
}
fn input() -> Input {
    Input {
        uri: API.to_owned(),
        asset_mode: "signed-download".to_owned(),
        bytes: 3,
    }
}
fn component() -> PathBuf {
    PathBuf::from(
        std::env::var_os("DEKOPON_TEST_ASSET_WASM").expect("built HTTP probe component path"),
    )
}
fn scripts(storage: Response) -> (HttpScript, HttpScript) {
    (
        HttpScript::new(
            "api.github.test",
            "GET",
            Response {
                status: 302,
                headers: vec![
                    dekopon_provider_sdk::provider::Header::new("location", STORAGE.as_bytes())
                        .unwrap(),
                ],
                body: vec![],
            },
        ),
        HttpScript::new("productionresultssa1.blob.core.windows.net", "GET", storage),
    )
}
fn bytes() -> Response {
    Response {
        status: 200,
        headers: vec![],
        body: BODY.to_vec(),
    }
}
fn assert_requests(requests: &[ObservedRequest]) {
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.sent.uri.as_str())
            .collect::<Vec<_>>(),
        [API, STORAGE, STORAGE]
    );
    for request in requests {
        assert!(
            request
                .guest
                .headers
                .iter()
                .all(|header| !header.name.eq_ignore_ascii_case("authorization"))
        );
    }
    let auth = |request: &ObservedRequest| {
        request
            .sent
            .headers
            .iter()
            .filter(|header| header.name.eq_ignore_ascii_case("authorization"))
            .count()
    };
    assert_eq!(auth(&requests[0]), 1);
    for request in &requests[1..] {
        assert_eq!(auth(request), 0);
        assert!(request.sent.headers.iter().all(|header| {
            !header
                .value
                .windows(b"testkit-synthetic-credential".len())
                .any(|part| part == b"testkit-synthetic-credential")
        }));
    }
    let ranges = requests[1..]
        .iter()
        .map(|request| {
            request
                .sent
                .headers
                .iter()
                .find(|header| header.name == "range")
                .unwrap()
                .value
                .clone()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ranges,
        [
            b"bytes=0-0".to_vec(),
            format!("bytes=0-{}", BODY.len() - 1).into_bytes()
        ]
    );
}
#[test]
fn real_component_downloads_one_redirect_with_probe_and_window() {
    let (api, storage) = scripts(bytes());
    let run = Harness::<Probe>::get(component()).signed_download(api, storage, 3);
    let requests = run.requests();
    let output = run
        .call("http-probe.fetch", serde_json::to_value(input()).unwrap())
        .unwrap();
    assert_eq!(output.status, 0, "{}", output.stderr);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        json!({"status":206,"bytes":b"tes"})
    );
    assert_requests(&requests());
}
#[test]
fn native_download_records_every_request_in_order() {
    let (api, storage) = scripts(bytes());
    let native = Native::<Probe>::new().signed_download(api, storage, 3);
    let output = native.call(
        "http-probe.fetch",
        &serde_json::to_string(&input()).unwrap(),
    );
    assert_eq!(output.status, 0, "{}", output.stderr);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        json!({"status":206,"bytes":b"tes"})
    );
    assert_eq!(
        native
            .requests()
            .iter()
            .map(|request| request.uri.as_str())
            .collect::<Vec<_>>(),
        [API, STORAGE, STORAGE]
    );
    assert!(native.requests().iter().all(|request| {
        request
            .headers
            .iter()
            .all(|header| !header.name.eq_ignore_ascii_case("authorization"))
    }));
}
#[test]
fn real_component_does_not_follow_a_storage_redirect() {
    let (api, storage) = scripts(Response {
        status: 302,
        headers: vec![
            dekopon_provider_sdk::provider::Header::new(
                "location",
                b"https://pipelines1.actions.githubusercontent.com/next?sig=second",
            )
            .unwrap(),
        ],
        body: vec![],
    });
    let run = Harness::<Probe>::get(component()).signed_download(api, storage, 4);
    let requests = run.requests();
    let error = run
        .call("http-probe.fetch", serde_json::to_value(input()).unwrap())
        .expect_err("storage redirect is terminal");
    assert!(
        matches!(error, HarnessError::Invocation(ref failure) if matches!(*failure.error, dekopon_broker_host::BrokerHostError::HostCallRejected { reason: dekopon_broker_host::HostCallRejection::Http(dekopon_http_host::PolicyViolation::RedirectRefused), .. })),
        "{error:?}"
    );
    assert_eq!(
        requests()
            .iter()
            .map(|request| request.sent.uri.clone())
            .collect::<Vec<_>>(),
        [API, STORAGE]
    );
}
#[test]
fn real_component_request_cap_refuses_window_after_probe() {
    let (api, storage) = scripts(bytes());
    let run = Harness::<Probe>::get(component()).signed_download(api, storage, 2);
    let requests = run.requests();
    let error = run
        .call("http-probe.fetch", serde_json::to_value(input()).unwrap())
        .expect_err("window needs a third request");
    assert!(
        matches!(error, HarnessError::Invocation(ref failure) if matches!(*failure.error, dekopon_broker_host::BrokerHostError::HostCallRejected { reason: dekopon_broker_host::HostCallRejection::Http(dekopon_http_host::PolicyViolation::HostCallLimit), .. })),
        "{error:?}"
    );
    assert_eq!(requests().len(), 2);
}
