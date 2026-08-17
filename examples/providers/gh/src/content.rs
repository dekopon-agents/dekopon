//! `gh.content.read`: one file or directory listing at a path and optional ref.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use dekopon_provider_http::{HttpError, Request, Response};
use dekopon_provider_sdk::ProviderError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    ACCEPT_JSON, MAX_CONTENT_OUT_BYTES, MAX_DIR_ENTRIES, decode, encode_path, endpoint,
    invalid_input, invalid_response, percent_encode, send_get, truncate_text, validate_login,
    validate_path, validate_ref, validate_repo,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Input {
    owner: String,
    repo: String,
    path: String,
    #[serde(default, rename = "ref")]
    reference: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
}

/// One entry of a directory listing response.
#[derive(Debug, Deserialize)]
struct RawEntry {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    size: Option<u64>,
    sha: String,
}

/// A file, symlink, or submodule response.
#[derive(Debug, Deserialize)]
struct RawContent {
    #[serde(rename = "type")]
    kind: String,
    path: String,
    sha: String,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    encoding: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    submodule_git_url: Option<String>,
}

pub(crate) fn read(
    input: Value,
    send: &mut dyn FnMut(Request) -> Result<Response, HttpError>,
) -> Result<Value, ProviderError> {
    let input = serde_json::from_value::<Input>(input).map_err(|_| invalid_input())?;
    validate_login(&input.owner)?;
    validate_repo(&input.repo)?;
    validate_path(&input.path)?;
    if let Some(reference) = input.reference.as_deref() {
        validate_ref(reference)?;
    }
    let endpoint = endpoint(input.endpoint.as_deref())?;

    let mut uri = format!(
        "{endpoint}/repos/{}/{}/contents/{}",
        percent_encode(&input.owner),
        percent_encode(&input.repo),
        encode_path(&input.path),
    );
    if let Some(reference) = input.reference.as_deref() {
        uri.push_str("?ref=");
        uri.push_str(&percent_encode(reference));
    }
    let response = send_get(send, uri, ACCEPT_JSON)?;

    // A directory is a JSON array; everything else is one object. Decide on the parsed shape
    // rather than sniffing bytes.
    let value = decode::<Value>(&response.body)?;
    if value.is_array() {
        return project_directory(&input.path, value);
    }
    let content = serde_json::from_value::<RawContent>(value).map_err(|_| invalid_response())?;
    project_content(&input.path, content)
}

fn project_directory(path: &str, value: Value) -> Result<Value, ProviderError> {
    let raw = serde_json::from_value::<Vec<RawEntry>>(value).map_err(|_| invalid_response())?;
    let truncated = raw.len() > MAX_DIR_ENTRIES;
    let entries = raw
        .into_iter()
        .take(MAX_DIR_ENTRIES)
        .map(|entry| {
            if entry.name.is_empty() || entry.name.len() > 512 || entry.sha.len() > 64 {
                return Err(invalid_response());
            }
            Ok(json!({
                "name": entry.name,
                "type": entry.kind,
                "size": entry.size,
                "sha": entry.sha,
            }))
        })
        .collect::<Result<Vec<_>, ProviderError>>()?;
    Ok(json!({
        "type": "dir",
        "path": path,
        "entries": entries,
        "entriesTruncated": truncated,
    }))
}

fn project_content(requested_path: &str, content: RawContent) -> Result<Value, ProviderError> {
    // The path echo binds the response to the request; a mismatch is a broken or hostile reply.
    if content.path != requested_path {
        return Err(invalid_response());
    }
    match content.kind.as_str() {
        "file" => project_file(content),
        "symlink" | "submodule" => {
            let target = content
                .target
                .or(content.submodule_git_url)
                .filter(|target| !target.is_empty() && target.len() <= 1024);
            Ok(json!({
                "type": content.kind,
                "path": content.path,
                "target": target,
            }))
        }
        _ => Err(invalid_response()),
    }
}

/// Decodes a file body and re-projects it bounded: UTF-8 text when valid, base64 otherwise.
fn project_file(content: RawContent) -> Result<Value, ProviderError> {
    if content.encoding.as_deref() != Some("base64") {
        return Err(invalid_response());
    }
    let raw = content.content.ok_or_else(invalid_response)?;
    // GitHub wraps base64 with newlines; strip all ASCII whitespace before decoding.
    let compact = raw
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<u8>>();
    let bytes = STANDARD.decode(&compact).map_err(|_| invalid_response())?;

    let (encoding, text, truncated) = match core::str::from_utf8(&bytes) {
        Ok(text) => {
            let (bounded, truncated) = truncate_text(text, MAX_CONTENT_OUT_BYTES);
            ("utf-8", bounded, truncated)
        }
        Err(_) => {
            let bounded = &bytes[..bytes.len().min(MAX_CONTENT_OUT_BYTES)];
            (
                "base64",
                STANDARD.encode(bounded),
                bounded.len() < bytes.len(),
            )
        }
    };

    Ok(json!({
        "type": "file",
        "path": content.path,
        "sha": content.sha,
        "size": content.size,
        "encoding": encoding,
        "content": text,
        "contentTruncated": truncated,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::invoke_with;
    use crate::testutil::{accept_of, capability, json_response, scripted, step};

    #[test]
    fn reads_a_utf8_file_with_ref_and_projects_it() {
        let encoded =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "fn main() {}\n");
        let output = invoke_with(
            &capability("gh.content.read"),
            json!({
                "owner": "octo",
                "repo": "hello",
                "path": "src/main.rs",
                "ref": "feature/x",
                "endpoint": "http://127.0.0.1:43123"
            }),
            scripted(vec![step(
                |request| {
                    assert_eq!(request.method, "GET");
                    assert_eq!(
                        request.uri,
                        "http://127.0.0.1:43123/repos/octo/hello/contents/src/main.rs?ref=feature%2Fx"
                    );
                    assert_eq!(accept_of(request), b"application/vnd.github+json");
                    assert!(request.body.is_empty());
                },
                json_response(
                    200,
                    &json!({
                        "type": "file",
                        "path": "src/main.rs",
                        "sha": "abc123",
                        "size": 13,
                        "encoding": "base64",
                        "content": format!("{encoded}\n"),
                    }),
                ),
            )]),
        )
        .expect("file read succeeds");

        assert_eq!(output["type"], "file");
        assert_eq!(output["encoding"], "utf-8");
        assert_eq!(output["content"], "fn main() {}\n");
        assert_eq!(output["contentTruncated"], false);
        assert_eq!(output["path"], "src/main.rs");
    }

    #[test]
    fn binary_files_come_back_as_bounded_base64() {
        let bytes = vec![0_u8, 159, 146, 150];
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
        let output = invoke_with(
            &capability("gh.content.read"),
            json!({"owner": "octo", "repo": "hello", "path": "logo.png"}),
            scripted(vec![step(
                |request| {
                    assert_eq!(
                        request.uri,
                        "https://api.github.com/repos/octo/hello/contents/logo.png"
                    );
                },
                json_response(
                    200,
                    &json!({
                        "type": "file",
                        "path": "logo.png",
                        "sha": "abc",
                        "size": 4,
                        "encoding": "base64",
                        "content": encoded,
                    }),
                ),
            )]),
        )
        .expect("binary read succeeds");

        assert_eq!(output["encoding"], "base64");
        assert_eq!(output["contentTruncated"], false);
        let decoded = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            output["content"].as_str().expect("content is a string"),
        )
        .expect("content decodes");
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn oversize_text_truncates_on_the_exact_boundary() {
        let text = "a".repeat(crate::MAX_CONTENT_OUT_BYTES + 1);
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &text);
        let output = invoke_with(
            &capability("gh.content.read"),
            json!({"owner": "octo", "repo": "hello", "path": "big.txt"}),
            scripted(vec![step(
                |_| {},
                json_response(
                    200,
                    &json!({
                        "type": "file",
                        "path": "big.txt",
                        "sha": "abc",
                        "size": text.len(),
                        "encoding": "base64",
                        "content": encoded,
                    }),
                ),
            )]),
        )
        .expect("oversize read succeeds");

        assert_eq!(output["contentTruncated"], true);
        assert_eq!(
            output["content"].as_str().expect("string").len(),
            crate::MAX_CONTENT_OUT_BYTES
        );
    }

    #[test]
    fn directory_listings_are_bounded_projections() {
        let entries = (0..3)
            .map(|index| {
                json!({
                    "name": format!("file-{index}.rs"),
                    "type": "file",
                    "size": 10,
                    "sha": format!("sha-{index}"),
                    "url": "https://ignored.example/should-not-matter",
                })
            })
            .collect::<Vec<_>>();
        let output = invoke_with(
            &capability("gh.content.read"),
            json!({"owner": "octo", "repo": "hello", "path": "src"}),
            scripted(vec![step(|_| {}, json_response(200, &json!(entries)))]),
        )
        .expect("directory read succeeds");

        assert_eq!(output["type"], "dir");
        assert_eq!(output["entries"].as_array().expect("entries").len(), 3);
        assert_eq!(output["entries"][1]["name"], "file-1.rs");
        assert_eq!(output["entriesTruncated"], false);
    }

    #[test]
    fn empty_path_lists_the_repository_root() {
        invoke_with(
            &capability("gh.content.read"),
            json!({"owner": "octo", "repo": "hello", "path": ""}),
            scripted(vec![step(
                |request| {
                    assert_eq!(
                        request.uri,
                        "https://api.github.com/repos/octo/hello/contents/"
                    );
                },
                json_response(200, &json!([])),
            )]),
        )
        .expect("root listing succeeds");
    }

    #[test]
    fn path_echo_mismatch_is_invalid_response() {
        let error = invoke_with(
            &capability("gh.content.read"),
            json!({"owner": "octo", "repo": "hello", "path": "a.txt"}),
            scripted(vec![step(
                |_| {},
                json_response(
                    200,
                    &json!({
                        "type": "file",
                        "path": "b.txt",
                        "sha": "abc",
                        "size": 1,
                        "encoding": "base64",
                        "content": "eA==",
                    }),
                ),
            )]),
        )
        .expect_err("mismatched echo fails");
        assert_eq!(error.code(), "invalid-response");
    }

    #[test]
    fn missing_files_are_not_found() {
        let error = invoke_with(
            &capability("gh.content.read"),
            json!({"owner": "octo", "repo": "hello", "path": "gone.txt"}),
            scripted(vec![step(|_| {}, json_response(404, &json!({})))]),
        )
        .expect_err("missing file fails");
        assert_eq!(error.code(), "not-found");
    }
}
