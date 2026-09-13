//! Test fixture for the broker host's `dekopon:http/client@1.0.0` import, reached through its
//! `httpprobe` word.
//!
//! It is not a provider to deploy. Each capability is one `httpprobe` subcommand declared through
//! the SDK's `clap` layer, each input field one kebab-case flag: `httpprobe fetch --uri <URI>`
//! proposes `http-probe.fetch`, `httpprobe conditional-write --uri <URI> --expected-etag <ETAG>`
//! proposes the two-call write, and `httpprobe purge --uri <URI>` proposes the delete. The dispatch
//! assembles exactly the input object `invoke` reads, with an optional field present only when its
//! flag was given.
//!
//! `fetch` also takes one of `--bearer <DRN>` or `--basic <USER> <DRN>`. Neither is an input field:
//! each proposes secret use, so the public DRN leaves on the proposal's `secret_use` for the broker
//! to authorize and never appears in the input `invoke` reads.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use dekopon_provider_http::{Header, Request, method};
use dekopon_provider_sdk::clap::{Arg, ArgAction, ArgMatches, Command};
use dekopon_provider_sdk::{
    CapabilityId, CommandInvocation, CommandRun, EffectKind, Provider, ProviderApiVersion,
    ProviderCapability, ProviderError, ProviderManifest, RiskLevel, SecretDrn, SecretUseProposal,
    cli,
};
use serde_json::{Map, Value, json};

/// Maximum response body this provider returns to its caller.
///
/// The broker host already bounds a provider's total serialized output, so an unbounded body field
/// would simply fail the whole invocation on a large response instead of returning the useful
/// prefix. Bounding it here keeps a big response readable and keeps base64 expansion (4 bytes out
/// per 3 bytes in) comfortably inside that ceiling.
const MAX_RETURNED_BODY_BYTES: usize = 64 * 1024;

/// Fetches one broker-authorized URI.
const FETCH: &str = "http-probe.fetch";
/// Reads a resource, then writes only if its observed etag still matches.
const CONDITIONAL_WRITE: &str = "http-probe.conditional-write";
/// Deletes one broker-authorized resource.
const PURGE: &str = "http-probe.purge";

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "provider",
        generate_all,
        pub_export_macro: true,
    });
}

struct HttpProbe;

impl Provider for HttpProbe {
    fn manifest() -> ProviderManifest {
        ProviderManifest {
            api_version: ProviderApiVersion::V1Alpha1,
            id: "http-probe".parse().expect("static provider ID is valid"),
            description: "Exercises the versioned broker HTTP import".to_owned(),
            command_words: vec!["httpprobe".to_owned()],
            capabilities: vec![
                ProviderCapability {
                    id: FETCH.parse().expect("static capability ID is valid"),
                    description: "Fetches one broker-authorized URI".to_owned(),
                    effect: EffectKind::ReadOnly,
                    risk: RiskLevel::Low,
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "uri": {"type": "string"},
                            "method": {"type": "string"},
                            "headers": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "name": {"type": "string"},
                                        "value": {"type": "string"}
                                    },
                                    "required": ["name", "value"],
                                    "additionalProperties": false
                                }
                            },
                            "body": {"type": "string"},
                            "catchError": {"type": "boolean"}
                        },
                        "required": ["uri"],
                        "additionalProperties": false
                    }),
                },
                ProviderCapability {
                    id: CONDITIONAL_WRITE
                        .parse()
                        .expect("static capability ID is valid"),
                    description:
                        "Reads a resource, then writes only if its observed etag still matches"
                            .to_owned(),
                    effect: EffectKind::ExternalWrite,
                    risk: RiskLevel::High,
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "uri": {"type": "string"},
                            "expectedEtag": {"type": "string"}
                        },
                        "required": ["uri"],
                        "additionalProperties": false
                    }),
                },
                ProviderCapability {
                    id: PURGE.parse().expect("static capability ID is valid"),
                    description: "Deletes one broker-authorized resource".to_owned(),
                    effect: EffectKind::ExternalWrite,
                    risk: RiskLevel::High,
                    input_schema: json!({
                        "type": "object",
                        "properties": {"uri": {"type": "string"}},
                        "required": ["uri"],
                        "additionalProperties": false
                    }),
                },
            ],
        }
    }

    fn invoke(capability: &CapabilityId, input: Value) -> Result<Value, ProviderError> {
        if capability.as_str() == CONDITIONAL_WRITE {
            return conditional_write(&input);
        }
        if capability.as_str() == PURGE {
            return purge(&input);
        }
        if capability.as_str() != FETCH {
            return Err(ProviderError::new(
                "unknown-capability",
                format!("unsupported capability {capability}"),
            ));
        }

        let uri = input
            .get("uri")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::new("invalid-input", "uri must be a string"))?;
        let selected_method = input
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or(method::GET);
        let mut request = Request::new(selected_method, uri)
            .map_err(|error| ProviderError::new("invalid-request", error.to_string()))?;
        if let Some(headers) = input.get("headers") {
            let headers = headers
                .as_array()
                .ok_or_else(|| ProviderError::new("invalid-input", "headers must be an array"))?;
            for header in headers {
                let name = header.get("name").and_then(Value::as_str).ok_or_else(|| {
                    ProviderError::new("invalid-input", "header name must be a string")
                })?;
                let value = header.get("value").and_then(Value::as_str).ok_or_else(|| {
                    ProviderError::new("invalid-input", "header value must be a string")
                })?;
                request =
                    request.with_header(Header::text(name, value).map_err(|error| {
                        ProviderError::new("invalid-request", error.to_string())
                    })?);
            }
        }
        if let Some(body) = input.get("body") {
            let body = body
                .as_str()
                .ok_or_else(|| ProviderError::new("invalid-input", "body must be a string"))?;
            request = request.with_body(body.as_bytes().to_vec());
        }
        let catch_error = input
            .get("catchError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match dekopon_provider_http::send(request) {
            Ok(response) => Ok(describe_response(
                response.status,
                &response.body,
                response.headers.len(),
            )),
            Err(error) if catch_error => Ok(json!({
                "caughtError": format!("{:?}", error.code)
            })),
            Err(error) => Err(ProviderError::new("http-failed", error.to_string())),
        }
    }

    fn run_command(argv: &[String], stdin: Option<&str>) -> Result<CommandRun, ProviderError> {
        cli::run_command(tree(), argv, stdin, dispatch)
    }
}

/// The `httpprobe` command tree: one subcommand per capability, one flag per input field.
fn tree() -> Command {
    Command::new("httpprobe")
        .version("0.1.0")
        .subcommand_required(true)
        .subcommand(
            Command::new("fetch")
                .about("Fetch one broker-authorized URI")
                .arg(uri())
                .arg(
                    Arg::new("method")
                        .long("method")
                        .value_name("METHOD")
                        .help("Request method token; GET when absent"),
                )
                .arg(
                    Arg::new("header")
                        .long("header")
                        .value_names(["NAME", "VALUE"])
                        .num_args(2)
                        .action(ArgAction::Append)
                        .help("One text request header; repeat for more, sent in order"),
                )
                .arg(
                    Arg::new("body")
                        .long("body")
                        .value_name("BODY")
                        .help("Buffered text request body"),
                )
                .arg(
                    Arg::new("catch-error")
                        .long("catch-error")
                        .action(ArgAction::SetTrue)
                        .help("Answer a failed request with its error code instead of failing"),
                )
                .arg(
                    Arg::new("bearer")
                        .long("bearer")
                        .value_name("DRN")
                        .conflicts_with("basic")
                        .help("Propose sending this secret DRN as a Bearer token"),
                )
                .arg(
                    Arg::new("basic")
                        .long("basic")
                        .value_names(["USER", "DRN"])
                        .num_args(2)
                        .help("Propose sending this username and secret DRN as Basic credentials"),
                ),
        )
        .subcommand(
            Command::new("conditional-write")
                .about("Read a resource, then write only if its etag still matches")
                .arg(uri())
                .arg(
                    Arg::new("expected-etag")
                        .long("expected-etag")
                        .value_name("ETAG")
                        .help("Refuse the write unless the read observes this etag"),
                ),
        )
        .subcommand(
            Command::new("purge")
                .about("Delete one broker-authorized resource")
                .arg(uri()),
        )
}

/// The `--uri` every subcommand requires.
fn uri() -> Arg {
    Arg::new("uri")
        .long("uri")
        .value_name("URI")
        .required(true)
        .help("The URI to request")
}

/// Turns clap's matches into the proposal for the selected subcommand.
///
/// Runs only after clap accepted the argv, so the subcommand and its `--uri` are present; the
/// refusals below are unreachable from the tree and name what was missing rather than panic.
fn dispatch(matches: ArgMatches, _stdin: Option<&str>) -> Result<CommandInvocation, ProviderError> {
    let (capability, input, secret_use) = match matches.subcommand() {
        Some(("fetch", fetch)) => (FETCH, fetch_input(fetch)?, fetch_secret_use(fetch)?),
        Some(("conditional-write", write)) => {
            let mut input = uri_input(write)?;
            if let Some(etag) = write.get_one::<String>("expected-etag") {
                input.insert("expectedEtag".to_owned(), json!(etag));
            }
            (CONDITIONAL_WRITE, input, None)
        }
        Some(("purge", purge)) => (PURGE, uri_input(purge)?, None),
        _ => {
            return Err(ProviderError::new(
                "usage",
                "httpprobe <fetch|conditional-write|purge>",
            ));
        }
    };
    Ok(CommandInvocation {
        capability: capability.parse().expect("static capability ID is valid"),
        input: Value::Object(input),
        secret_use,
    })
}

/// `--bearer` as a refusal names it.
const BEARER_FLAG: &str = "--bearer <DRN>";
/// `--basic` as a refusal names it.
const BASIC_FLAG: &str = "--basic <USER> <DRN>";

/// `fetch`'s secret use: the proposal `--bearer` or `--basic` asks for, and none without either.
///
/// Clap has already refused the two together and a `--basic` short of its pair; what it cannot
/// judge is the values. The DRN parses as the core [`SecretDrn`], and the Basic username is judged
/// by decoding the finished proposal through [`SecretUseProposal`]'s own deserializer, so the rule
/// refusing a name here is the one the broker decodes with rather than a copy of it. A refusal is
/// the guest's decline, which the shell reports as a usage error at exit 2. Only the message
/// reaches the model, so it names the flag.
fn fetch_secret_use(matches: &ArgMatches) -> Result<Option<SecretUseProposal>, ProviderError> {
    if let Some(drn) = matches.get_one::<String>("bearer") {
        let secret = secret_drn(BEARER_FLAG, drn)?;
        return Ok(Some(SecretUseProposal::HttpBearer { secret }));
    }
    let Some(values) = matches.get_many::<String>("basic") else {
        return Ok(None);
    };
    let values = values.collect::<Vec<_>>();
    let [username, drn] = values.as_slice() else {
        return Err(flag_usage(BASIC_FLAG, "takes one username and one DRN"));
    };
    let secret = secret_drn(BASIC_FLAG, drn)?;
    serde_json::from_value(json!({
        "kind": "httpBasic",
        "secret": secret.as_str(),
        "username": username,
    }))
    .map(Some)
    .map_err(|error| flag_usage(BASIC_FLAG, error))
}

/// Parses one flag's DRN, refusing a non-canonical one under that flag's name.
fn secret_drn(flag: &str, value: &str) -> Result<SecretDrn, ProviderError> {
    value
        .parse::<SecretDrn>()
        .map_err(|error| flag_usage(flag, error))
}

/// A decline naming the `fetch` flag whose value caused it, and why.
fn flag_usage(flag: &str, cause: impl core::fmt::Display) -> ProviderError {
    ProviderError::new("usage", format!("httpprobe fetch {flag}: {cause}"))
}

/// A subcommand's input object, starting from the `uri` every one of them requires.
fn uri_input(matches: &ArgMatches) -> Result<Map<String, Value>, ProviderError> {
    let uri = matches
        .get_one::<String>("uri")
        .ok_or_else(|| ProviderError::new("usage", "--uri <URI> is required"))?;
    let mut input = Map::new();
    input.insert("uri".to_owned(), json!(uri));
    Ok(input)
}

/// `fetch`'s input object: the `uri`, then each optional field only when its flag was given.
fn fetch_input(matches: &ArgMatches) -> Result<Map<String, Value>, ProviderError> {
    let mut input = uri_input(matches)?;
    if let Some(selected) = matches.get_one::<String>("method") {
        input.insert("method".to_owned(), json!(selected));
    }
    if let Some(values) = matches.get_many::<String>("header") {
        // `num_args(2)` makes clap hand back whole name/value pairs, in the order given.
        let values = values.collect::<Vec<_>>();
        let (pairs, _) = values.as_chunks::<2>();
        let headers = pairs
            .iter()
            .map(|[name, value]| json!({"name": name, "value": value}))
            .collect();
        input.insert("headers".to_owned(), Value::Array(headers));
    }
    if let Some(body) = matches.get_one::<String>("body") {
        input.insert("body".to_owned(), json!(body));
    }
    if matches.get_flag("catch-error") {
        input.insert("catchError".to_owned(), json!(true));
    }
    Ok(input)
}

/// Builds the probe's response summary, including a bounded copy of the body.
///
/// `body` is always base64, so any byte sequence round-trips. `bodyText` is added only when the
/// returned bytes are valid UTF-8; an invalid encoding omits the field rather than failing the
/// invocation, because a caller asking for a probe still wants the status and the raw bytes.
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

/// Returns the returnable prefix of a body, never cutting a character in half.
///
/// Slicing at a raw byte offset made `bodyText` vanish from bodies that were perfectly valid
/// UTF-8, purely because the 64 KiB mark landed mid-character — roughly three times in four for a
/// multibyte character straddling the boundary. The consumer path is `jq -r .bodyText`, so the
/// script saw a bare `null` and could not tell "this body was binary" from "I cut it badly".
fn bounded_prefix(body: &[u8]) -> &[u8] {
    if body.len() <= MAX_RETURNED_BODY_BYTES {
        return body;
    }
    let candidate = &body[..MAX_RETURNED_BODY_BYTES];
    match core::str::from_utf8(candidate) {
        Ok(_) => candidate,
        // An error with no length is an *incomplete* trailing sequence, meaning the cut split a
        // character; backing up to the last complete one keeps the body readable as text.
        // A genuinely invalid byte keeps the full prefix and omits `bodyText`, as before.
        Err(error) if error.error_len().is_none() => &candidate[..error.valid_up_to()],
        Err(_) => candidate,
    }
}

dekopon_provider_sdk::export_provider_with_cli!(HttpProbe, bindings);

/// Reads a resource and writes only if what it observed is still current.
///
/// The pre-read is the point. It exists so the broker host has an in-tree capability that makes
/// *two* authorized calls in one invocation and refuses between them, which is what exercises
/// `maxRequests`, per-call evidence, and the host-call limit. Until the GitHub provider moved to
/// its own repository, `gh.pull-request.approve` was the only capability shaped like this, and host
/// coverage of that shape should not depend on a provider that is no longer in this tree.
fn conditional_write(input: &Value) -> Result<Value, ProviderError> {
    let uri = input
        .get("uri")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::new("invalid-input", "uri is required"))?;

    let read = dekopon_provider_http::send(
        Request::new(method::GET, uri)
            .map_err(|error| ProviderError::new("invalid-request", error.to_string()))?,
    )
    .map_err(|error| ProviderError::new("http-failed", error.to_string()))?;

    let observed = read
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("etag"))
        .map(|header| String::from_utf8_lossy(&header.value).into_owned())
        .unwrap_or_default();

    // Refusing here is what makes the write conditional rather than unconditional, and it must
    // happen before the write rather than being reported after it.
    if let Some(expected) = input.get("expectedEtag").and_then(Value::as_str)
        && expected != observed
    {
        return Err(ProviderError::new(
            "precondition-failed",
            format!("resource moved: expected {expected:?}, observed {observed:?}"),
        ));
    }

    let write = dekopon_provider_http::send(
        Request::new(method::POST, uri)
            .map_err(|error| ProviderError::new("invalid-request", error.to_string()))?
            .with_header(
                Header::new("if-match", observed.clone().into_bytes())
                    .map_err(|error| ProviderError::new("invalid-request", error.to_string()))?,
            )
            .with_body(b"{}".to_vec()),
    )
    .map_err(|error| ProviderError::new("http-failed", error.to_string()))?;

    Ok(json!({
        "observedEtag": observed,
        "readStatus": read.status,
        "writeStatus": write.status,
    }))
}

/// Deletes one resource.
///
/// Exists so the manifest exposes more than any one deployment grants, which is the realistic
/// shape: `examples/conditional-write/` deliberately leaves this out of both its policy and its
/// constraint sets, and the example tests assert that an ungranted capability is refused twice
/// over — by Cedar, and by the missing constraint set before Cedar is consulted.
fn purge(input: &Value) -> Result<Value, ProviderError> {
    let uri = input
        .get("uri")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::new("invalid-input", "uri is required"))?;
    let response = dekopon_provider_http::send(
        Request::new(method::DELETE, uri)
            .map_err(|error| ProviderError::new("invalid-request", error.to_string()))?,
    )
    .map_err(|error| ProviderError::new("http-failed", error.to_string()))?;
    Ok(json!({"status": response.status}))
}

#[cfg(test)]
mod tests {
    use dekopon_provider_sdk::{
        CommandInvocation, CommandRun, EffectKind, Provider, ProviderError, SecretUseProposal,
    };
    use serde_json::{Value, json};

    use super::{
        CONDITIONAL_WRITE, FETCH, HttpProbe, MAX_RETURNED_BODY_BYTES, PURGE, describe_response,
    };

    const URI: &str = "http://127.0.0.1:8080/records/1";
    const DRN: &str = "drn:com.xrl:secret:test:http-probe/token";

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    /// The whole proposal a well-formed argv produces.
    fn invocation(words: &[&str]) -> CommandInvocation {
        let run = HttpProbe::run_command(&argv(words), None).expect("a well-formed argv proposes");
        let CommandRun::Proposal(proposed) = run else {
            panic!("expected a proposal for {words:?}, got {run:?}");
        };
        proposed
    }

    /// The capability and input an argv proposes when it names no secret.
    fn proposal(words: &[&str]) -> (String, Value) {
        let proposed = invocation(words);
        assert_eq!(proposed.secret_use, None, "{words:?}");
        (proposed.capability.as_str().to_owned(), proposed.input)
    }

    fn rendered(words: &[&str]) -> (String, String, u8) {
        let run = HttpProbe::run_command(&argv(words), None).expect("rendered, not declined");
        let CommandRun::Rendered {
            stdout,
            stderr,
            status,
        } = run
        else {
            panic!("expected rendered text for {words:?}, got {run:?}");
        };
        (stdout, stderr, status)
    }

    /// The guest's own decline of an argv clap accepted.
    fn declined(words: &[&str]) -> ProviderError {
        HttpProbe::run_command(&argv(words), None).expect_err("the guest declines")
    }

    /// Each credential flag proposes its own secret use, and the input is exactly what `fetch`
    /// sends without one: the DRN rides the proposal, never the object `invoke` reads.
    #[test]
    fn each_credential_flag_proposes_its_secret_use_beside_an_unchanged_input() {
        assert_eq!(
            invocation(&["fetch", "--uri", URI, "--bearer", DRN]),
            CommandInvocation {
                capability: FETCH.parse().expect("static capability ID is valid"),
                input: json!({"uri": URI}),
                secret_use: Some(SecretUseProposal::HttpBearer {
                    secret: DRN.parse().expect("canonical DRN fixture"),
                }),
            }
        );
        assert_eq!(
            invocation(&["fetch", "--uri", URI, "--basic", "user-a", DRN]),
            CommandInvocation {
                capability: FETCH.parse().expect("static capability ID is valid"),
                input: json!({"uri": URI}),
                secret_use: Some(SecretUseProposal::HttpBasic {
                    secret: DRN.parse().expect("canonical DRN fixture"),
                    username: "user-a".to_owned(),
                }),
            }
        );
    }

    /// A proposal names at most one secret use, so both flags together is clap's usage error, as
    /// is a `--basic` missing half its pair.
    #[test]
    fn both_credential_flags_or_half_a_basic_pair_is_a_usage_error() {
        let (stdout, stderr, status) = rendered(&[
            "fetch", "--uri", URI, "--bearer", DRN, "--basic", "user-a", DRN,
        ]);
        assert_eq!(status, 2);
        assert!(stdout.is_empty(), "{stdout:?}");
        assert!(stderr.contains("cannot be used with"), "{stderr:?}");
        assert!(stderr.contains("--bearer <DRN>"), "{stderr:?}");
        assert!(stderr.contains("--basic <USER> <DRN>"), "{stderr:?}");

        let (_, stderr, status) = rendered(&["fetch", "--uri", URI, "--basic", "user-a"]);
        assert_eq!(status, 2);
        assert!(stderr.contains("--basic <USER> <DRN>"), "{stderr:?}");
    }

    /// A value clap cannot judge is the guest's decline, which the shell reports as a usage error
    /// at exit 2; the message names the flag and the rule the value broke. The retired curl
    /// builtin's `${drn:…}` marker is not a DRN.
    #[test]
    fn a_non_canonical_drn_or_invalid_username_is_declined_naming_its_flag() {
        let cases: [(&[&str], &str, &str); 6] = [
            (
                &[
                    "fetch",
                    "--uri",
                    URI,
                    "--bearer",
                    "${drn:com.xrl:secret:test:http-probe/token}",
                ],
                "--bearer <DRN>",
                "canonical",
            ),
            (
                &[
                    "fetch",
                    "--uri",
                    URI,
                    "--bearer",
                    "drn:com.xrl:secret:Test:token",
                ],
                "--bearer <DRN>",
                "canonical",
            ),
            (
                &["fetch", "--uri", URI, "--basic", "user-a", "not-a-drn"],
                "--basic <USER> <DRN>",
                "canonical",
            ),
            (
                &["fetch", "--uri", URI, "--basic", "user:a", DRN],
                "--basic <USER> <DRN>",
                "username",
            ),
            (
                &["fetch", "--uri", URI, "--basic", "", DRN],
                "--basic <USER> <DRN>",
                "username",
            ),
            (
                &["fetch", "--uri", URI, "--basic", "line\nbreak", DRN],
                "--basic <USER> <DRN>",
                "username",
            ),
        ];
        for (words, flag, cause) in cases {
            let error = declined(words);
            assert_eq!(error.code(), "usage", "{words:?}");
            assert!(
                error
                    .message()
                    .starts_with(&format!("httpprobe fetch {flag}: ")),
                "{words:?}: {}",
                error.message()
            );
            assert!(
                error.message().contains(cause),
                "{words:?}: {}",
                error.message()
            );
        }
    }

    #[test]
    fn manifest_declares_a_read_a_conditional_write_and_a_delete() {
        let manifest = HttpProbe::manifest();
        assert_eq!(manifest.id.as_str(), "http-probe");
        assert_eq!(manifest.command_words, ["httpprobe"]);
        let declared = manifest
            .capabilities
            .iter()
            .map(|capability| capability.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(declared, vec![FETCH, CONDITIONAL_WRITE, PURGE]);
        // Every capability takes a `uri` and nothing else is required of any of them.
        for capability in &manifest.capabilities {
            assert_eq!(
                capability.input_schema["required"],
                json!(["uri"]),
                "{}",
                capability.id
            );
        }
        // The delete exists so a deployment can expose less than the manifest does; nothing in
        // this repository grants it, which examples/conditional-write/ asserts from the other side.
        let purge = manifest
            .capabilities
            .iter()
            .find(|capability| capability.id.as_str() == PURGE)
            .expect("the manifest declares a delete");
        assert_eq!(purge.effect, EffectKind::ExternalWrite);
    }

    /// Each subcommand proposes the capability of its name, and a flag that was not given leaves
    /// its field out, so the input is exactly what a caller writing the object by hand would send.
    #[test]
    fn every_subcommand_proposes_its_capability_with_exactly_the_input_invoke_reads() {
        assert_eq!(
            proposal(&["fetch", "--uri", URI]),
            (FETCH.to_owned(), json!({"uri": URI}))
        );
        assert_eq!(
            proposal(&[
                "fetch",
                "--uri",
                URI,
                "--method",
                "PUT",
                "--header",
                "accept",
                "text/plain",
                "--header",
                "x-trace",
                "a b",
                "--body",
                "{}",
                "--catch-error",
            ]),
            (
                FETCH.to_owned(),
                json!({
                    "uri": URI,
                    "method": "PUT",
                    "headers": [
                        {"name": "accept", "value": "text/plain"},
                        {"name": "x-trace", "value": "a b"}
                    ],
                    "body": "{}",
                    "catchError": true
                })
            )
        );
        assert_eq!(
            proposal(&[
                "conditional-write",
                "--uri",
                URI,
                "--expected-etag",
                "\"v1\""
            ]),
            (
                CONDITIONAL_WRITE.to_owned(),
                json!({"uri": URI, "expectedEtag": "\"v1\""})
            )
        );
        assert_eq!(
            proposal(&["conditional-write", "--uri", URI]),
            (CONDITIONAL_WRITE.to_owned(), json!({"uri": URI}))
        );
        assert_eq!(
            proposal(&["purge", "--uri", URI]),
            (PURGE.to_owned(), json!({"uri": URI}))
        );
    }

    #[test]
    fn help_lists_every_subcommand_on_stdout_at_status_zero() {
        let (stdout, stderr, status) = rendered(&["--help"]);
        assert_eq!(status, 0);
        assert!(stderr.is_empty(), "{stderr:?}");
        assert!(
            stdout.starts_with("Usage: httpprobe <COMMAND>\n"),
            "{stdout:?}"
        );
        for subcommand in ["fetch", "conditional-write", "purge"] {
            assert!(
                stdout.contains(&format!("\n  {subcommand} ")),
                "{subcommand}: {stdout:?}"
            );
        }
        assert!(!stdout.contains('\u{1b}'), "plain, never coloured");
    }

    #[test]
    fn a_missing_uri_an_unknown_subcommand_or_half_a_header_is_a_usage_error() {
        let (stdout, stderr, status) = rendered(&["purge"]);
        assert_eq!(status, 2);
        assert!(stdout.is_empty(), "{stdout:?}");
        assert!(stderr.starts_with("error: "), "{stderr:?}");
        assert!(stderr.contains("--uri <URI>"), "{stderr:?}");
        assert!(stderr.contains("Usage: httpprobe purge"), "{stderr:?}");

        let (_, stderr, status) = rendered(&["bogus"]);
        assert_eq!(status, 2);
        assert!(
            stderr.starts_with("error: unrecognized subcommand 'bogus'"),
            "{stderr:?}"
        );

        let (_, stderr, status) = rendered(&["fetch", "--uri", URI, "--header", "accept"]);
        assert_eq!(status, 2);
        assert!(stderr.contains("--header <NAME> <VALUE>"), "{stderr:?}");
    }

    #[test]
    fn utf8_bodies_are_returned_as_both_base64_and_text() {
        let described = describe_response(200, b"hello probe", 4);
        assert_eq!(described["status"], json!(200));
        assert_eq!(described["bodyBytes"], json!(11));
        assert_eq!(described["headerCount"], json!(4));
        assert_eq!(described["body"], json!("aGVsbG8gcHJvYmU="));
        assert_eq!(described["bodyText"], json!("hello probe"));
        assert_eq!(described["bodyTruncated"], json!(false));
    }

    #[test]
    fn invalid_utf8_omits_body_text_without_failing() {
        let described = describe_response(200, &[0xff, 0xfe], 1);
        assert_eq!(described["body"], json!("//4="));
        assert!(described.get("bodyText").is_none());
        assert_eq!(described["bodyBytes"], json!(2));
    }

    #[test]
    fn oversized_bodies_are_truncated_and_flagged() {
        let body = vec![b'x'; MAX_RETURNED_BODY_BYTES + 100];
        let described = describe_response(200, &body, 0);
        assert_eq!(described["bodyBytes"], json!(body.len()));
        assert_eq!(described["bodyTruncated"], json!(true));
        assert_eq!(
            described["bodyText"].as_str().map(str::len),
            Some(MAX_RETURNED_BODY_BYTES)
        );
    }

    #[test]
    fn truncation_never_cuts_a_character_in_half() {
        // An all-ASCII body can never exercise this: the cut has to land inside a multibyte
        // character, which is where a valid UTF-8 body used to lose `bodyText` entirely.
        let mut body = vec![b'x'; MAX_RETURNED_BODY_BYTES - 1];
        body.extend_from_slice("€tail".as_bytes());
        assert!(
            core::str::from_utf8(&body).is_ok(),
            "the body is valid UTF-8"
        );

        let described = describe_response(200, &body, 0);
        assert_eq!(described["bodyTruncated"], json!(true));
        let text = described["bodyText"]
            .as_str()
            .expect("a valid UTF-8 body keeps its text");
        assert_eq!(text.len(), MAX_RETURNED_BODY_BYTES - 1);
        assert!(text.ends_with('x'), "the partial character was dropped");
    }

    #[test]
    fn a_body_that_is_binary_rather_than_badly_cut_still_omits_its_text() {
        let mut body = vec![0xff_u8; MAX_RETURNED_BODY_BYTES];
        body.extend_from_slice(b"tail");
        let described = describe_response(200, &body, 0);
        assert!(described.get("bodyText").is_none());
        assert_eq!(described["bodyTruncated"], json!(true));
    }

    #[test]
    fn an_empty_body_still_reports_every_field() {
        let described = describe_response(204, b"", 0);
        assert_eq!(described["body"], json!(""));
        assert_eq!(described["bodyText"], json!(""));
        assert_eq!(described["bodyBytes"], json!(0));
    }
}
