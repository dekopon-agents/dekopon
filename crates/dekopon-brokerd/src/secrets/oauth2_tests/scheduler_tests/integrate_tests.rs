use super::*;

fn scheduled_entries(path: &Path, endpoint: &str, names: &[&str]) -> MapResolver {
    let mut entry = resolver(path, endpoint, names);
    for name in names {
        let mut source = source_value(path, endpoint);
        source["refresh"] = json!("scheduled");
        entry.entries.get_mut(&drn(name)).unwrap().source = serde_json::from_value(source).unwrap();
    }
    entry
}

fn scheduled_resolver(path: &Path, endpoint: &str, names: &[&str]) -> ScheduledCatalog {
    ScheduledCatalog {
        resolver: Some(Arc::new(scheduled_entries(path, endpoint, names))),
    }
}

#[tokio::test]
async fn shared_record_has_one_attempt_per_backoff_window() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    write_record(&path, 100_000);
    let api = TokenApi::serving(|_| ("503 Service Unavailable", "{}".to_owned()));
    let scheduled = scheduled_resolver(&path, &api.endpoint, &["one", "two"]);
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
        while api.requests().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first attempt");
    tokio::time::sleep(Duration::from_secs(1)).await;
    stop.send(()).expect("stop receiver");
    task.await.unwrap().unwrap();
    assert_eq!(api.requests().len(), 1);
}

#[tokio::test]
async fn shutdown_deadline_waits_for_a_started_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    write_record(&path, 100_000);
    let (release, parked) = std::sync::mpsc::channel::<()>();
    let (entered, ready) = std::sync::mpsc::channel::<()>();
    let api = TokenApi::serving(move |_| {
        entered.send(()).expect("notify");
        parked.recv().expect("release");
        (
            "200 OK",
            json!({"access_token":NEW_ACCESS,"refresh_token":NEW_REFRESH,"expires_in":86400})
                .to_string(),
        )
    });
    let scheduled = scheduled_resolver(&path, &api.endpoint, &["one"]);
    let deadline = ShutdownDeadline::default();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(refresh_scheduler::serve_with_clock(
        async {
            stopped.await.expect("stop");
            Ok(())
        },
        move || scheduled.clone(),
        deadline.clone(),
        Arc::new(|| 22 * 3600),
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while ready.try_recv().is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("endpoint reached");
    deadline.start(Duration::from_millis(200));
    stop.send(()).expect("stop receiver");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let returned_before_release = task.is_finished();
    release.send(()).expect("release");
    assert!(
        !returned_before_release,
        "scheduler returned while its rotation was in flight"
    );
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("bounded")
        .unwrap();
    assert!(matches!(
        result,
        Err(crate::BrokerdError::Server(
            crate::ServerError::ShutdownTimeout
        ))
    ));
    let record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(record["lastRefresh"], 22 * 3600);
    let access = super::super::super::oauth2::resolve_with_clock(
        super::super::super::oauth2::RefreshInputs {
            credential: Some(drn("one")),
            path: path.clone(),
            seed_path: None,
            endpoint: api.endpoint.clone(),
            client_id: "public-client-id".to_owned(),
            timeout: Duration::from_secs(5),
            uid: rustix::process::geteuid().as_raw(),
            client: reqwest::Client::new(),
            clock: Arc::new(|| 22 * 3600),
            scheduled: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(access, NEW_ACCESS.as_bytes());
    assert_eq!(api.requests().len(), 1);
}

#[tokio::test]
async fn a_server_failure_survives_a_drain_past_the_shutdown_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    write_record(&path, 100_000);
    let (release, parked) = std::sync::mpsc::channel::<()>();
    let (entered, ready) = std::sync::mpsc::channel::<()>();
    let api = TokenApi::serving(move |_| {
        entered.send(()).expect("notify");
        parked.recv().expect("release");
        success()
    });
    let scheduled = scheduled_resolver(&path, &api.endpoint, &["one"]);
    let deadline = ShutdownDeadline::default();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(refresh_scheduler::serve_with_clock(
        async {
            stopped.await.expect("stop");
            Err(crate::BrokerdError::Server(crate::ServerError::Accept {
                source: std::io::ErrorKind::ConnectionAborted.into(),
            }))
        },
        move || scheduled.clone(),
        deadline.clone(),
        Arc::new(|| 22 * 3600),
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while ready.try_recv().is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("endpoint reached");
    deadline.start(Duration::from_millis(200));
    stop.send(()).expect("stop receiver");
    tokio::time::sleep(Duration::from_secs(1)).await;
    release.send(()).expect("release");
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("bounded")
        .unwrap();
    assert!(matches!(
        result,
        Err(crate::BrokerdError::Server(
            crate::ServerError::Accept { .. }
        ))
    ));
}

#[tokio::test(start_paused = true)]
async fn a_malformed_scheduled_record_is_not_reported_on_each_tick() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.json");
    let live = dir.path().join("live.json");
    fs::write(&bad, "{\"state\":").unwrap();
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o600)).unwrap();
    write_record(&live, 100_000);
    let api = TokenApi::serving(|_| ("503 Service Unavailable", "{}".to_owned()));
    let mut entries = scheduled_entries(&bad, &api.endpoint, &["bad"]);
    entries
        .entries
        .extend(scheduled_entries(&live, &api.endpoint, &["live"]).entries);
    let scheduled = ScheduledCatalog {
        resolver: Some(Arc::new(entries)),
    };
    let now = Arc::new(AtomicU64::new(22 * 3600));
    let capture = CaptureLayer::new();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(
        refresh_scheduler::serve_with_clock(
            async {
                stopped.await.expect("stop");
                Ok(())
            },
            move || scheduled.clone(),
            ShutdownDeadline::default(),
            Arc::new({
                let now = Arc::clone(&now);
                move || now.load(Ordering::SeqCst)
            }),
        )
        .with_subscriber(subscriber),
    );
    for tick in 1..=3 {
        tokio::time::timeout(Duration::from_secs(20), async {
            while api.requests().len() < tick {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("live record attempted on this tick");
        now.fetch_add(5 * 60, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(5 * 60)).await;
    }
    stop.send(()).expect("stop receiver");
    task.await.unwrap().unwrap();
    assert!(api.requests().len() >= 3);
    assert!(
        capture
            .events_where(
                "dekopon_brokerd",
                &[("event", "broker_credential_record_invalid")]
            )
            .is_empty()
    );
}

#[tokio::test]
async fn loaded_map_inspects_all_sealed_and_malformed_records_without_http() {
    let dir = tempfile::tempdir().unwrap();
    let api = TokenApi::serving(|_| success());
    let sentinel = "malformed-record-private-sentinel";
    let paths = ["one", "two", "bad"].map(|name| dir.path().join(format!("{name}.json")));
    for path in &paths[..2] {
        fs::write(
            path,
            json!({"version":2,"state":"sealed","since":10,"cause":"reauthorization-required"})
                .to_string(),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(&paths[2], format!("{{\"state\":{sentinel}}}")).unwrap();
    fs::set_permissions(&paths[2], fs::Permissions::from_mode(0o600)).unwrap();
    let entries: Vec<Value> = ["one", "two", "bad"].iter().enumerate().map(|(index, name)| json!({
        "drn":drn(name), "source":source_value(&paths[index], &api.endpoint),
        "bindings":[{"id":format!("binding-{name}"),"capability":"vm.session.create","sink":"httpBearer",
            "allowedHosts":["runner.example"],"allowedMethods":["POST"],
            "allowedPaths":[{"match":"exact","path":"/v1/sessions"}]}]
    })).collect();
    let map_path = dir.path().join("map.json");
    fs::write(&map_path, json!({"apiVersion":"dekopon.dev/secret-map/v1alpha1","mapRevision":"test","secrets":entries}).to_string()).unwrap();
    fs::set_permissions(&map_path, fs::Permissions::from_mode(0o600)).unwrap();
    let capture = CaptureLayer::new();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let catalog = super::super::super::load(&map_path, rustix::process::geteuid().as_raw())
        .with_subscriber(subscriber)
        .await
        .expect("load valid map with unavailable records");
    assert_eq!(catalog.drns().count(), 3);
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
    assert!(api.requests().is_empty());
}

#[tokio::test(start_paused = true)]
async fn next_tick_reads_the_swapped_active_generation() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.json");
    let second = dir.path().join("second.json");
    write_record(&first, 100_000);
    write_record(&second, 100_000);
    let api = TokenApi::serving(|count| {
        (
            "200 OK",
            json!({"access_token":format!("access-{count}"),
        "refresh_token":format!("refresh-{count}"),"expires_in":86400})
            .to_string(),
        )
    });
    let current = Arc::new(Mutex::new(scheduled_resolver(
        &first,
        &api.endpoint,
        &["one"],
    )));
    let active = Arc::clone(&current);
    let now = Arc::new(AtomicU64::new(22 * 3600));
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(refresh_scheduler::serve_with_clock(
        async {
            stopped.await.expect("stop");
            Ok(())
        },
        move || active.lock().clone(),
        ShutdownDeadline::default(),
        Arc::new({
            let now = Arc::clone(&now);
            move || now.load(Ordering::SeqCst)
        }),
    ));
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let record: Value = serde_json::from_slice(&fs::read(&first).unwrap()).unwrap();
            if record["lastRefresh"] == 22 * 3600 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("initial generation refresh");
    *current.lock() = scheduled_resolver(&second, &api.endpoint, &["two"]);
    now.store(22 * 3600 + 5 * 60, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(5 * 60)).await;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let record: Value = serde_json::from_slice(&fs::read(&second).unwrap()).unwrap();
            if record["lastRefresh"] == 22 * 3600 + 5 * 60 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("swapped generation refresh");
    stop.send(()).expect("stop receiver");
    task.await.unwrap().unwrap();
    assert_eq!(api.requests().len(), 2);
}
