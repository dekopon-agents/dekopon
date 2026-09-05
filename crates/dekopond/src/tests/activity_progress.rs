use super::*;
use crate::activity::ActivityLease;
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

#[tokio::test(flavor = "multi_thread")]
async fn gateway_answers_survive_enforced_slack_channel_quota_with_progress_on_or_off() {
    use dekopon_harness::history::DeliveryDisposition;
    for progress in [false, true] {
        for outage in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let calls = Arc::new(Mutex::new(Vec::new()));
            let recorded = calls.clone();
            let posted = Arc::new(tokio::sync::Notify::new());
            let post_seen = posted.clone();
            let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
            let server = tokio::spawn(async move {
                let mut stopped = std::pin::pin!(stopped);
                let mut last = HashMap::<String, Instant>::new();
                let mut sequence = 1;
                loop {
                    let (mut stream, _) = tokio::select! {
                        result = &mut stopped => { result.unwrap(); break; }
                        accepted = tokio::time::timeout(Duration::from_secs(15), listener.accept()) => accepted.unwrap().unwrap(),
                    };
                    let (path, _, body) = tokio::time::timeout(
                        Duration::from_secs(3),
                        read_http_request_parts(&mut stream),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                    let body: Value = serde_json::from_str(&body).unwrap();
                    let channel = body["channel"].as_str().unwrap().to_owned();
                    let cosmetic = body["text"] == "Working…";
                    let now = Instant::now();
                    let status = match path.as_str() {
                        "/api/chat.postMessage" => {
                            if (outage && !cosmetic)
                                || last.get(&channel).is_some_and(|prior| {
                                    now.duration_since(*prior) < Duration::from_secs(1)
                                })
                            {
                                429
                            } else {
                                last.insert(channel.clone(), now);
                                sequence += 1;
                                200
                            }
                        }
                        "/api/chat.delete" => 200,
                        _ => panic!("unexpected Slack endpoint {path}"),
                    };
                    let timestamp = if path == "/api/chat.delete" {
                        body["ts"].as_str().unwrap().to_owned()
                    } else {
                        format!("1700000000.{sequence:06}")
                    };
                    let response =
                        json!({"ok":status == 200,"channel":channel,"ts":timestamp}).to_string();
                    let retry = if outage { 6 } else { 1 };
                    let wire = format!(
                        "HTTP/1.1 {status} OK\r\nRetry-After: {retry}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                        response.len()
                    );
                    recorded.lock().unwrap().push((path, body, status));
                    stream.write_all(wire.as_bytes()).await.unwrap();
                    if cosmetic {
                        post_seen.notify_one();
                    }
                }
            });
            let transport = slack_with(
                &base,
                SlackExperience::Classic,
                SlackActivityConfig {
                    mode: if progress {
                        ActivityMode::Native
                    } else {
                        ActivityMode::Off
                    },
                    classic_fallback: SlackActivityFallback::None,
                    progress_message: progress,
                },
            );
            let directory = temporary();
            let (broker, mut observed) =
                stub_broker(directory.path(), listings(3, &["echo.echo"])).await;
            let model = BlockedModel::new("same paid-for answer");
            let mut runner = runner_with(broker, Arc::new(model.clone()), 4);
            if let Some(driver) = transport.activity() {
                Arc::get_mut(&mut runner)
                    .unwrap()
                    .activities
                    .insert("dev".into(), driver);
            }
            let bound = persistent_route(model_config(), window());
            let mut inbound = message("same request");
            inbound.activity = Some(progress_target());
            inbound.reply = ReplyTarget::Slack {
                channel: "C1".into(),
                thread_ts: Some("1700000000.000001".into()),
            };
            let session = tokio::spawn(run_session(
                runner.clone(),
                bound.clone(),
                inbound.clone(),
                transport.replier(),
            ));
            model.wait_until_entered().await;
            if progress {
                tokio::time::timeout(Duration::from_secs(3), posted.notified())
                    .await
                    .unwrap();
            }
            model.release();
            tokio::time::timeout(Duration::from_secs(12), session)
                .await
                .unwrap()
                .unwrap();
            assert_surface_checks(&mut observed, 3);
            let seed = session_seed(
                &runner,
                &bound,
                &inbound,
                listings(1, &["echo.echo"]).remove(0),
            );
            let [record] = seed.history.turns() else {
                panic!("one completed job")
            };
            assert_eq!(record.generated.as_deref(), Some("same paid-for answer"));
            assert_eq!(
                record.delivery,
                if outage {
                    DeliveryDisposition::Failed
                } else {
                    DeliveryDisposition::Accepted {
                        text: "same paid-for answer".into(),
                    }
                }
            );
            let checkpoint = dekopon_harness::checkpoint::memory_checkpoints()
                .load(&record.job)
                .unwrap();
            assert!(checkpoint.finalized);
            assert_eq!(checkpoint.record, *record);
            assert_eq!(checkpoint.state.accounting.calls.len(), 1);
            assert_eq!(checkpoint.state.accounting.calls[0].attempts.len(), 1);
            if !outage {
                let target = inbound.reply.clone();
                let replier = transport.replier();
                let (a, b) = tokio::join!(
                    replier.reply(target.clone(), OutboundReply::text("concurrent A")),
                    replier.reply(target, OutboundReply::text("concurrent B"))
                );
                assert!(a.unwrap().accepted());
                assert!(b.unwrap().accepted());
            }
            // Cleanup is independently bounded; it can be shed while finals wait.
            let finals: Vec<_> = calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(path, body, _)| {
                    path == "/api/chat.postMessage" && body["text"] != "Working…"
                })
                .cloned()
                .collect();
            assert_eq!(
                finals
                    .iter()
                    .filter(|(_, body, status)| body["text"] == "same paid-for answer"
                        && *status == 200)
                    .count(),
                usize::from(!outage)
            );
            if outage {
                assert_eq!(
                    finals.len(),
                    1,
                    "Retry-After beyond the bound is not retried"
                );
                assert_eq!(finals[0].2, 429);
            } else {
                for text in ["same paid-for answer", "concurrent A", "concurrent B"] {
                    assert_eq!(
                        finals
                            .iter()
                            .filter(|(_, body, status)| body["text"] == text && *status == 200)
                            .count(),
                        1,
                        "exactly one accepted copy of each final"
                    );
                }
            }
            stop.send(()).unwrap();
            server.await.unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_image_answer_takes_the_same_physical_channel_slot_as_a_text_answer() {
    // `files.completeUploadExternal` creates a channel message exactly as `chat.postMessage` does,
    // so it is paced with it. Obtaining the upload URL and sending the bytes create nothing.
    let base = Arc::new(std::sync::OnceLock::<String>::new());
    let handler_base = Arc::clone(&base);
    let stamps = Arc::new(Mutex::new(Vec::<(String, Instant)>::new()));
    let recorder = Arc::clone(&stamps);
    let http = spawn_http_mock(move |path, _| {
        recorder
            .lock()
            .unwrap()
            .push((path.to_owned(), Instant::now()));
        match path {
            "/api/chat.postMessage" => json!({"ok":true,"channel":"C1","ts":"1700000000.000002"}),
            "/api/files.getUploadURLExternal" => json!({
                "ok": true,
                "file_id": "F1",
                "upload_url": format!("{}/upload", handler_base.get().expect("mock base")),
            }),
            "/upload" => json!({"ok":true}),
            "/api/files.completeUploadExternal" => json!({"ok":true,"files":[{"id":"F1"}]}),
            other => panic!("unexpected Slack endpoint {other}"),
        }
    });
    base.set(http.base.clone()).expect("mock base is set once");
    let replier = slack(&http.base).replier();
    let target = ReplyTarget::Slack {
        channel: "C1".into(),
        thread_ts: None,
    };
    let image = generated_image();

    assert!(
        replier
            .reply(target.clone(), OutboundReply::text("text first"))
            .await
            .unwrap()
            .accepted()
    );
    assert!(
        replier
            .reply(target.clone(), OutboundReply::with_image("look", image))
            .await
            .unwrap()
            .accepted()
    );
    assert!(
        replier
            .reply(target, OutboundReply::text("text after"))
            .await
            .unwrap()
            .accepted()
    );

    let stamps = stamps.lock().unwrap().clone();
    let at = |path: &str| {
        stamps
            .iter()
            .find(|(seen, _)| seen == path)
            .unwrap_or_else(|| panic!("{path} was called"))
            .1
    };
    let first_text = at("/api/chat.postMessage");
    let describe = at("/api/files.getUploadURLExternal");
    let complete = at("/api/files.completeUploadExternal");
    let second_text = stamps
        .iter()
        .filter(|(path, _)| path == "/api/chat.postMessage")
        .nth(1)
        .expect("the text answer after the image")
        .1;
    assert!(
        describe.duration_since(first_text) < Duration::from_millis(500),
        "obtaining an upload URL creates no channel message and is not paced"
    );
    assert!(
        complete.duration_since(first_text) >= Duration::from_secs(1),
        "the image completion waits for the channel slot the text answer took"
    );
    assert!(
        second_text.duration_since(complete) >= Duration::from_secs(1),
        "a text answer after an image completion waits for the slot the image took"
    );
}

/// An activity driver whose progress removal takes long enough that "did shutdown wait?" is a
/// question with one answer rather than a race.
struct SlowCleanupActivity {
    posted: Arc<tokio::sync::Notify>,
    deleted: Arc<std::sync::atomic::AtomicBool>,
}
impl ChatActivity for SlowCleanupActivity {
    fn show(&self, _: ActivityTarget) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async { Ok(()) })
    }
    fn hide(&self, _: ActivityTarget) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async { Ok(()) })
    }
    fn refresh_interval(&self) -> Option<Duration> {
        None
    }
    fn retire(&self, _: &ActivityTarget) {}
    fn progress_enabled(&self) -> bool {
        true
    }
    fn post_progress(
        &self,
        _: ActivityTarget,
        _: dekopon_harness::activity::ActivityLabel,
    ) -> BoxFuture<'_, Result<Option<crate::transport::slack::OwnedProgressArtifact>, TransportError>>
    {
        Box::pin(async {
            self.posted.notify_one();
            Ok(Some(
                crate::transport::slack::OwnedProgressArtifact::fixture("C1", "1700000000.000002"),
            ))
        })
    }
    fn delete_progress<'a>(
        &'a self,
        _: &'a crate::transport::slack::OwnedProgressArtifact,
    ) -> BoxFuture<'a, Result<(), TransportError>> {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            self.deleted
                .store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_drains_activity_cleanup_before_the_routing_loop_returns() {
    // A SIGTERM mid-session used to tear the runtime down on top of a detached worker that had
    // only just started removing the ⌛ message, leaving it in somebody's channel forever.
    let posted = Arc::new(tokio::sync::Notify::new());
    let deleted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let driver: Arc<dyn ChatActivity> = Arc::new(SlowCleanupActivity {
        posted: Arc::clone(&posted),
        deleted: Arc::clone(&deleted),
    });
    let mut lease = ActivityLease::start(Some(driver), Some(progress_target()), false);
    tokio::time::timeout(Duration::from_secs(4), posted.notified())
        .await
        .expect("the progress artifact is created");
    lease.finish_in_background();
    assert!(
        !deleted.load(std::sync::atomic::Ordering::Acquire),
        "removal is still in flight when shutdown begins"
    );

    let directory = temporary();
    let (runner, routes) = idle_routing_loop(directory.path()).await;
    let (_sender, receiver) = mpsc::channel(4);
    let outcome = crate::serve(
        runner,
        routes,
        Arc::new(BTreeMap::new()),
        Arc::new(BTreeMap::new()),
        receiver,
        std::future::ready(()),
        Duration::from_secs(5),
    )
    .await;

    assert_eq!(outcome, crate::ServeOutcome::Shutdown);
    assert!(
        deleted.load(std::sync::atomic::Ordering::Acquire),
        "the routing loop drains activity cleanup inside the shutdown grace"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn every_transport_that_cannot_connect_is_named_in_one_refusal() {
    // Two configurations claiming one Slack installation fail *each other*; naming only the first
    // one connected hides the half of the conflict an operator has to look at to resolve it.
    let directory = temporary();
    std::fs::write(
        directory.path().join("dekopon.yaml"),
        catalog_text(true, Some("reasoning")),
    )
    .expect("catalog fixture writes");
    for name in ["first.sock", "second.sock"] {
        std::fs::write(
            directory.path().join(name),
            b"an ordinary file, not a socket",
        )
        .expect("socket stand-in writes");
    }
    let mut document = document(directory.path());
    document["broker"]["serverUid"] = json!(crate::current_uid());
    document["transports"] = json!([
        {
            "name": "first-local",
            "kind": "local",
            "socketPath": directory.path().join("first.sock")
        },
        {
            "name": "second-local",
            "kind": "local",
            "socketPath": directory.path().join("second.sock")
        }
    ]);
    document["routes"][0]["transport"] = json!("first-local");
    let (_broker, _observed) = stub_broker(directory.path(), listings(2, &["echo.echo"])).await;
    let path = write_config(directory.path(), &document);

    let error = crate::run(&path, std::future::pending())
        .await
        .expect_err("neither transport can bind its socket");
    let crate::DekopondError::TransportConnect { problems } = &error else {
        panic!("one refusal naming both transports, not the first one: {error:?}");
    };
    assert_eq!(problems.len(), 2, "{problems:?}");
    let rendered = error.to_string();
    assert!(
        rendered.contains("first-local") && rendered.contains("second-local"),
        "the refusal names both transports: {rendered}"
    );
}

#[tokio::test]
async fn authenticated_duplicate_slack_startup_refuses_before_opening_another_socket() {
    let first_socket = spawn_socket_mock(vec![]);
    let next_socket = spawn_socket_mock(vec![]);
    let http = spawn_http_mock(slack_handler(vec![first_socket.url, next_socket.url]));
    let mut first = slack(&http.base);
    first.connect().await.unwrap();
    let retained_replier = first.replier();
    let mut duplicate = crate::transport::slack::SlackTransport::new(
        "independent-config".into(),
        http.base.clone(),
        "another-app-token".into(),
        "another-bot-token".into(),
        SlackExperience::Classic,
        SlackActivityConfig::default(),
    )
    .unwrap();
    let error = duplicate.connect().await.unwrap_err();
    assert!(
        matches!(error, TransportError::Service { code } if code == "duplicate-slack-installation")
    );
    drop(first);
    let error = duplicate.connect().await.unwrap_err();
    assert!(
        matches!(error, TransportError::Service { code } if code == "duplicate-slack-installation"),
        "a retained reply/activity owner still owns physical installation budgets"
    );
    assert_eq!(
        http.calls()
            .iter()
            .filter(|(path, _)| path == "/api/apps.connections.open")
            .count(),
        1
    );
    drop(retained_replier);
    duplicate.connect().await.unwrap();
    assert_eq!(
        http.calls()
            .iter()
            .filter(|(path, _)| path == "/api/auth.test")
            .count(),
        4
    );
    assert_eq!(
        http.calls()
            .iter()
            .filter(|(path, _)| path == "/api/apps.connections.open")
            .count(),
        2
    );
}
