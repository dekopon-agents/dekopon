use base64::{Engine as _, engine::general_purpose::STANDARD};
use dekopon_provider_sdk::clap::{Args, Parser, Subcommand};
use dekopon_provider_sdk::provider::{
    Assets, Capability, Code, Failure, Header, Http, HttpBuildError, HttpError, Part, Proposal,
    Provider, Request, SpliceError, Stdout, StreamedRequest, Usage,
};
use dekopon_provider_sdk::{EffectKind, RiskLevel, SecretDrn, SecretUseProposal};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::io::Write;

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
    #[serde(skip_serializing_if = "Option::is_none")]
    uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    headers: Option<Vec<HeaderInput>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    catch_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    splice_body: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    asset_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    after_write_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    references: Option<Vec<String>>,
    #[serde(rename = "catch_stream_error", skip_serializing_if = "Option::is_none")]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_etag: Option<String>,
}
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct PurgeInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    asset_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
}
#[derive(Debug)]
enum ProbeError {
    UriRequired,
    UnknownAssetMode,
    InvalidRequest(HttpBuildError),
    HttpFailed(HttpError),
    StreamHttp(HttpError),
    Splice(SpliceError),
    Asset(dekopon_provider_sdk::asset::AssetError),
    PreconditionFailed { expected: String, observed: String },
    AssetTestFailure,
    Output,
}
impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UriRequired => f.write_str("uri must be a string"),
            Self::UnknownAssetMode => f.write_str("unknown asset test mode"),
            Self::InvalidRequest(error) => write!(f, "{error}"),
            Self::HttpFailed(error) => write!(f, "{error}"),
            Self::StreamHttp(error) => f.write_str(&error.message),
            Self::Splice(error) => error.fmt(f),
            Self::Asset(error) => f.write_str(&error.message),
            Self::PreconditionFailed { expected, observed } => {
                write!(
                    f,
                    "resource moved: expected {expected:?}, observed {observed:?}"
                )
            }
            Self::AssetTestFailure => f.write_str("failure after attach"),
            Self::Output => f.write_str("provider stdout closed"),
        }
    }
}
impl Failure for ProbeError {
    fn code(&self) -> Code {
        Code::new(match self {
            Self::UriRequired | Self::UnknownAssetMode => "invalid-input",
            Self::InvalidRequest(_) => "invalid-request",
            Self::HttpFailed(_) => "http-failed",
            Self::StreamHttp(error) => error.code.as_str(),
            Self::Splice(SpliceError::Closed) => "output-error",
            Self::Splice(SpliceError::Http(error)) => error.code.as_str(),
            Self::Asset(error) => error.code.as_str(),
            Self::PreconditionFailed { .. } => "precondition-failed",
            Self::AssetTestFailure => "asset-test-failure",
            Self::Output => "output-error",
        })
    }
}
fn invalid_request(error: HttpBuildError) -> ProbeError {
    ProbeError::InvalidRequest(error)
}
fn http_failed(error: HttpError) -> ProbeError {
    ProbeError::HttpFailed(error)
}
fn stream_failed(error: HttpError) -> ProbeError {
    ProbeError::StreamHttp(error)
}
fn asset_failed(error: dekopon_provider_sdk::asset::AssetError) -> ProbeError {
    ProbeError::Asset(error)
}

impl Provider for HttpProbe {
    const ID: &'static str = "http-probe";
    const COMMAND_WORDS: &'static [&'static str] = &["httpprobe"];
    const DESCRIPTION: &'static str = "Exercises the versioned broker HTTP import";
    type Args = Command;
    type Capabilities = (Fetch, ConditionalWrite, Purge);
    fn propose(args: Command, _: bool) -> Result<Proposal<Self>, Usage> {
        match args.action {
            Action::Fetch(fetch) => {
                let input = FetchInput {
                    uri: Some(fetch.uri.uri),
                    method: fetch.method,
                    body: fetch.body,
                    catch_error: fetch.catch_error.then_some(true),
                    splice_body: None,
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
            Action::Purge(uri) => Ok(Proposal::to::<Purge>(PurgeInput {
                uri: Some(uri.uri),
                asset_mode: None,
                reference: None,
                bytes: None,
            })),
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
    type Error = ProbeError;
    fn run(
        input: FetchInput,
        (http, assets): (Http, Assets),
        out: &mut Stdout,
    ) -> Result<(), ProbeError> {
        let value = (|| -> Result<Option<Value>, ProbeError> {
            if let Some(mode) = input.asset_mode.as_deref() {
                return asset_probe(mode, &input, &http, &assets).map(Some);
            }
            let uri = input.uri.ok_or(ProbeError::UriRequired)?;
            let mut request = Request::new(input.method.as_deref().unwrap_or("GET"), uri)
                .map_err(invalid_request)?;
            for header in input.headers.unwrap_or_default() {
                request = request
                    .with_header(Header::text(header.name, header.value).map_err(invalid_request)?);
            }
            if let Some(body) = input.body {
                request = request.with_body(body.into_bytes());
            }
            if input.splice_body.unwrap_or(false) {
                let response = http.open(request).map_err(http_failed)?;
                response.body.splice(out).map_err(ProbeError::Splice)?;
                return Ok(None);
            }
            match http.send(request) {
                Ok(response) => Ok(Some(describe_response(
                    response.status,
                    &response.body,
                    response.headers.len(),
                ))),
                Err(error) if input.catch_error.unwrap_or(false) => {
                    Ok(Some(json!({"caughtError":format!("{:?}", error.code)})))
                }
                Err(error) => Err(http_failed(error)),
            }
        })()?;
        if let Some(value) = value {
            writeln!(out, "{value}").map_err(|_| ProbeError::Output)?;
        }
        Ok(())
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
    type Error = ProbeError;
    fn run(input: WriteInput, http: Http, out: &mut Stdout) -> Result<(), ProbeError> {
        let value = (|| -> Result<Value, ProbeError> {
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
                return Err(ProbeError::PreconditionFailed { expected, observed });
            }
            let request = Request::new("POST", &input.uri)
                .map_err(invalid_request)?
                .with_header(
                    Header::new("if-match", observed.clone().into_bytes())
                        .map_err(invalid_request)?,
                )
                .with_body(b"{}".to_vec());
            let write = http.send(request).map_err(http_failed)?;
            Ok(json!({"observedEtag":observed,"readStatus":read.status,"writeStatus":write.status}))
        })()?;
        writeln!(out, "{value}").map_err(|_| ProbeError::Output)
    }
}
impl Capability for Purge {
    type Provider = HttpProbe;
    const NAME: &'static str = "purge";
    const DESCRIPTION: &'static str = "Deletes one broker-authorized resource";
    const EFFECT: EffectKind = EffectKind::ExternalWrite;
    const RISK: RiskLevel = RiskLevel::High;
    type Input = PurgeInput;
    type Needs = (Http, Assets);
    type Error = ProbeError;
    fn run(
        input: PurgeInput,
        (http, assets): (Http, Assets),
        out: &mut Stdout,
    ) -> Result<(), ProbeError> {
        let value = (|| -> Result<Value, ProbeError> {
            if let Some(mode) = input.asset_mode.as_deref() {
                let asset_input = FetchInput {
                    uri: input.uri,
                    method: None,
                    headers: None,
                    body: None,
                    catch_error: None,
                    splice_body: None,
                    asset_mode: None,
                    bytes: input.bytes,
                    after_write_error: None,
                    reference: input.reference,
                    references: None,
                    catch_stream_error: None,
                };
                return asset_probe(mode, &asset_input, &http, &assets);
            }
            let uri = input.uri.ok_or(ProbeError::UriRequired)?;
            let response = http
                .send(Request::new("DELETE", uri).map_err(invalid_request)?)
                .map_err(http_failed)?;
            Ok(json!({"status":response.status}))
        })()?;
        writeln!(out, "{value}").map_err(|_| ProbeError::Output)
    }
}

fn catch_write_error<E>(write: impl FnOnce() -> Result<(), E>) -> Value {
    json!({"caught":write().is_err()})
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
    if mode == "budget-write" {
        let writer = assets
            .allocate(
                "text/plain",
                dekopon_provider_sdk::asset::Encoding::Identity,
            )
            .map_err(asset_failed)?;
        let bytes = input.bytes.unwrap_or(1025).min(65536) as usize;
        return Ok(catch_write_error(|| writer.write_all(&vec![b'x'; bytes])));
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
        let response = response.map_err(stream_failed)?;
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
        "fail" => Err(ProbeError::AssetTestFailure),
        "attach" => Ok(json!({"ok":true})),
        _ => Err(ProbeError::UnknownAssetMode),
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
    use dekopon_provider_sdk::CommandRunOutcome;
    use dekopon_provider_sdk::provider;
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
                    false
                ),
                CommandRunOutcome::Proposed { .. }
            ));
        }
        assert_eq!(
            provider::invoke_native::<HttpProbe>(
                "http-probe.fetch",
                r#"{"unknown":true}"#,
                provider::NativeStdio {
                    stdin: None,
                    stdout: Box::new(std::io::sink())
                }
            )
            .status,
            2
        );
        assert_eq!(
            manifest.capabilities[2].input_schema["additionalProperties"],
            false
        );
        assert!(matches!(provider::command::<HttpProbe>(
            &["purge".into(), "--uri".into(), "https://example.test/".into()], false,
        ), CommandRunOutcome::Proposed { capability, input, .. }
            if capability.as_str() == "http-probe.purge" && input == json!({"uri":"https://example.test/"})));
    }
    #[test]
    fn streamed_host_failure_code_reaches_the_provider_response() {
        use dekopon_provider_sdk::provider::{
            HttpError, HttpErrorCode, Port, Response, StreamedResponse,
        };

        struct StreamHost;
        struct StreamProbe;
        struct Stream;
        impl Provider for StreamProbe {
            const ID: &'static str = "stream-host-test";
            const COMMAND_WORDS: &'static [&'static str] = &["streamtest"];
            const DESCRIPTION: &'static str = "streamed host failure witness";
            type Args = Command;
            type Capabilities = (Stream,);
            fn propose(_: Command, _: bool) -> Result<Proposal<Self>, Usage> {
                Err(Usage::new("no proposal"))
            }
        }
        impl Capability for Stream {
            type Provider = StreamProbe;
            const NAME: &'static str = "stream";
            const DESCRIPTION: &'static str = "stream request host failure";
            const EFFECT: EffectKind = EffectKind::ReadOnly;
            const RISK: RiskLevel = RiskLevel::Low;
            type Input = PurgeInput;
            type Needs = Http;
            type Error = ProbeError;
            fn run(input: PurgeInput, http: Http, out: &mut Stdout) -> Result<(), ProbeError> {
                let request =
                    StreamedRequest::new("POST", input.uri.ok_or(ProbeError::UriRequired)?)
                        .map_err(invalid_request)?;
                let response = http.stream(request).map_err(stream_failed)?;
                writeln!(out, "{}", json!({"status": response.status}))
                    .map_err(|_| ProbeError::Output)
            }
        }
        impl Port for StreamHost {
            fn now_unix_millis(&mut self) -> u64 {
                0
            }
            fn now_nanos(&mut self) -> u64 {
                0
            }
            fn fill_random(&mut self, out: &mut [u8]) {
                out.fill(0xa5);
            }
            fn settings(&mut self) -> Option<String> {
                None
            }
            fn send(&mut self, _: Request) -> Result<Response, HttpError> {
                Err(HttpError {
                    code: HttpErrorCode::Denied,
                    message: "unexpected buffered request".into(),
                })
            }
            fn open(
                &mut self,
                _: Request,
            ) -> Result<dekopon_provider_sdk::provider::OpenedResponse, HttpError> {
                unreachable!("stream-only witness")
            }
            fn stream(&mut self, _: StreamedRequest<'_>) -> Result<StreamedResponse, HttpError> {
                Err(HttpError {
                    code: HttpErrorCode::RequestTooLarge,
                    message: "host rejected streamed request".into(),
                })
            }
        }
        let outcome = provider::with_port(StreamHost, || {
            provider::invoke_native::<StreamProbe>(
                "stream-host-test.stream",
                r#"{"uri":"https://example.test/path"}"#,
                provider::NativeStdio {
                    stdin: None,
                    stdout: Box::new(std::io::sink()),
                },
            )
        });
        assert_eq!(outcome.status, 1);
        assert_eq!(outcome.stderr, "host rejected streamed request\n");
    }

    #[test]
    fn budget_write_error_is_caught_as_successful_guest_output() {
        assert_eq!(
            catch_write_error(|| Err::<(), _>("over budget")),
            json!({"caught":true})
        );
    }

    #[test]
    fn buffered_and_asset_failures_keep_their_distinct_codes_and_messages() {
        use dekopon_provider_sdk::asset::{AssetError, AssetErrorCode};
        use dekopon_provider_sdk::provider::HttpErrorCode;

        let buffered = http_failed(HttpError {
            code: HttpErrorCode::Connect,
            message: "connection refused".into(),
        });
        assert!(matches!(buffered, ProbeError::HttpFailed(_)));
        assert_eq!(buffered.code(), Code::new("http-failed"));
        assert_eq!(buffered.to_string(), "connect: connection refused");

        let asset = asset_failed(AssetError {
            code: AssetErrorCode::OverBudget,
            message: "asset budget exhausted".into(),
        });
        assert!(matches!(asset, ProbeError::Asset(_)));
        assert_eq!(asset.code(), Code::new("over-budget"));
        assert_eq!(asset.to_string(), "asset budget exhausted");
        assert_eq!(ProbeError::UriRequired.code(), Code::new("invalid-input"));
        assert_eq!(
            ProbeError::AssetTestFailure.code(),
            Code::new("asset-test-failure")
        );
    }

    #[test]
    fn secret_flags_propose_only_secret_use_not_credentials_in_input() {
        let uri = "https://example.test/records/1";
        let drn = "drn:com.xrl:secret:test:http-probe/token";
        for (flags, expected) in [
            (vec!["--bearer", drn], "httpBearer"),
            (vec!["--basic", "user-a", drn], "httpBasic"),
        ] {
            let mut words = vec!["fetch", "--uri", uri];
            words.extend(flags);
            let argv = words
                .iter()
                .map(|word| (*word).to_owned())
                .collect::<Vec<_>>();
            let CommandRunOutcome::Proposed {
                capability,
                input,
                secret_use,
            } = provider::command::<HttpProbe>(&argv, false)
            else {
                panic!("valid secret flag must propose");
            };
            assert_eq!(capability.as_str(), "http-probe.fetch");
            assert_eq!(input, json!({"uri":uri}));
            assert!(input.get("secret").is_none());
            assert_eq!(serde_json::to_value(secret_use).unwrap()["kind"], expected);
        }
    }
    #[test]
    fn invalid_secret_and_incomplete_arguments_do_not_propose() {
        let uri = "https://example.test/records/1";
        for words in [
            vec!["fetch", "--uri", uri, "--bearer", "not-a-drn"],
            vec!["fetch", "--uri", uri, "--basic", "user-a", "not-a-drn"],
        ] {
            let argv = words
                .iter()
                .map(|word| (*word).to_owned())
                .collect::<Vec<_>>();
            assert!(matches!(provider::command::<HttpProbe>(&argv, false),
                CommandRunOutcome::Failed { error } if error.code == "usage"));
        }
        for words in [
            vec!["purge"],
            vec!["bogus"],
            vec!["fetch", "--uri", uri, "--header", "accept"],
        ] {
            let argv = words
                .iter()
                .map(|word| (*word).to_owned())
                .collect::<Vec<_>>();
            assert!(matches!(
                provider::command::<HttpProbe>(&argv, false),
                CommandRunOutcome::Rendered { status: 2, .. }
            ));
        }
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
