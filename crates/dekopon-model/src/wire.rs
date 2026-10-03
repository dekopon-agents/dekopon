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
    #[error("request repeats the top-level key `{0}`")]
    Duplicate(Field),
}

/// The top-level keys a proxy reads or edits. Every other key is skipped without being stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Field {
    Model,
    Stream,
    Store,
    Models,
    Route,
}

impl Field {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Stream => "stream",
            Self::Store => "store",
            Self::Models => "models",
            Self::Route => "route",
        }
    }
}

impl std::fmt::Display for Field {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "lowercase")]
enum TopKey {
    Model,
    Stream,
    Store,
    Models,
    Route,
    #[serde(other)]
    Other,
}

impl TopKey {
    const fn field(self) -> Option<Field> {
        match self {
            Self::Model => Some(Field::Model),
            Self::Stream => Some(Field::Stream),
            Self::Store => Some(Field::Store),
            Self::Models => Some(Field::Models),
            Self::Route => Some(Field::Route),
            Self::Other => None,
        }
    }
}

/// What a proxy reads from a request before forwarding it: the configured model it names, its
/// stream flag, its image count, and where each recorded top-level field sits so a rewrite can
/// leave every other byte as the client sent it.
#[derive(Debug)]
pub struct RequestPeek {
    pub model: String,
    pub stream: Option<bool>,
    pub images: usize,
    fields: Vec<(Field, std::ops::Range<usize>)>,
}

impl RequestPeek {
    pub fn of(body: &[u8]) -> Result<Self, PeekError> {
        let images = serde_json::from_slice::<Scan>(body)
            .map_err(|_invalid| PeekError::NotJson)?
            .images;
        let fields = top_level_fields(body)?;
        let raw = |wanted: Field| {
            fields
                .iter()
                .find(|(field, _)| *field == wanted)
                .and_then(|(_, range)| body.get(range.clone()))
        };
        let model = raw(Field::Model)
            .and_then(|raw| serde_json::from_slice::<String>(raw).ok())
            .ok_or(PeekError::NoModel)?;
        let stream = raw(Field::Stream).and_then(|raw| serde_json::from_slice::<bool>(raw).ok());
        Ok(Self {
            model,
            stream,
            images,
            fields,
        })
    }

    #[must_use]
    pub fn has(&self, wanted: Field) -> bool {
        self.fields.iter().any(|(field, _)| *field == wanted)
    }

    /// Replaces each named top-level value with the given raw JSON, inserting a field the request
    /// lacks at the front of the object, which always holds `model`.
    #[must_use]
    pub fn rewrite(&self, body: &[u8], replacements: &[(Field, &str)]) -> Vec<u8> {
        let mut edits = Vec::new();
        let mut inserted = String::new();
        for (wanted, raw) in replacements {
            match self.fields.iter().find(|(field, _)| field == wanted) {
                Some((_, range)) => edits.push((range.clone(), *raw)),
                None => {
                    inserted.push('"');
                    inserted.push_str(wanted.name());
                    inserted.push_str("\":");
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
        out.extend_from_slice(inserted.as_bytes());
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

/// Validates the body and counts its image blocks in one pass; a `serde_json::Value` tree of the
/// same body costs many times its size.
#[derive(Default)]
struct Scan {
    images: usize,
    image_type: bool,
}

#[derive(Deserialize, PartialEq)]
#[serde(field_identifier, rename_all = "lowercase")]
enum Key {
    Type,
    #[serde(other)]
    Other,
}

impl<'de> Deserialize<'de> for Scan {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ScanVisitor)
    }
}

struct ScanVisitor;

impl<'de> serde::de::Visitor<'de> for ScanVisitor {
    type Value = Scan;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("JSON")
    }

    fn visit_bool<E>(self, _: bool) -> Result<Scan, E> {
        Ok(Scan::default())
    }

    fn visit_i64<E>(self, _: i64) -> Result<Scan, E> {
        Ok(Scan::default())
    }

    fn visit_u64<E>(self, _: u64) -> Result<Scan, E> {
        Ok(Scan::default())
    }

    fn visit_f64<E>(self, _: f64) -> Result<Scan, E> {
        Ok(Scan::default())
    }

    fn visit_unit<E>(self) -> Result<Scan, E> {
        Ok(Scan::default())
    }

    fn visit_str<E>(self, text: &str) -> Result<Scan, E> {
        Ok(Scan {
            images: 0,
            image_type: matches!(text, "image" | "image_url" | "input_image"),
        })
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut items: A) -> Result<Scan, A::Error> {
        let mut images = 0_usize;
        while let Some(item) = items.next_element::<Scan>()? {
            images = images.saturating_add(item.images);
        }
        Ok(Scan {
            images,
            image_type: false,
        })
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut entries: A) -> Result<Scan, A::Error> {
        let mut images = 0_usize;
        while let Some(key) = entries.next_key::<Key>()? {
            let value = entries.next_value::<Scan>()?;
            let own = usize::from(key == Key::Type && value.image_type);
            images = images.saturating_add(value.images).saturating_add(own);
        }
        Ok(Scan {
            images,
            image_type: false,
        })
    }
}

/// Walks a body serde_json already accepted, so every step is on valid JSON. Only the recorded
/// keys are kept, once each, so the list never outgrows `Field`.
fn top_level_fields(body: &[u8]) -> Result<Vec<(Field, std::ops::Range<usize>)>, PeekError> {
    let mut fields = Vec::new();
    let mut at = skip_space(body, 0);
    if body.get(at) != Some(&b'{') {
        return Err(PeekError::NotObject);
    }
    at = skip_space(body, at + 1);
    while body.get(at).ok_or(PeekError::NotObject)? != &b'}' {
        let key_end = string_end(body, at).ok_or(PeekError::NotObject)?;
        let key = body
            .get(at..key_end)
            .and_then(|raw| serde_json::from_slice::<TopKey>(raw).ok())
            .ok_or(PeekError::NotObject)?;
        at = skip_space(body, key_end);
        at = skip_space(body, at + 1);
        let value_end = value_end(body, at).ok_or(PeekError::NotObject)?;
        if let Some(field) = key.field() {
            if fields.iter().any(|(seen, _)| *seen == field) {
                return Err(PeekError::Duplicate(field));
            }
            fields.push((field, at..value_end));
        }
        at = skip_space(body, value_end);
        if body.get(at) == Some(&b',') {
            at = skip_space(body, at + 1);
        }
    }
    Ok(fields)
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
        let rewritten = peek.rewrite(
            body,
            &[(Field::Model, "\"gpt-5\""), (Field::Store, "false")],
        );
        let expected = br#"{"store":false, "model" : "gpt-5",
  "messages":[{"role":"user","content":[{"type":"image","source":{}},{"type":"text","text":"a \"model\": b"}]}],
  "stream":true }"#;
        assert_eq!(
            String::from_utf8(rewritten).unwrap(),
            String::from_utf8(expected.to_vec()).unwrap()
        );
        assert_eq!(peek.rewrite(body, &[(Field::Stream, "true")]), body);
        assert!(peek.has(Field::Stream) && !peek.has(Field::Store));
    }

    #[test]
    fn a_duplicated_key_is_refused() {
        for (body, field) in [
            (
                &br#"{"model":"astra","store":false,"stream":true,"store":true}"#[..],
                Field::Store,
            ),
            (
                br#"{"model":"astra","stream":false,"stream":true}"#,
                Field::Stream,
            ),
            (br#"{"m\u006fdel":"astra","model":"terra"}"#, Field::Model),
        ] {
            assert_eq!(
                RequestPeek::of(body).unwrap_err(),
                PeekError::Duplicate(field)
            );
        }
        let peek = RequestPeek::of(br#"{"model":"astra","x":1,"x":2}"#).unwrap();
        assert_eq!(peek.fields.len(), 1);
    }

    #[test]
    fn a_body_of_a_million_keys_keeps_only_the_recorded_ones() {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let mut body = Vec::with_capacity(8 * 1024 * 1024);
        body.extend_from_slice(br#"{"model":"astra","#);
        let mut keys = 0_usize;
        'fill: for a in ALPHABET {
            for b in ALPHABET {
                for c in ALPHABET {
                    for d in ALPHABET {
                        if body.len() + 16 > 8 * 1024 * 1024 {
                            break 'fill;
                        }
                        body.extend_from_slice(&[b'"', *a, *b, *c, *d, b'"', b':', b'0', b',']);
                        keys += 1;
                    }
                }
            }
        }
        body.extend_from_slice(br#""route":1}"#);
        assert!(keys > 900_000, "{keys}");
        let peek = RequestPeek::of(&body).unwrap();
        assert_eq!(peek.model, "astra");
        assert!(peek.has(Field::Route) && !peek.has(Field::Models));
        assert_eq!(peek.fields.len(), 2);
        assert!(peek.fields.capacity() <= 8);
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
