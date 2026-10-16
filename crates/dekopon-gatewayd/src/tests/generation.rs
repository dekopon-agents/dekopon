use super::*;
use crate::{
    config::memory::{PolicyConfig, ResolvedPolicy},
    memory::generation::prepare,
};

fn policy(kind: &str, retain: &[&str]) -> ResolvedPolicy {
    let config: PolicyConfig = serde_json::from_value(json!({
        "capture": {"user":"uncapped", "assistant":"uncapped", "toolRequest":"uncapped", "toolResult":"uncapped"},
        "routines": [
            {"id":"original", "kind":"original"},
            {"id":"summary", "kind":kind, "model":"router", "parts":[{"id":"core", "maxBytes":8}, {"id":"detail", "maxBytes":16}]}
        ],
        "retain":retain,
        "selection":{"mode":"fixed", "routine":"summary"}
    }))
    .expect("policy fixture");
    let mut resolved = crate::config::memory::resolve_policy(
        Some(&config),
        None,
        &["router".to_owned()].into(),
        &Default::default(),
    )
    .expect("valid policy");
    resolved.models.insert("router".to_owned(), model_config());
    resolved
}

fn captured_turn() -> ConversationTurn {
    use dekopon_agent::prompt::{Exchange, Representation};
    ConversationTurn::completed("PRIVATE_SOURCE_SENTINEL", "answer").with_exchanges(vec![
        Exchange {
            tool: "shell".into(),
            request: vec![Representation::original("PRIVATE_SOURCE_SENTINEL", "")],
            result: vec![Representation::original("result", "")],
            outcome: "success".into(),
            exit_status: Some(0),
        },
    ])
}

#[test]
fn one_shot_parts_are_ordered_bounded_and_a_failed_summary_keeps_no_original() {
    let model = ModelScript::new([answer(
        r#"{"parts":[{"id":"core","text":"first"},{"id":"detail","text":"second"}]}"#,
    )]);
    let policy = policy("progressiveOneShot", &["summary"]);
    let prepared = prepare(&captured_turn(), &policy, |_| {
        Some(Arc::new(ScriptedModel(Arc::clone(&model))))
    });
    assert_eq!(model.requests(), 1);
    assert_eq!(
        prepared
            .representations
            .iter()
            .filter(|item| item.text.is_some())
            .map(|item| (item.part.as_deref(), item.text.as_deref()))
            .collect::<Vec<_>>(),
        [
            (Some("core"), Some("first")),
            (Some("detail"), Some("second"))
        ]
    );
    assert_eq!(
        prepared.availability,
        ["summary:part:core", "summary:part:detail"]
    );
    assert!(!prepared.original);
    assert_eq!(prepared.exchanges[0].request[0].captured_bytes, 23);
    assert!(prepared.exchanges[0].request[0].text.is_none());
    assert!(prepared.exchanges[0].result[0].text.is_none());
    let prompt = model.prompt(0);
    assert!(prompt[1].1.contains("PRIVATE_SOURCE_SENTINEL"));
    assert!(
        !serde_json::to_string(&prepared.representations)
            .unwrap()
            .contains("PRIVATE_SOURCE_SENTINEL")
    );

    let failing = ModelScript::new([]);
    let prepared = prepare(&captured_turn(), &policy, |_| {
        Some(Arc::new(ScriptedModel(Arc::clone(&failing))))
    });
    assert_eq!(failing.requests(), 1);
    assert_eq!(prepared.availability, ["summary:generation-failed"]);
    assert_eq!(prepared.representations.len(), 1);
    assert!(prepared.representations[0].text.is_none());
    assert!(
        !serde_json::to_string(&prepared.exchanges)
            .unwrap()
            .contains("PRIVATE_SOURCE_SENTINEL")
    );

    let oversized = ModelScript::new([answer(
        r#"{"parts":[{"id":"core","text":"abcdefgh🦀"},{"id":"detail","text":"123456789"}]}"#,
    )]);
    let bounded = prepare(&captured_turn(), &policy, |_| {
        Some(Arc::new(ScriptedModel(Arc::clone(&oversized))))
    });
    let texts: Vec<_> = bounded
        .representations
        .iter()
        .filter_map(|item| item.text.as_deref())
        .collect();
    assert_eq!(texts, ["abcdefgh", "12345678"]);
    assert_eq!(texts.iter().map(|text| text.len()).sum::<usize>(), 16);
}

#[test]
fn turducken_parts_receive_prior_parts_and_keep_a_successful_prefix_after_failure() {
    let model = ModelScript::scripted([Some(answer("first")), None]);
    let prepared = prepare(
        &captured_turn(),
        &policy("progressiveTurducken", &["summary"]),
        |_| Some(Arc::new(ScriptedModel(Arc::clone(&model)))),
    );
    assert_eq!(model.requests(), 2);
    assert!(model.prompt(1)[1].1.contains("Earlier parts:\nfirst"));
    assert_eq!(prepared.representations.len(), 2);
    assert!(prepared.representations[0].text.is_none());
    assert_eq!(prepared.representations[1].part.as_deref(), Some("core"));
    assert_eq!(
        prepared.availability,
        ["summary:generation-failed", "summary:part:core"]
    );
}

#[tokio::test]
async fn a_failed_summary_replies_and_persists_only_metadata() {
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let journal = temporary();
    let (broker, _) = stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let model = ModelScript::new([answer("reply delivered")]);
    let runner = journaled_runner(
        broker,
        Arc::new(Arc::clone(&model)) as Arc<dyn ModelFactory>,
        journal.path(),
    );
    let mut route = persistent_route(model_config(), recall_window(RecallSource::Journal));
    route.memory_policy = Some(policy("progressiveOneShot", &["summary"]));
    let inbound = message("PRIVATE_SOURCE_SENTINEL");
    let stem = ConversationKey::private(
        &route.agent,
        &route.transport,
        &inbound.conversation.key(),
        &inbound.subject,
    )
    .journal_stem();
    let driver = Arc::new(RecordingDriver::default());
    run_session(
        Arc::clone(&runner),
        route,
        inbound,
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(driver.replies(), ["reply delivered"]);
    assert_eq!(model.requests(), 2);
    let bytes = fs::read_to_string(journal.path().join(stem).with_extension("jsonl"))
        .expect("journaled completion");
    assert!(!bytes.contains("PRIVATE_SOURCE_SENTINEL"));
    let line: Value = serde_json::from_str(bytes.trim()).expect("v2 turn");
    assert_eq!(line["sourceEligibility"], false);
    assert_eq!(
        line["representations"][0]["sourceRecordId"],
        line["recordId"]
    );
    assert!(
        line["representations"][0]["capturedBytes"]
            .as_u64()
            .unwrap()
            > 23
    );
    assert_eq!(line["availability"], json!(["summary:generation-failed"]));
    assert!(line.get("user").is_none());
    assert!(line.get("answer").is_none());
    let events = capture.events_where(
        "dekopon_gatewayd",
        &[
            ("event", "gateway_memory_deviation"),
            ("reason", "generation-failed"),
        ],
    );
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].field("routine"), Some("summary"));
    assert!(!format!("{:?}", events[0]).contains("PRIVATE_SOURCE_SENTINEL"));
}

#[tokio::test]
async fn generation_uses_the_agents_metered_model_budget() {
    use dekopon_model_token_governor::{Budget, MeterSpec, Metering, Tokens, UnixMillis};
    let mut charges = Vec::new();
    for generate in [false, true] {
        let directory = temporary();
        let journal = temporary();
        let (broker, _) = stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
        let model = if generate {
            ModelScript::new([
                answer("delivered"),
                answer(
                    r#"{"parts":[{"id":"core","text":"first"},{"id":"detail","text":"second"}]}"#,
                ),
            ])
        } else {
            ModelScript::new([answer("delivered")])
        };
        let runner = journaled_runner(
            broker,
            Arc::new(Arc::clone(&model)) as Arc<dyn ModelFactory>,
            journal.path(),
        );
        let mut owned = Arc::into_inner(runner).expect("fresh runner");
        owned.metering = Arc::new(Metering::new(
            vec![Budget::new(
                "reviewer".parse().unwrap(),
                None,
                &[MeterSpec::Rolling {
                    limit: Tokens(1_000_000),
                    period: Duration::from_secs(5 * 3600),
                }],
                UnixMillis::now(),
            )],
            Metering::system_clock(),
        ));
        let runner = Arc::new(owned);
        let mut route = persistent_route(model_config(), recall_window(RecallSource::Journal));
        if generate {
            route.memory_policy = Some(policy("progressiveOneShot", &["summary"]));
        }
        let driver = Arc::new(RecordingDriver::default());
        run_session(
            Arc::clone(&runner),
            route,
            message("question"),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
        assert_eq!(driver.replies(), ["delivered"]);
        assert_eq!(model.requests(), if generate { 2 } else { 1 });
        charges.push(
            runner
                .metering
                .statuses(&"reviewer".parse().unwrap())
                .unwrap()[0]
                .1
                .used
                .0,
        );
    }
    assert!(
        charges[1] > charges[0],
        "summary inference charges the same agent meter"
    );
}

#[test]
fn capacity_eviction_keeps_the_committed_publication_but_a_new_grant_invalidates_it() {
    let store = ConversationStore::new(0);
    let key = private_conversation_key("dev", "conversation", SUBJECT);
    let allowed = granted(&["cli-probe.upper"]);
    let other = granted(&["cli-probe.lower"]);
    let first = store.begin(&key, &allowed, window(), None, Instant::now());
    let generation = first.assets.generation().expect("persistent generation");
    let access = first.assets.clone();
    assert!(first.lease.commit(
        window(),
        ConversationTurn::completed("one", "reply"),
        TakenIn {
            newest: None,
            steers: Vec::new(),
            recalled: None
        },
        &first.cache_key,
        Instant::now()
    ));
    assert!(store.publication_eligible(&key, &allowed, generation));
    let replacement = store.begin(&key, &other, window(), None, Instant::now());
    assert!(!store.publication_eligible(&key, &allowed, generation));
    drop(replacement);
    drop(access);
}

struct SlowModel(Arc<SlowSummary>);

struct SlowSummary {
    notified: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    calls: AtomicUsize,
}

impl ModelFactory for Arc<SlowSummary> {
    fn build(
        &self,
        _model: &ModelConfig,
        _runtime: tokio::runtime::Handle,
        _cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<SharedModel, SessionError> {
        Ok(Arc::new(SlowModel(Arc::clone(self))))
    }
}

impl ChatModel for SlowModel {
    fn complete(
        &self,
        _messages: &[ModelMessage],
        _tools: &[ModelTool],
        _options: &CompletionOptions,
        _on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
    ) -> Result<AssistantTurn, InferenceError> {
        if self.0.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok(answer("delivered before summary"));
        }
        if let Some(sender) = self.0.notified.lock().take() {
            sender.send(()).expect("session receives generation start");
        }
        self.0
            .release
            .lock()
            .recv_timeout(Duration::from_secs(5))
            .expect("test releases generation");
        Err(InferenceError::Protocol(
            dekopon_model::error::ProtocolFailure::NoChoices,
        ))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_generation_cannot_hold_the_success_reply() {
    let directory = temporary();
    let journal = temporary();
    let (broker, _) = stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let (notified, started) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let model = Arc::new(SlowSummary {
        notified: Mutex::new(Some(notified)),
        release: Mutex::new(released),
        calls: AtomicUsize::new(0),
    });
    let runner = journaled_runner(
        broker,
        Arc::new(Arc::clone(&model)) as Arc<dyn ModelFactory>,
        journal.path(),
    );
    let mut route = persistent_route(model_config(), recall_window(RecallSource::Journal));
    route.memory_policy = Some(policy("progressiveOneShot", &["summary"]));
    let driver = Arc::new(RecordingDriver::default());
    let replies = Arc::clone(&driver);
    let task = tokio::spawn(run_session(
        runner,
        route,
        message("PRIVATE_SOURCE_SENTINEL"),
        driver as Arc<dyn ChatDriver>,
    ));
    started.await.expect("generation started");
    assert_eq!(replies.replies(), ["delivered before summary"]);
    release.send(()).expect("release generation");
    task.await.expect("session finishes");
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
}
