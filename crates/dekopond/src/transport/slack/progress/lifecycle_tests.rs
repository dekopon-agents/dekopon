use super::*;
use crate::activity::ActivityLease;
use tokio::sync::Notify;

#[derive(Default)]
struct Fake {
    events: Mutex<Vec<String>>,
    entered: Notify,
    release: Notify,
    removed: Notify,
    hidden: Notify,
    delayed: bool,
    fail_post: bool,
    retry_delete: bool,
    deletes: AtomicUsize,
}
impl Fake {
    fn record(&self, text: &str) {
        self.events.lock().unwrap().push(text.into());
    }
}
impl ChatActivity for Fake {
    fn show(&self, _: ActivityTarget) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async {
            self.record("show");
            Ok(())
        })
    }
    fn hide(&self, _: ActivityTarget) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async {
            self.record("hide");
            self.hidden.notify_one();
            Ok(())
        })
    }
    fn refresh_interval(&self) -> Option<Duration> {
        None
    }
    fn progress_enabled(&self) -> bool {
        true
    }
    fn post_progress(
        &self,
        _: ActivityTarget,
        _: ActivityLabel,
    ) -> BoxFuture<'_, Result<Option<OwnedProgressArtifact>, TransportError>> {
        Box::pin(async {
            self.record("post-start");
            self.entered.notify_one();
            if self.delayed {
                self.release.notified().await;
            }
            if self.fail_post {
                return Err(TransportError::Response);
            }
            self.record("post-finish");
            Ok(Some(OwnedProgressArtifact {
                channel: "C1".into(),
                timestamp: "1700000000.000002".into(),
            }))
        })
    }
    fn update_progress<'a>(
        &'a self,
        _: &'a OwnedProgressArtifact,
        label: ActivityLabel,
    ) -> BoxFuture<'a, Result<(), TransportError>> {
        Box::pin(async move {
            self.record(&format!("update:{}", label.as_str()));
            Ok(())
        })
    }
    fn delete_progress<'a>(
        &'a self,
        _: &'a OwnedProgressArtifact,
    ) -> BoxFuture<'a, Result<(), TransportError>> {
        Box::pin(async {
            self.record("delete");
            if self.deletes.fetch_add(1, Ordering::AcqRel) == 0 && self.retry_delete {
                return Err(TransportError::ActivityRateLimited {
                    retry_after: Duration::from_millis(50),
                });
            }
            self.removed.notify_one();
            Ok(())
        })
    }
}
fn target(user: &str) -> ActivityTarget {
    ActivityTarget::Slack {
        channel_id: "C1".into(),
        thread_ts: "1700000000.000001".into(),
        message_ts: "1700000000.000001".into(),
        initiator_user_id: user.into(),
    }
}
async fn wait(n: &Notify) {
    tokio::time::timeout(Duration::from_secs(4), n.notified())
        .await
        .expect("bounded worker");
}

#[tokio::test]
async fn delayed_creation_is_owned_by_old_generation_and_never_blocks_final_delivery() {
    let fake = Arc::new(Fake {
        delayed: true,
        ..Default::default()
    });
    let driver = fake.clone() as Arc<dyn ChatActivity>;
    let mut lease = ActivityLease::start(Some(driver.clone()), Some(target("U1")), false);
    wait(&fake.entered).await;
    lease.seal();
    fake.record("final-delivery");
    lease.finish_in_background();
    // Native status shares the target across users and inbound message generations.
    let later = ActivityLease::start(Some(driver), Some(target("U2")), false);
    assert!(later.publisher().is_none());
    assert_eq!(
        *fake.events.lock().unwrap(),
        ["show", "post-start", "final-delivery"]
    );
    fake.release.notify_one();
    wait(&fake.hidden).await;
    assert_eq!(
        *fake.events.lock().unwrap(),
        [
            "show",
            "post-start",
            "final-delivery",
            "post-finish",
            "delete",
            "hide"
        ]
    );
}
#[tokio::test]
async fn stop_drop_and_explicit_terminal_paths_cleanup_only_confirmed_creation() {
    for exit in ["stop", "drop", "failed", "declined", "reply-failed"] {
        let fake = Arc::new(Fake::default());
        let mut lease = ActivityLease::start(Some(fake.clone()), Some(target(exit)), false);
        wait(&fake.entered).await;
        match exit {
            "stop" => lease.control().finish(),
            "drop" => drop(lease),
            _ => {
                lease.seal();
                lease.finish_in_background();
            }
        }
        wait(&fake.hidden).await;
        assert_eq!(fake.deletes.load(Ordering::Acquire), 1, "{exit}");
    }
    let fake = Arc::new(Fake {
        fail_post: true,
        ..Default::default()
    });
    let mut lease = ActivityLease::start(Some(fake.clone()), Some(target("unknown")), false);
    wait(&fake.entered).await;
    lease.finish_in_background();
    wait(&fake.hidden).await;
    assert_eq!(
        fake.deletes.load(Ordering::Acquire),
        0,
        "unknown creation is neither searched nor deleted nor retried"
    );
    assert_eq!(
        fake.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| *e == "post-start")
            .count(),
        1
    );
}
#[tokio::test]
async fn optional_continuation_decline_posts_nothing_and_cleanup_honors_retry_after() {
    let fake = Arc::new(Fake::default());
    let mut lease = ActivityLease::start(Some(fake.clone()), Some(target("optional")), true);
    tokio::time::sleep(Duration::from_millis(30)).await;
    lease.finish_in_background();
    wait(&fake.hidden).await;
    assert_eq!(*fake.events.lock().unwrap(), ["show", "hide"]);
    let retry = Arc::new(Fake {
        retry_delete: true,
        ..Default::default()
    });
    let mut lease = ActivityLease::start(Some(retry.clone()), Some(target("retry")), false);
    wait(&retry.entered).await;
    let start = Instant::now();
    lease.finish_in_background();
    wait(&retry.hidden).await;
    assert!(start.elapsed() >= Duration::from_millis(50));
    assert_eq!(retry.deletes.load(Ordering::Acquire), 2);
}

fn emit_flood(publisher: dekopon_harness::activity::ActivityPublisher) {
    use dekopon_harness::{
        bootstrap::SessionBootstrap,
        history::History,
        runtime::ShellRuntime,
        session::{PromptLimits, SessionEngine},
    };
    use dekopon_model::{model::*, usage::AttemptRecorder};
    struct Model;
    impl ChatModel for Model {
        fn complete(
            &self,
            _: &[ModelMessage],
            _: &[ModelTool],
            recorder: &dyn AttemptRecorder,
        ) -> Result<AssistantTurn, ModelError> {
            {
                let attempt = recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
                let result: Result<AssistantTurn, ModelError> = {
                    Ok(AssistantTurn { content:None, tool_calls:vec![ModelToolCall { id:"batch".into(), kind:"function".into(), function:ModelFunctionCall { name:"bash".into(), arguments:json!({"script":"for i in 1 2 3 4 5 6 7 8 9 10; do test.read; test.other; done"}).to_string() } }], usage:None, replay_items:Vec::new() })
                };
                if let Ok(turn) = &result
                    && let Some(usage) = turn.usage
                {
                    recorder.observe(
                        attempt,
                        dekopon_model::usage::UsageObservation {
                            usage,
                            invalid: [false; 5],
                        },
                    )?;
                }
                result
            }
        }
    }
    struct Invoker;
    impl dekopon_shell::CapabilityInvoker for Invoker {
        fn granted(&self) -> Vec<String> {
            vec!["test.read".into(), "test.other".into()]
        }
        fn describe(&self, c: &str) -> Option<dekopon_shell::CapabilityDescription> {
            Some(dekopon_shell::CapabilityDescription {
                capability: c.into(),
                description: "PRIVATE".into(),
                input_schema: json!({"type":"object"}),
            })
        }
        fn invoke(
            &self,
            _: &str,
            _: Value,
            _: Option<dekopon_core::SecretUseProposal>,
        ) -> dekopon_shell::CapabilityCallResult {
            dekopon_shell::CapabilityCallResult::Succeeded(json!({"secret":"no status"}))
        }
    }
    let labels = std::collections::BTreeMap::from([(
        "test.other".into(),
        ActivityLabel::sanitized("Fetching Wikipedia page"),
    )]);
    let runtime = ShellRuntime {
        invoker: Invoker,
        limits: Default::default(),
        curl_capability: None,
    };
    let result = SessionEngine::new(&Model, &runtime).run(
        SessionBootstrap::new(
            "request",
            PromptLimits {
                max_steps: 1,
                max_capability_calls: 30,
            },
            "fixture",
        )
        .with_activity(&publisher, &labels),
        &mut History::default(),
    );
    assert!(
        result.is_err(),
        "step exhaustion does not erase submitted activity"
    );
}
/// Lets the worker run to the point the assertion is about, without spending wall-clock time.
async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

// The clock is paused rather than slept through: the two-second update interval is the property
// under test, and a test that proves it by waiting 2.1 real seconds proves it only on a machine
// that was not busy. `emit_flood` runs inline rather than on the blocking pool so the runtime is
// never idle with the clock free to auto-advance past the interval.
#[tokio::test(start_paused = true)]
async fn runtime_flood_coalesces_to_one_update_and_optional_work_enables_posting() {
    let fake = Arc::new(Fake::default());
    let mut lease = ActivityLease::start(Some(fake.clone()), Some(target("flood")), false);
    wait(&fake.entered).await;
    emit_flood(lease.publisher().unwrap());
    tokio::time::advance(Duration::from_millis(100)).await;
    settle().await;
    assert!(
        !fake
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.starts_with("update:")),
        "a flood inside one interval posts nothing"
    );
    tokio::time::advance(Duration::from_millis(2000)).await;
    settle().await;
    let events = fake.events.lock().unwrap().clone();
    assert_eq!(
        events.iter().filter(|e| e.starts_with("update:")).count(),
        1,
        "{events:?}"
    );
    assert!(events.contains(&"update:Fetching Wikipedia page".into()));
    lease.finish_in_background();
    wait(&fake.hidden).await;
    let optional = Arc::new(Fake::default());
    let mut lease = ActivityLease::start(Some(optional.clone()), Some(target("work")), true);
    emit_flood(lease.publisher().unwrap());
    wait(&optional.entered).await;
    lease.finish_in_background();
    wait(&optional.hidden).await;
    assert_eq!(optional.deletes.load(Ordering::Acquire), 1);
}

fn native(endpoint: String) -> SlackTransport {
    SlackTransport::new(
        "native-fixture".into(),
        endpoint,
        "app".into(),
        "bot".into(),
        SlackExperience::Agent,
        SlackActivityConfig {
            mode: ActivityMode::Native,
            classic_fallback: SlackActivityFallback::Reaction,
            progress_message: false,
        },
    )
    .unwrap()
}
fn wire(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("bounded native activity lifecycle");
}

#[tokio::test]
async fn uncertain_native_status_quarantines_later_generations_even_after_fallback_and_cleanup() {
    for fallback in [
        r#"{"ok":true}"#,
        r#"{"ok":false,"error":"already_reacted"}"#,
    ] {
        let owned_reaction = fallback == r#"{"ok":true}"#;
        let mut responses = vec![vec![], wire(fallback), wire(r#"{"ok":true}"#)];
        if owned_reaction {
            responses.push(wire(r#"{"ok":true}"#));
        }
        let server = dekopon_test_support::LoopbackServer::sequence(responses);
        let transport = native(server.url());
        let driver = transport.activity().unwrap();
        let mut lease = ActivityLease::start(Some(driver.clone()), Some(target("U1")), false);
        let mut calls = Vec::new();
        until(|| {
            calls.extend(server.recorded());
            calls.len() >= 2
        })
        .await;
        lease.finish_in_background();
        until(|| transport.replier.active_activity.lock().unwrap().is_empty()).await;
        calls.extend(server.recorded());
        assert_eq!(calls.len(), if owned_reaction { 4 } else { 3 });
        let later = ActivityLease::start(Some(driver), Some(target("U2")), false);
        assert!(
            later.publisher().is_none(),
            "successful fallback/cleanup cannot establish ordering of the lost native write"
        );
        assert!(
            server.recorded().is_empty(),
            "quarantine issues no new native writes"
        );
    }
}

#[tokio::test]
async fn repeated_native_hide_refusals_retire_ownership_without_disabling_new_generations() {
    let responses = (0..8).flat_map(|_| {
        [
            wire(r#"{"ok":true}"#),
            wire(r#"{"ok":false,"error":"missing_scope"}"#),
        ]
    });
    let server = dekopon_test_support::LoopbackServer::sequence(responses);
    let transport = native(server.url());
    let driver = transport.activity().unwrap();
    let mut calls = Vec::new();
    for generation in 0..8 {
        let mut admitted = None;
        until(|| {
            let lease = ActivityLease::start(
                Some(driver.clone()),
                Some(target(&format!("U{generation}"))),
                false,
            );
            if lease.publisher().is_some() {
                admitted = Some(lease);
                true
            } else {
                false
            }
        })
        .await;
        let mut lease = admitted.unwrap();
        until(|| {
            calls.extend(server.recorded());
            calls.len() == generation * 2 + 1
        })
        .await;
        lease.finish_in_background();
        until(|| {
            calls.extend(server.recorded());
            calls.len() == (generation + 1) * 2
                && transport.replier.active_activity.lock().unwrap().is_empty()
        })
        .await;
    }
    assert_eq!(
        calls.len(),
        16,
        "each native generation shows and attempts one definitive refused hide"
    );
    assert!(transport.replier.active_activity.lock().unwrap().is_empty());
}

#[tokio::test]
async fn uncertain_reaction_cleanup_is_not_hidden_by_a_definitive_native_cleanup_error() {
    let server = dekopon_test_support::LoopbackServer::sequence([
        wire(r#"{"ok":false,"error":"temporary_failure"}"#),
        wire(r#"{"ok":true}"#),
        wire(r#"{"ok":false,"error":"missing_scope"}"#),
        vec![],
    ]);
    let transport = native(server.url());
    transport.replier.show(target("U1")).await.unwrap();
    let error = transport.replier.hide(target("U1")).await.unwrap_err();
    assert!(
        crate::activity::uncertain(&error),
        "uncertain removal must reach quarantine, not be discarded behind missing_scope"
    );
    transport.replier.retire(&target("U1"));
}
