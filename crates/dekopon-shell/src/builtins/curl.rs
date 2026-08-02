//! The `curl` builtin: a flag parser, not an HTTP client.
//!
//! This crate links no HTTP client and opens no socket. `curl` translates curl-style flags into the
//! `{uri, method, headers, body}` JSON shape and hands that to one capability through the exact same
//! [`CapabilityInvoker::invoke`](crate::CapabilityInvoker::invoke) path `cap <id> {...}` uses.
//! The only thing in this project permitted to speak HTTP on the wire is the existing
//! `dekopon:http@1.0.0` contract, reached through a capability such as `http-probe.fetch`.
//!
//! Which capability that is, is the embedder's choice, not the script's: the target is fixed for
//! the whole execution and a script cannot redirect it.
//!
//! Phase 1 wiring note: `dekopon-run shell` is direct mode, whose Wasmtime linker is empty by
//! design and can never instantiate an HTTP-importing component. `curl` therefore cannot make a
//! real network call there, and that is correct rather than a gap. Real broker-backed HTTP arrives
//! with the broker-backed runner path in a later phase.

use serde_json::{Value, json};

use super::{Builtin, BuiltinContext, CommandFailure, CommandResult, unsupported_flag};

/// `curl [-X METHOD] [-H "Name: value"]... [-d BODY] URL`.
pub(crate) struct Curl;

impl Builtin for Curl {
    fn name(&self) -> &'static str {
        "curl"
    }

    fn run(
        &self,
        context: &mut BuiltinContext<'_>,
        arguments: &[String],
        _input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let request = parse(arguments)?;
        let Some(capability) = context.curl_capability else {
            return Err(CommandFailure::Status {
                message: "curl: command not found: no HTTP capability is available to this session"
                    .to_owned(),
                status: crate::ExitCode::NOT_FOUND,
            });
        };
        let capability = capability.to_owned();
        context.invoke_capability(&capability, request)
    }
}

/// Parses curl-style flags into the buffered-HTTP request shape.
///
/// Only a bare URL, `-X`/`--request`, repeatable `-H`/`--header`, and `-d`/`--data`/`--data-raw`
/// are supported. Everything else is an explicit error: silently accepting `-o` or `-L` would let a
/// script believe it wrote a file or followed a redirect when neither happened.
pub(crate) fn parse(arguments: &[String]) -> Result<Value, CommandFailure> {
    let mut uri: Option<String> = None;
    let mut method: Option<String> = None;
    let mut headers: Vec<Value> = Vec::new();
    let mut body: Option<String> = None;

    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        match argument {
            "-X" | "--request" => {
                let value = take_value(arguments, &mut index, argument)?;
                method = Some(value.to_ascii_uppercase());
            }
            "-H" | "--header" => {
                let value = take_value(arguments, &mut index, argument)?;
                headers.push(parse_header(&value)?);
            }
            "-d" | "--data" | "--data-raw" => {
                let value = take_value(arguments, &mut index, argument)?;
                body = Some(value);
            }
            flag if flag.starts_with('-') && flag.len() > 1 => {
                return Err(unsupported_flag("curl", flag));
            }
            positional => {
                if uri.is_some() {
                    return Err(CommandFailure::usage(
                        "curl: exactly one URL argument is supported",
                    ));
                }
                uri = Some(positional.to_owned());
                index += 1;
            }
        }
    }

    let Some(uri) = uri else {
        return Err(CommandFailure::usage("curl: a URL argument is required"));
    };

    // Real curl infers POST from a request body when no method was given; matching that keeps
    // `curl -d '{"a":1}' https://host` doing what a model expects.
    let method = method.unwrap_or_else(|| {
        if body.is_some() {
            "POST".to_owned()
        } else {
            "GET".to_owned()
        }
    });

    let mut request = json!({
        "uri": uri,
        "method": method,
        "headers": Value::Array(headers),
    });
    if let Some(body) = body {
        if let Some(fields) = request.as_object_mut() {
            fields.insert("body".to_owned(), Value::String(body));
        }
    }
    Ok(request)
}

fn take_value(
    arguments: &[String],
    index: &mut usize,
    flag: &str,
) -> Result<String, CommandFailure> {
    let Some(value) = arguments.get(*index + 1) else {
        return Err(CommandFailure::usage(format!(
            "curl: {flag} requires a value"
        )));
    };
    *index += 2;
    Ok(value.clone())
}

fn parse_header(header: &str) -> Result<Value, CommandFailure> {
    let Some((name, value)) = header.split_once(':') else {
        return Err(CommandFailure::usage(format!(
            "curl: header {header:?} must be formatted as \"Name: value\""
        )));
    };
    let name = name.trim();
    if name.is_empty() {
        return Err(CommandFailure::usage(format!(
            "curl: header {header:?} has an empty field name"
        )));
    }
    Ok(json!({"name": name, "value": value.trim()}))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::{Value, json};

    use crate::{
        CapabilityCallResult, CapabilityInvoker, ExitCode,
        builtins::{Builtin, BuiltinContext, CommandFailure},
        limits::{Budget, Limits},
    };

    use super::{Curl, parse};

    #[derive(Default)]
    struct RecordingInvoker {
        calls: std::cell::RefCell<Vec<(String, Value)>>,
    }

    impl CapabilityInvoker for RecordingInvoker {
        fn granted(&self) -> Vec<String> {
            vec!["http-probe.fetch".to_owned()]
        }

        fn invoke(&self, capability: &str, input: Value) -> CapabilityCallResult {
            self.calls
                .borrow_mut()
                .push((capability.to_owned(), input.clone()));
            CapabilityCallResult::Succeeded(json!({"status": 200}))
        }
    }

    fn arguments(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn a_bare_url_becomes_a_get_request() {
        assert_eq!(
            parse(&arguments(&["https://example.test/a"])).expect("valid arguments"),
            json!({"uri": "https://example.test/a", "method": "GET", "headers": []})
        );
    }

    #[test]
    fn headers_are_repeatable_and_trimmed() {
        assert_eq!(
            parse(&arguments(&[
                "-H",
                "Accept: application/json",
                "--header",
                "X-Trace:abc",
                "https://example.test/",
            ]))
            .expect("valid arguments"),
            json!({
                "uri": "https://example.test/",
                "method": "GET",
                "headers": [
                    {"name": "Accept", "value": "application/json"},
                    {"name": "X-Trace", "value": "abc"},
                ],
            })
        );
    }

    #[test]
    fn a_body_infers_post_but_never_overrides_an_explicit_method() {
        assert_eq!(
            parse(&arguments(&["-d", "{\"a\":1}", "https://example.test/"]))
                .expect("valid arguments")["method"],
            json!("POST")
        );
        assert_eq!(
            parse(&arguments(&[
                "-X",
                "put",
                "--data-raw",
                "payload",
                "https://example.test/",
            ]))
            .expect("valid arguments"),
            json!({
                "uri": "https://example.test/",
                "method": "PUT",
                "headers": [],
                "body": "payload",
            })
        );
    }

    #[test]
    fn unsupported_flags_are_reported_not_ignored() {
        for flag in [
            "-o",
            "-L",
            "-i",
            "--include",
            "-s",
            "--silent",
            "-f",
            "--fail",
            "-k",
        ] {
            let failure = parse(&arguments(&[flag, "https://example.test/"]))
                .expect_err("unsupported flags must fail");
            let CommandFailure::Status { message, status } = failure else {
                panic!("an unsupported flag must stay recoverable");
            };
            assert!(message.contains("option not yet supported"), "{message}");
            assert!(message.contains(flag), "{message}");
            assert_eq!(status, ExitCode::SYNTAX);
        }
    }

    #[test]
    fn malformed_arguments_are_reported_cleanly() {
        assert!(matches!(
            parse(&arguments(&[])),
            Err(CommandFailure::Status { .. })
        ));
        assert!(matches!(
            parse(&arguments(&["-X"])),
            Err(CommandFailure::Status { .. })
        ));
        assert!(matches!(
            parse(&arguments(&["-H", "no-colon", "https://example.test/"])),
            Err(CommandFailure::Status { .. })
        ));
        assert!(matches!(
            parse(&arguments(&["https://a.test/", "https://b.test/"])),
            Err(CommandFailure::Status { .. })
        ));
    }

    #[test]
    fn requests_are_delivered_through_the_ordinary_capability_seam() {
        let invoker = RecordingInvoker::default();
        let mut budget = Budget::start(Limits::default());
        let mut buffers = BTreeMap::new();
        let mut context = BuiltinContext {
            invoker: &invoker,
            budget: &mut budget,
            buffers: &mut buffers,
            curl_capability: Some("http-probe.fetch"),
        };

        let result = Curl
            .run(
                &mut context,
                &arguments(&["-H", "Accept: application/json", "https://example.test/"]),
                None,
            )
            .expect("the capability call succeeds");

        assert_eq!(result.value, json!({"status": 200}));
        let calls = invoker.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "http-probe.fetch");
        assert_eq!(calls[0].1["uri"], json!("https://example.test/"));
        assert_eq!(budget.capability_calls(), 1);
    }

    #[test]
    fn without_a_configured_capability_curl_is_command_not_found() {
        let invoker = RecordingInvoker::default();
        let mut budget = Budget::start(Limits::default());
        let mut buffers = BTreeMap::new();
        let mut context = BuiltinContext {
            invoker: &invoker,
            budget: &mut budget,
            buffers: &mut buffers,
            curl_capability: None,
        };

        let failure = Curl
            .run(&mut context, &arguments(&["https://example.test/"]), None)
            .expect_err("no capability is configured");
        let CommandFailure::Status { message, status } = failure else {
            panic!("an unavailable capability must stay recoverable");
        };
        assert!(message.contains("command not found"), "{message}");
        assert_eq!(status, ExitCode::NOT_FOUND);
        assert!(invoker.calls.borrow().is_empty());
    }
}
