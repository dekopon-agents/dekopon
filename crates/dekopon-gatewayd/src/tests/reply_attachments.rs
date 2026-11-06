use super::*;
use dekopon_test_support::CaptureLayer;

const IMAGES: u64 = 5;

fn five_image_broker_replies() -> Vec<(ResponseEnvelope, Vec<std::os::fd::OwnedFd>)> {
    let mut replies = vec![plain_response(probe_listing())];
    for index in 1..=IMAGES {
        replies.push(plain_response(upper_proposal(&format!("image {index}"))));
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(format!("image {index}").as_bytes());
        replies.push(asset_response(&png, "image/png"));
    }
    replies.push(plain_response(upper_proposal("send")));
    replies.push(queued_response_of((1..=IMAGES).collect()));
    replies
}

fn queued_response_of(ids: Vec<u64>) -> (ResponseEnvelope, Vec<std::os::fd::OwnedFd>) {
    plain_response(ResponseEnvelope::invocation(
        record_success(),
        Vec::new(),
        Vec::new(),
        ids,
    ))
}

fn five_image_models(answer_text: Option<&str>) -> Arc<ModelScript> {
    let create = (1..=IMAGES)
        .map(|index| format!("probe upper --text 'image {index}'"))
        .collect::<Vec<_>>()
        .join("\n");
    ModelScript::scripted([
        Some(script_call(&create)),
        Some(script_call("probe upper --text send")),
        answer_text.map(answer),
    ])
}

fn slack_inbound() -> InboundMessage {
    let mut inbound = message("draw five kittens");
    inbound.reply = ReplyTarget::Slack {
        channel: "d0123abc".into(),
        thread_ts: Some("1712345678.000100".into()),
    };
    inbound
}

struct SlackStandIn {
    mock: HttpMock,
}

impl SlackStandIn {
    fn start(refused_upload: Option<usize>) -> Self {
        let base = Arc::new(Mutex::new(String::new()));
        let response_base = Arc::clone(&base);
        let described = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mock = spawn_http_mock(move |path, body| match path {
            "/api/files.getUploadURLExternal" => {
                let index = described.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if Some(index) == refused_upload {
                    json!({"ok": false, "error": "internal_error"})
                } else {
                    json!({
                        "ok": true,
                        "upload_url": format!("{}/upload", response_base.lock()),
                        "file_id": format!("f-{index}")
                    })
                }
            }
            "/upload" => json!({"uploaded": true}),
            "/api/files.completeUploadExternal" => {
                let request: Value = serde_json::from_str(body).expect("completion JSON");
                json!({"ok": true, "files": request["files"]})
            }
            _ => json!({"ok": true, "channel": "d0123abc", "ts": "1712345678.000101"}),
        });
        *base.lock() = mock.base.clone();
        Self { mock }
    }

    fn calls_to(&self, path: &str) -> Vec<String> {
        self.mock
            .calls()
            .into_iter()
            .filter(|(called, _)| called == path)
            .map(|(_, body)| body)
            .collect()
    }

    fn completions(&self) -> Vec<Value> {
        self.calls_to("/api/files.completeUploadExternal")
            .iter()
            .map(|body| serde_json::from_str(body).expect("completion JSON"))
            .collect()
    }
}

async fn run_five_images(slack: &SlackStandIn, models: Arc<ModelScript>) {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker_assets(directory.path(), five_image_broker_replies()).await;
    tokio::time::timeout(
        Duration::from_secs(20),
        run_session(
            runner(broker, models, 4),
            route(model_config()),
            slack_inbound(),
            crate::tests::slack(&slack.mock.base).driver(),
        ),
    )
    .await
    .expect("session finishes");
}

fn dispatched(capture: &CaptureLayer, dispatched: &str) -> Vec<String> {
    let mut ids: Vec<String> = capture
        .events_where(
            "dekopon_agent::audit",
            &[
                ("audit.event", "agent.asset.send"),
                ("dispatched", dispatched),
            ],
        )
        .iter()
        .filter_map(|record| record.field("asset.id").map(str::to_owned))
        .collect();
    ids.sort();
    ids
}

fn shared_answer(share: &Value) -> Value {
    let blocks: Value = serde_json::from_str(
        share["blocks"]
            .as_str()
            .expect("the footer travels in blocks"),
    )
    .expect("blocks JSON");
    blocks[0]["text"].clone()
}

fn ids(range: impl IntoIterator<Item = u64>) -> Vec<String> {
    range.into_iter().map(|id| id.to_string()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn five_images_are_one_slack_share_with_the_answer() {
    let (capture, _guard) = capture_spans();
    let slack = SlackStandIn::start(None);
    run_five_images(&slack, five_image_models(Some("Five kittens, best first."))).await;

    let completions = slack.completions();
    assert_eq!(completions.len(), 1, "one file share carries every image");
    let share = &completions[0];
    assert_eq!(
        share["files"].as_array().map(Vec::len),
        Some(IMAGES as usize)
    );
    assert_eq!(shared_answer(share), "Five kittens, best first.");
    assert_eq!(share["thread_ts"], "1712345678.000100");
    assert!(
        slack.calls_to("/api/chat.postMessage").is_empty(),
        "the answer is the share's comment, not a second message"
    );
    assert_eq!(dispatched(&capture, "true"), ids(1..=IMAGES));
    assert!(dispatched(&capture, "false").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_model_is_told_a_send_is_queued_not_sent() {
    let slack = SlackStandIn::start(None);
    let models = five_image_models(Some("Five kittens."));
    run_five_images(&slack, Arc::clone(&models)).await;

    let tool = tool_message(&models, 2);
    for id in 1..=IMAGES {
        assert!(
            tool.lines().any(|line| line
                == format!(
                    "[gateway: chat-asset:{id} queued ({id} of 10 for this reply); it is delivered after your final answer, so say it will be attached, not that it was sent]"
                )),
            "{tool}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_abandoned_turn_records_no_successful_dispatch() {
    let (capture, _guard) = capture_spans();
    let slack = SlackStandIn::start(None);
    run_five_images(&slack, five_image_models(None)).await;

    assert!(slack.completions().is_empty());
    assert!(dispatched(&capture, "true").is_empty());
    assert_eq!(dispatched(&capture, "false"), ids(1..=IMAGES));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_upload_mid_group_marks_only_that_asset_undispatched() {
    let (capture, _guard) = capture_spans();
    let slack = SlackStandIn::start(Some(2));
    run_five_images(&slack, five_image_models(Some("Five kittens."))).await;

    let completions = slack.completions();
    assert_eq!(completions.len(), 1);
    let shared: Vec<&str> = completions[0]["files"]
        .as_array()
        .expect("files array")
        .iter()
        .filter_map(|file| file["id"].as_str())
        .collect();
    assert_eq!(shared, ["f-0", "f-1", "f-3", "f-4"]);
    assert_eq!(
        shared_answer(&completions[0]),
        "Five kittens.\n\nNot attached: asset-3.png"
    );
    assert_eq!(dispatched(&capture, "true"), ids([1, 2, 4, 5]));
    assert_eq!(dispatched(&capture, "false"), ids([3]));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_sent_asset_is_noted_and_never_dispatched() {
    let (capture, _guard) = capture_spans();
    let slack = SlackStandIn::start(None);
    let directory = temporary();
    let mut replies = five_image_broker_replies();
    replies.pop();
    replies.push(queued_response_of(vec![1, 99]));
    let (broker, _observed) = stub_broker_assets(directory.path(), replies).await;
    let models = five_image_models(Some("One kitten."));
    tokio::time::timeout(
        Duration::from_secs(20),
        run_session(
            runner(broker, Arc::clone(&models), 4),
            route(model_config()),
            slack_inbound(),
            crate::tests::slack(&slack.mock.base).driver(),
        ),
    )
    .await
    .expect("session finishes");

    let tool = tool_message(&models, 2);
    assert!(
        tool.lines()
            .any(|line| line.starts_with("[gateway: chat-asset:99 was not queued: ")),
        "{tool}"
    );
    assert_eq!(
        slack.completions()[0]["files"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(dispatched(&capture, "true"), ids([1]));
    assert_eq!(dispatched(&capture, "false"), ids([99]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_send_the_broker_denies_is_never_queued() {
    let (capture, _guard) = capture_spans();
    let slack = SlackStandIn::start(None);
    let directory = temporary();
    let mut replies = five_image_broker_replies();
    replies.pop();
    replies.push(plain_response(ResponseEnvelope::invocation(
        record_result(InvocationOutcome::Denied, Some("asset.send is not granted")),
        Vec::new(),
        Vec::new(),
        vec![1],
    )));
    let (broker, _observed) = stub_broker_assets(directory.path(), replies).await;
    let models = five_image_models(Some("No kittens."));
    tokio::time::timeout(
        Duration::from_secs(20),
        run_session(
            runner(broker, Arc::clone(&models), 4),
            route(model_config()),
            slack_inbound(),
            crate::tests::slack(&slack.mock.base).driver(),
        ),
    )
    .await
    .expect("session finishes");

    assert!(!tool_message(&models, 2).contains("queued"));
    assert!(slack.completions().is_empty(), "nothing is shared");
    assert_eq!(slack.calls_to("/api/chat.postMessage").len(), 1);
    assert!(dispatched(&capture, "true").is_empty());
}

fn telegram_inbound() -> InboundMessage {
    let mut inbound = message("draw three kittens");
    inbound.reply = ReplyTarget::Telegram {
        chat_id: 42,
        reply_to: None,
        message_thread_id: None,
    };
    inbound
}

fn three_image_broker_replies() -> Vec<(ResponseEnvelope, Vec<std::os::fd::OwnedFd>)> {
    let mut replies = five_image_broker_replies();
    replies.truncate(1 + 2 * 3);
    replies.push(plain_response(upper_proposal("send")));
    replies.push(queued_response_of(vec![1, 2, 3]));
    replies
}

fn three_image_models(answer_text: &str) -> Arc<ModelScript> {
    let create = (1..=3)
        .map(|index| format!("probe upper --text 'image {index}'"))
        .collect::<Vec<_>>()
        .join("\n");
    ModelScript::new([
        script_call(&create),
        script_call("probe upper --text send"),
        answer(answer_text),
    ])
}

async fn run_three_images_on_telegram(accepted_photos: usize, answer_text: &str) -> CaptureLayer {
    let (capture, _guard) = capture_spans();
    let photos = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let http = spawn_http_mock(move |path, _body| {
        let photo = path.contains("sendPhoto");
        let index = photos.fetch_add(usize::from(photo), std::sync::atomic::Ordering::SeqCst);
        if photo && index < accepted_photos {
            json!({
                "ok": true,
                "result": {"message_id": 12, "chat": {"id": 42}, "photo": [{"file_id": "photo"}]}
            })
        } else {
            json!({"ok": false, "description": "rejected"})
        }
    });
    let directory = temporary();
    let (broker, _observed) =
        stub_broker_assets(directory.path(), three_image_broker_replies()).await;
    tokio::time::timeout(
        Duration::from_secs(20),
        run_session(
            runner(broker, three_image_models(answer_text), 4),
            route(model_config()),
            telegram_inbound(),
            crate::tests::telegram(&http.base).driver(),
        ),
    )
    .await
    .expect("session finishes");
    capture
}

#[tokio::test(flavor = "multi_thread")]
async fn a_text_failure_after_every_photo_still_records_each_photo_dispatched() {
    let capture = run_three_images_on_telegram(3, &"x".repeat(1_100)).await;
    assert_eq!(dispatched(&capture, "true"), ids(1..=3));
    assert!(dispatched(&capture, "false").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_photo_failure_after_an_earlier_photo_records_only_the_delivered_one() {
    let capture = run_three_images_on_telegram(1, "Three kittens.").await;
    assert_eq!(dispatched(&capture, "true"), ids([1]));
    assert_eq!(dispatched(&capture, "false"), ids([2, 3]));
}
