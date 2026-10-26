use serde::{Serialize, ser::SerializeStruct};

use crate::{
    body::{JsonBody, OpaqueSource},
    model::{ModelKey, ToolCallId},
    text::Text,
};

#[derive(Clone, Debug, PartialEq)]
pub struct ReplayCall {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: Text,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReplayReasoning {
    pub model: ModelKey,
    pub value: OpaqueSource,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ReplayMessage {
    User(Vec<Text>),
    Assistant {
        content: Option<Text>,
        calls: Vec<ReplayCall>,
        reasoning: Vec<ReplayReasoning>,
    },
    Tool {
        id: ToolCallId,
        text: Text,
    },
}

impl Serialize for ReplayMessage {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ReplayMessage", 2)?;
        state.serialize_field("role", self.role())?;
        state.serialize_field("content", "[journal slices]")?;
        state.end()
    }
}

impl ReplayMessage {
    pub const fn role(&self) -> &'static str {
        match self {
            Self::User(_) => "user",
            Self::Assistant { .. } => "assistant",
            Self::Tool { .. } => "tool",
        }
    }

    pub fn content(&self) -> Option<&str> {
        let text = match self {
            Self::User(texts) => {
                if let [text] = texts.as_slice() {
                    Some(text)
                } else {
                    None
                }
            }
            Self::Tool { text, .. } => Some(text),
            Self::Assistant { content, .. } => content.as_ref(),
        };
        text.and_then(Text::as_owned)
    }

    pub(crate) fn cacheable(&self) -> bool {
        let nonempty = |text: &Text| match text {
            Text::Owned(text) => !text.is_empty(),
            Text::File { len, .. } => *len > 0,
            Text::Opaque(_) => false,
        };
        match self {
            Self::User(texts) => texts.iter().any(nonempty),
            Self::Tool { text, .. } => nonempty(text),
            Self::Assistant { .. } => false,
        }
    }

    pub(crate) fn is_empty_for(&self, model: &ModelKey) -> bool {
        match self {
            Self::Assistant { content, calls, .. } => {
                content.is_none() && calls.is_empty() && !self.admits_opaque(model)
            }
            Self::User(_) | Self::Tool { .. } => false,
        }
    }

    pub(crate) fn admits_opaque(&self, model: &ModelKey) -> bool {
        match self {
            Self::Assistant { reasoning, .. } => reasoning.iter().any(|item| item.model == *model),
            Self::User(_) | Self::Tool { .. } => false,
        }
    }

    pub(crate) fn without_opaque(&self) -> Self {
        match self {
            Self::Assistant { content, calls, .. } => Self::Assistant {
                content: content.clone(),
                calls: calls.clone(),
                reasoning: Vec::new(),
            },
            Self::User(_) | Self::Tool { .. } => self.clone(),
        }
    }

    pub(crate) fn chat(
        &self,
        body: &mut JsonBody,
        model: Option<&ModelKey>,
        cache: Option<&crate::openai::CacheControl>,
    ) -> Result<(), serde_json::Error> {
        body.raw(b"{\"role\":");
        body.value(self.role())?;
        match self {
            Self::User(texts) => {
                body.raw(b",\"content\":");
                chat_text(body, texts, cache)?;
            }
            Self::Tool { id, text } => {
                body.raw(b",\"content\":");
                chat_text(body, std::slice::from_ref(text), cache)?;
                body.raw(b",\"tool_call_id\":");
                body.value(id)?;
            }
            Self::Assistant {
                content,
                calls,
                reasoning,
            } => {
                body.raw(b",\"content\":");
                match content {
                    Some(text) => body.text(text)?,
                    None => body.raw(b"null"),
                }
                if !calls.is_empty() {
                    body.raw(b",\"tool_calls\":[");
                    for (index, call) in calls.iter().enumerate() {
                        if index > 0 {
                            body.raw(b",");
                        }
                        body.raw(b"{\"id\":");
                        body.value(&call.id)?;
                        body.raw(b",\"type\":\"function\",\"function\":{\"name\":");
                        body.value(&call.name)?;
                        body.raw(b",\"arguments\":");
                        body.text(&call.arguments)?;
                        body.raw(b"}}");
                    }
                    body.raw(b"]");
                }
                let mut first = true;
                for item in reasoning.iter().filter(|item| model == Some(&item.model)) {
                    if first {
                        body.raw(b",\"reasoning_details\":[");
                        first = false;
                    } else {
                        body.raw(b",");
                    }
                    body.source(&item.value);
                }
                if !first {
                    body.raw(b"]");
                }
            }
        }
        body.raw(b"}");
        Ok(())
    }

    pub(crate) fn responses(
        &self,
        body: &mut JsonBody,
        model: &ModelKey,
    ) -> Result<(), serde_json::Error> {
        let mut first = true;
        let mut comma = |body: &mut JsonBody| {
            if !first {
                body.raw(b",");
            }
            first = false;
        };
        match self {
            Self::User(text) => {
                comma(body);
                responses_text(body, "user", "input_text", text)?;
            }
            Self::Tool { id, text } => {
                comma(body);
                body.raw(b"{\"type\":\"function_call_output\",\"call_id\":");
                body.value(id)?;
                body.raw(b",\"output\":");
                body.text(text)?;
                body.raw(b"}");
            }
            Self::Assistant {
                content,
                calls,
                reasoning,
            } => {
                for item in reasoning.iter().filter(|item| item.model == *model) {
                    comma(body);
                    body.source(&item.value);
                }
                if let Some(text) = content {
                    comma(body);
                    responses_text(body, "assistant", "output_text", std::slice::from_ref(text))?;
                }
                for call in calls {
                    comma(body);
                    body.raw(b"{\"type\":\"function_call\",\"call_id\":");
                    body.value(&call.id)?;
                    body.raw(b",\"name\":");
                    body.value(&call.name)?;
                    body.raw(b",\"arguments\":");
                    body.text(&call.arguments)?;
                    body.raw(b"}");
                }
            }
        }
        Ok(())
    }
}

fn chat_text(
    body: &mut JsonBody,
    texts: &[Text],
    cache: Option<&crate::openai::CacheControl>,
) -> Result<(), serde_json::Error> {
    if cache.is_some() {
        body.raw(b"[{\"type\":\"text\",\"text\":");
    }
    body.joined(texts)?;
    if let Some(cache) = cache {
        body.raw(b",\"cache_control\":");
        body.value(cache)?;
        body.raw(b"}]");
    }
    Ok(())
}

fn responses_text(
    body: &mut JsonBody,
    role: &str,
    kind: &str,
    text: &[Text],
) -> Result<(), serde_json::Error> {
    body.raw(b"{\"type\":\"message\",\"role\":");
    body.value(role)?;
    body.raw(b",\"content\":[{\"type\":");
    body.value(kind)?;
    body.raw(b",\"text\":");
    body.joined(text)?;
    if kind == "output_text" {
        body.raw(b",\"annotations\":[]");
    }
    body.raw(b"}]}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ReplayCall, ReplayMessage};
    use crate::{
        model::{CompletionOptions, ModelMessage},
        text::Text,
    };
    use std::{io::Write, sync::Arc, time::Duration};

    fn slice(source: &str) -> Text {
        let encoded = serde_json::to_vec(source).unwrap();
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&encoded).unwrap();
        Text::File {
            file: Arc::new(file),
            offset: 1,
            len: encoded.len() as u64 - 2,
        }
    }

    #[tokio::test]
    async fn every_backend_encodes_replay_strings_from_file_slices() {
        let user = slice("user\né");
        let answer = slice("answer\n\\é");
        let arguments = slice("{\"path\":\"é\"}");
        let output = slice("result\n\\é");
        let messages = [
            ModelMessage::Journal {
                message: ReplayMessage::User(vec![user]),
            },
            ModelMessage::Journal {
                message: ReplayMessage::Assistant {
                    content: Some(answer),
                    calls: vec![ReplayCall {
                        id: "call-1".into(),
                        name: "bash".into(),
                        arguments,
                    }],
                    reasoning: Vec::new(),
                },
            },
            ModelMessage::Journal {
                message: ReplayMessage::Tool {
                    id: "call-1".into(),
                    text: output,
                },
            },
        ];
        let options = CompletionOptions::default();
        let codex = crate::codex::request_body_json("gpt-test", &messages, &[], &options)
            .await
            .unwrap();
        assert_eq!(codex["input"][0]["content"][0]["text"], "user\né");
        assert_eq!(codex["input"][1]["content"][0]["text"], "answer\n\\é");
        assert_eq!(codex["input"][2]["arguments"], "{\"path\":\"é\"}");
        assert_eq!(codex["input"][3]["output"], "result\n\\é");
        let openai = crate::openai::request_body_json(&messages).await;
        let router = crate::openrouter::OpenRouterClient::new(
            "gpt-test",
            "synthetic-test-token".to_owned(),
            Duration::from_secs(1),
            Default::default(),
        )
        .unwrap();
        let encoded = router
            .prepare(crate::inference::GenerateRequest {
                messages: &messages,
                tools: &[],
                options: &options,
            })
            .await
            .unwrap();
        let router: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        for body in [openai, router] {
            assert_eq!(body["messages"][0]["content"], "user\né");
            assert_eq!(body["messages"][1]["content"], "answer\n\\é");
            assert_eq!(
                body["messages"][1]["tool_calls"][0]["function"]["arguments"],
                "{\"path\":\"é\"}"
            );
            assert_eq!(body["messages"][2]["content"], "result\n\\é");
        }
    }

    #[tokio::test]
    async fn file_opaque_replay_requires_the_full_model_key_and_never_enters_a_literal() {
        use crate::{
            body::{Chunk, JsonBody, OpaqueSource},
            model::{Backend, ModelKey},
        };
        let raw = br#"{"type":"reasoning","encrypted_content":"opaque-file-canary"}"#;
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(raw).unwrap();
        let key = ModelKey::new(Backend::Codex, "alias").with_reported(Some("model-a".to_owned()));
        let message = ReplayMessage::Assistant {
            content: None,
            calls: Vec::new(),
            reasoning: vec![super::ReplayReasoning {
                model: key.clone(),
                value: OpaqueSource::File {
                    file: Arc::new(file),
                    offset: 0,
                    len: raw.len() as u64,
                },
            }],
        };
        let mut body = JsonBody::default();
        message.responses(&mut body, &key).unwrap();
        let chunks = body.finish();
        assert!(matches!(
            chunks.as_slice(),
            [Chunk::Opaque(OpaqueSource::File { .. })]
        ));
        assert_eq!(crate::body::collect(&chunks).await.unwrap(), raw);
        let other =
            ModelKey::new(Backend::Codex, "alias").with_reported(Some("model-b".to_owned()));
        assert!(message.is_empty_for(&other));
        let mut body = JsonBody::default();
        message.responses(&mut body, &other).unwrap();
        assert!(body.finish().is_empty());
        let audit = ModelMessage::Journal { message };
        assert!(
            !serde_json::to_string(&audit)
                .unwrap()
                .contains("opaque-file-canary")
        );
        assert!(!format!("{audit:?}").contains("opaque-file-canary"));
    }

    #[tokio::test]
    async fn explicit_prefix_cache_markers_keep_the_stable_file_text_anchor() {
        use crate::openrouter::settings::{Cache, CacheStyle, Settings, Ttl};
        let messages = [
            ModelMessage::system("instructions"),
            ModelMessage::Journal {
                message: ReplayMessage::User(vec![slice("history")]),
            },
            ModelMessage::user("current"),
        ];
        let options = CompletionOptions::default().with_stable_prefix(2);
        let router = crate::openrouter::OpenRouterClient::new(
            "alias",
            "synthetic-test-token".to_owned(),
            Duration::from_secs(1),
            Settings {
                cache: Some(Cache {
                    style: CacheStyle::ExplicitPrefix,
                    ttl: Some(Ttl::OneHour),
                }),
                ..Default::default()
            },
        )
        .unwrap();
        let encoded = router
            .prepare(crate::inference::GenerateRequest {
                messages: &messages,
                tools: &[],
                options: &options,
            })
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(body["messages"][1]["content"][0]["text"], "history");
        assert_eq!(
            body["messages"][1]["content"][0]["cache_control"]["ttl"],
            "1h"
        );
        assert_eq!(body["messages"][2]["content"][0]["text"], "current");
        assert_eq!(
            body["messages"][2]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
    }
}
