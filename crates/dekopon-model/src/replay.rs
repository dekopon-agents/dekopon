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

struct Counted(usize);

impl std::io::Write for Counted {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn json_len(value: &(impl Serialize + ?Sized)) -> usize {
    let mut count = Counted(0);
    match serde_json::to_writer(&mut count, value) {
        Ok(()) => count.0,
        Err(_unencodable) => 0,
    }
}

fn text_len(text: &Text) -> usize {
    match text {
        Text::Owned(text) => json_len(text.as_str()),
        Text::File { len, .. } => usize::try_from(*len)
            .unwrap_or(usize::MAX)
            .saturating_add(2),
        Text::Opaque(source) => usize::try_from(source.len()).unwrap_or(usize::MAX),
    }
}

fn joined_len(texts: &[Text]) -> usize {
    texts.iter().fold(2, |total, text| {
        total.saturating_add(text_len(text).saturating_sub(2))
    })
}

fn list_len(items: impl Iterator<Item = usize>) -> usize {
    items.enumerate().fold(0, |total, (index, len)| {
        total
            .saturating_add(len)
            .saturating_add(usize::from(index > 0))
    })
}

impl ReplayMessage {
    /// The chat-completions encoding's exact size, every reasoning item admitted and no cache
    /// marker, from slice lengths alone; no slice is read.
    pub fn encoded_len(&self) -> usize {
        let open = br#"{"role":"#.len() + json_len(self.role()) + br#","content":"#.len();
        let body = match self {
            Self::User(texts) => joined_len(texts),
            Self::Tool { id, text } => joined_len(std::slice::from_ref(text))
                .saturating_add(br#","tool_call_id":"#.len())
                .saturating_add(json_len(id)),
            Self::Assistant {
                content,
                calls,
                reasoning,
            } => {
                let content = content.as_ref().map_or(b"null".len(), text_len);
                let calls = if calls.is_empty() {
                    0
                } else {
                    list_len(calls.iter().map(|call| {
                        br#"{"id":,"type":"function","function":{"name":,"arguments":}}"#
                            .len()
                            .saturating_add(json_len(&call.id))
                            .saturating_add(json_len(call.name.as_str()))
                            .saturating_add(text_len(&call.arguments))
                    }))
                    .saturating_add(br#","tool_calls":[]"#.len())
                };
                let reasoning = if reasoning.is_empty() {
                    0
                } else {
                    list_len(
                        reasoning
                            .iter()
                            .map(|item| usize::try_from(item.value.len()).unwrap_or(usize::MAX)),
                    )
                    .saturating_add(br#","reasoning_details":[]"#.len())
                };
                content.saturating_add(calls).saturating_add(reasoning)
            }
        };
        open.saturating_add(body).saturating_add(1)
    }

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

    pub fn is_empty_for(&self, model: &ModelKey) -> bool {
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
            &[],
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

    fn shapes(
        mut text: impl FnMut(&str) -> Text,
        reasoning: crate::body::OpaqueSource,
        key: &crate::model::ModelKey,
    ) -> Vec<ReplayMessage> {
        let call = |id: &str, arguments: Text| ReplayCall {
            id: id.into(),
            name: "bash".into(),
            arguments,
        };
        vec![
            ReplayMessage::User(vec![
                text("user\né"),
                Text::Owned("plain \"quoted\"".to_owned()),
                text("tail\\"),
            ]),
            ReplayMessage::Assistant {
                content: Some(text("answer\n")),
                calls: vec![
                    call("call-1", text("{\"path\":\"é\"}")),
                    call("call-2", Text::Owned("{\"cut\":12}".to_owned())),
                ],
                reasoning: vec![
                    super::ReplayReasoning {
                        model: key.clone(),
                        value: reasoning,
                    },
                    super::ReplayReasoning {
                        model: key.clone(),
                        value: crate::body::OpaqueSource::owned(b"{\"x\":1}"),
                    },
                ],
            },
            ReplayMessage::Assistant {
                content: None,
                calls: vec![call("call-3", text("[1,2]"))],
                reasoning: Vec::new(),
            },
            ReplayMessage::Assistant {
                content: Some(Text::Owned("only text".to_owned())),
                calls: Vec::new(),
                reasoning: Vec::new(),
            },
            ReplayMessage::Tool {
                id: "call-1".into(),
                text: text("result\n\\é"),
            },
            ReplayMessage::Tool {
                id: "call-3".into(),
                text: Text::Owned(String::new()),
            },
        ]
    }

    #[tokio::test]
    async fn estimates_never_read_a_slice() {
        use crate::{
            body::{JsonBody, OpaqueSource},
            model::{Backend, ModelKey},
        };
        let key = ModelKey::new(Backend::OpenRouter, "alias");
        let raw = br#"{"type":"reasoning.encrypted","data":"opaque"}"#;
        let mut reasoning = tempfile::tempfile().unwrap();
        reasoning.write_all(raw).unwrap();
        let mut files = vec![Arc::new(reasoning)];
        let sliced = shapes(
            |source| {
                let text = slice(source);
                if let Text::File { file, .. } = &text {
                    files.push(Arc::clone(file));
                }
                text
            },
            OpaqueSource::File {
                file: Arc::clone(&files[0]),
                offset: 0,
                len: raw.len() as u64,
            },
            &key,
        );
        for file in &files {
            file.set_len(0).unwrap();
        }
        let owned = shapes(
            |source| Text::Owned(source.to_owned()),
            OpaqueSource::owned(raw),
            &key,
        );
        for (sliced, owned) in sliced.into_iter().zip(owned) {
            let mut body = JsonBody::default();
            owned.chat(&mut body, Some(&key), None).unwrap();
            let encoded = crate::body::collect(&body.finish()).await.unwrap();
            serde_json::from_slice::<serde_json::Value>(&encoded).unwrap();
            assert_eq!(owned.encoded_len(), encoded.len());
            assert_eq!(
                ModelMessage::Journal { message: sliced }.encoded_len(),
                encoded.len()
            );
        }
    }

    #[tokio::test]
    async fn cut_texts_encode_valid_json_on_every_body() {
        let whole = slice("ab\né");
        for (limit, expected) in [(3, "ab"), (5, "ab\n")] {
            let cut = whole.prefix(limit).unwrap();
            let shown = "{\"cut\":17}";
            let messages = [
                ModelMessage::Journal {
                    message: ReplayMessage::User(vec![cut.clone()]),
                },
                ModelMessage::Journal {
                    message: ReplayMessage::Assistant {
                        content: Some(cut.clone()),
                        calls: vec![
                            ReplayCall {
                                id: "call-1".into(),
                                name: "bash".into(),
                                arguments: cut.clone(),
                            },
                            ReplayCall {
                                id: "call-2".into(),
                                name: "bash".into(),
                                arguments: Text::Owned(shown.to_owned()),
                            },
                        ],
                        reasoning: Vec::new(),
                    },
                },
                ModelMessage::Journal {
                    message: ReplayMessage::Tool {
                        id: "call-1".into(),
                        text: cut.clone(),
                    },
                },
                ModelMessage::Journal {
                    message: ReplayMessage::Tool {
                        id: "call-2".into(),
                        text: cut,
                    },
                },
            ];
            let options = CompletionOptions::default();
            let codex = crate::codex::request_body_json("gpt-test", &messages, &[], &options)
                .await
                .unwrap();
            assert_eq!(codex["input"][0]["content"][0]["text"], expected);
            assert_eq!(codex["input"][1]["content"][0]["text"], expected);
            assert_eq!(codex["input"][2]["arguments"], expected);
            assert_eq!(codex["input"][3]["arguments"], shown);
            assert_eq!(codex["input"][4]["output"], expected);
            assert_eq!(codex["input"][5]["output"], expected);
            let openai = crate::openai::request_body_json(&messages).await;
            let router = crate::openrouter::OpenRouterClient::new(
                "gpt-test",
                "synthetic-test-token".to_owned(),
                Duration::from_secs(1),
                Default::default(),
                &[],
            )
            .unwrap()
            .prepare(crate::inference::GenerateRequest {
                messages: &messages,
                tools: &[],
                options: &options,
            })
            .await
            .unwrap();
            let router: serde_json::Value = serde_json::from_slice(&router).unwrap();
            for body in [openai, router] {
                assert_eq!(body["messages"][0]["content"], expected);
                assert_eq!(body["messages"][1]["content"], expected);
                let calls = &body["messages"][1]["tool_calls"];
                assert_eq!(calls[0]["function"]["arguments"], expected);
                assert_eq!(calls[1]["function"]["arguments"], shown);
                assert_eq!(body["messages"][2]["content"], expected);
                assert_eq!(body["messages"][3]["content"], expected);
            }
            let rendered: serde_json::Value =
                serde_json::from_str(codex["input"][3]["arguments"].as_str().unwrap()).unwrap();
            assert_eq!(rendered, serde_json::json!({"cut": 17}));
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
            &[],
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
