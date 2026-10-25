use super::*;

async fn cold_asset_broker(
    directory: &Path,
    responses: Vec<ResponseEnvelope>,
) -> (ResolvedBroker, mpsc::UnboundedReceiver<RequestEnvelope>) {
    let socket = directory.join("broker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let (observed, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut responses = responses
            .into_iter()
            .map(|reply| (reply, Vec::new()))
            .collect();
        while !VecDeque::is_empty(&responses) {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = DescriptorStream::new(stream);
            let (request, inputs) = stream
                .read_frame::<RequestEnvelope>(FrameLimits::default())
                .await
                .unwrap();
            if let BrokerRequest::Run { assets, .. } = &request.request {
                assert_eq!(assets.len(), 1);
                let descriptor = inputs.into_iter().next().expect("the input descriptor");
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut std::fs::File::from(descriptor), &mut bytes)
                    .unwrap();
                assert_eq!(bytes, b"pixels");
            }
            let (response, descriptors) =
                dekopon_test_support::next_broker_reply(&request, &mut responses).unwrap();
            assert!(descriptors.is_empty());
            observed.send(request).unwrap();
            stream
                .write_frame(&response, &[], FrameLimits::default())
                .await
                .unwrap();
        }
    });
    (
        ResolvedBroker {
            socket_path: socket,
            server_uid: crate::current_uid(),
            frame: FrameLimits::default(),
        },
        receiver,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_native_session_never_loses_its_task_when_image_edit_reads_a_cold_chat_asset() {
    cold_asset_turn(Some("image edit --image chat-asset:1")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_native_session_never_loses_its_task_when_asset_reads_a_cold_chat_asset() {
    cold_asset_turn(Some("asset read chat-asset:1")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_native_session_never_loses_its_task_when_the_model_fetches_a_cold_chat_asset() {
    cold_asset_turn(None).await;
}

struct ColdImage(Arc<AtomicUsize>);
impl AssetFetcher for ColdImage {
    fn fetch(&self, _: &AssetSourceRef, _: u64) -> BoxFuture<'_, Result<Vec<u8>, TransportError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(b"pixels".to_vec()) })
    }
}

async fn cold_asset_turn(command: Option<&str>) {
    let directory = temporary();
    let mut responses = vec![ResponseEnvelope::capabilities(
        vec![capability("image.edit")],
        vec!["image".to_owned(), "asset".to_owned()],
        BTreeMap::new(),
    )];
    let first = match command {
        Some(command) => {
            responses.push(ResponseEnvelope::command_run(
                serde_json::from_value(json!({
                    "outcome": "proposed",
                    "capability": "image.edit",
                    "input": {"image": "chat-asset:1"}
                }))
                .unwrap(),
            ));
            responses.push(ResponseEnvelope::invocation(
                record_success(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ));
            script_call(command)
        }
        None => AssistantTurn::new(
            None,
            vec![ModelToolCall {
                id: "cold-fetch".into(),
                kind: "function".into(),
                function: ModelFunctionCall {
                    name: "fetch_chat_asset".into(),
                    arguments: json!({"id": 1}).to_string(),
                },
            }],
            None,
        ),
    };
    let (broker, mut observed) = cold_asset_broker(directory.path(), responses).await;
    let models = ModelScript::new([first, answer("The command completed.")]);
    models.capture_images.store(true, Ordering::SeqCst);
    let fetches = Arc::new(AtomicUsize::new(0));
    let mut runner = runner(broker, Arc::clone(&models), 4);
    Arc::get_mut(&mut runner)
        .unwrap()
        .asset_fetchers
        .insert("dev".into(), Arc::new(ColdImage(Arc::clone(&fetches))));
    let mut model = model_config();
    if let ModelConfig::OpenaiCompatible { modalities, .. } = &mut model {
        *modalities = vec![crate::config::Modality::Image];
    }
    let mut inbound = message("Use this attachment");
    inbound.assets = vec![pending("input.png", "image/png", 6)];
    let driver = Arc::new(RecordingDriver::default());
    run_session(
        runner,
        route(model),
        inbound,
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(driver.replies(), ["The command completed."], "{command:?}");
    assert_eq!(models.requests(), 2, "{command:?}");
    assert_eq!(fetches.load(Ordering::SeqCst), 1, "{command:?}");
    assert!(matches!(
        observed.recv().await.unwrap().request,
        BrokerRequest::Capabilities { .. }
    ));
    match command {
        Some(command) => {
            let BrokerRequest::Run {
                word, argv, assets, ..
            } = observed.recv().await.unwrap().request
            else {
                panic!("the script must run the command with its cold input");
            };
            let argv = argv
                .into_iter()
                .map(|arg| match arg {
                    dekopon_broker_protocol::Arg::Text(text) => text,
                    dekopon_broker_protocol::Arg::Asset(index) => {
                        format!("chat-asset:{}", assets[usize::from(index.get())].id)
                    }
                })
                .collect::<Vec<_>>();
            let (expected_word, expected_argv) = command.split_once(' ').unwrap();
            assert_eq!(word, expected_word);
            assert_eq!(argv.join(" "), expected_argv);
            assert_eq!(assets.len(), 1);
            assert_eq!(assets[0].id, 1);
            assert_eq!(assets[0].bytes, Some(6));
        }
        None => assert_eq!(*models.image_bytes.lock(), [b"pixels".to_vec()]),
    }
    assert!(observed.recv().await.is_none(), "no retry");
}
