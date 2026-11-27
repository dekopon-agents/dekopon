use std::time::{SystemTime, UNIX_EPOCH};

use dekopon_capability::SecretUseGrant;
use dekopon_core::SecretBytes;
use reqwest::{Method, Url};
use serde::{Deserialize, Serialize};

use super::{
    ByteLimitKey, ErrorCode, HttpError, PreparedRequest, Response, byte_limit_error, http_error,
    secret_allows_request,
};

const MAX_ARGV: usize = 70;
const MAX_ARGV_AND_STDIN_BYTES: usize = 24_576;

#[derive(Clone)]
pub(super) struct VmExecStdin {
    pub(super) grant: SecretUseGrant,
    pub(super) token: SecretBytes,
    jti: String,
    exp: u64,
    helper: String,
    phase: Phase,
}

#[derive(Clone)]
enum Phase {
    Create,
    AwaitingSession(Url),
    Exec(Url),
    Spent,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    session_id: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Exec {
    argv: Vec<String>,
    deadline_ms: u32,
}

#[derive(Serialize)]
struct ExecWithStdin<'a> {
    #[serde(flatten)]
    exec: Exec,
    stdin: &'a str,
}

impl VmExecStdin {
    pub(super) fn new(
        grant: SecretUseGrant,
        token: SecretBytes,
        jti: String,
        exp: u64,
        helper: String,
    ) -> Self {
        Self {
            grant,
            token,
            jti,
            exp,
            helper,
            phase: Phase::Create,
        }
    }

    pub(super) fn authorize(
        &mut self,
        request: &mut PreparedRequest,
        streamed: bool,
        max_request_bytes: u64,
    ) -> Result<(), HttpError> {
        if streamed || request.method != Method::POST || request.url.query().is_some() {
            return Err(denied());
        }
        match &self.phase {
            Phase::Create if request.url.path().ends_with("/v1/sessions") => {
                self.phase = Phase::AwaitingSession(request.url.clone());
            }
            Phase::Exec(url)
                if url == &request.url && secret_allows_request(&self.grant, request) =>
            {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|source| http_error(ErrorCode::Internal, source.to_string()))?;
                if now.as_secs() >= self.exp {
                    return Err(http_error(ErrorCode::Protocol, "signed exec token expired"));
                }
                let body = exec_body(&request.body, &self.token, &self.helper)?;
                let request_bytes =
                    request.request_bytes - request.body.len() as u64 + body.len() as u64;
                if request_bytes > max_request_bytes {
                    return Err(byte_limit_error(
                        ByteLimitKey::MaxRequestBytes,
                        max_request_bytes,
                        request_bytes,
                        "request exceeds the authorized byte limit",
                    ));
                }
                request.body = body;
                self.phase = Phase::Spent;
                tracing::Span::current().record("secret.jti", self.jti.as_str());
            }
            Phase::Create | Phase::AwaitingSession(_) | Phase::Exec(_) | Phase::Spent => {
                return Err(denied());
            }
        }
        Ok(())
    }

    pub(super) fn observe_response(&mut self, response: &Response) {
        if let Phase::AwaitingSession(mut url) = std::mem::replace(&mut self.phase, Phase::Spent) {
            if !matches!(response.status, 200 | 201) {
                return;
            }
            if let Ok(Session { session_id }) = serde_json::from_slice(&response.body)
                && (1..=128).contains(&session_id.len())
                && session_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                let path = format!("{}/{session_id}/exec", url.path());
                url.set_path(&path);
                self.phase = Phase::Exec(url);
            }
        }
    }
}

fn exec_body(body: &[u8], token: &SecretBytes, helper: &str) -> Result<Vec<u8>, HttpError> {
    let exec: Exec = serde_json::from_slice(body).map_err(|error| {
        http_error(
            ErrorCode::Protocol,
            format!(
                "signed exec requires argv and deadlineMs without stdin ({:?})",
                error.classify()
            ),
        )
    })?;
    if exec.argv.first().is_none_or(String::is_empty) {
        return Err(http_error(
            ErrorCode::Protocol,
            "signed exec requires a command",
        ));
    }
    if exec.argv.first().map(String::as_str) != Some(helper) {
        return Err(http_error(
            ErrorCode::Denied,
            "signed exec command is not the pinned helper",
        ));
    }
    let argv_bytes = exec.argv.iter().map(String::len).sum::<usize>();
    let stdin = std::str::from_utf8(token.expose())
        .map_err(|error| http_error(ErrorCode::Internal, error.to_string()))?;
    if exec.argv.len() > MAX_ARGV || argv_bytes + stdin.len() > MAX_ARGV_AND_STDIN_BYTES {
        return Err(http_error(
            ErrorCode::RequestTooLarge,
            "signed exec exceeds the argv and stdin limits",
        ));
    }
    serde_json::to_vec(&ExecWithStdin { exec, stdin }).map_err(|error| {
        http_error(
            ErrorCode::Internal,
            format!(
                "signed exec body could not be encoded ({:?})",
                error.classify()
            ),
        )
    })
}

fn denied() -> HttpError {
    http_error(
        ErrorCode::Denied,
        "request is outside the signed exec sequence",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn signed_exec_counts_the_token_with_actual_utf8_argv_and_refuses_caller_stdin() {
        let token = SecretBytes::new(vec![b'x'; 1_024]);
        let over_budget =
            json!({"argv": ["login-fixture", "é".repeat(12_000)], "deadlineMs": 25_000});
        let invalid =
            exec_body(over_budget.to_string().as_bytes(), &token, "login-fixture").unwrap_err();
        assert_eq!(invalid.code, ErrorCode::RequestTooLarge);

        let piped =
            json!({"argv": ["login-fixture"], "deadlineMs": 25_000, "stdin": "caller-data"});
        let invalid = exec_body(piped.to_string().as_bytes(), &token, "login-fixture").unwrap_err();
        assert_eq!(invalid.code, ErrorCode::Protocol);
    }

    #[test]
    fn signed_exec_refuses_a_command_other_than_the_pinned_helper() {
        let token = SecretBytes::new(vec![b'x'; 16]);
        let other = json!({"argv": ["other-helper"], "deadlineMs": 25_000});
        let denied = exec_body(other.to_string().as_bytes(), &token, "login-fixture").unwrap_err();
        assert_eq!(denied.code, ErrorCode::Denied);
    }
}
