use super::*;
use dekopon_harness::activity::ActivityLabel;
use tokio::io::AsyncWriteExt as _;
fn progress_target() -> ActivityTarget {
    ActivityTarget::Slack {
        channel_id: "C1".into(),
        thread_ts: "1700000000.000001".into(),
        message_ts: "1700000000.000001".into(),
        initiator_user_id: "U1".into(),
    }
}
fn progress_driver(base: &str) -> Arc<dyn ChatActivity> {
    slack_with(
        base,
        SlackExperience::Classic,
        SlackActivityConfig {
            mode: ActivityMode::Native,
            classic_fallback: SlackActivityFallback::None,
            progress_message: true,
        },
    )
    .activity()
    .unwrap()
}
#[tokio::test]
async fn progress_message_posts_plain_text_updates_and_deletes_only_validated_owned_handle() {
    let http = spawn_http_mock(|path, _| match path {
        "/api/chat.postMessage" | "/api/chat.update" | "/api/chat.delete" => {
            json!({"ok":true,"channel":"C1","ts":"1700000000.000002"})
        }
        _ => panic!("unexpected cosmetic API {path}"),
    });
    let driver = progress_driver(&http.base);
    let label = ActivityLabel::sanitized("\u{202e}<@U1> <!channel> https://private *secret*\n");
    let owned = driver
        .post_progress(progress_target(), label.clone())
        .await
        .unwrap()
        .unwrap();
    driver.update_progress(&owned, label).await.unwrap();
    driver.delete_progress(&owned).await.unwrap();
    let calls = http.calls();
    assert_eq!(calls.len(), 3);
    let bodies = calls
        .iter()
        .map(|(_, text)| serde_json::from_str::<Value>(text).unwrap())
        .collect::<Vec<_>>();
    for body in &bodies[..2] {
        assert_eq!(body["blocks"][0]["text"]["type"], "plain_text");
        assert_eq!(body["blocks"][0]["text"]["emoji"], false);
        assert_eq!(body["text"], "Working…");
        assert_eq!(body["mrkdwn"], false);
        assert_eq!(body["parse"], "none");
        assert_eq!(body["link_names"], false);
        assert_eq!(body["unfurl_links"], false);
        assert_eq!(body["unfurl_media"], false);
        let text = body["blocks"][0]["text"]["text"].as_str().unwrap();
        assert!(!text.contains('\u{202e}'));
        assert!(!text.contains('\n'));
        assert!(!text.contains("<@"));
        assert!(!text.contains("<!"));
        assert!(text.len() <= 84);
    }
    assert_eq!(bodies[0]["thread_ts"], "1700000000.000001");
    assert_eq!(bodies[0]["reply_broadcast"], false);
    assert_eq!(bodies[1]["as_user"], true);
    assert_eq!(bodies[2], json!({"channel":"C1","ts":"1700000000.000002"}));
}
#[tokio::test]
async fn progress_message_foreign_inbound_or_malformed_creation_never_yields_removal_authority() {
    for response in [
        json!({"ok":true,"channel":"OTHER","ts":"1700000000.000002"}),
        json!({"ok":true,"channel":"C1","ts":"1700000000.000001"}),
        json!({"ok":true,"channel":"C1","ts":"bad"}),
        json!({"ok":true}),
        json!({"ok":false,"error":"SECRET arbitrary content"}),
    ] {
        let http = spawn_http_mock(move |_, _| response.clone());
        let driver = progress_driver(&http.base);
        assert!(
            driver
                .post_progress(progress_target(), ActivityLabel::default())
                .await
                .is_err()
        );
        assert_eq!(http.calls().len(), 1);
    }
}
#[tokio::test]
async fn progress_message_permission_failure_disables_only_the_cosmetic_surface() {
    let http = spawn_http_mock(|_, _| json!({"ok":false,"error":"missing_scope"}));
    let driver = progress_driver(&http.base);
    assert!(
        driver
            .post_progress(progress_target(), ActivityLabel::default())
            .await
            .is_err()
    );
    assert!(!driver.progress_enabled());
    assert!(
        driver
            .post_progress(progress_target(), ActivityLabel::default())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(http.calls().len(), 1);
}
#[tokio::test]
async fn progress_message_rate_limit_and_response_bound_are_enforced_on_loopback() {
    for response in ["HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nRetry-After: 7\r\nConnection: close\r\n\r\n".to_owned(),format!("HTTP/1.1 200 OK\r\nContent-Length: 65537\r\nConnection: close\r\n\r\n{}", "x".repeat(65537))] {
        let rate_limited = response.starts_with("HTTP/1.1 429");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}",listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream,_) = listener.accept().await.unwrap();
            assert!(read_http_request_parts(&mut stream).await.is_some());
            if let Err(error) = stream.write_all(response.as_bytes()).await { assert!(matches!(error.kind(), std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset)); }
        });
        let driver = progress_driver(&base);
        let error = match driver.post_progress(progress_target(),ActivityLabel::default()).await { Err(error) => error, Ok(_) => panic!("response must refuse") };
        if rate_limited {
            assert!(matches!(error,TransportError::ActivityRateLimited { retry_after } if retry_after == Duration::from_secs(7)));
            assert!(matches!(driver.post_progress(progress_target(),ActivityLabel::default()).await,Err(TransportError::ActivityRateLimited { .. })),"cooldown prevents another HTTP transmission");
        } else { assert!(matches!(error,TransportError::Response)); }
        server.await.unwrap();
    }
}
#[tokio::test]
async fn progress_message_and_activity_labels_configuration_is_strict_bounded_and_consumed() {
    let directory = temporary();
    let mut doc = document(directory.path());
    doc["transports"][0] = json!({"name":"dev","kind":"slackSocketMode","appTokenEnv":"APP_TOKEN","botTokenEnv":"BOT_TOKEN","activity":{"mode":"native","progressMessage":true}});
    doc["routes"][0]["activityLabels"] = json!({"echo.echo":"Fetching Wikipedia page\u{202e}"});
    let config = load(directory.path(), &doc).await.unwrap();
    let routes = RoutingTable::bind(&config, &catalog(true, Some("reasoning"))).unwrap();
    assert_eq!(
        routes
            .route("dev", &ConversationKind::DirectMessage)
            .unwrap()
            .activity_labels["echo.echo"]
            .as_str(),
        "Fetching Wikipedia page"
    );
    doc["transports"][0]["activity"]["progressMessage"] = json!("yes");
    assert!(load(directory.path(), &doc).await.is_err());
    doc["transports"][0]["activity"]["progressMessage"] = json!(true);
    doc["transports"][0]["activity"]["mode"] = json!("off");
    doc["routes"][0]["activityLabels"] = json!({"invalid capability":"x".repeat(81)});
    let error = load(directory.path(), &doc).await.unwrap_err();
    let text = error.to_string();
    assert!(text.contains("activityLabels"));
    assert!(text.contains("Slack"));
    doc["transports"][0]["activity"]["mode"] = json!("native");
    doc["routes"][0]["activityLabels"] = Value::Object(
        (0..257)
            .map(|i| (format!("test.cap{i}"), json!("safe")))
            .collect(),
    );
    assert!(load(directory.path(), &doc).await.is_err());
}
