use super::*;

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

#[tokio::test(flavor = "multi_thread")]
async fn native_history_drops_the_bot_footer_before_the_model_sees_it() {
    let directory = temporary();
    let (broker, _asked) = naming_broker(directory.path(), &[(SUBJECT, "xavier")]).await;
    let models = ModelScript::new([answer("done")]);
    let user = "literal\n\n-# model · 1 step · 7 s";
    let driver = HistoryDriver::new(Ok(vec![
        past("0000000001", "U1", "tel.16035550101", user),
        bot_past("0000000002", "Earlier answer.\n\n-# model · 1 step · 7 s"),
        bot_past("0000000003", "-# model · 1 step · 7 s"),
    ]));
    run_session(
        runner(broker, Arc::clone(&models), 4),
        persistent_route(model_config(), recall_window(RecallSource::Platform)),
        message("continue"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    let prompt = models.prompt(0);
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
