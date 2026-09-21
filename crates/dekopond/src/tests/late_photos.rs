use super::*;
use crate::transport::CancelRequest;

struct LateModel {
    blocked: Arc<BlockedModel>,
    script: Arc<ModelScript>,
}
impl ModelFactory for Arc<LateModel> {
    fn build(&self, _: &ModelConfig) -> Result<SharedModel, SessionError> {
        Ok(Arc::new(LateModelHandle(Arc::clone(self))))
    }
}
struct LateModelHandle(Arc<LateModel>);
impl ChatModel for LateModelHandle {
    fn complete(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
        on_event: &mut dyn FnMut(TurnEvent) -> ControlFlow<()>,
    ) -> Result<AssistantTurn, ModelError> {
        BlockedHandle(Arc::clone(&self.0.blocked)).complete(messages, tools, options, on_event)?;
        ScriptedModel(Arc::clone(&self.0.script)).complete(messages, tools, options, on_event)
    }
}

fn photo(text: &str) -> InboundMessage {
    let mut input = message(text);
    input.transport_kind = dekopon_broker_protocol::ChatTransportKind::Whatsapp;
    input.subject = ExternalSubject::whatsapp("16034700182").unwrap();
    input.conversation.container = Some("123:456".into());
    input.conversation.id = "16034700182".into();
    input.message_id = "wamid.photo".into();
    input.reply = ReplyTarget::WhatsApp {
        recipient: "16034700182".into(),
    };
    input.assets = vec![crate::asset::PendingAsset {
        name: "photo.png".into(),
        mime: "image/png".into(),
        size: None,
        source: Some(crate::asset::AssetSourceRef::WhatsApp {
            media_id: "123".into(),
            mime: "image/png".into(),
        }),
    }];
    input
}
fn request(text: &str) -> InboundMessage {
    let mut input = photo(text);
    input.assets.clear();
    input
}

struct Fixture {
    _directory: tempfile::TempDir,
    runner: Arc<SessionRunner>,
    routes: Arc<RoutingTable>,
    route: crate::routes::BoundRoute,
    driver: Arc<RecordingDriver>,
    model: Arc<LateModel>,
    observed: mpsc::UnboundedReceiver<RequestEnvelope>,
    running: Option<tokio::task::JoinHandle<()>>,
}
impl Fixture {
    async fn new(responses: Vec<ResponseEnvelope>, turns: Vec<AssistantTurn>) -> Self {
        let directory = temporary();
        let mut doc = document(directory.path());
        doc["routes"][0]["memory"] = json!({"mode":"persistent"});
        doc["models"][0]["modalities"] = json!(["image"]);
        let config = resolved(directory.path(), &doc).await;
        let routes =
            Arc::new(RoutingTable::bind(&config, &catalog(true, Some("reasoning"))).unwrap());
        let route = routes.route(&request("edit")).unwrap().clone();
        let (broker, observed) = stub_broker(directory.path(), responses).await;
        let model = Arc::new(LateModel {
            blocked: BlockedModel::new("unused"),
            script: ModelScript::new(turns),
        });
        let runner = runner_with(broker, Arc::new(Arc::clone(&model)), 1);
        let driver = Arc::new(RecordingDriver::default());
        let running = tokio::spawn(run_session(
            Arc::clone(&runner),
            route.clone(),
            request("make a version"),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            let entered = model.blocked.entered_signal.lock().await;
            while entered.try_recv().is_err() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        Self {
            _directory: directory,
            runner,
            routes,
            route,
            driver,
            model,
            observed,
            running: Some(running),
        }
    }
    fn capture(&self, mut input: InboundMessage) -> InboundMessage {
        input.late_photos = self.runner.active_sessions.late_photos(&self.route, &input);
        assert!(
            input.late_photos.is_some(),
            "receipt associated before debounce"
        );
        input
    }
    async fn retain(&self, input: InboundMessage) {
        run_session(
            Arc::clone(&self.runner),
            self.route.clone(),
            input,
            Arc::clone(&self.driver) as Arc<dyn ChatDriver>,
        )
        .await;
    }
    async fn finish(&mut self) {
        self.model.blocked.release();
        self.running.take().unwrap().await.unwrap();
    }
    fn capability_requests(&mut self) -> usize {
        let mut count = 0;
        while let Ok(envelope) = self.observed.try_recv() {
            assert!(
                matches!(envelope.request, BrokerRequest::Capabilities { .. }),
                "retention never invokes a provider"
            );
            count += 1;
        }
        count
    }
    fn inventory(&self) -> Vec<crate::asset::AssetRef> {
        let key = ConversationKey::private(
            &self.route.agent,
            "dev",
            &photo("").conversation.key(),
            &photo("").subject,
        );
        let seed = self.runner.conversations.begin(
            &key,
            &["cli-probe.upper".into()],
            self.route.memory.window().unwrap(),
            Instant::now(),
        );
        self.runner
            .assets
            .assets_for_access(&seed.assets, Vec::new(), true, Instant::now())
            .inventory
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn late_photos_busy_retention_is_lazy_and_next_request_sees_inventory_without_extra_calls() {
    let mut f = Fixture::new(
        listings(3, &["cli-probe.upper"]),
        vec![answer("First version."), answer("Next request.")],
    )
    .await;
    f.retain(f.capture(photo(""))).await;
    assert!(f.driver.replies().is_empty(), "notice waits for completion");
    f.finish().await;
    assert_eq!(f.model.script.requests(), 1);
    assert!(f.driver.replies()[0].contains("Would you like another version"));
    assert!(f.driver.replies()[0].contains("have not been downloaded"));
    f.retain(request("use the additional photo")).await;
    assert_eq!(f.model.script.requests(), 2);
    let next = f
        .model
        .script
        .prompt(1)
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(next.contains("Chat Asset #1"));
    assert!(
        !f.model
            .script
            .prompt(0)
            .iter()
            .any(|(_, text)| text.contains("Chat Asset #1"))
    );
    assert_eq!(f.capability_requests(), 3);
    assert!(
        f.runner.asset_fetchers.is_empty(),
        "retention requires no byte fetcher"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn late_photos_debounced_past_completion_acknowledge_once_and_never_start_a_run() {
    let mut f = Fixture::new(
        listings(2, &["cli-probe.upper"]),
        vec![answer("First version.")],
    )
    .await;
    let mut collector = burst_collector(None);
    let receipt = f.capture(photo(""));
    assert!(matches!(
        collector.offer(0, receipt),
        crate::collection::Offered::Pending
    ));
    f.finish().await;
    assert_eq!(f.driver.replies(), ["First version."]);
    let batch = collector.take_due(collector.deadline().unwrap()).remove(0);
    f.retain(batch).await;
    assert_eq!(f.driver.replies().len(), 2);
    assert!(f.driver.replies()[1].contains("Would you like another version"));
    assert_eq!(f.model.script.requests(), 1);
    assert_eq!(f.capability_requests(), 2);
    assert!(
        collector
            .take_due(tokio::time::Instant::now() + Duration::from_secs(60))
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn late_photos_cancel_keeps_registered_references_but_drops_pending_and_never_overrides_stopped()
 {
    let mut f = Fixture::new(
        listings(2, &["cli-probe.upper"]),
        vec![answer("Not delivered")],
    )
    .await;
    let pending = f.capture(photo(""));
    f.retain(f.capture(photo(""))).await;
    let input = photo("");
    assert_eq!(
        f.runner.active_sessions.cancel(&CancelRequest {
            transport: "dev".into(),
            conversation_id: input.conversation.key(),
            subject: input.subject.canonical(),
            via: CancelVia::StopReply
        }),
        CancelOutcome::Cancelled
    );
    f.retain(pending).await;
    f.finish().await;
    assert_eq!(f.driver.replies(), [crate::session::STOPPED_REPLY]);
    assert_eq!(f.inventory().len(), 1);
    assert_eq!(f.capability_requests(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn late_photos_failure_notice_never_claims_a_generated_version() {
    for after_completion in [false, true] {
        let mut f = Fixture::new(listings(3, &["cli-probe.upper"]), vec![]).await;
        let pending = f.capture(photo(""));
        // Admit one photo before failure so the temporary generation survives its unanswered turn.
        if after_completion {
            f.retain(f.capture(photo(""))).await;
        } else {
            f.retain(pending.clone()).await;
        }
        f.finish().await;
        if after_completion {
            f.retain(pending).await;
        }
        assert!(f.driver.replies()[0].contains("did not complete successfully"));
        assert!(!f.driver.replies()[0].contains("Would you like another version"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn late_photos_receipt_scope_excludes_other_actors_routes_audiences_and_transports() {
    let mut f = Fixture::new(listings(2, &["cli-probe.upper"]), vec![answer("Done")]).await;
    let original = photo("");
    let mut others = Vec::new();
    let mut input = original.clone();
    input.subject = ExternalSubject::whatsapp("16034700183").unwrap();
    others.push(input);
    let mut input = original.clone();
    input.conversation.id = "16034700183".into();
    others.push(input);
    let mut input = original.clone();
    input.conversation.container = Some("123:457".into());
    others.push(input);
    let mut input = original.clone();
    input.transport = "other".into();
    others.push(input);
    let mut input = original.clone();
    input.transport_kind = dekopon_broker_protocol::ChatTransportKind::Telegram;
    others.push(input);
    let mut input = original.clone();
    input.reply = ReplyTarget::WhatsApp {
        recipient: "16034700183".into(),
    };
    others.push(input);
    let mut input = original.clone();
    input.text = "use these instructions".into();
    others.push(input);
    for input in others {
        assert!(
            f.runner
                .active_sessions
                .late_photos(&f.route, &input)
                .is_none()
        );
    }
    let mut route = f.route.clone();
    route.memory = MemoryPolicy::OneShot;
    assert!(
        f.runner
            .active_sessions
            .late_photos(&route, &original)
            .is_none()
    );
    route = f.route.clone();
    route.cache_key = cache_key::for_route();
    assert!(
        f.runner
            .active_sessions
            .late_photos(&route, &original)
            .is_none()
    );
    route = f.route.clone();
    route.agent = "other".parse().unwrap();
    // Even an internal forged handle cannot register into another route's generation.
    let captured = f.capture(original);
    run_session(
        Arc::clone(&f.runner),
        route,
        captured,
        Arc::clone(&f.driver) as Arc<dyn ChatDriver>,
    )
    .await;
    f.finish().await;
    assert!(f.inventory().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn late_photos_fresh_grant_refusal_and_generation_replacement_never_fall_back_to_inference() {
    for replace in [false, true] {
        let responses = if replace {
            listings(2, &["cli-probe.upper"])
        } else {
            vec![
                listings(1, &["cli-probe.upper"]).remove(0),
                ResponseEnvelope::capabilities(Vec::new(), Vec::new()),
            ]
        };
        let mut f = Fixture::new(responses, vec![answer("Done")]).await;
        let pending = f.capture(photo(""));
        if replace {
            let key = ConversationKey::private(
                &f.route.agent,
                "dev",
                &photo("").conversation.key(),
                &photo("").subject,
            );
            f.runner
                .conversations
                .remove(&key, EvictionReason::GrantChanged);
        }
        f.retain(pending).await;
        f.finish().await;
        assert_eq!(f.model.script.requests(), 1);
        assert!(f.inventory().is_empty());
        assert!(f.driver.replies()[0].contains("not retained"));
        assert!(
            !f.driver
                .replies()
                .last()
                .unwrap()
                .contains("temporary inventory")
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn late_photos_old_receipt_cannot_attach_to_a_later_run_in_the_same_generation() {
    let mut f = Fixture::new(
        listings(3, &["cli-probe.upper"]),
        vec![answer("First"), answer("Second")],
    )
    .await;
    let pending = f.capture(photo(""));
    f.finish().await;
    f.retain(request("a new request")).await;
    f.retain(pending).await;
    assert_eq!(f.model.script.requests(), 2);
    assert_eq!(f.driver.replies().len(), 3);
    assert!(f.driver.replies()[2].contains("not retained"));
    assert!(f.inventory().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn late_photos_caption_join_is_explicitly_refused_without_discarding_photos() {
    let mut f = Fixture::new(listings(2, &["cli-probe.upper"]), vec![answer("Done")]).await;
    let mut collector = burst_collector(None);
    assert!(matches!(
        collector.offer(0, f.capture(photo(""))),
        crate::collection::Offered::Pending
    ));
    assert!(matches!(
        collector.offer(0, photo("change the background")),
        crate::collection::Offered::Refused(_, "late-instructions")
    ));
    let batch = collector.take_due(collector.deadline().unwrap()).remove(0);
    assert_eq!(batch.assets.len(), 1);
    assert!(batch.text.is_empty());
    f.retain(batch).await;
    f.finish().await;
    assert_eq!(f.inventory().len(), 1);
    assert_eq!(f.model.script.requests(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn late_photos_metadata_count_and_known_byte_edges_are_bounded() {
    for (count, size, accepted) in [
        (
            crate::asset::MAX_ASSETS_PER_CONVERSATION,
            crate::transport::whatsapp::MAX_IMAGE_BYTES as u64,
            true,
        ),
        (crate::asset::MAX_ASSETS_PER_CONVERSATION + 1, 1, false),
        (
            1,
            crate::transport::whatsapp::MAX_IMAGE_BYTES as u64 + 1,
            false,
        ),
    ] {
        let mut f = Fixture::new(listings(2, &["cli-probe.upper"]), vec![answer("Done")]).await;
        let mut input = photo("");
        input.assets[0].size = Some(size);
        input.assets = vec![input.assets[0].clone(); count];
        f.retain(f.capture(input)).await;
        f.finish().await;
        assert_eq!(f.inventory().len(), if accepted { count } else { 0 });
        assert_eq!(f.model.script.requests(), 1);
    }
}

#[tokio::test]
async fn late_photos_signed_whatsapp_webhook_dispatch_retains_a_busy_photo_after_debounce() {
    let (capture, _subscriber) = capture_spans();
    let mut f = Fixture::new(
        listings(2, &["cli-probe.upper"]),
        vec![answer("First version")],
    )
    .await;
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = probe.local_addr().unwrap();
    drop(probe);
    let mut transport = crate::transport::whatsapp::WhatsappTransport::new(
        "dev".into(),
        address,
        "/wa".into(),
        "123".into(),
        "456".into(),
        "v23.0".into(),
        "http://127.0.0.1:9".into(),
        "secret".into(),
        "verify".into(),
        "access".into(),
        liveness_settings(LivenessMode::Off),
    )
    .unwrap();
    transport.connect().await.unwrap();
    let body = serde_json::to_vec(&json!({"object":"whatsapp_business_account", "entry":[{"id":"123","changes":[{"field":"messages","value":{"messaging_product":"whatsapp","metadata":{"phone_number_id":"456"},"contacts":[{"wa_id":"16034700182"}],"messages":[{"id":"wamid.late-photo","from":"16034700182","type":"image","image":{"id":"321","mime_type":"image/png"}}]}}]}]})).unwrap();
    let signature: String = crate::transport::whatsapp::hmac_sha256(b"secret", &body)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let response = reqwest::Client::new()
        .post(format!("http://{address}/wa"))
        .header("x-hub-signature-256", format!("sha256={signature}"))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let input = next_message(&mut transport).await;
    assert_eq!(input.assets[0].size, None);
    let drivers = BTreeMap::from([("dev".into(), Arc::clone(&f.driver) as Arc<dyn ChatDriver>)]);
    let mut collector = burst_collector(None);
    let mut sessions = tokio::task::JoinSet::new();
    crate::dispatch(
        &f.runner,
        &f.routes,
        &BTreeMap::new(),
        &drivers,
        &[],
        &mut sessions,
        &mut collector,
        input,
    );
    assert!(sessions.is_empty());
    f.finish().await;
    let input = collector.take_due(collector.deadline().unwrap()).remove(0);
    assert!(input.late_photos.is_some());
    assert_eq!(input.constituents.len(), 1);
    f.retain(input).await;
    assert_eq!(f.model.script.requests(), 1);
    assert_eq!(f.inventory().len(), 1);
    assert_eq!(f.capability_requests(), 2);
    assert_eq!(f.driver.replies().len(), 2);
    let text = capture.text();
    assert!(text.contains("wamid.late-photo"));
    assert!(text.contains("gateway_input_disposition") && text.contains("late-acknowledged"));
    assert!(
        capture
            .span_parents()
            .iter()
            .any(|(name, parent)| *name == "gateway.message"
                && parent.as_deref() == Some("transport.receive"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn late_photos_during_a_slow_provider_call_neither_restart_nor_add_provider_work() {
    let directory = temporary();
    let socket = directory.path().join("broker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let reached = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let invocation_count = Arc::new(AtomicUsize::new(0));
    let (entered, released, invoked) = (
        Arc::clone(&reached),
        Arc::clone(&release),
        Arc::clone(&invocation_count),
    );
    let broker_task = tokio::spawn(async move {
        let mut calls = tokio::task::JoinSet::new();
        // Initial grant, command, parked invocation, late-input fresh grant: no other work.
        for _ in 0..4 {
            let (stream, _) = listener.accept().await.unwrap();
            let (entered, released, invoked) = (
                Arc::clone(&entered),
                Arc::clone(&released),
                Arc::clone(&invoked),
            );
            calls.spawn(async move {
                let mut stream = dekopon_broker_protocol::DescriptorStream::new(stream);
                let (envelope, _) = stream
                    .read_frame::<RequestEnvelope>(FrameLimits::default())
                    .await
                    .unwrap();
                let response = match envelope.request {
                    BrokerRequest::Capabilities { .. } => probe_listing(),
                    BrokerRequest::RunCommand { .. } => upper_proposal("done"),
                    BrokerRequest::Invoke { .. } => {
                        invoked.fetch_add(1, Ordering::SeqCst);
                        entered.notify_one();
                        released.notified().await;
                        ResponseEnvelope::invocation(
                            record_output(json!({"text":"done"})),
                            Vec::new(),
                            Vec::new(),
                            Vec::new(),
                        )
                    }
                    other => panic!("unexpected request: {other:?}"),
                };
                stream
                    .write_frame(&response, &[], FrameLimits::default())
                    .await
                    .unwrap();
            });
        }
        while let Some(call) = calls.join_next().await {
            call.unwrap();
        }
    });
    let broker = ResolvedBroker {
        socket_path: socket,
        server_uid: crate::current_uid(),
        frame: FrameLimits::default(),
    };
    let model = ModelScript::new([
        script_call("probe upper --text done"),
        answer("Version complete"),
    ]);
    let runner = runner(broker, Arc::clone(&model), 1);
    let route = persistent_route(model_config(), window());
    let driver = Arc::new(RecordingDriver::default());
    let running = tokio::spawn(run_session(
        Arc::clone(&runner),
        route.clone(),
        request("make a version"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    ));
    tokio::time::timeout(Duration::from_secs(5), reached.notified())
        .await
        .unwrap();
    let mut input = photo("");
    input.late_photos = runner.active_sessions.late_photos(&route, &input);
    assert!(input.late_photos.is_some());
    run_session(
        Arc::clone(&runner),
        route,
        input,
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert!(!running.is_finished());
    assert_eq!(model.requests(), 1);
    assert_eq!(invocation_count.load(Ordering::SeqCst), 1);
    assert!(driver.replies().is_empty());
    release.notify_one();
    running.await.unwrap();
    broker_task.await.unwrap();
    assert_eq!(model.requests(), 2);
    assert_eq!(invocation_count.load(Ordering::SeqCst), 1);
    assert!(driver.replies()[0].contains("Would you like another version"));
}

#[tokio::test(flavor = "multi_thread")]
async fn late_photos_zero_retention_budget_refuses_without_claiming_saved_bytes() {
    let directory = temporary();
    let (broker, mut observed) =
        stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let model = BlockedModel::new("Done");
    let mut runner = runner_with(broker, Arc::new(Arc::clone(&model)), 1);
    Arc::get_mut(&mut runner).unwrap().assets =
        Arc::new(AssetStore::with_retention(1, Duration::from_secs(60), 0));
    let route = persistent_route(model_config(), window());
    let driver = Arc::new(RecordingDriver::default());
    let running = tokio::spawn(run_session(
        Arc::clone(&runner),
        route.clone(),
        request("make a version"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    ));
    model.wait_until_entered().await;
    let mut input = photo("");
    input.late_photos = runner.active_sessions.late_photos(&route, &input);
    run_session(
        Arc::clone(&runner),
        route,
        input,
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert!(driver.replies()[0].contains("not retained"));
    model.release();
    running.await.unwrap();
    assert_eq!(driver.replies()[1], "Done");
    for _ in 0..2 {
        assert!(matches!(
            observed.recv().await.unwrap().request,
            BrokerRequest::Capabilities { .. }
        ));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn late_photos_evicted_references_get_truthful_completion_notice_and_no_stale_inventory() {
    let mut f = Fixture::new(listings(2, &["cli-probe.upper"]), vec![answer("Done")]).await;
    f.retain(f.capture(photo(""))).await;
    let key = ConversationKey::private(
        &f.route.agent,
        "dev",
        &photo("").conversation.key(),
        &photo("").subject,
    );
    f.runner
        .conversations
        .remove(&key, EvictionReason::Capacity);
    f.finish().await;
    assert!(f.driver.replies()[0].contains("no longer available"));
    assert!(!f.driver.replies()[0].contains("temporary inventory"));
    assert!(f.inventory().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn late_photos_receipt_before_completion_binds_even_when_dispatch_runs_after_completion() {
    let mut f = Fixture::new(listings(2, &["cli-probe.upper"]), vec![answer("Done")]).await;
    let queued_receipt = photo("");
    f.finish().await;
    let late = f.capture(queued_receipt);
    f.retain(late).await;
    assert_eq!(f.model.script.requests(), 1);
    assert_eq!(f.driver.replies().len(), 2);
    assert!(f.driver.replies()[1].contains("temporary inventory"));
    let input = photo("");
    assert!(
        f.runner
            .active_sessions
            .late_photos(&f.route, &input)
            .is_none(),
        "an actual idle receipt retains ordinary admission"
    );
    assert_eq!(
        f.runner.active_sessions.cancel(&CancelRequest {
            transport: "dev".into(),
            conversation_id: input.conversation.key(),
            subject: input.subject.canonical(),
            via: CancelVia::StopReply
        }),
        CancelOutcome::NoSession
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn late_photos_evicted_receipt_history_fails_closed_instead_of_starting_paid_work() {
    let mut f = Fixture::new(
        listings(2, &["cli-probe.upper"]),
        vec![answer("First"), answer("Second")],
    )
    .await;
    let queued_receipt = photo("");
    f.finish().await;
    f.retain(request("second request")).await;
    let late = f.capture(queued_receipt);
    assert!(matches!(
        late.late_photos,
        Some(crate::session::LatePhotoReceipt::HistoryUnavailable)
    ));
    f.retain(late).await;
    assert_eq!(f.model.script.requests(), 2);
    assert_eq!(
        f.capability_requests(),
        2,
        "unknown history never opens a broker leg"
    );
    assert!(f.driver.replies()[2].contains("not retained"));
    assert!(
        f.runner
            .active_sessions
            .late_photos(&f.route, &photo(""))
            .is_none(),
        "watermark does not block new idle receipts"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn late_photos_old_queued_receipt_does_not_join_a_newer_active_run() {
    let mut f = Fixture::new(listings(3, &["cli-probe.upper"]), vec![answer("First")]).await;
    let queued_receipt = photo("");
    f.finish().await;
    let next_model = BlockedModel::new("Second");
    Arc::get_mut(&mut f.runner).unwrap().models =
        Arc::new(ModelCache::new(Arc::new(Arc::clone(&next_model))));
    let running = tokio::spawn(run_session(
        Arc::clone(&f.runner),
        f.route.clone(),
        request("new request"),
        Arc::clone(&f.driver) as Arc<dyn ChatDriver>,
    ));
    next_model.wait_until_entered().await;
    let late = f.capture(queued_receipt);
    f.retain(late).await;
    assert!(f.driver.replies()[1].contains("not retained"));
    next_model.release();
    running.await.unwrap();
    assert_eq!(f.driver.replies()[2], "Second");
    assert!(f.inventory().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn late_photos_authorization_concurrency_accepts_the_ceiling_and_refuses_one_past_it() {
    let directory = temporary();
    let (broker, reached, release) = parked_broker(
        directory.path(),
        listings(1, &["cli-probe.upper"]),
        listings(1, &["cli-probe.upper"]).remove(0),
    )
    .await;
    let model = BlockedModel::new("Done");
    let runner = runner_with(broker, Arc::new(Arc::clone(&model)), 1);
    let route = persistent_route(model_config(), window());
    let driver = Arc::new(RecordingDriver::default());
    let running = tokio::spawn(run_session(
        Arc::clone(&runner),
        route.clone(),
        request("make a version"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    ));
    model.wait_until_entered().await;
    let mut input = photo("");
    input.late_photos = runner.active_sessions.late_photos(&route, &input);
    let late = tokio::spawn(run_session(
        Arc::clone(&runner),
        route.clone(),
        input.clone(),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    ));
    tokio::time::timeout(Duration::from_secs(5), reached.notified())
        .await
        .unwrap();
    run_session(
        Arc::clone(&runner),
        route,
        input,
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(driver.replies().len(), 1);
    assert!(driver.replies()[0].contains("not retained"));
    model.release();
    running.await.unwrap();
    assert_eq!(driver.replies()[1], "Done");
    release.notify_one();
    late.await.unwrap();
    assert!(driver.replies()[2].contains("temporary inventory"));
}

#[test]
fn late_photos_expired_conversation_refuses_before_metadata_registration() {
    let store = ConversationStore::new(1);
    let input = photo("");
    let route = persistent_route(model_config(), window());
    let key = ConversationKey::private(
        &route.agent,
        "dev",
        &input.conversation.key(),
        &input.subject,
    );
    let granted = vec!["cli-probe.upper".into()];
    let old = Instant::now() - window().idle_timeout - Duration::from_secs(1);
    let seed = store.begin(&key, &granted, window(), old);
    let receipt = seed.input;
    seed.lease.commit(
        window(),
        ConversationTurn::unanswered("prior request"),
        &seed.cache_key,
        old,
    );
    let result = store.retain_late_assets(
        &receipt,
        &granted,
        window(),
        &seed.cache_key,
        || -> Option<()> { panic!("expired generation must never register metadata") },
    );
    assert!(matches!(
        result,
        Err(crate::conversation::LateAssetRefusal::Expired)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn late_photos_repeated_batches_keep_only_the_bounded_inventory() {
    let mut f = Fixture::new(listings(4, &["cli-probe.upper"]), vec![answer("Done")]).await;
    for _ in 0..3 {
        let mut input = photo("");
        input.assets = vec![input.assets[0].clone(); crate::asset::MAX_ASSETS_PER_CONVERSATION];
        f.retain(f.capture(input)).await;
    }
    f.finish().await;
    let inventory = f.inventory();
    assert_eq!(inventory.len(), crate::asset::MAX_ASSETS_PER_CONVERSATION);
    assert_eq!(
        inventory[0].id,
        (2 * crate::asset::MAX_ASSETS_PER_CONVERSATION + 1) as u64
    );
    assert_eq!(f.driver.replies().len(), 1);
    assert_eq!(
        f.driver.replies()[0]
            .matches("Would you like another version")
            .count(),
        1
    );
}
