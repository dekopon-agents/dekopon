use super::*;
use crate::config::ResolvedWakes;
use crate::wake::{Anchor, Probe, Resolved, Verdict, WakeStore, store::Schedule};
use dekopon_agent::wake::{WakeId, WakeRefusal};
use std::time::SystemTime;

const DAY: Duration = Duration::from_secs(86_400);

fn space_anchor() -> Anchor {
    Anchor::from_inbound(
        &owned_slack_message("digest", false),
        &route(model_config()).agent,
    )
    .unwrap()
    .as_space()
    .unwrap()
}

fn store(directory: &Path) -> Arc<WakeStore> {
    wakes::wake_store_with_horizon(directory, DAY * 3)
}

fn digest(
    store: &WakeStore,
    anchor: Anchor,
    now: SystemTime,
    every: Duration,
) -> Result<dekopon_agent::wake::WakeSummary, WakeRefusal> {
    store.register(
        anchor,
        "daily waiting items".into(),
        now,
        Schedule::Watch {
            probe: Probe::scripted_for_test("echo changed").into_digest(),
            every,
            until: DAY * 3,
        },
    )
}

#[test]
fn a_second_digest_is_typed_and_refused_across_threads_agents_and_transport_aliases() {
    let directory = temporary();
    let store = store(directory.path());
    let now = SystemTime::now();
    let anchor = space_anchor();
    let first = digest(&store, anchor.clone(), now, DAY).unwrap();
    let mut message = owned_slack_message("another digest", false);
    message.transport = "another-slack".into();
    message.conversation.thread = Some("1700000000.999999".into());
    let other = Anchor::from_inbound(&message, &"another-agent".parse().unwrap())
        .unwrap()
        .as_space()
        .unwrap();
    assert_eq!(
        digest(&store, other, now, DAY),
        Err(WakeRefusal::DigestExists)
    );
    let rows = fs::read_to_string(directory.path().join("wakes/wakes.jsonl")).unwrap();
    assert_eq!(rows.lines().count(), 1);
    let row: Value = serde_json::from_str(rows.lines().next().unwrap()).unwrap();
    assert_eq!(row["id"], first.id.0);
    assert_eq!(row["run_as"], "space");
}

#[test]
fn only_digests_have_the_daily_interval_and_quiet_recurring_lifecycle() {
    let directory = temporary();
    let store = store(directory.path());
    let now = SystemTime::now();
    let anchor = space_anchor();
    assert_eq!(
        digest(&store, anchor.clone(), now, Duration::from_secs(600)),
        Err(WakeRefusal::Interval { minimum: DAY })
    );
    let id = digest(&store, anchor.clone(), now, DAY).unwrap().id;
    let ordinary = store
        .register(
            anchor.clone(),
            "ordinary watch".into(),
            now,
            Schedule::Watch {
                probe: Probe::scripted_for_test("echo changed"),
                every: Duration::from_secs(600),
                until: DAY,
            },
        )
        .unwrap()
        .id;
    let tick = store
        .take_due(now + Duration::from_secs(600))
        .unwrap()
        .ticks
        .pop()
        .unwrap();
    assert_eq!(tick.id(), ordinary);
    assert!(matches!(
        tick.resolve(
            &store,
            Verdict::Fire {
                output: "ordinary".into()
            },
            now + Duration::from_secs(600)
        )
        .unwrap(),
        Resolved::Fired(_)
    ));
    let tick = store.take_due(now + DAY).unwrap().ticks.pop().unwrap();
    assert_eq!(tick.id(), id);
    assert!(matches!(
        tick.resolve(
            &store,
            Verdict::Fire {
                output: "changed".into()
            },
            now + DAY
        )
        .unwrap(),
        Resolved::Fired(_)
    ));
    assert!(
        store
            .take_due(now + DAY + DAY / 2)
            .unwrap()
            .ticks
            .is_empty()
    );
    let tick = store.take_due(now + DAY * 2).unwrap().ticks.pop().unwrap();
    assert!(matches!(
        tick.resolve(
            &store,
            Verdict::Fire {
                output: "changed".into()
            },
            now + DAY * 2
        )
        .unwrap(),
        Resolved::Rearmed
    ));
    let due = store.take_due(now + DAY * 3).unwrap();
    assert!(due.ticks.is_empty() && due.fired.is_empty());
    assert!(
        store
            .list_for(&anchor, Some(&anchor), now + DAY * 3)
            .is_empty()
    );
    assert!(
        digest(&store, anchor, now + DAY * 3, DAY).is_ok(),
        "only an explicit request renews the horizon"
    );
}

#[test]
fn cancelling_a_leased_digest_prevents_a_late_probe_from_resurrecting_it() {
    let directory = temporary();
    let store = store(directory.path());
    let now = SystemTime::now();
    let anchor = space_anchor();
    let id = digest(&store, anchor.clone(), now, DAY).unwrap().id;
    let tick = store.take_due(now + DAY).unwrap().ticks.pop().unwrap();
    store.cancel_for(&anchor, Some(&anchor), id).unwrap();
    assert!(matches!(
        tick.resolve(
            &store,
            Verdict::Fire {
                output: "changed".into()
            },
            now + DAY
        )
        .unwrap(),
        Resolved::Gone
    ));
    assert!(store.take_due(now + DAY * 2).unwrap().ticks.is_empty());
}

#[test]
fn legacy_personal_and_s5_subscription_rows_load_beside_distinct_space_watch_and_digest_rows() {
    let directory = temporary();
    let store = store(directory.path());
    let now = SystemTime::now();
    let person = Anchor::from_inbound(
        &wakes::slack_message("legacy"),
        &route(model_config()).agent,
    )
    .unwrap();
    let personal = store
        .register(
            person.clone(),
            "legacy".into(),
            now,
            Schedule::Once { after: DAY },
        )
        .unwrap()
        .id;
    store
        .register(
            person.clone(),
            "legacy watch".into(),
            now,
            Schedule::Watch {
                probe: Probe::scripted_for_test("echo ordinary"),
                every: Duration::from_secs(600),
                until: DAY,
            },
        )
        .unwrap();
    let space = space_anchor();
    let target = crate::pull_request_links::PullRequestLinkContext::parse(
        "https://github.com/acme/widget/pull/17",
    )
    .unwrap()
    .targets
    .remove(0);
    store.subscribe(space.clone(), target, now).unwrap();
    store
        .register(
            space.clone(),
            "new ordinary space watch".into(),
            now,
            Schedule::Watch {
                probe: Probe::scripted_for_test("echo ordinary"),
                every: Duration::from_secs(600),
                until: DAY,
            },
        )
        .unwrap();
    digest(&store, space.clone(), now, DAY).unwrap();
    let path = directory.path().join("wakes/wakes.jsonl");
    let mut rows = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|row| serde_json::from_str::<Value>(row).unwrap())
        .collect::<Vec<_>>();
    rows[2].as_object_mut().unwrap().remove("run_as");
    fs::write(
        &path,
        rows.iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let bounds = store.bounds();
    drop(store);
    let restored = WakeStore::open(&ResolvedWakes {
        path: path.clone(),
        bounds,
    })
    .unwrap();
    assert_eq!(restored.list(person.subject(), now).len(), 2);
    assert_eq!(restored.list_subscriptions(&space, now).unwrap().len(), 1);
    assert_eq!(restored.list_for(&space, Some(&space), now).len(), 2);
    assert_eq!(
        restored.cancel_for(&space, Some(&space), personal),
        Err(WakeRefusal::NotFound)
    );
    restored.cancel_for(&person, None, personal).unwrap();
    let ticks = restored
        .take_due(now + Duration::from_secs(600))
        .unwrap()
        .ticks;
    assert_eq!(ticks.len(), 2);
    for tick in ticks {
        assert!(matches!(
            tick.resolve(
                &restored,
                Verdict::Fire {
                    output: "ordinary".into()
                },
                now + Duration::from_secs(600)
            )
            .unwrap(),
            Resolved::Fired(_)
        ));
    }
    assert_eq!(restored.list_for(&space, Some(&space), now).len(), 1);
    let mut malformed = rows[4].clone();
    malformed["unexpected"] = json!(true);
    fs::write(&path, format!("{malformed}\n")).unwrap();
    assert!(matches!(
        WakeStore::open(&ResolvedWakes {
            path: path.clone(),
            bounds
        }),
        Err(crate::wake::WakeStoreError::Corrupt { line: 1 })
    ));
    malformed.as_object_mut().unwrap().remove("unexpected");
    malformed["subject"] = rows[0]["subject"].clone();
    fs::write(&path, format!("{malformed}\n")).unwrap();
    assert!(matches!(
        WakeStore::open(&ResolvedWakes { path, bounds }),
        Err(crate::wake::WakeStoreError::Corrupt { line: 1 })
    ));
}

#[tokio::test]
async fn a_digest_decline_sends_no_slack_http_requests() {
    let directory = temporary();
    let (broker, _observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::chat_capabilities(
            vec![capability("cli-probe.upper")],
            Vec::new(),
            BTreeMap::new(),
            None,
            Some("channel-space".parse().unwrap()),
        )],
    )
    .await;
    let models = ModelScript::new([decline_reply()]);
    let running = runner(broker, Arc::clone(&models), 4);
    let http = spawn_http_mock(slack_handler(Vec::new()));
    let transport = slack_with(
        &http.base,
        SlackExperience::Classic,
        LivenessConfig::default(),
    );
    let mut message = subscriptions::subscription_wake();
    message.message_id = MessageId::Wake {
        id: WakeId(1),
        notice: "digest".into(),
        origin: crate::wake::WakeOrigin::Digest,
    };
    let mut bound = route(model_config());
    bound.transport = message.transport.clone();
    bound.wakes = true;
    run_session(running, bound, message, transport.driver()).await;
    assert_eq!(models.requests(), 1);
    assert!(http.calls().is_empty());
}

#[tokio::test]
async fn a_digest_is_one_bounded_threaded_slack_post_without_native_mentions() {
    let directory = temporary();
    let (broker, _observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::chat_capabilities(
            vec![capability("cli-probe.upper")],
            Vec::new(),
            BTreeMap::new(),
            None,
            Some("channel-space".parse().unwrap()),
        )],
    )
    .await;
    let text = format!(
        "<@U0123> <!channel> <!subteam^S0123> {}",
        "waiting on octocat 🐙 ".repeat(400)
    );
    let models = ModelScript::new([answer(&text)]);
    let running = runner(broker, models, 4);
    let http = spawn_http_mock(slack_handler(Vec::new()));
    let transport = slack_with(
        &http.base,
        SlackExperience::Classic,
        LivenessConfig::default(),
    );
    let mut message = subscriptions::subscription_wake();
    let thread = message.conversation.thread.clone().unwrap();
    message.message_id = MessageId::Wake {
        id: WakeId(1),
        notice: "digest".into(),
        origin: crate::wake::WakeOrigin::Digest,
    };
    let mut bound = route(model_config());
    bound.transport = message.transport.clone();
    bound.wakes = true;
    run_session(running, bound, message, transport.driver()).await;
    let calls = http.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "/api/chat.postMessage");
    let body: Value = serde_json::from_str(&calls[0].1).unwrap();
    assert_eq!(body["thread_ts"], thread);
    let text = body["text"].as_str().unwrap();
    assert!(!text.contains("<@") && !text.contains("<!"));
    assert!(text.len() <= 1900);
    assert!(text.contains("octocat"));
}
