use dekopon_model_token_governor::ModelUsage;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ResponsesUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
    #[serde(default)]
    input_tokens_details: Option<InputTokensDetails>,
    #[serde(default)]
    output_tokens_details: Option<OutputTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct InputTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OutputTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

impl From<ResponsesUsage> for ModelUsage {
    fn from(usage: ResponsesUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            cache_write_tokens: None,
            cached_input_tokens: usage
                .input_tokens_details
                .and_then(|details| details.cached_tokens),
            output_tokens: usage.output_tokens,
            reasoning_output_tokens: usage
                .output_tokens_details
                .and_then(|details| details.reasoning_tokens),
            total_tokens: usage.total_tokens,
        }
    }
}

/// `CacheWrite` is `u64` only for OpenRouter: compatible endpoints put arbitrary values there.
#[derive(Debug, Deserialize)]
pub struct ChatUsage<CacheWrite = serde::de::IgnoredAny> {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails<CacheWrite>>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct PromptTokensDetails<CacheWrite> {
    cached_tokens: Option<u64>,
    cache_write_tokens: Option<CacheWrite>,
}

#[derive(Debug, Deserialize)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

impl<CacheWrite> From<ChatUsage<CacheWrite>> for ModelUsage {
    fn from(usage: ChatUsage<CacheWrite>) -> Self {
        Self {
            input_tokens: usage.prompt_tokens,
            cache_write_tokens: None,
            cached_input_tokens: usage
                .prompt_tokens_details
                .and_then(|details| details.cached_tokens),
            output_tokens: usage.completion_tokens,
            reasoning_output_tokens: usage
                .completion_tokens_details
                .and_then(|details| details.reasoning_tokens),
            total_tokens: usage.total_tokens,
        }
    }
}

impl ChatUsage<u64> {
    #[must_use]
    pub fn into_openrouter(self) -> ModelUsage {
        let cache_write_tokens = self
            .prompt_tokens_details
            .as_ref()
            .and_then(|details| details.cache_write_tokens);
        ModelUsage {
            cache_write_tokens,
            ..self.into()
        }
    }
}

/// Anthropic Messages usage, from `message_start` or `message_delta`. Its `input_tokens` excludes
/// cache reads and writes, so normalization adds them back.
#[derive(Debug, Deserialize)]
pub struct MessagesUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

impl From<MessagesUsage> for ModelUsage {
    fn from(usage: MessagesUsage) -> Self {
        let input_tokens = usage.input_tokens.map(|input| {
            input
                .saturating_add(usage.cache_creation_input_tokens.unwrap_or(0))
                .saturating_add(usage.cache_read_input_tokens.unwrap_or(0))
        });
        Self {
            input_tokens,
            cached_input_tokens: usage.cache_read_input_tokens,
            cache_write_tokens: usage.cache_creation_input_tokens,
            output_tokens: usage.output_tokens,
            reasoning_output_tokens: None,
            total_tokens: input_tokens
                .zip(usage.output_tokens)
                .map(|(input, output)| input.saturating_add(output)),
        }
    }
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum PeekError {
    #[error("request body is not JSON")]
    NotJson,
    #[error("request body is not a JSON object")]
    NotObject,
    #[error("request names no model")]
    NoModel,
}

/// What a proxy reads from a request before forwarding it: the configured model it names, its
/// stream flag, its image count, and where each top-level field sits so a rewrite can leave every
/// other byte as the client sent it.
#[derive(Debug)]
pub struct RequestPeek {
    pub model: String,
    pub stream: Option<bool>,
    pub images: usize,
    fields: Vec<(String, std::ops::Range<usize>)>,
}

impl RequestPeek {
    pub fn of(body: &[u8]) -> Result<Self, PeekError> {
        let value = serde_json::from_slice::<serde_json::Value>(body)
            .map_err(|_invalid| PeekError::NotJson)?;
        let object = value.as_object().ok_or(PeekError::NotObject)?;
        let model = object
            .get("model")
            .and_then(serde_json::Value::as_str)
            .ok_or(PeekError::NoModel)?
            .to_owned();
        Ok(Self {
            model,
            stream: object.get("stream").and_then(serde_json::Value::as_bool),
            images: images(&value),
            fields: top_level_fields(body).ok_or(PeekError::NotObject)?,
        })
    }

    /// Replaces each named top-level value with the given raw JSON, inserting a field the request
    /// lacks at the front of the object.
    #[must_use]
    pub fn rewrite(&self, body: &[u8], replacements: &[(&str, &str)]) -> Vec<u8> {
        let mut edits = Vec::new();
        let mut inserted = String::new();
        for (key, raw) in replacements {
            match self.fields.iter().find(|(field, _)| field == key) {
                Some((_, range)) => edits.push((range.clone(), *raw)),
                None => {
                    inserted.push_str(&serde_json::Value::from(*key).to_string());
                    inserted.push(':');
                    inserted.push_str(raw);
                    inserted.push(',');
                }
            }
        }
        edits.sort_by_key(|(range, _)| range.start);
        let mut out = Vec::with_capacity(body.len() + inserted.len() + 64);
        let open = body
            .iter()
            .position(|byte| *byte == b'{')
            .map_or(0, |index| index + 1);
        out.extend_from_slice(body.get(..open).unwrap_or_default());
        if self.fields.is_empty() {
            out.extend_from_slice(inserted.trim_end_matches(',').as_bytes());
        } else {
            out.extend_from_slice(inserted.as_bytes());
        }
        let mut cursor = open;
        for (range, raw) in edits {
            out.extend_from_slice(body.get(cursor..range.start).unwrap_or_default());
            out.extend_from_slice(raw.as_bytes());
            cursor = range.end;
        }
        out.extend_from_slice(body.get(cursor..).unwrap_or_default());
        out
    }
}

fn images(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Object(object) => {
            let own = usize::from(matches!(
                object.get("type").and_then(serde_json::Value::as_str),
                Some("image" | "image_url" | "input_image")
            ));
            own + object.values().map(images).sum::<usize>()
        }
        serde_json::Value::Array(items) => items.iter().map(images).sum(),
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => 0,
    }
}

/// Walks a body serde_json already accepted, so every step is on valid JSON.
fn top_level_fields(body: &[u8]) -> Option<Vec<(String, std::ops::Range<usize>)>> {
    let mut fields = Vec::new();
    let mut at = skip_space(body, 0);
    if *body.get(at)? != b'{' {
        return None;
    }
    at = skip_space(body, at + 1);
    while *body.get(at)? != b'}' {
        let key_end = string_end(body, at)?;
        let key = serde_json::from_slice::<String>(body.get(at..key_end)?).ok()?;
        at = skip_space(body, key_end);
        at = skip_space(body, at + 1);
        let value_end = value_end(body, at)?;
        fields.push((key, at..value_end));
        at = skip_space(body, value_end);
        if *body.get(at)? == b',' {
            at = skip_space(body, at + 1);
        }
    }
    Some(fields)
}

fn skip_space(body: &[u8], mut at: usize) -> usize {
    while body.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    at
}

fn string_end(body: &[u8], start: usize) -> Option<usize> {
    let mut at = start + 1;
    loop {
        match *body.get(at)? {
            b'\\' => at += 2,
            b'"' => return Some(at + 1),
            _ => at += 1,
        }
    }
}

fn value_end(body: &[u8], start: usize) -> Option<usize> {
    match *body.get(start)? {
        b'"' => string_end(body, start),
        b'{' | b'[' => {
            let mut depth = 0_usize;
            let mut at = start;
            loop {
                match *body.get(at)? {
                    b'"' => {
                        at = string_end(body, at)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(at + 1);
                        }
                    }
                    _ => {}
                }
                at += 1;
            }
        }
        _ => {
            let mut at = start;
            while body.get(at).is_some_and(|byte| {
                !matches!(byte, b',' | b'}' | b']') && !byte.is_ascii_whitespace()
            }) {
                at += 1;
            }
            Some(at)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn usage<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn responses_usage_carries_cached_input_and_reasoning() {
        let parsed: ResponsesUsage = usage(json!({
            "input_tokens": 120, "output_tokens": 30, "total_tokens": 150,
            "input_tokens_details": {"cached_tokens": 100},
            "output_tokens_details": {"reasoning_tokens": 7}
        }));
        assert_eq!(
            ModelUsage::from(parsed),
            ModelUsage {
                input_tokens: Some(120),
                cached_input_tokens: Some(100),
                cache_write_tokens: None,
                output_tokens: Some(30),
                reasoning_output_tokens: Some(7),
                total_tokens: Some(150),
            }
        );
    }

    #[test]
    fn only_openrouter_reads_cache_writes_from_chat_usage() {
        let body = json!({
            "prompt_tokens": 11, "completion_tokens": 4, "total_tokens": 15,
            "prompt_tokens_details": {"cached_tokens": 3, "cache_write_tokens": 5}
        });
        let compatible = ModelUsage::from(usage::<ChatUsage>(body.clone()));
        assert_eq!(compatible.cache_write_tokens, None);
        assert_eq!(compatible.cached_input_tokens, Some(3));
        let openrouter = usage::<ChatUsage<u64>>(body).into_openrouter();
        assert_eq!(openrouter.cache_write_tokens, Some(5));
        assert_eq!(openrouter.input_tokens, Some(11));
    }

    #[test]
    fn messages_input_includes_cache_reads_and_writes() {
        let start: MessagesUsage = usage(json!({
            "input_tokens": 25, "output_tokens": 1,
            "cache_creation_input_tokens": 2_000, "cache_read_input_tokens": 30_000,
            "cache_creation": {"ephemeral_5m_input_tokens": 2_000, "ephemeral_1h_input_tokens": 0},
            "server_tool_use": {"web_search_requests": 0},
            "service_tier": "standard"
        }));
        assert_eq!(
            ModelUsage::from(start),
            ModelUsage {
                input_tokens: Some(32_025),
                cached_input_tokens: Some(30_000),
                cache_write_tokens: Some(2_000),
                output_tokens: Some(1),
                reasoning_output_tokens: None,
                total_tokens: Some(32_026),
            }
        );
    }

    #[test]
    fn a_messages_delta_reports_output_alone() {
        let delta: MessagesUsage = usage(json!({"output_tokens": 503}));
        let delta = ModelUsage::from(delta);
        assert_eq!(delta.input_tokens, None);
        assert_eq!(delta.output_tokens, Some(503));
        assert_eq!(delta.total_tokens, None);
    }

    #[test]
    fn a_rewrite_changes_only_the_named_fields() {
        let body = br#"{ "model" : "astra",
  "messages":[{"role":"user","content":[{"type":"image","source":{}},{"type":"text","text":"a \"model\": b"}]}],
  "stream":true }"#;
        let peek = RequestPeek::of(body).unwrap();
        assert_eq!(peek.model, "astra");
        assert_eq!(peek.stream, Some(true));
        assert_eq!(peek.images, 1);
        let rewritten = peek.rewrite(body, &[("model", "\"gpt-5\""), ("store", "false")]);
        let expected = br#"{"store":false, "model" : "gpt-5",
  "messages":[{"role":"user","content":[{"type":"image","source":{}},{"type":"text","text":"a \"model\": b"}]}],
  "stream":true }"#;
        assert_eq!(
            String::from_utf8(rewritten).unwrap(),
            String::from_utf8(expected.to_vec()).unwrap()
        );
        assert_eq!(peek.rewrite(body, &[("stream", "true")]), body);
    }

    #[test]
    fn a_peek_refuses_what_it_cannot_route() {
        assert_eq!(RequestPeek::of(b"[1]").unwrap_err(), PeekError::NotObject);
        assert_eq!(
            RequestPeek::of(b"{\"x\":1}").unwrap_err(),
            PeekError::NoModel
        );
        assert_eq!(RequestPeek::of(b"{").unwrap_err(), PeekError::NotJson);
    }
}
