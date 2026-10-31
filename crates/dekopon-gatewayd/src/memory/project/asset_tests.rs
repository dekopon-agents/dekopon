use super::*;
use crate::{
    asset::{MemoryAssets, retention_tests},
    memory::{
        engine::tests::{assets::World, live_resources},
        journal::{
            JournalMemoryTools,
            records::{Header, tests::fixture},
            tools::Admission,
        },
        record::{AssetSideLine, SlotLine, Subject},
    },
};
use dekopon_agent::memory_tools::{MemoryRefusal, MemoryTools as _, Target};
use dekopon_model::model::CompletionOptions;
use std::ops::ControlFlow;

fn project_world(world: &World, prior: &[PriorRecord], images: bool) -> Projected {
    let snapshot = world.snapshot();
    project(&ProjectionInput {
        file: &snapshot.file,
        records: &snapshot.records,
        prior,
        hot_turns: 1,
        max_turns: 4,
        max_bytes: 64 * 1024,
        model: &fixture().model,
        assets: Some(MemoryAssets {
            store: &world.store,
            access: &world.access,
            images_supported: images,
        }),
    })
    .unwrap()
}

fn memory_tools(world: &World, images: bool) -> JournalMemoryTools<'_> {
    JournalMemoryTools::new(
        &world.journal,
        world.stem.clone(),
        Admission {
            grant: fixture().grant,
            horizon_ms: 0,
            sealed_at_ms: None,
        },
    )
    .with_assets(MemoryAssets {
        store: &world.store,
        access: &world.access,
        images_supported: images,
    })
}

fn pin(world: &World, header: &Header, index: usize, rung: &str) {
    let tools = memory_tools(world, true);
    tools
        .set_pin(Target::Asset(header.arrived[index].number), rung)
        .unwrap();
    for line in tools.into_pins() {
        assert!(world.journal.write_pin(&world.stem, line).unwrap());
    }
}

async fn request(world: &World, projected: Projected) -> serde_json::Value {
    let vendor =
        crate::tests::memory::vendor(vec![crate::tests::memory::router_answer("ok")]).await;
    let resources = live_resources(Arc::clone(&world.journal), &vendor.endpoint, 60_000);
    let current = resources.current.load();
    let model = &current.routes.models["summary"];
    let (_cancel, cancelled) = tokio::sync::watch::channel(false);
    let client = resources
        .models
        .client_bound(model, tokio::runtime::Handle::current(), cancelled)
        .unwrap();
    tokio::task::spawn_blocking(move || {
        client.complete(
            &projected.messages,
            &[],
            &CompletionOptions::default(),
            &mut |_| ControlFlow::Continue(()),
        )
    })
    .await
    .unwrap()
    .unwrap();
    let mut bodies = vendor.bodies.lock();
    assert_eq!(bodies.len(), 1);
    bodies.pop().unwrap()
}

fn no_source_fields(value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                assert!(!["url", "file_id", "media_id", "attachment_id"].contains(&key.as_str()));
                no_source_fields(value);
            }
        }
        serde_json::Value::Array(values) => values.iter().for_each(no_source_fields),
        serde_json::Value::String(text) => assert_ne!(text, "sender-name-canary"),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

#[tokio::test(start_paused = true)]
async fn an_asset_summary_replays_to_every_participant_of_a_space() {
    let (capture, _guard) = crate::tests::capture_spans();
    let world = World::shared();
    for _ in 0..6 {
        world.arrive(&[], None, 1);
    }
    let header = world.arrive(&["image/jpeg"], Some(b"jpeg pixels"), 1);
    world.generate(&header, 0, "caption").await;
    world.arrive(&[], None, 1);
    let a = project_world(&world, &[], true);
    assert!(a.repacked);
    assert_eq!(a.assets.rungs.summary, 1);
    let prior = a.prior.clone();
    let b = project_world(&world, &prior, true);
    let request_a = request(&world, a).await;
    let request_b = request(&world, b).await;
    assert_eq!(request_a, request_b);
    assert!(
        request_a["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["content"].as_str().is_some_and(|text| {
                    text.contains("[gateway: chat-asset:1 summary 1/1 — caption]")
                })
            })
    );
    no_source_fields(&request_a);
    for line in &world
        .snapshot()
        .records
        .iter()
        .find(|head| head.header.id == header.id)
        .unwrap()
        .asset_lines
    {
        match line {
            AssetSideLine::Slot(line) => no_source_fields(&serde_json::to_value(line).unwrap()),
            AssetSideLine::Pin(line) => no_source_fields(&serde_json::to_value(line).unwrap()),
        }
    }
    let dm = world.direct();
    dm.arrive(&[], None, 1);
    let dm_request = request(&dm, project_world(&dm, &[], true)).await;
    assert!(
        !dm_request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["content"]
                .as_str()
                .is_some_and(|text| text.contains("chat-asset:")))
    );
    let records = capture.records();
    assert!(
        records
            .iter()
            .any(|record| record.field("memory.summary.subject") == Some("asset"))
    );
    for record in records {
        for key in [
            "url",
            "file_id",
            "media_id",
            "attachment_id",
            "file.name",
            "asset.name",
            "asset.source",
        ] {
            assert_eq!(record.field(key), None, "{record:?}");
        }
        assert_ne!(record.field("name"), Some("sender-name-canary"));
    }
}

#[tokio::test(start_paused = true)]
async fn a_native_pin_steps_down_when_the_reply_model_cannot_see_images() {
    let world = World::new();
    let header = world.arrive(&["image/png"], Some(b"pixels"), 1);
    world.generate(&header, 0, "caption").await;
    pin(&world, &header, 0, "native");
    let first = project_world(&world, &[], true);
    assert_eq!(first.assets.rungs.native, 1);
    let down = project_world(&world, &first.prior, false);
    assert_eq!(down.assets.rungs.summary, 1);
    assert_eq!(down.assets.native_unshowable, 1);
    assert_eq!(down.prior[0].assets[0].pin, Some(Rung::Native));
    let restored = project_world(&world, &down.prior, true);
    assert_eq!(restored.assets.rungs.native, 1);
    let down_request = request(&world, down).await;
    assert!(
        down_request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["content"].as_str().is_some_and(|text| {
                    text.contains("[gateway: chat-asset:1 summary 1/1 — caption]")
                })
            })
    );
    assert_eq!(
        request(&world, first).await,
        request(&world, restored).await
    );
}

#[tokio::test(start_paused = true)]
async fn a_pin_on_an_image_without_image_modality_is_unavailable() {
    let world = World::new();
    let header = world.arrive(&["image/png", "text/plain"], Some(b"contents"), 1);
    for index in 0..2 {
        world.generate(&header, index, "caption").await;
    }
    let tools = memory_tools(&world, false);
    for asset in &header.arrived {
        assert!(
            matches!(tools.set_pin(Target::Asset(asset.number), "native"), Err(MemoryRefusal::RepresentationUnavailable { available }) if available == [Rung::Summary(NonZeroU8::MIN), Rung::Metadata])
        );
        assert_eq!(
            tools
                .read(Target::Asset(asset.number), "summary-1", None)
                .unwrap(),
            "caption"
        );
        assert!(
            matches!(tools.read(Target::Asset(asset.number), "native", None), Err(MemoryRefusal::RepresentationUnavailable { available }) if available == [Rung::Summary(NonZeroU8::MIN), Rung::Metadata])
        );
    }
    let tools = memory_tools(&world, true);
    assert!(matches!(
        tools.set_pin(Target::Asset(header.arrived[1].number), "native"),
        Err(MemoryRefusal::RepresentationUnavailable { .. })
    ));
}

#[tokio::test(start_paused = true)]
async fn an_evicted_native_pin_keeps_its_summary_without_a_download() {
    let world = World::new();
    let header = world.arrive(&["image/png"], Some(b"pixels"), 1);
    world.generate(&header, 0, "caption").await;
    pin(&world, &header, 0, "native");
    let first = project_world(&world, &[], true);
    let prior = first.prior.clone();
    drop(first);
    retention_tests::reclaim(&world.store, &world.access, header.arrived[0].number.get());
    let _permits = retention_tests::hold_downloads(&world.store).await;
    let projected = project_world(&world, &prior, true);
    assert_eq!(projected.assets.native_unresident, 1);
    assert_eq!(projected.assets.rungs.summary, 1);
    assert_eq!(projected.assets.native_bytes, 0);
    assert_eq!(projected.prior[0].assets[0].pin, Some(Rung::Native));
    assert_eq!(world.slots(&header, 0), [Slot::Generated("caption".into())]);
    let body = request(&world, projected).await;
    assert!(body["messages"].as_array().unwrap().iter().any(|message| {
        message["content"]
            .as_str()
            .is_some_and(|text| text.contains("[gateway: chat-asset:1 summary 1/1 — caption]"))
    }));
}

#[tokio::test(start_paused = true)]
async fn clearing_all_pins_covers_each_record_and_asset_subject() {
    let world = World::new();
    let header = world.arrive(&["image/png", "application/pdf"], Some(b"resident"), 1);
    let tools = memory_tools(&world, true);
    tools
        .set_pin(Target::Record(header.id.0.get()), "native")
        .unwrap();
    for asset in &header.arrived {
        tools
            .set_pin(Target::Asset(asset.number), "native")
            .unwrap();
    }
    tools.clear_pin(None).unwrap();
    let pins = tools.into_pins();
    assert_eq!(pins.len(), 6);
    for line in pins {
        assert!(world.journal.write_pin(&world.stem, line).unwrap());
    }
    let snapshot = world.snapshot();
    assert_eq!(snapshot.records[0].header.pin, None);
    for asset in &snapshot.records[0].header.arrived {
        assert_eq!(asset.join(&snapshot.records[0].asset_lines).pin, None);
    }
}

#[tokio::test(start_paused = true)]
async fn a_native_attachment_follows_its_summarized_record_envelope() {
    let world = World::new();
    let first = world.arrive(&["image/png"], Some(b"first pixels"), 1);
    let second = world.arrive(&["image/png"], Some(b"second pixels"), 1);
    for (header, caption) in [(&first, "first record"), (&second, "second record")] {
        assert!(
            world
                .journal
                .write_slot(
                    &world.stem,
                    SlotLine {
                        at_ms: 1,
                        record: header.id,
                        subject: Subject::Record,
                        part: NonZeroU8::MIN,
                        slot: Slot::Generated(caption.into()),
                    }
                )
                .unwrap()
        );
        pin(&world, header, 0, "native");
    }
    world.arrive(&[], None, 1);
    let projected = project_world(&world, &[], true);
    assert_eq!(projected.rungs.summary, 2);
    assert_eq!(projected.assets.rungs.native, 2);
    let body = request(&world, projected).await;
    let messages = body["messages"].as_array().unwrap();
    let first_at = messages
        .iter()
        .position(|message| {
            message["content"]
                .as_str()
                .is_some_and(|text| text.contains("first record"))
        })
        .unwrap();
    let second_at = messages
        .iter()
        .position(|message| {
            message["content"]
                .as_str()
                .is_some_and(|text| text.contains("second record"))
        })
        .unwrap();
    assert!(
        messages[first_at]["content"]
            .as_str()
            .unwrap()
            .starts_with(EARLIER)
    );
    assert_eq!(second_at, first_at + 2);
    for (at, number, bytes) in [
        (first_at + 1, 1, b"first pixels".as_slice()),
        (second_at + 1, 2, b"second pixels".as_slice()),
    ] {
        assert_eq!(messages[at]["role"], "user");
        let parts = messages[at]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[0],
            serde_json::json!({"type":"text", "text":format!("chat-asset:{number}:")})
        );
        use base64::Engine as _;
        let image = parts[1]["image_url"]["url"].as_str().unwrap();
        let encoded = image.strip_prefix("data:image/png;base64,").unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
            bytes
        );
    }
}

#[tokio::test(start_paused = true)]
async fn attachment_memory_off_refuses_asset_pins_and_keeps_their_replay_inert() {
    let world = World::new();
    let header = world.arrive(&["image/png"], Some(b"pixels"), 1);
    let project_off = |prior| {
        let snapshot = world.snapshot();
        project(&ProjectionInput {
            file: &snapshot.file,
            records: &snapshot.records,
            prior,
            hot_turns: 1,
            max_turns: 4,
            max_bytes: 64 * 1024,
            model: &fixture().model,
            assets: None,
        })
        .unwrap()
    };
    let baseline = request(&world, project_off(&[])).await;
    world.generate(&header, 0, "caption").await;
    pin(&world, &header, 0, "native");
    let enabled = project_world(&world, &[], true);
    assert_eq!(enabled.assets.rungs.native, 1);
    let prior = enabled.prior.clone();
    drop(enabled);
    let off = JournalMemoryTools::new(
        &world.journal,
        world.stem.clone(),
        Admission {
            grant: fixture().grant,
            horizon_ms: 0,
            sealed_at_ms: None,
        },
    );
    let number = header.arrived[0].number;
    assert_eq!(
        off.attachment_targets(),
        dekopon_agent::memory_tools::AttachmentTargets::Off
    );
    assert!(
        matches!(off.set_pin(Target::Asset(number), "native"), Err(MemoryRefusal::RepresentationUnavailable { available }) if available.is_empty())
    );
    assert!(
        matches!(off.read(Target::Asset(number), "summary-1", None), Err(MemoryRefusal::RepresentationUnavailable { available }) if available.is_empty())
    );
    off.clear_pin(None).unwrap();
    assert!(off.into_pins().is_empty());
    let projected = project_off(&prior);
    assert_eq!(projected.assets.native_bytes, 0);
    assert_eq!(projected.assets.rungs, RungCounts::default());
    assert!(projected.prior[0].assets.is_empty());
    assert_eq!(request(&world, projected).await, baseline);
    let restored = project_world(&world, &[], true);
    assert_eq!(restored.assets.rungs.native, 1);
}
