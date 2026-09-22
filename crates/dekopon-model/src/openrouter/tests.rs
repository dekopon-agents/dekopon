use super::*;
use crate::{
    mock::{MockResponse, MockServer},
    model::{CompletionOptions, ModelTool, assistant_message},
};
use serde_json::json;
use tracing::instrument::WithSubscriber as _;

const TOOL: &str = include_str!("../fixtures/openrouter-tool.sse");
const ANSWER: &str = include_str!("../fixtures/openrouter-answer.sse");

fn control() -> TurnControl {
    TurnControl::new(tokio::sync::watch::channel(false).1, Duration::from_secs(3)).unwrap()
}
fn client(server: &MockServer, settings: Settings) -> OpenRouterClient {
    OpenRouterClient::new(
        "vendor/model",
        "synthetic-key".into(),
        Duration::from_secs(3),
        settings,
    )
    .unwrap()
    .with_endpoint(server.base_url())
}
fn tool() -> ModelTool {
    ModelTool {
        name: "run".into(),
        description: "Run a synthetic script".into(),
        parameters: json!({"type":"object"}),
    }
}
async fn generate(
    client: &OpenRouterClient,
    messages: &[ModelMessage],
) -> Result<AssistantTurn, InferenceError> {
    client
        .generate(
            GenerateRequest {
                messages,
                tools: &[tool()],
                options: &CompletionOptions::default().with_prompt_cache_key("conversation-7"),
            },
            &mut |_| ControlFlow::Continue(()),
            &control(),
        )
        .await
}
fn request_json(request: &str) -> Value {
    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
}
fn requests_have_only_the_authored_headers(server: &MockServer) {
    for request in server.requests() {
        let headers = request
            .split_once("\r\n\r\n")
            .unwrap()
            .0
            .to_ascii_lowercase();
        for header in headers.lines().skip(1) {
            let name = header.split_once(':').unwrap().0;
            assert!(
                matches!(
                    name,
                    "authorization"
                        | "content-type"
                        | "accept"
                        | "x-openrouter-cache"
                        | "content-length"
                        | "host"
                ),
                "unexpected header: {name}"
            );
        }
        assert!(headers.contains("x-openrouter-cache: false"));
        assert!(headers.contains("authorization: bearer synthetic-key"));
        assert!(headers.contains("content-type: application/json"));
        assert!(headers.contains("accept: text/event-stream"));
        assert!(headers.contains(&format!(
            "content-length: {}",
            request.split_once("\r\n\r\n").unwrap().1.len()
        )));
        for forbidden in [
            "traceparent:",
            "http-referer:",
            "x-title:",
            "transfer-encoding:",
        ] {
            assert!(!headers.contains(forbidden));
        }
    }
}

#[tokio::test]
async fn two_turns_merge_native_reasoning_and_replay_the_exact_second_request() {
    let server = MockServer::start(vec![
        MockResponse::sse(TOOL).split(1),
        MockResponse::sse(ANSWER),
    ]);
    let client = client(&server, Settings::default());
    let mut messages = vec![ModelMessage::system("rules"), ModelMessage::user("go")];
    let trace = crate::trace_capture::TraceCapture::default();
    let mut events = Vec::new();
    let turn = client
        .generate(
            GenerateRequest {
                messages: &messages,
                tools: &[tool()],
                options: &CompletionOptions::default(),
            },
            &mut |event| {
                events.push(event);
                ControlFlow::Continue(())
            },
            &control(),
        )
        .with_subscriber(trace.subscriber())
        .await
        .unwrap();
    let usage = turn.usage.unwrap();
    assert_eq!(usage.input_tokens, Some(100));
    assert_eq!(usage.cached_input_tokens, Some(75));
    assert_eq!(usage.cache_write_tokens, Some(10));
    assert_eq!(usage.output_tokens, Some(12));
    assert_eq!(usage.reasoning_output_tokens, Some(5));
    assert_eq!(usage.total_tokens, Some(112));
    assert_eq!(turn.tool_calls.len(), 1);
    assert!(
        events
            .iter()
            .all(|event| matches!(event, TurnEvent::ToolCallStarted { .. }))
    );
    for hidden in [
        "cipher-tail",
        "think-more",
        "signature-0",
        "signature-2",
        "standalone",
    ] {
        assert!(
            !format!(
                "{turn:?} {} {}",
                serde_json::to_string(&turn).unwrap(),
                trace.text()
            )
            .contains(hidden)
        );
    }
    messages.push(assistant_message(&turn));
    messages.push(ModelMessage::tool("call-1", "ok"));
    let answer = generate(&client, &messages).await.unwrap();
    assert_eq!(answer.content.as_deref(), Some("done"));
    assert_eq!(answer.usage.unwrap().cache_write_tokens, None);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        request_json(&requests[1]),
        serde_json::from_str::<Value>(include_str!("../fixtures/openrouter-replay.json")).unwrap()
    );
    assert_eq!(
        request_json(&requests[0])["messages"],
        json!([{"role":"system","content":"rules"},{"role":"user","content":"go"}])
    );
    requests_have_only_the_authored_headers(&server);
}

#[tokio::test]
async fn text_only_assistant_replay_omits_empty_tool_calls() {
    let server = MockServer::start(vec![MockResponse::sse(ANSWER)]);
    let client = client(&server, Settings::default());
    let native = generate(&client, &[]).await.unwrap();
    for turn in [
        native,
        AssistantTurn::new(Some("portable".into()), vec![], None),
    ] {
        let messages = [assistant_message(&turn)];
        let bytes = client
            .prepare(GenerateRequest {
                messages: &messages,
                tools: &[],
                options: &CompletionOptions::default(),
            })
            .await
            .unwrap();
        let request: Value = serde_json::from_slice(&bytes).unwrap();
        let assistant = &request["messages"][0];
        assert!(assistant.get("tool_calls").is_none());
        assert!(assistant.get("reasoning_details").is_none());
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"].as_str(), turn.content.as_deref());
    }
}

#[test]
fn a_large_indexed_stream_completes_with_all_reasoning_and_calls() {
    let count = 50_000;
    let mut stream = RouterStream::new();
    let secrets = DiagnosticSecrets::default();
    for suffix in ["{", "}"] {
        let chunk = json!({"choices":[{"delta":{
            "reasoning_details": (0..count).map(|index| json!({
                "index": index, "text": suffix
            })).collect::<Vec<_>>(),
            "tool_calls": (0..count).map(|index| json!({
                "index": index, "id": format!("call-{index}"), "type":"function",
                "function":{"name":"run", "arguments": suffix}
            })).collect::<Vec<_>>()
        }, "finish_reason":"tool_calls"}]});
        stream
            .apply(
                serde_json::from_value(chunk).unwrap(),
                &mut |_| ControlFlow::Continue(()),
                secrets,
            )
            .unwrap();
    }
    let turn = stream.finish(&ClientIdentity::new(), secrets).unwrap();
    assert_eq!(turn.tool_calls.len(), count);
    let replay = turn.openrouter_replay().unwrap();
    assert_eq!(replay.reasoning.len(), count);
    for (index, item) in replay.reasoning.iter().enumerate() {
        assert_eq!(item, &json!({"index":index,"text":"{}"}));
        assert_eq!(turn.tool_calls[index].function.arguments, "{}");
    }
}

#[test]
fn indexed_merges_preserve_first_seen_order_and_unindexed_reasoning() {
    let mut stream = RouterStream::new();
    let secrets = DiagnosticSecrets::default();
    let chunk = json!({"choices":[{"delta":{
        "reasoning_details":[
            {"index":9,"text":"a","summary":"s","data":"d","signature":null},
            {"text":"unindexed"},
            {"index":2,"text":"b"},
            {"index":9,"text":"A","summary":"S","data":"D","signature":"first"},
            {"index":2,"text":"B"},
            {"index":9,"signature":"later"},
            {"index":null,"text":"also unindexed"}
        ],
        "tool_calls":[
            {"index":9,"id":"nine","type":"function","function":{"name":"run","arguments":"{"}},
            {"index":2,"id":"two","type":"function","function":{"name":"run","arguments":"{"}},
            {"index":2,"function":{"arguments":"}"}},
            {"index":9,"function":{"arguments":"}"}}
        ]
    },"finish_reason":"tool_calls"}]});
    stream
        .apply(
            serde_json::from_value(chunk).unwrap(),
            &mut |_| ControlFlow::Continue(()),
            secrets,
        )
        .unwrap();
    let turn = stream.finish(&ClientIdentity::new(), secrets).unwrap();
    let replay = turn.openrouter_replay().unwrap();
    assert_eq!(
        replay.reasoning,
        vec![
            json!({"index":9,"text":"aA","summary":"sS","data":"dD","signature":"first"}),
            json!({"text":"unindexed"}),
            json!({"index":2,"text":"bB"}),
            json!({"index":null,"text":"also unindexed"}),
        ]
    );
    assert_eq!(
        replay
            .calls
            .iter()
            .map(|call| call["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["nine", "two"]
    );
    assert!(
        turn.tool_calls
            .iter()
            .all(|call| call.function.arguments == "{}")
    );
}

#[tokio::test]
async fn omitted_controls_stay_absent_and_authored_controls_map_exactly() {
    for settings in [
        Settings::default(),
        Settings {
            generation: Some(settings::Generation {
                max_output_tokens: NonZeroU32::new(1),
                temperature: Some(0.0),
                top_p: Some(1.0),
            }),
            reasoning: Some(settings::Reasoning {
                effort: settings::Effort::Xhigh,
            }),
            routing: Some(settings::Routing {
                allow_fallbacks: Some(false),
                require_parameters: Some(true),
                only: Some(vec!["alpha".into()]),
            }),
            cache: None,
        },
    ] {
        let authored = settings.generation.is_some();
        let server = MockServer::start(vec![MockResponse::sse(ANSWER)]);
        generate(&client(&server, settings), &[ModelMessage::user("go")])
            .await
            .unwrap();
        let body = request_json(&server.requests()[0]);
        for absent in [
            "parallel_tool_calls",
            "stream_options",
            "prompt_cache_key",
            "usage",
            "cache",
        ] {
            assert!(body.get(absent).is_none());
        }
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["stream"], true);
        if authored {
            assert_eq!(body["max_tokens"], 1);
            assert_eq!(body["temperature"], 0.0);
            assert_eq!(body["top_p"], 1.0);
            assert_eq!(body["reasoning"], json!({"effort":"xhigh"}));
            assert_eq!(
                body["provider"],
                json!({"allow_fallbacks":false,"require_parameters":true,"only":["alpha"]})
            );
        } else {
            for absent in [
                "max_tokens",
                "temperature",
                "top_p",
                "reasoning",
                "provider",
            ] {
                assert!(body.get(absent).is_none());
            }
        }
        requests_have_only_the_authored_headers(&server);
    }
}

#[tokio::test]
async fn only_authored_routing_members_and_ttl_are_forwarded() {
    for ttl in [
        None,
        Some(settings::Ttl::FiveMinutes),
        Some(settings::Ttl::OneHour),
    ] {
        let settings = Settings {
            routing: Some(settings::Routing {
                require_parameters: Some(false),
                ..Default::default()
            }),
            cache: Some(settings::Cache {
                style: CacheStyle::ExplicitPrefix,
                ttl,
            }),
            ..Default::default()
        };
        let server = MockServer::start(vec![MockResponse::sse(ANSWER)]);
        generate(&client(&server, settings), &[ModelMessage::system("rules")])
            .await
            .unwrap();
        let body = request_json(&server.requests()[0]);
        assert_eq!(body["provider"], json!({"require_parameters":false}));
        let marker = &body["messages"][0]["content"][0]["cache_control"];
        let expected = match ttl {
            None => json!({"type":"ephemeral"}),
            Some(settings::Ttl::FiveMinutes) => json!({"type":"ephemeral","ttl":"5m"}),
            Some(settings::Ttl::OneHour) => json!({"type":"ephemeral","ttl":"1h"}),
        };
        assert_eq!(marker, &expected);
        requests_have_only_the_authored_headers(&server);
    }
}

#[tokio::test]
async fn explicit_cache_marks_only_the_last_of_one_two_or_three_leading_system_messages() {
    for count in 1..=3 {
        let server = MockServer::start(vec![MockResponse::sse(ANSWER)]);
        let client = client(
            &server,
            Settings {
                cache: Some(settings::Cache {
                    style: CacheStyle::ExplicitPrefix,
                    ttl: None,
                }),
                ..Default::default()
            },
        );
        let mut messages = (0..count)
            .map(|index| ModelMessage::system(format!("system-{index}")))
            .collect::<Vec<_>>();
        messages.push(ModelMessage::user("go"));
        messages.push(ModelMessage::system("not-leading"));
        generate(&client, &messages).await.unwrap();
        let body = request_json(&server.requests()[0]);
        for index in 0..count {
            if index + 1 == count {
                assert_eq!(
                    body["messages"][index]["content"],
                    json!([{"type":"text","text":format!("system-{index}"),"cache_control":{"type":"ephemeral"}}])
                );
            } else {
                assert_eq!(
                    body["messages"][index]["content"],
                    format!("system-{index}")
                );
            }
        }
        assert_eq!(body["messages"][count + 1]["content"], "not-leading");
        requests_have_only_the_authored_headers(&server);
    }
}

#[tokio::test]
async fn zero_leading_system_messages_refuses_explicit_cache_before_sending() {
    let server = MockServer::start(Vec::new());
    let client = client(
        &server,
        Settings {
            cache: Some(settings::Cache {
                style: CacheStyle::ExplicitPrefix,
                ttl: None,
            }),
            ..Default::default()
        },
    );
    for messages in [
        vec![],
        vec![ModelMessage::user("go"), ModelMessage::system("late")],
    ] {
        assert!(matches!(
            generate(&client, &messages).await,
            Err(InferenceError::InvalidRequest(RequestError::CacheAnchor))
        ));
    }
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn a_changed_provider_is_accepted_and_reasoning_is_still_forwarded() {
    let beta = "data: {\"provider\":\"beta\",\"choices\":[{\"delta\":{\"content\":\"yes\"},\"finish_reason\":\"stop\"}]}\n\n";
    let server = MockServer::start(vec![
        MockResponse::sse(TOOL),
        MockResponse::sse(beta),
        MockResponse::sse(ANSWER),
    ]);
    let client = client(&server, Settings::default());
    let first = generate(&client, &[]).await.unwrap();
    let second = generate(&client, &[assistant_message(&first)])
        .await
        .unwrap();
    assert_eq!(second.content.as_deref(), Some("yes"));
    generate(
        &client,
        &[assistant_message(&first), assistant_message(&second)],
    )
    .await
    .unwrap();
    let requests = server.requests();
    for request in &requests[1..] {
        assert_eq!(
            request_json(request)["messages"][0]["reasoning_details"],
            Value::Array(first.openrouter_replay().unwrap().reasoning.clone())
        );
    }
    requests_have_only_the_authored_headers(&server);
}

#[tokio::test]
async fn session_id_is_sent_only_with_a_cache_key() {
    let server = MockServer::start(vec![MockResponse::sse(ANSWER), MockResponse::sse(ANSWER)]);
    let client = client(&server, Settings::default());
    generate(&client, &[]).await.unwrap();
    client
        .generate(
            GenerateRequest {
                messages: &[],
                tools: &[],
                options: &CompletionOptions::default(),
            },
            &mut |_| ControlFlow::Continue(()),
            &control(),
        )
        .await
        .unwrap();
    let requests = server.requests();
    assert_eq!(request_json(&requests[0])["session_id"], "conversation-7");
    assert!(request_json(&requests[1]).get("session_id").is_none());
}

#[tokio::test]
async fn unknown_fields_and_types_in_the_stream_are_tolerated() {
    let body = [
        json!({"id":"gen-1","object":"chat.completion.chunk","created":1,"system_fingerprint":"fp","future":{"x":1},"choices":[{"index":0,"logprobs":null,"native_finish_reason":"future","delta":{"role":"assistant","refusal":null,"annotations":[],"future":1,"content":"ok"}}]}),
        json!({"object":"future.event","type":"future.kind","choices":[]}),
        json!({"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.future","index":0,"blob":{"a":1}}]},"finish_reason":"stop"}]}),
    ]
    .iter()
    .map(|chunk| format!("data: {chunk}\n\n"))
    .collect::<String>()
        + "data: [DONE]\n\n";
    let server = MockServer::start(vec![MockResponse::sse(&body)]);
    let turn = generate(&client(&server, Settings::default()), &[])
        .await
        .unwrap();
    assert_eq!(turn.content.as_deref(), Some("ok"));
    assert_eq!(
        turn.openrouter_replay().unwrap().reasoning,
        vec![json!({"type":"reasoning.future","index":0,"blob":{"a":1}})]
    );
}

#[tokio::test]
async fn refusals_have_typed_classes_and_never_retry() {
    for status in [401, 403, 429, 500] {
        let server = MockServer::start(vec![
            MockResponse::failure(status, json!({"error":{"message":"refused","code":status}}))
                .header("retry-after", "7")
                .header("x-request-id", "request"),
        ]);
        let error = generate(&client(&server, Settings::default()), &[])
            .await
            .unwrap_err();
        let context = match (status, error) {
            (
                401 | 403,
                InferenceError::Authentication(crate::error::AuthError::Provider(context)),
            ) => context,
            (429, InferenceError::RateLimited(crate::error::RateLimitError(context))) => context,
            (500, InferenceError::Provider(context)) => context,
            _ => panic!("wrong refusal class"),
        };
        assert_eq!(context.status, Some(status));
        assert_eq!(context.code, Some(status.to_string()));
        assert_eq!(context.request_id.as_deref(), Some("request"));
        assert_eq!(context.retry_after, Some(Duration::from_secs(7)));
        assert_eq!(server.requests().len(), 1);
        requests_have_only_the_authored_headers(&server);
    }
}

#[tokio::test]
async fn malformed_missing_terminal_and_incomplete_calls_are_protocol_errors() {
    for body in [
        "data: {\n\n",
        "data: [DONE]\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call\",\"type\":\"function\",\"function\":{\"name\":\"run\",\"arguments\":\"{\"}}]},\"finish_reason\":\"length\"}]}\n\n",
    ] {
        let server = MockServer::start(vec![MockResponse::sse(body)]);
        assert!(matches!(
            generate(&client(&server, Settings::default()), &[]).await,
            Err(InferenceError::Protocol(_))
        ));
        assert_eq!(server.requests().len(), 1);
        requests_have_only_the_authored_headers(&server);
    }
}

#[tokio::test]
async fn finish_reasons_keep_complete_text_but_refuse_filtering_and_unexecutable_tools() {
    for reason in ["stop", "length", "extension", "content_filter"] {
        let body = format!(
            "data: {}\n\n",
            json!({"choices":[{"delta":{"content":"answer"},"finish_reason":reason}]})
        );
        let server = MockServer::start(vec![MockResponse::sse(&body)]);
        let result = generate(&client(&server, Settings::default()), &[]).await;
        if reason == "content_filter" {
            assert!(matches!(result, Err(InferenceError::Provider(_))));
        } else {
            assert_eq!(result.unwrap().content.as_deref(), Some("answer"));
        }
        requests_have_only_the_authored_headers(&server);
    }
    let server = MockServer::start(vec![MockResponse::sse(
        &TOOL.replace("\"type\":\"function\"", "\"type\":\"computer\""),
    )]);
    assert!(matches!(
        generate(&client(&server, Settings::default()), &[]).await,
        Err(InferenceError::Unsupported(_))
    ));
    requests_have_only_the_authored_headers(&server);
}

#[tokio::test]
async fn http_200_errors_keep_numeric_codes_and_exclude_credentials_from_diagnostics_and_spans() {
    for choices in [None, Some(json!([]))] {
        let mut body = json!({"error":{"code":529,"message":"synthetic-key"}});
        if let Some(choices) = choices {
            body["choices"] = choices;
        }
        let server = MockServer::start(vec![
            MockResponse::sse(&format!("data: {body}\n\n")).header("x-request-id", "synthetic-key"),
        ]);
        let trace = crate::trace_capture::TraceCapture::default();
        let error = generate(&client(&server, Settings::default()), &[])
            .with_subscriber(trace.subscriber())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, InferenceError::Provider(context) if context.code.as_deref() == Some("529") && context.status == Some(200))
        );
        assert!(!format!("{error} {error:?} {}", trace.text()).contains("synthetic-key"));
        requests_have_only_the_authored_headers(&server);
    }
}

#[tokio::test]
async fn a_silent_reasoning_body_cancels_without_waiting_for_the_deadline() {
    let (reply, ready, release) = MockResponse::sse(ANSWER).stalled();
    let server = MockServer::start(vec![reply]);
    let client = client(&server, Settings::default());
    let (cancel, watch) = tokio::sync::watch::channel(false);
    let control = TurnControl::new(watch, Duration::from_secs(3)).unwrap();
    let options = CompletionOptions::default();
    let mut observe = |_| ControlFlow::Continue(());
    let (result, ()) = tokio::join!(
        client.generate(
            GenerateRequest {
                messages: &[],
                tools: &[],
                options: &options
            },
            &mut observe,
            &control
        ),
        async {
            ready.await.unwrap();
            cancel.send(true).unwrap();
        }
    );
    drop(release);
    assert!(matches!(result, Err(InferenceError::Cancelled)));
    assert_eq!(server.requests().len(), 1);
    requests_have_only_the_authored_headers(&server);
}

#[tokio::test]
async fn the_total_deadline_stops_a_silent_body_without_retry() {
    let (reply, _ready, release) = MockResponse::sse(ANSWER).stalled();
    let server = MockServer::start(vec![reply]);
    let client = client(&server, Settings::default());
    let control =
        TurnControl::new(tokio::sync::watch::channel(false).1, Duration::from_secs(1)).unwrap();
    let result = client
        .generate(
            GenerateRequest {
                messages: &[],
                tools: &[],
                options: &CompletionOptions::default(),
            },
            &mut |_| ControlFlow::Continue(()),
            &control,
        )
        .await;
    drop(release);
    assert!(matches!(result, Err(InferenceError::DeadlineExceeded)));
    assert_eq!(server.requests().len(), 1);
    requests_have_only_the_authored_headers(&server);
}

#[test]
fn sampling_settings_refuse_nonfinite_and_out_of_range_values() {
    use settings::SettingsProblem::{Temperature, TopP};
    for (temperature, top_p, expected) in [
        (Some(1.0), Some(0.5), vec![]),
        (Some(f64::NAN), None, vec![Temperature]),
        (Some(2.5), None, vec![Temperature]),
        (None, Some(f64::NAN), vec![TopP]),
        (None, Some(0.0), vec![TopP]),
    ] {
        let settings = Settings {
            generation: Some(settings::Generation {
                temperature,
                top_p,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(settings.problems(), expected);
    }
}

#[test]
fn routing_cache_and_sampling_problems_are_collected_without_clamping() {
    let settings = Settings {
        generation: Some(settings::Generation {
            temperature: Some(f64::NAN),
            top_p: Some(0.0),
            ..Default::default()
        }),
        routing: Some(settings::Routing {
            only: Some(vec!["".into(), " ".into()]),
            ..Default::default()
        }),
        cache: Some(settings::Cache {
            style: CacheStyle::Automatic,
            ttl: Some(settings::Ttl::OneHour),
        }),
        ..Default::default()
    };
    assert_eq!(settings.problems().len(), 5);
    let empty = Settings {
        routing: Some(settings::Routing {
            only: Some(vec![]),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(empty.problems(), vec![settings::SettingsProblem::EmptyOnly]);
    assert!(matches!(
        OpenRouterClient::new("model", "key".into(), Duration::from_secs(1), settings),
        Err(InferenceError::InvalidRequest(
            RequestError::OpenRouterSetting(_)
        ))
    ));
}

#[tokio::test]
async fn socket_and_body_io_failures_stay_transport_errors_without_retry() {
    for reply in [
        MockResponse::hang_up(),
        MockResponse::sse(ANSWER).interrupted_at(1),
    ] {
        let server = MockServer::start(vec![reply]);
        let error = generate(&client(&server, Settings::default()), &[])
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            InferenceError::Transport(TransportFailure::Http { .. })
        ));
        assert_eq!(server.requests().len(), 1);
        requests_have_only_the_authored_headers(&server);
    }
}

#[tokio::test]
async fn a_null_optional_delta_and_absent_usage_are_not_invented_as_zero() {
    let server = MockServer::start(vec![MockResponse::sse(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":null,\"reasoning_details\":null,\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\n",
    )]);
    let turn = generate(&client(&server, Settings::default()), &[])
        .await
        .unwrap();
    assert_eq!(turn.content.as_deref(), Some("answer"));
    assert!(turn.usage.is_none());
    requests_have_only_the_authored_headers(&server);
}

#[test]
fn constructor_refusals_are_local_typed_request_errors() {
    assert!(matches!(
        OpenRouterClient::new(
            " ",
            "key".into(),
            Duration::from_secs(1),
            Settings::default()
        ),
        Err(InferenceError::InvalidRequest(RequestError::EmptyModel))
    ));
    assert!(matches!(
        OpenRouterClient::new(
            "model",
            " ".into(),
            Duration::from_secs(1),
            Settings::default()
        ),
        Err(InferenceError::InvalidRequest(
            RequestError::EmptyCredential
        ))
    ));
    assert!(matches!(
        OpenRouterClient::new("model", "key".into(), Duration::ZERO, Settings::default()),
        Err(InferenceError::InvalidRequest(RequestError::ZeroTimeout))
    ));
    let client = OpenRouterClient::new(
        "model",
        "key".into(),
        Duration::from_nanos(1),
        Settings::default(),
    )
    .unwrap();
    assert_eq!(
        client.endpoint,
        "https://openrouter.ai/api/v1/chat/completions"
    );
}

#[tokio::test]
async fn openrouter_trace_records_requested_controls_and_native_usage_without_private_state() {
    for body in [
        TOOL,
        "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\n",
    ] {
        let server = MockServer::start(vec![MockResponse::sse(body)]);
        let settings = Settings {
            generation: Some(settings::Generation {
                max_output_tokens: NonZeroU32::new(7),
                temperature: Some(0.5),
                top_p: Some(0.8),
            }),
            reasoning: Some(settings::Reasoning {
                effort: settings::Effort::High,
            }),
            routing: Some(settings::Routing {
                only: Some(vec!["alpha".into()]),
                allow_fallbacks: Some(false),
                require_parameters: Some(true),
            }),
            cache: Some(settings::Cache {
                style: settings::CacheStyle::ExplicitPrefix,
                ttl: Some(settings::Ttl::FiveMinutes),
            }),
        };
        let client = client(&server, settings).with_name("router-trace");
        let capture = crate::trace_capture::TraceCapture::default();
        let turn = generate(&client, &[ModelMessage::system("rules")])
            .with_subscriber(capture.subscriber())
            .await
            .unwrap();
        capture.assert_exchange("openrouter", "chat-completions");
        for (field, value) in [
            ("model.name", "\"router-trace\""),
            ("model.stream", "true"),
            ("generation.max_output_tokens", "7"),
            ("generation.temperature", "0.5"),
            ("generation.top_p", "0.8"),
            ("reasoning.effort", "\"high\""),
            ("routing.only", "\"alpha\""),
            ("routing.allow_fallbacks", "false"),
            ("routing.require_parameters", "true"),
            ("cache.style", "\"explicitPrefix\""),
            ("cache.ttl", "\"5m\""),
        ] {
            assert_eq!(capture.field(field).as_deref(), Some(value), "{field}");
        }
        assert!(capture.field("timing.first_event_ms").is_some());
        assert_eq!(
            capture.field("stream.first_delta_ms").is_some(),
            turn.content.is_some()
        );
        if turn.usage.is_some() {
            assert_eq!(
                capture.field("model.upstream").as_deref(),
                Some("\"alpha\"")
            );
            assert_eq!(
                capture.field("model.returned").as_deref(),
                Some("\"vendor/model\"")
            );
            for (field, value) in [
                ("input_tokens", 100),
                ("output_tokens", 12),
                ("total_tokens", 112),
                ("cached_input_tokens", 75),
                ("cache_write_tokens", 10),
                ("reasoning_output_tokens", 5),
            ] {
                assert_eq!(
                    capture.field(&format!("usage.{field}")),
                    Some(value.to_string())
                );
            }
        } else {
            assert!(!capture.text().contains("usage."));
            assert!(capture.field("model.upstream").is_none());
        }
        for secret in [
            "synthetic-key",
            "conversation-7",
            "think-more",
            "cipher-tail",
            "signature-2",
        ] {
            assert!(!capture.text().contains(secret));
        }
        requests_have_only_the_authored_headers(&server);
    }
}

#[tokio::test]
async fn a_public_router_override_rejects_remote_hosts_and_invalidates_native_replay() {
    let server = MockServer::start(vec![MockResponse::sse(TOOL)]);
    let client = client(&server, Settings::default());
    let first = generate(&client, &[]).await.unwrap();
    let client = client.with_loopback_endpoint(&server.base_url()).unwrap();
    assert!(matches!(
        generate(&client, &[assistant_message(&first)]).await,
        Err(InferenceError::Protocol(
            ProtocolFailure::ContinuationMismatch
        ))
    ));
    assert!(matches!(
        client.with_loopback_endpoint("https://openrouter.ai/api/v1/chat/completions"),
        Err(InferenceError::InvalidRequest(
            RequestError::InvalidLoopbackEndpoint
        ))
    ));
    assert_eq!(server.requests().len(), 1);
    requests_have_only_the_authored_headers(&server);
}
