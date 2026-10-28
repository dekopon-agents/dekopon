use super::*;
use crate::routes::BoundRoute;
use tracing_subscriber::{Layer, layer::Context, prelude::*};

#[derive(Clone, Default)]
struct Notices(Arc<Mutex<Vec<Value>>>);

impl<S> Layer<S> for Notices
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, context: Context<'_, S>) {
        #[derive(Default)]
        struct Fields(serde_json::Map<String, Value>);
        impl tracing::field::Visit for Fields {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.insert(field.name().into(), json!(value));
            }
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                self.0.insert(field.name().into(), json!(value));
            }
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0
                    .insert(field.name().into(), json!(format!("{value:?}")));
            }
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields.0.get("event") != Some(&json!("gateway_memory_notice")) {
            return;
        }
        let native = opentelemetry::Context::current();
        let array = native
            .get::<dekopon_telemetry::EventStringArray>()
            .expect("native notice kinds");
        fields.0.insert(array.name.into(), json!(array.values));
        fields.0.insert(
            "span".into(),
            json!(context.event_span(event).unwrap().name()),
        );
        self.0.lock().push(Value::Object(fields.0));
    }
}

struct Fixture {
    _broker_dir: tempfile::TempDir,
    journal_dir: tempfile::TempDir,
    runner: Arc<SessionRunner>,
    models: Arc<ModelScript>,
    route: BoundRoute,
    inbound: InboundMessage,
    stem: String,
}

impl Fixture {
    async fn new(grants: &[&str]) -> Self {
        let directory = temporary();
        let journal_dir = temporary();
        let (broker, _observed) = stub_broker(directory.path(), listings(2, grants)).await;
        let models = ModelScript::new([answer("reply"), answer("next reply")]);
        let runner = journaled_runner(broker, Arc::new(Arc::clone(&models)), journal_dir.path());
        let mut window = recall_window(RecallSource::Journal);
        window.limits = HistoryLimits {
            max_turns: 12,
            max_bytes: 65536,
        };
        let route = persistent_route(model_config(), window);
        let inbound = message("current question");
        let stem = ConversationKey::of(
            &route.agent,
            &route.transport,
            &inbound.conversation,
            Some(&inbound.subject),
        )
        .journal_stem();
        Self {
            _broker_dir: directory,
            journal_dir,
            runner,
            models,
            route,
            inbound,
            stem,
        }
    }

    fn append(&self, grants: &[&str], text: &str) {
        append_captured(
            self.runner.journal.as_ref().unwrap(),
            &self.stem,
            grants,
            &captured(text, Some("old answer")),
        );
    }

    async fn run(&self) {
        run_session(
            Arc::clone(&self.runner),
            self.route.clone(),
            self.inbound.clone(),
            Arc::new(RecordingDriver::default()) as Arc<dyn ChatDriver>,
        )
        .await;
    }

    fn notice(&self, request: usize) -> String {
        let prompt = self.models.prompt(request);
        let notices: Vec<_> = prompt
            .iter()
            .filter(|(_, text)| text.starts_with("[gateway: memory]\n"))
            .collect();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].0, "system");
        assert_eq!(&prompt[1], notices[0]);
        assert!(notices[0].1.len() <= 1024);
        notices[0].1.clone()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_window_that_omits_older_records_tells_the_model_the_first_record_shown() {
    let notices = Notices::default();
    let subscriber = tracing_subscriber::registry().with(notices.clone());
    let _guard = tracing::subscriber::set_default(subscriber);
    let fixture = Fixture::new(&["cli-probe.upper"]).await;
    for _ in 0..26 {
        fixture.append(&["cli-probe.upper"], "old question");
    }
    fixture.run().await;
    let notice = fixture.notice(0);
    assert!(notice.contains("Turns before record 2 are not shown: maxTurns=12, maxBytes=65536"));
    assert!(notice.contains("read_memory_record"));
    assert!(notice.contains("Omitted window-fit: 13 records"));
    let events = notices.0.lock();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["span"], "gateway.session");
    assert_eq!(
        events[0]["memory.notice.kinds"],
        json!(["omitted", "window-omission", "fidelity"])
    );
    assert_eq!(events[0]["memory.record.first_shown"], json!(2_u64));
    assert_eq!(events[0]["memory.records.omitted"], json!(13_u64));
    assert_eq!(events[0]["memory.notice.bytes"], json!(notice.len() as u64));
}

#[tokio::test]
async fn a_thread_reply_with_no_recalled_turns_tells_the_model_earlier_messages_are_missing() {
    let mut fixture = Fixture::new(&["cli-probe.upper"]).await;
    fixture.inbound.conversation.thread = Some("earlier-message".into());
    fixture.run().await;
    assert!(
        fixture
            .notice(0)
            .contains("thread with earlier messages; none are in your memory")
    );
}

#[tokio::test]
async fn a_recall_that_ends_at_a_grant_change_says_so() {
    let fixture = Fixture::new(&["cli-probe.upper"]).await;
    fixture.append(&["other.grant"], "older grant");
    fixture.append(&["cli-probe.upper"], "current grant");
    fixture.run().await;
    assert!(fixture.notice(0).contains("different capability grant"));
}

#[tokio::test]
async fn the_memory_notice_is_never_replayed_as_history() {
    let mut fixture = Fixture::new(&["cli-probe.upper"]).await;
    fixture.inbound.conversation.thread = Some("earlier-message".into());
    fixture.run().await;
    assert!(fixture.notice(0).contains("none are in your memory"));
    fixture.run().await;
    let notice = fixture.notice(1);
    assert!(!notice.contains("none are in your memory"));
    assert!(notice.contains("every tool call with its result"));
    let stem = ConversationKey::of(
        &fixture.route.agent,
        &fixture.route.transport,
        &fixture.inbound.conversation,
        Some(&fixture.inbound.subject),
    )
    .journal_stem();
    let path = fixture.journal_dir.path().join(format!("{stem}.jsonl"));
    assert_eq!(journal_records(&path).len(), 2);
    assert!(
        !fs::read_to_string(path)
            .unwrap()
            .contains("[gateway: memory]")
    );
}

#[tokio::test]
async fn a_whole_window_carries_no_omission_fact() {
    let fixture = Fixture::new(&["cli-probe.upper"]).await;
    fixture.append(&["cli-probe.upper"], "whole conversation");
    fixture.run().await;
    assert_eq!(fixture.notice(0), JOURNAL_FIDELITY_NOTICE);
}

#[tokio::test]
async fn a_corrupt_recall_tells_the_model_even_when_projection_finds_no_file() {
    let fixture = Fixture::new(&["cli-probe.upper"]).await;
    let path = fixture
        .journal_dir
        .path()
        .join(format!("{}.jsonl", fixture.stem));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "broken\n").unwrap();
    fixture.run().await;
    assert!(
        fixture
            .notice(0)
            .contains("Memory recall or projection failed this turn (corrupt)")
    );
}

#[tokio::test]
async fn a_thread_starter_with_no_history_has_no_memory_notice() {
    let mut fixture = Fixture::new(&["cli-probe.upper"]).await;
    fixture.inbound.message_id = MessageId::Native("thread-start".into());
    fixture.inbound.conversation.thread = Some("thread-start".into());
    fixture.run().await;
    assert!(
        !fixture
            .models
            .prompt(0)
            .iter()
            .any(|(_, text)| text.starts_with("[gateway: memory]"))
    );
}

#[tokio::test]
async fn records_dropped_to_fit_the_byte_window_are_named_in_the_request() {
    let mut fixture = Fixture::new(&["cli-probe.upper"]).await;
    fixture.route.memory = MemoryPolicy::Persistent(MemoryWindow {
        limits: HistoryLimits {
            max_turns: 12,
            max_bytes: 400,
        },
        ..recall_window(RecallSource::Journal)
    });
    fixture.append(&["cli-probe.upper"], &"older".repeat(50));
    fixture.append(&["cli-probe.upper"], &"newer".repeat(50));
    fixture.run().await;
    assert!(fixture.notice(0).contains("Omitted window-fit: 1"));
}

#[tokio::test]
async fn a_bash_turn_followed_by_resident_replay_reports_that_bash_is_not_kept() {
    let mut fixture = Fixture::new(&["cli-probe.upper"]).await;
    *fixture.models.turns.lock() = VecDeque::from([
        Ok(script_call("printf resident-bash-sentinel")),
        Ok(answer("after bash")),
        Ok(answer("next reply")),
    ]);
    fixture.route.memory = MemoryPolicy::Persistent(recall_window(RecallSource::None));
    fixture.run().await;
    assert!(
        fixture
            .models
            .prompt(1)
            .iter()
            .any(|(role, text)| role == "tool" && text.contains("resident-bash-sentinel"))
    );
    fixture.run().await;
    assert_eq!(fixture.notice(2), FIDELITY_NOTICE);
    assert!(
        !fixture
            .models
            .prompt(2)
            .iter()
            .any(|(_, text)| text.contains("resident-bash-sentinel"))
    );
}

#[tokio::test]
async fn platform_recall_reports_conversation_only_fidelity() {
    let mut fixture = Fixture::new(&["cli-probe.upper"]).await;
    fixture.route.memory = MemoryPolicy::Persistent(recall_window(RecallSource::Platform));
    let driver = HistoryDriver::new(Ok(vec![
        past("0000000001", "U1", SUBJECT, "older question"),
        bot_past("0000000002", "older reply"),
    ]));
    run_session(
        Arc::clone(&fixture.runner),
        fixture.route.clone(),
        fixture.inbound.clone(),
        driver as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(fixture.notice(0), FIDELITY_NOTICE);
}
