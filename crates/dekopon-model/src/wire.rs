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
    pub(crate) fn into_openrouter(self) -> ModelUsage {
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
}
