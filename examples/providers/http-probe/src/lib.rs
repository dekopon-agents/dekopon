use base64::{Engine as _, engine::general_purpose::STANDARD};
use dekopon_provider_sdk::clap::{Args, Parser, Subcommand};
use dekopon_provider_sdk::provider::{
    Assets, Capability, Code, Failure, Header, Http, Part, Proposal, Provider, Request,
    StreamedRequest, Usage,
};
use dekopon_provider_sdk::{EffectKind, RiskLevel, SecretDrn, SecretUseProposal};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

const MAX_RETURNED_BODY_BYTES: usize = 64 * 1024;
struct HttpProbe;
struct Fetch;
struct ConditionalWrite;
struct Purge;

#[derive(Parser)]
#[command(name = "httpprobe", version = "0.1.0", subcommand_required = true)]
struct Command {
    #[command(subcommand)]
    action: Action,
}
#[derive(Subcommand)]
enum Action {
    /// Fetch one broker-authorized URI
    Fetch(FetchArgs),
    /// Read a resource, then write only if its observed etag still matches
    ConditionalWrite(WriteArgs),
    /// Delete one broker-authorized resource
    Purge(UriArg),
}
#[derive(Args)]
struct UriArg {
    /// The URI to request
    #[arg(long, required = true)]
    uri: String,
}
#[derive(Args)]
struct WriteArgs {
    #[command(flatten)]
    uri: UriArg,
    /// Refuse the write unless the read observes this etag
    #[arg(long)]
    expected_etag: Option<String>,
}
#[derive(Args)]
struct FetchArgs {
    #[command(flatten)]
    uri: UriArg,
    /// Request method token; GET when absent
    #[arg(long)]
    method: Option<String>,
    /// One text request header; repeat for more, sent in order
    #[arg(long, num_args = 2, value_names = ["NAME", "VALUE"])]
    header: Vec<String>,
    /// Buffered text request body
    #[arg(long)]
    body: Option<String>,
    /// Answer a failed request with its error code instead of failing
    #[arg(long)]
    catch_error: bool,
    /// Propose sending this secret DRN as a Bearer token
    #[arg(long, conflicts_with = "basic")]
    bearer: Option<String>,
    /// Propose sending this username and secret DRN as Basic credentials
    #[arg(long, num_args = 2, value_names = ["USER", "DRN"])]
    basic: Vec<String>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct FetchInput {
    uri: Option<String>,
    method: Option<String>,
    headers: Option<Vec<HeaderInput>>,
    body: Option<String>,
    catch_error: Option<bool>,
    asset_mode: Option<String>,
    bytes: Option<u64>,
    after_write_error: Option<String>,
    reference: Option<String>,
    references: Option<Vec<String>>,
    #[serde(rename = "catch_stream_error")]
    catch_stream_error: Option<bool>,
}
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct HeaderInput {
    name: String,
    value: String,
}
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WriteInput {
    uri: String,
    expected_etag: Option<String>,
}
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PurgeInput {
    uri: String,
}
#[derive(Debug)]
struct ProbeError {
    code: Code,
    message: String,
}
impl ProbeError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code: Code::new(code),
            message: message.into(),
        }
    }
}
impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl Failure for ProbeError {
    fn code(&self) -> Code {
        self.code
    }
}
fn invalid_request(error: impl std::fmt::Display) -> ProbeError {
    ProbeError::new("invalid-request", error.to_string())
}
fn http_failed(error: impl std::fmt::Display) -> ProbeError {
    ProbeError::new("http-failed", error.to_string())
}
fn asset_failed(error: dekopon_provider_sdk::asset::AssetError) -> ProbeError {
    ProbeError::new(error.code.as_str(), error.message)
}

impl Provider for HttpProbe {
    const ID: &'static str = "http-probe";
    const COMMAND_WORDS: &'static [&'static str] = &["httpprobe"];
    const DESCRIPTION: &'static str = "Exercises the versioned broker HTTP import";
    type Args = Command;
    type Capabilities = (Fetch, ConditionalWrite, Purge);
    fn propose(args: Command, _: Option<&str>) -> Result<Proposal<Self>, Usage> {
        match args.action {
            Action::Fetch(fetch) => {
                let input = FetchInput {
                    uri: Some(fetch.uri.uri),
                    method: fetch.method,
                    body: fetch.body,
                    catch_error: fetch.catch_error.then_some(true),
                    headers: (!fetch.header.is_empty()).then(|| {
                        fetch
                            .header
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|pair| HeaderInput {
                                name: pair[0].clone(),
                                value: pair[1].clone(),
                            })
                            .collect()
                    }),
                    asset_mode: None,
                    bytes: None,
                    after_write_error: None,
                    reference: None,
                    references: None,
                    catch_stream_error: None,
                };
                let secret_use = if let Some(drn) = fetch.bearer {
                    Some(SecretUseProposal::HttpBearer {
                        secret: drn.parse::<SecretDrn>().map_err(|e| {
                            Usage::new(format!("httpprobe fetch --bearer <DRN>: {e}"))
                        })?,
                    })
                } else if let [username, drn] = fetch.basic.as_slice() {
                    let secret = drn.parse::<SecretDrn>().map_err(|e| {
                        Usage::new(format!("httpprobe fetch --basic <USER> <DRN>: {e}"))
                    })?;
                    Some(serde_json::from_value(json!({"kind":"httpBasic","secret":secret.as_str(),"username":username}))
                        .map_err(|e| Usage::new(format!("httpprobe fetch --basic <USER> <DRN>: {e}")))?)
                } else {
                    None
                };
                let proposal = Proposal::to::<Fetch>(input);
                Ok(if let Some(secret) = secret_use {
                    proposal.with_secret_use(secret)
                } else {
                    proposal
                })
            }
            Action::ConditionalWrite(write) => Ok(Proposal::to::<ConditionalWrite>(WriteInput {
                uri: write.uri.uri,
                expected_etag: write.expected_etag,
            })),
            Action::Purge(uri) => Ok(Proposal::to::<Purge>(PurgeInput { uri: uri.uri })),
        }
    }
}
impl Capability for Fetch {
    type Provider = HttpProbe;
    const NAME: &'static str = "fetch";
    const DESCRIPTION: &'static str = "Fetches one broker-authorized URI";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = FetchInput;
    type Needs = (Http, Assets);
    type Output = Value;
    type Error = ProbeError;
    fn run(input: FetchInput, (http, assets): (Http, Assets)) -> Result<Value, ProbeError> {
        if let Some(mode) = input.asset_mode.as_deref() {
            return asset_probe(mode, &input, &http, &assets);
        }
        let uri = input
            .uri
            .ok_or_else(|| ProbeError::new("invalid-input", "uri must be a string"))?;
        let mut request =
            Request::new(input.method.as_deref().unwrap_or("GET"), uri).map_err(invalid_request)?;
        for header in input.headers.unwrap_or_default() {
            request = request
                .with_header(Header::text(header.name, header.value).map_err(invalid_request)?);
        }
        if let Some(body) = input.body {
            request = request.with_body(body.into_bytes());
        }
        match http.send(request) {
            Ok(response) => Ok(describe_response(
                response.status,
                &response.body,
                response.headers.len(),
            )),
            Err(error) if input.catch_error.unwrap_or(false) => {
                Ok(json!({"caughtError":format!("{:?}", error.code)}))
            }
            Err(error) => Err(http_failed(error)),
        }
    }
}
impl Capability for ConditionalWrite {
    type Provider = HttpProbe;
    const NAME: &'static str = "conditional-write";
    const DESCRIPTION: &'static str =
        "Reads a resource, then writes only if its observed etag still matches";
    const EFFECT: EffectKind = EffectKind::ExternalWrite;
    const RISK: RiskLevel = RiskLevel::High;
    type Input = WriteInput;
    type Needs = Http;
    type Output = Value;
    type Error = ProbeError;
    fn run(input: WriteInput, http: Http) -> Result<Value, ProbeError> {
        let read = http
            .send(Request::new("GET", &input.uri).map_err(invalid_request)?)
            .map_err(http_failed)?;
        let observed = read
            .headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case("etag"))
            .map(|header| String::from_utf8_lossy(&header.value).into_owned())
            .unwrap_or_default();
        if let Some(expected) = input.expected_etag
            && expected != observed
        {
            return Err(ProbeError::new(
                "precondition-failed",
                format!("resource moved: expected {expected:?}, observed {observed:?}"),
            ));
        }
        let request = Request::new("POST", &input.uri)
            .map_err(invalid_request)?
            .with_header(
                Header::new("if-match", observed.clone().into_bytes()).map_err(invalid_request)?,
            )
            .with_body(b"{}".to_vec());
        let write = http.send(request).map_err(http_failed)?;
        Ok(json!({"observedEtag":observed,"readStatus":read.status,"writeStatus":write.status}))
    }
}
impl Capability for Purge {
    type Provider = HttpProbe;
    const NAME: &'static str = "purge";
    const DESCRIPTION: &'static str = "Deletes one broker-authorized resource";
    const EFFECT: EffectKind = EffectKind::ExternalWrite;
    const RISK: RiskLevel = RiskLevel::High;
    type Input = PurgeInput;
    type Needs = Http;
    type Output = Value;
    type Error = ProbeError;
    fn run(input: PurgeInput, http: Http) -> Result<Value, ProbeError> {
        let response = http
            .send(Request::new("DELETE", input.uri).map_err(invalid_request)?)
            .map_err(http_failed)?;
        Ok(json!({"status":response.status}))
    }
}

fn asset_probe(
    mode: &str,
    input: &FetchInput,
    http: &Http,
    assets: &Assets,
) -> Result<Value, ProbeError> {
    if mode == "read" {
        let handle = assets
            .open(input.reference.as_deref().unwrap())
            .map_err(asset_failed)?;
        let mut chunk = [0; 65536];
        let mut bytes = 0;
        loop {
            let count = handle.read(&mut chunk).map_err(asset_failed)?;
            if count == 0 {
                break;
            }
            bytes += count;
        }
        return Ok(json!({"read":bytes}));
    }
    if mode == "send" {
        let handle = assets
            .open(input.reference.as_deref().unwrap())
            .map_err(asset_failed)?;
        assets.send(&handle).map_err(asset_failed)?;
        return Ok(json!({"sent":true}));
    }
    if mode == "stream" {
        let handles = input
            .references
            .as_ref()
            .unwrap()
            .iter()
            .map(|reference| assets.open(reference).map_err(asset_failed))
            .collect::<Result<Vec<_>, _>>()?;
        let mut request =
            StreamedRequest::new("POST", input.uri.as_deref().unwrap()).map_err(invalid_request)?;
        request.body = handles
            .iter()
            .map(|handle| Part::asset(handle, dekopon_provider_sdk::asset::Encoding::Base64))
            .collect();
        let response = http.stream(request);
        if input.catch_stream_error.unwrap_or(false) {
            return Ok(json!({"caught":response.is_err()}));
        }
        let response = response.map_err(http_failed)?;
        let writer = assets
            .allocate(
                "text/plain",
                dekopon_provider_sdk::asset::Encoding::Identity,
            )
            .map_err(asset_failed)?;
        let mut bytes = [0; 65536];
        loop {
            let count = response.body.read(&mut bytes).map_err(asset_failed)?;
            if count == 0 {
                break;
            }
            writer.write_all(&bytes[..count]).map_err(asset_failed)?;
        }
        assets.attach(writer).map_err(asset_failed)?;
        return Ok(json!({"status":response.status}));
    }
    let writer = assets
        .allocate(
            "text/plain",
            dekopon_provider_sdk::asset::Encoding::Identity,
        )
        .map_err(asset_failed)?;
    writer.write_all(b"asset ").map_err(asset_failed)?;
    writer.write_all(b"probe").map_err(asset_failed)?;
    if mode == "channel" {
        return Ok(json!({"ok":true}));
    }
    let attached = assets.attach(writer);
    if mode == "catch-denied" {
        return Ok(json!({"caught":attached.is_err()}));
    }
    attached.map_err(asset_failed)?;
    match mode {
        "trap" => panic!("asset test trap after attach"),
        "timeout" => loop {
            std::hint::spin_loop();
        },
        "fail" => Err(ProbeError::new(
            "asset-test-failure",
            "failure after attach",
        )),
        "attach" => Ok(json!({"ok":true})),
        _ => Err(ProbeError::new("invalid-input", "unknown asset test mode")),
    }
}
fn describe_response(status: u16, body: &[u8], header_count: usize) -> Value {
    let returned = bounded_prefix(body);
    let mut fields = Map::new();
    fields.insert("status".to_owned(), json!(status));
    fields.insert("bodyBytes".to_owned(), json!(body.len()));
    fields.insert("headerCount".to_owned(), json!(header_count));
    fields.insert("body".to_owned(), json!(STANDARD.encode(returned)));
    fields.insert(
        "bodyTruncated".to_owned(),
        json!(returned.len() < body.len()),
    );
    if let Ok(text) = core::str::from_utf8(returned) {
        fields.insert("bodyText".to_owned(), json!(text));
    }
    Value::Object(fields)
}
fn bounded_prefix(body: &[u8]) -> &[u8] {
    if body.len() <= MAX_RETURNED_BODY_BYTES {
        return body;
    }
    let candidate = &body[..MAX_RETURNED_BODY_BYTES];
    match core::str::from_utf8(candidate) {
        Ok(_) => candidate,
        Err(error) if error.error_len().is_none() => &candidate[..error.valid_up_to()],
        Err(_) => candidate,
    }
}
dekopon_provider_sdk::export!(HttpProbe);

#[cfg(test)]
mod tests {
    use super::*;
    use dekopon_provider_sdk::provider;
    use dekopon_provider_sdk::{CommandRunOutcome, ComponentResponse};
    #[test]
    fn typed_manifest_and_command_cover_all_routes() {
        let manifest = provider::manifest::<HttpProbe>().unwrap();
        assert_eq!(
            manifest
                .capabilities
                .iter()
                .map(|cap| cap.id.as_str())
                .collect::<Vec<_>>(),
            [
                "http-probe.fetch",
                "http-probe.conditional-write",
                "http-probe.purge"
            ]
        );
        for sub in ["fetch", "conditional-write", "purge"] {
            assert!(matches!(
                provider::command::<HttpProbe>(
                    &[sub.into(), "--uri".into(), "https://example.test/".into()],
                    None
                ),
                CommandRunOutcome::Proposed { .. }
            ));
        }
        assert!(
            matches!(provider::call::<HttpProbe>("http-probe.fetch", r#"{"unknown":true}"#), ComponentResponse::Failed { error } if error.code == "invalid-input")
        );
    }
    #[test]
    fn body_rendering_stays_bounded_and_preserves_binary() {
        assert_eq!(
            describe_response(200, b"hello probe", 4)["body"],
            "aGVsbG8gcHJvYmU="
        );
        assert_eq!(describe_response(200, &[0xff, 0xfe], 1)["body"], "//4=");
        assert!(
            describe_response(200, &[0xff, 0xfe], 1)
                .get("bodyText")
                .is_none()
        );
        let mut body = vec![b'x'; MAX_RETURNED_BODY_BYTES - 1];
        body.extend_from_slice("€tail".as_bytes());
        let response = describe_response(200, &body, 0);
        assert_eq!(
            response["bodyText"].as_str().unwrap().len(),
            MAX_RETURNED_BODY_BYTES - 1
        );
        assert_eq!(response["bodyTruncated"], true);
    }
}
