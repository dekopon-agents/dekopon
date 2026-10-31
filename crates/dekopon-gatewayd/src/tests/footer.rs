use super::*;
use dekopon_broker_protocol::ChatTransportKind;

async fn exchange(driver: Arc<dyn ChatDriver>, target: ReplyTarget) {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = ModelScript::new([answer("First answer."), answer("Second answer.")]);
    let runner = runner(broker, Arc::clone(&models), 4);
    let route = persistent_route(model_config(), window());
    for (index, text) in ["first question", "second question"]
        .into_iter()
        .enumerate()
    {
        let mut incoming = message(text);
        incoming.message_id = MessageId::Native(format!("footer-{index}"));
        incoming.reply = target.clone();
        run_session(
            Arc::clone(&runner),
            route.clone(),
            incoming,
            Arc::clone(&driver),
        )
        .await;
    }
    let prompt = models.prompt(1);
    assert_eq!(
        prompt
            .iter()
            .filter(|(role, _)| role == "assistant")
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>(),
        ["First answer."]
    );
    assert!(prompt.iter().all(|(_, text)| !text.contains(" · ")));
}

fn assert_footer(line: &str) {
    let seconds = line
        .strip_prefix("portable · 1 step · ")
        .unwrap()
        .strip_suffix(" s")
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert_eq!(line, format!("portable · 1 step · {seconds} s"));
}

#[tokio::test(flavor = "multi_thread")]
async fn slack_posts_the_exact_footer_and_the_next_prompt_has_none() {
    let http = spawn_http_mock(slack_handler(Vec::new()));
    exchange(
        slack(&http.base).driver(),
        ReplyTarget::Slack {
            channel: "d0123abc".into(),
            thread_ts: None,
        },
    )
    .await;
    let posts = http
        .calls()
        .into_iter()
        .filter(|(path, _)| path == "/api/chat.postMessage")
        .map(|(_, body)| serde_json::from_str::<Value>(&body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(posts.len(), 2);
    for (post, answer) in posts.iter().zip(["First answer.", "Second answer."]) {
        assert_eq!(post["text"], answer);
        assert_eq!(post["blocks"][0]["text"], answer);
        assert_eq!(post["blocks"].as_array().unwrap().len(), 2);
        let line = post["blocks"][1]["elements"][0]["text"].as_str().unwrap();
        assert_footer(line);
        assert_eq!(
            post["blocks"][1],
            json!({"type":"context","elements":[{"type":"plain_text","text":line}]})
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_posts_the_exact_footer_and_the_next_prompt_has_none() {
    let http = spawn_http_mock(discord_handler(String::new()));
    exchange(
        discord(&http.base).driver(),
        ReplyTarget::Discord {
            channel_id: DISCORD_CHANNEL.into(),
            reply_to: None,
        },
    )
    .await;
    let posts = http
        .calls()
        .into_iter()
        .filter(|(path, _)| path.ends_with("/messages"))
        .map(|(_, body)| serde_json::from_str::<Value>(&body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(posts.len(), 2);
    for (post, answer) in posts.iter().zip(["First answer.", "Second answer."]) {
        let content = post["content"].as_str().unwrap();
        let (body, line) = content.rsplit_once("\n\n-# ").unwrap();
        assert_eq!(body, answer);
        assert_footer(line);
        assert_eq!(content, format!("{answer}\n\n-# {line}"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn telegram_posts_the_exact_italic_footer_and_the_next_prompt_has_none() {
    let http =
        spawn_http_mock(|_, _| json!({"ok":true,"result":{"message_id":12,"chat":{"id":42}}}));
    exchange(
        telegram(&http.base).driver(),
        ReplyTarget::Telegram {
            chat_id: 42,
            reply_to: None,
            message_thread_id: None,
        },
    )
    .await;
    let posts = http
        .calls()
        .into_iter()
        .filter(|(path, _)| path.contains("sendMessage"))
        .map(|(_, body)| serde_json::from_str::<Value>(&body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(posts.len(), 2);
    for (post, answer) in posts.iter().zip(["First answer.", "Second answer."]) {
        let content = post["text"].as_str().unwrap();
        let (body, line) = content.rsplit_once("\n\n").unwrap();
        assert_eq!(body, answer);
        assert_footer(line);
        assert_eq!(content, format!("{answer}\n\n{line}"));
        assert_eq!(
            post["entities"],
            json!([{"type":"italic", "offset":answer.encode_utf16().count()+2,"length":line.encode_utf16().count()}])
        );
        assert!(post.get("parse_mode").is_none());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn whatsapp_posts_the_exact_italic_footer_and_the_next_prompt_has_none() {
    let http = spawn_http_mock(
        |_, _| json!({"messaging_product":"whatsapp","messages":[{"id":"wamid.reply"}]}),
    );
    let transport = crate::transport::whatsapp::WhatsappTransport::new(
        "wa".into(),
        "127.0.0.1:0".parse().unwrap(),
        "/wa".into(),
        "123".into(),
        "456".into(),
        "v23.0".into(),
        http.base.clone(),
        "app-fixture".into(),
        "verify-fixture".into(),
        "access-fixture".into(),
        LivenessSettings::default(),
    )
    .unwrap();
    exchange(
        transport.driver(),
        ReplyTarget::WhatsApp {
            recipient: "16034700182".into(),
        },
    )
    .await;
    let posts = http
        .calls()
        .into_iter()
        .map(|(_, body)| serde_json::from_str::<Value>(&body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(posts.len(), 2);
    for (post, answer) in posts.iter().zip(["First answer.", "Second answer."]) {
        let content = post["text"]["body"].as_str().unwrap();
        let (body, line) = content.rsplit_once("\n\n_").unwrap();
        let line = line.strip_suffix('_').unwrap();
        assert_eq!(body, answer);
        assert_footer(line);
        assert_eq!(content, format!("{answer}\n\n_{line}_"));
    }
}

async fn cold_recall(
    transport_kind: ChatTransportKind,
    prior: Vec<crate::transport::PastMessage>,
) -> Vec<(String, String)> {
    let directory = temporary();
    let (broker, _asked) = naming_broker(directory.path(), &[(SUBJECT, "xavier")]).await;
    let models = ModelScript::new([answer("done")]);
    let driver = HistoryDriver::new(Ok(prior));
    let mut incoming = message("continue");
    incoming.transport_kind = transport_kind;
    run_session(
        runner(broker, Arc::clone(&models), 4),
        persistent_route(model_config(), recall_window(RecallSource::Platform)),
        incoming,
        driver as Arc<dyn ChatDriver>,
    )
    .await;
    models.prompt(0)
}

#[tokio::test(flavor = "multi_thread")]
async fn native_history_drops_the_bot_footer_before_the_model_sees_it() {
    let user = "literal\n\n-# model · 1 step · 7 s";
    let prompt = cold_recall(
        ChatTransportKind::Discord,
        vec![
            past("0000000001", "U1", "tel.16035550101", user),
            bot_past("0000000002", "Earlier answer.\n\n-# model · 1 step · 7 s"),
            bot_past("0000000003", "-# model · 1 step · 7 s"),
        ],
    )
    .await;
    assert_eq!(
        prompt
            .iter()
            .filter(|(role, _)| role == "assistant")
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>(),
        ["Earlier answer."]
    );
    assert!(
        prompt
            .iter()
            .any(|(role, text)| role == "user" && text.ends_with(user))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_discord_subtext_answer_keeps_its_text_without_the_footer_on_cold_recall() {
    let prompt = cold_recall(
        ChatTransportKind::Discord,
        vec![bot_past(
            "0000000001",
            "-# brief answer\n\n-# model-a · 1 step · 7 s",
        )],
    )
    .await;
    assert_eq!(
        prompt
            .iter()
            .filter(|(role, _)| role == "assistant")
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>(),
        ["-# brief answer"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn slack_cold_recall_preserves_footer_shaped_answer_text() {
    let answer = "Example footer:\n\n-# model-a · 1 step · 7 s";
    let prompt = cold_recall(
        ChatTransportKind::Slack,
        vec![bot_past("0000000001", answer)],
    )
    .await;
    assert_eq!(
        prompt
            .iter()
            .filter(|(role, _)| role == "assistant")
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>(),
        [answer]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn github_webhook_and_local_answers_carry_no_footer() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = ModelScript::new([answer("Webhook answer."), answer("Local answer.")]);
    let runner = runner(broker, models, 4);
    let driver = Arc::new(RecordingDriver::default());
    for target in [
        ReplyTarget::GithubTrace,
        ReplyTarget::Local { connection: 1 },
    ] {
        let mut incoming = message("answer");
        incoming.reply = target;
        run_session(
            Arc::clone(&runner),
            route(model_config()),
            incoming,
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
    }
    assert_eq!(driver.replies(), ["Webhook answer.", "Local answer."]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_chat_answer_carries_no_footer() {
    let http = spawn_http_mock(slack_handler(Vec::new()));
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let models = ModelScript::results([Err(InferenceError::Protocol(
        dekopon_model::error::ProtocolFailure::NoChoices,
    ))]);
    let mut incoming = message("answer");
    incoming.reply = ReplyTarget::Slack {
        channel: "d0123abc".into(),
        thread_ts: None,
    };
    run_session(
        runner(broker, models, 4),
        route(model_config()),
        incoming,
        slack(&http.base).driver(),
    )
    .await;
    let posts = http
        .calls()
        .into_iter()
        .filter(|(path, _)| path == "/api/chat.postMessage")
        .map(|(_, body)| serde_json::from_str::<Value>(&body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0]["blocks"],
        json!([{"type":"markdown", "text":FAILURE_REPLY}])
    );
    assert_eq!(posts[0]["text"], FAILURE_REPLY);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_chat_answer_carries_no_footer() {
    let http = spawn_http_mock(slack_handler(Vec::new()));
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let models = InterruptibleModel::new([answer("must not be posted")]);
    let runner = runner_with(
        broker,
        Arc::new(Arc::clone(&models)) as Arc<dyn ModelFactory>,
        4,
    );
    let mut incoming = message("answer");
    incoming.reply = ReplyTarget::Slack {
        channel: "d0123abc".into(),
        thread_ts: None,
    };
    let running = tokio::spawn(run_session(
        Arc::clone(&runner),
        route(model_config()),
        incoming,
        slack(&http.base).driver(),
    ));
    models.wait_until_entered().await;
    assert_eq!(
        runner
            .gate
            .cancel(&cancel(SUBJECT, CancelVia::NativeStop))
            .running,
        CancelOutcome::Cancelled
    );
    running.await.unwrap();
    let posts = http
        .calls()
        .into_iter()
        .filter(|(path, _)| path == "/api/chat.postMessage")
        .map(|(_, body)| serde_json::from_str::<Value>(&body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0]["blocks"],
        json!([{"type":"markdown", "text":crate::session::STOPPED_REPLY}])
    );
    assert_eq!(posts[0]["text"], crate::session::STOPPED_REPLY);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_short_slack_image_answer_is_one_file_share_ending_with_its_footer() {
    let base = Arc::new(Mutex::new(String::new()));
    let response_base = Arc::clone(&base);
    let http = spawn_http_mock(move |path, _| match path {
        "/api/files.getUploadURLExternal" => json!({
            "ok":true,"upload_url":format!("{}/upload", response_base.lock()),"file_id":"f-generated"
        }),
        "/upload" => json!({"uploaded":true}),
        "/api/files.completeUploadExternal" => json!({"ok":true,"files":[{"id":"f-generated"}]}),
        _ => json!({"ok":true,"channel":"d0123abc","ts":"1712345678.000101"}),
    });
    *base.lock() = http.base.clone();
    slack(&http.base)
        .driver()
        .reply(
            &ReplyTarget::Slack {
                channel: "d0123abc".into(),
                thread_ts: Some("1712345678.000100".into()),
            },
            OutboundReply::with_images("Here is your kitty.", generated_images(1)).with_footer(
                Some(crate::progress::Footer::new(
                    &["model-a".into()],
                    1,
                    Duration::from_secs(7),
                )),
            ),
        )
        .await
        .unwrap();
    let calls = http.calls();
    assert_eq!(
        calls.len(),
        3,
        "one upload and file share, with no separate answer post"
    );
    assert_eq!(calls[2].0, "/api/files.completeUploadExternal");
    let posted: Value = serde_json::from_str(&calls[2].1).unwrap();
    assert_eq!(posted["channel_id"], "d0123abc");
    assert_eq!(posted["thread_ts"], "1712345678.000100");
    assert!(posted.get("initial_comment").is_none());
    let blocks: Value = serde_json::from_str(posted["blocks"].as_str().unwrap()).unwrap();
    assert_eq!(
        blocks,
        json!([
            {"type":"markdown","text":"Here is your kitty."},
            {"type":"context","elements":[{"type":"plain_text","text":"model-a · 1 step · 7 s"}]}
        ])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_short_telegram_image_answer_is_one_captioned_message_ending_with_its_footer() {
    let http = spawn_http_mock(|_, _| {
        json!({"ok":true,"result":{
            "message_id":12,"chat":{"id":-1001},"message_thread_id":77,"photo":[{"file_id":"photo"}]
        }})
    });
    telegram(&http.base)
        .driver()
        .reply(
            &ReplyTarget::Telegram {
                chat_id: -1001,
                reply_to: Some(3),
                message_thread_id: Some(77),
            },
            OutboundReply::with_images("Here is your kitty. 🐈", generated_images(1)).with_footer(
                Some(crate::progress::Footer::new(
                    &["model-a".into()],
                    1,
                    Duration::from_secs(7),
                )),
            ),
        )
        .await
        .unwrap();
    let calls = http.calls();
    assert_eq!(
        calls.len(),
        1,
        "the photo, answer and footer arrive together"
    );
    assert!(calls[0].0.ends_with("/sendPhoto"));
    let multipart = &calls[0].1;
    assert!(
        multipart.contains(
            "name=\"caption\"\r\n\r\nHere is your kitty. 🐈\n\nmodel-a · 1 step · 7 s\r\n"
        )
    );
    assert!(multipart.contains(&format!("name=\"caption_entities\"\r\n\r\n{}\r\n", json!([
        {"type":"italic","offset":"Here is your kitty. 🐈\n\n".encode_utf16().count(),"length":"model-a · 1 step · 7 s".encode_utf16().count()}
    ]))));
    assert!(multipart.contains("name=\"reply_parameters\"\r\n\r\n{\"message_id\":3}\r\n"));
    assert!(multipart.contains("name=\"message_thread_id\"\r\n\r\n77\r\n"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_short_whatsapp_image_answer_is_one_captioned_message_ending_with_its_footer() {
    let http = spawn_http_mock(|path, _| match path {
        "/v23.0/456/media" => json!({"id":"987"}),
        "/v23.0/456/messages" => {
            json!({"messaging_product":"whatsapp","messages":[{"id":"wamid.reply"}]})
        }
        other => panic!("unexpected WhatsApp image call: {other}"),
    });
    let transport = crate::transport::whatsapp::WhatsappTransport::new(
        "wa".into(),
        "127.0.0.1:0".parse().unwrap(),
        "/wa".into(),
        "123".into(),
        "456".into(),
        "v23.0".into(),
        http.base.clone(),
        "app-fixture".into(),
        "verify-fixture".into(),
        "access-fixture".into(),
        LivenessSettings::default(),
    )
    .unwrap();
    transport
        .driver()
        .reply(
            &ReplyTarget::WhatsApp {
                recipient: "16034700182".into(),
            },
            OutboundReply::with_images("Here is your kitty.", generated_images(1)).with_footer(
                Some(crate::progress::Footer::new(
                    &["model-a".into()],
                    1,
                    Duration::from_secs(7),
                )),
            ),
        )
        .await
        .unwrap();
    let calls = http.calls();
    assert_eq!(
        calls.len(),
        2,
        "one media upload and one captioned image send"
    );
    assert_eq!(calls[1].0, "/v23.0/456/messages");
    let posted: Value = serde_json::from_str(&calls[1].1).unwrap();
    assert_eq!(posted["type"], "image");
    assert_eq!(
        posted["image"],
        json!({"id":"987","caption":"Here is your kitty.\n\n_model-a · 1 step · 7 s_"})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_telegram_footer_that_outgrows_the_caption_leaves_the_answer_on_the_image() {
    let http = spawn_http_mock(|_, _| {
        json!({"ok":true,"result":{
            "message_id":12,"chat":{"id":42},"photo":[{"file_id":"photo"}]
        }})
    });
    let text = "🐈".repeat(510);
    telegram(&http.base)
        .driver()
        .reply(
            &ReplyTarget::Telegram {
                chat_id: 42,
                reply_to: Some(3),
                message_thread_id: None,
            },
            OutboundReply::with_images(&text, generated_images(1)).with_footer(Some(
                crate::progress::Footer::new(&["model-a".into()], 1, Duration::from_secs(7)),
            )),
        )
        .await
        .unwrap();
    let calls = http.calls();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].0.ends_with("/sendPhoto"));
    assert!(
        calls[0]
            .1
            .contains(&format!("name=\"caption\"\r\n\r\n{text}\r\n"))
    );
    assert!(!calls[0].1.contains("caption_entities"));
    assert!(calls[1].0.ends_with("/sendMessage"));
    let footer: Value = serde_json::from_str(&calls[1].1).unwrap();
    assert_eq!(footer["text"], "model-a · 1 step · 7 s");
    assert_eq!(
        footer["entities"],
        json!([{"type":"italic","offset":0,"length":22}])
    );
}
