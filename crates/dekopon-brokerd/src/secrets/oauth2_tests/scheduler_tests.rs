use std::{sync::atomic::AtomicU64, time::Duration};

use super::*;
use crate::{ShutdownDeadline, refresh_scheduler, secrets::ScheduledCatalog};

#[test]
fn boot_inspection_reports_each_sealed_drn_and_malformed_record_without_values() {
    let dir = tempfile::tempdir().unwrap();
    let capture = CaptureLayer::new();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let sentinel = "sensitive-invalid-record-sentinel";
    tracing::subscriber::with_default(subscriber, || {
        for name in ["one", "two"] {
            let path = dir.path().join(format!("{name}.json"));
            fs::write(
                &path,
                json!({"version":2,"state":"sealed","since":10,"cause":"reauthorization-required"})
                    .to_string(),
            )
            .unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            super::super::oauth2::inspect_boot(
                &path,
                rustix::process::geteuid().as_raw(),
                &drn(name),
            );
        }
        let malformed = dir.path().join("malformed.json");
        fs::write(&malformed, format!("{{\"state\":{sentinel}}}")).unwrap();
        fs::set_permissions(&malformed, fs::Permissions::from_mode(0o600)).unwrap();
        super::super::oauth2::inspect_boot(
            &malformed,
            rustix::process::geteuid().as_raw(),
            &drn("malformed"),
        );
    });
    for name in ["one", "two"] {
        assert_eq!(
            capture
                .events_where(
                    "dekopon_brokerd::audit",
                    &[
                        ("audit.event", "broker.credential.sealed"),
                        ("credential", drn(name).as_str())
                    ]
                )
                .len(),
            1
        );
    }
    assert_eq!(
        capture
            .events_where(
                "dekopon_brokerd::secrets::oauth2",
                &[("event", "broker_credential_record_invalid")]
            )
            .len(),
        1
    );
    assert!(!capture.text().contains(sentinel));
}

#[tokio::test]
async fn scheduled_refresh_forces_at_hour_22_with_long_lived_access() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    write_record(&path, 100_000);
    let api = TokenApi::serving(|_| {
        (
            "200 OK",
            json!({"access_token":NEW_ACCESS,"refresh_token":NEW_REFRESH,"expires_in": 200_000})
                .to_string(),
        )
    });
    let now = Arc::new(AtomicU64::new(22 * 3600));
    let mut entry = resolver(&path, &api.endpoint, &["one"]);
    let mut source = source_value(&path, &api.endpoint);
    source["refresh"] = json!("scheduled");
    entry.entries.get_mut(&drn("one")).unwrap().source = serde_json::from_value(source).unwrap();
    let scheduled = ScheduledCatalog {
        resolver: Some(Arc::new(entry)),
    };
    let capture = CaptureLayer::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let result = scheduled
        .refresh(
            &drn("one"),
            Arc::new({
                let now = Arc::clone(&now);
                move || now.load(Ordering::SeqCst)
            }),
        )
        .with_subscriber(subscriber)
        .await;
    assert!(result.is_ok());
    assert_eq!(api.requests().len(), 1);
    let record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(record["expiresAt"], 22 * 3600 + 200_000);
    assert_eq!(
        capture
            .records()
            .iter()
            .filter(|record| matches!(
                record,
                Record::Span {
                    name: "broker.credential.refresh",
                    ..
                }
            ) && record.field("trigger") == Some("scheduled"))
            .count(),
        1
    );
}

#[tokio::test]
async fn scheduled_and_authorized_use_share_the_record_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    write_record(&path, 22 * 3600 + 30);
    let api = TokenApi::serving(|_| {
        (
            "200 OK",
            json!({"access_token":NEW_ACCESS,"refresh_token":NEW_REFRESH,"expires_in":86400})
                .to_string(),
        )
    });
    let mut entry = resolver(&path, &api.endpoint, &["one"]);
    let mut source = source_value(&path, &api.endpoint);
    source["refresh"] = json!("scheduled");
    entry.entries.get_mut(&drn("one")).unwrap().source = serde_json::from_value(source).unwrap();
    let scheduled = ScheduledCatalog {
        resolver: Some(Arc::new(entry)),
    };
    let now: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| 22 * 3600);
    let key = drn("one");
    let due = scheduled.refresh(&key, Arc::clone(&now));
    let use_attempt =
        super::super::oauth2::resolve_with_clock(super::super::oauth2::RefreshInputs {
            credential: Some(drn("one")),
            path: path.clone(),
            seed_path: None,
            endpoint: api.endpoint.clone(),
            client_id: "public-client-id".to_owned(),
            timeout: Duration::from_secs(5),
            uid: rustix::process::geteuid().as_raw(),
            client: reqwest::Client::new(),
            clock: now,
            scheduled: false,
        });
    let (scheduled_result, use_result) = tokio::join!(due, use_attempt);
    assert!(scheduled_result.is_ok());
    assert_eq!(use_result.unwrap(), NEW_ACCESS.as_bytes());
    assert_eq!(api.requests().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn transient_due_attempt_retries_after_five_minutes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    write_record(&path, 100_000);
    let api = TokenApi::serving(|count| {
        if count == 1 {
            ("503 Service Unavailable", "{}".to_owned())
        } else {
            (
                "200 OK",
                json!({"access_token":NEW_ACCESS,"refresh_token":NEW_REFRESH,"expires_in":86400})
                    .to_string(),
            )
        }
    });
    let now = Arc::new(AtomicU64::new(22 * 3600));
    let mut entry = resolver(&path, &api.endpoint, &["one"]);
    let mut source = source_value(&path, &api.endpoint);
    source["refresh"] = json!("scheduled");
    entry.entries.get_mut(&drn("one")).unwrap().source = serde_json::from_value(source).unwrap();
    let scheduled = ScheduledCatalog {
        resolver: Some(Arc::new(entry)),
    };
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(refresh_scheduler::serve_with_clock(
        async {
            stopped.await.expect("shutdown");
            Ok(())
        },
        move || scheduled.clone(),
        ShutdownDeadline::default(),
        Arc::new({
            let now = Arc::clone(&now);
            move || now.load(Ordering::SeqCst)
        }),
    ));
    tokio::time::timeout(Duration::from_secs(20), async {
        while api.requests().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first due request");
    tokio::time::advance(Duration::from_secs(5 * 60)).await;
    now.store(22 * 3600 + 5 * 60, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(20), async {
        while api.requests().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("retry due request");
    stop.send(()).expect("stop receiver");
    task.await.unwrap().unwrap();
    let record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(record["lastRefresh"], 22 * 3600 + 5 * 60);
    assert_eq!(api.requests().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn reboot_recomputes_a_failed_due_attempt_from_the_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    write_record(&path, 100_000);
    let api = TokenApi::serving(|count| {
        if count == 1 {
            ("503 Service Unavailable", "{}".to_owned())
        } else {
            (
                "200 OK",
                json!({"access_token":NEW_ACCESS,"refresh_token":NEW_REFRESH,"expires_in":86400})
                    .to_string(),
            )
        }
    });
    let mut entry = resolver(&path, &api.endpoint, &["one"]);
    let mut source = source_value(&path, &api.endpoint);
    source["refresh"] = json!("scheduled");
    entry.entries.get_mut(&drn("one")).unwrap().source = serde_json::from_value(source).unwrap();
    let scheduled = ScheduledCatalog {
        resolver: Some(Arc::new(entry)),
    };
    for expected in [1, 2] {
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let current = scheduled.clone();
        let task = tokio::spawn(refresh_scheduler::serve_with_clock(
            async {
                stopped.await.expect("stop");
                Ok(())
            },
            move || current.clone(),
            ShutdownDeadline::default(),
            Arc::new(|| 22 * 3600),
        ));
        tokio::time::timeout(Duration::from_secs(20), async {
            while api.requests().len() < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("boot due request");
        if expected == 2 {
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    let record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                    if record["lastRefresh"] == 22 * 3600 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("persist on reboot");
        }
        stop.send(()).expect("stop receiver");
        task.await.unwrap().unwrap();
    }
    assert_eq!(api.requests().len(), 2);
}

#[tokio::test]
async fn shutdown_drains_an_in_flight_scheduled_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    write_record(&path, 100_000);
    let (release, parked) = std::sync::mpsc::channel::<()>();
    let (entered, ready) = std::sync::mpsc::channel::<()>();
    let api = TokenApi::serving(move |_| {
        entered.send(()).expect("notify endpoint arrival");
        parked.recv().expect("release endpoint");
        (
            "200 OK",
            json!({"access_token":NEW_ACCESS,"refresh_token":NEW_REFRESH,"expires_in":86400})
                .to_string(),
        )
    });
    let mut entry = resolver(&path, &api.endpoint, &["one"]);
    let mut source = source_value(&path, &api.endpoint);
    source["refresh"] = json!("scheduled");
    entry.entries.get_mut(&drn("one")).unwrap().source = serde_json::from_value(source).unwrap();
    let scheduled = ScheduledCatalog {
        resolver: Some(Arc::new(entry)),
    };
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(refresh_scheduler::serve_with_clock(
        async {
            stopped.await.expect("stop");
            Ok(())
        },
        move || scheduled.clone(),
        ShutdownDeadline::default(),
        Arc::new(|| 22 * 3600),
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while ready.try_recv().is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("endpoint received due attempt");
    stop.send(()).expect("stop receiver");
    tokio::task::yield_now().await;
    let still_draining = !task.is_finished();
    release.send(()).expect("release parked endpoint");
    assert!(still_draining);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("drained before deadline")
        .unwrap()
        .unwrap();
    let record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(record["lastRefresh"], 22 * 3600);
}

#[tokio::test(start_paused = true)]
async fn idle_scheduler_rotates_without_provider_traffic_and_stops_with_server() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    write_record(&path, 100_000);
    let api = TokenApi::serving(|count| {
        ("200 OK", json!({"access_token":format!("access-{count}"),"refresh_token":format!("refresh-{count}"),"expires_in": 86400}).to_string())
    });
    let now = Arc::new(AtomicU64::new(1));
    let mut entry = resolver(&path, &api.endpoint, &["one"]);
    let mut source = source_value(&path, &api.endpoint);
    source["refresh"] = json!("scheduled");
    entry.entries.get_mut(&drn("one")).unwrap().source = serde_json::from_value(source).unwrap();
    let scheduled = ScheduledCatalog {
        resolver: Some(Arc::new(entry)),
    };
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(refresh_scheduler::serve_with_clock(
        async {
            stopped.await.expect("stop sent");
            Ok(())
        },
        move || scheduled.clone(),
        ShutdownDeadline::default(),
        Arc::new({
            let now = Arc::clone(&now);
            move || now.load(Ordering::SeqCst)
        }),
    ));
    tokio::task::yield_now().await;
    for hour in [22, 44, 66] {
        now.store(hour * 3600, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(22 * 3600)).await;
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                if record["lastRefresh"] == hour * 3600
                    && api.requests().len() == (hour / 22) as usize
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("scheduled HTTP request");
    }
    now.store(72 * 3600, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(6 * 3600)).await;
    stop.send(()).expect("stop receiver");
    task.await.unwrap().unwrap();
    let record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(record["lastRefresh"], 66 * 3600);
    assert_eq!(record["expiresAt"], 90 * 3600);
    let access = super::super::oauth2::resolve_with_clock(super::super::oauth2::RefreshInputs {
        credential: Some(drn("one")),
        path: path.clone(),
        seed_path: None,
        endpoint: api.endpoint.clone(),
        client_id: "public-client-id".to_owned(),
        timeout: Duration::from_secs(5),
        uid: rustix::process::geteuid().as_raw(),
        client: reqwest::Client::new(),
        clock: Arc::new({
            let now = Arc::clone(&now);
            move || now.load(Ordering::SeqCst)
        }),
        scheduled: false,
    })
    .await
    .unwrap();
    assert_eq!(access, b"access-3");
    assert_eq!(api.requests().len(), 3);
}
