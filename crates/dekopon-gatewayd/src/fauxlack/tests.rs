use super::world::{Draft, Progress, Publication};
use super::*;
use crate::{
    progress::ProgressText,
    transport::{ChatTransport, LivenessTarget, OutboundReply},
};
use std::time::Duration;

const TEAM: &str = include_str!("../../tests/gateway/team.yaml");

#[test]
fn manifest_reports_all_semantic_conflicts_and_preserves_authority_errors() {
    let error = Fauxlack::from_manifest(
        r#"
servers:
  - id: acme
    users:
      - { id: ualice, name: Alice }
      - { id: ualice, name: Duplicate }
      - { id: cperson, name: Space }
      - { id: UPPER, name: Bad }
    channels:
      - { id: notspace, name: Bad, members: [missing] }
      - { id: BAD, name: Invalid, members: [] }
    directMessages:
      - { id: dbad, members: [missing, missing] }
      - { id: dsmall, members: [ualice] }
    files:
      - { name: zero }
      - { name: double, canvas: a, snippet: b }
      - { name: invalid, image: { mime: image/png, base64: '?' } }
      - { name: zero, snippet: duplicate }
  - { id: acme }
"#,
    )
    .expect_err("invalid manifest");
    let ManifestError::Invalid { problems } = error else {
        panic!("semantic problems");
    };
    assert!(problems.iter().any(|p| matches!(p, ManifestProblem::Subject { source, .. } if source == &dekopon_core::ExternalSubject::fauxlack("acme", "UPPER").unwrap_err())));
    let categories = problems
        .iter()
        .map(|problem| match problem {
            ManifestProblem::Subject { .. } => "subject",
            ManifestProblem::Kind { .. } => "kind",
            ManifestProblem::Conversation { .. } => "conversation",
            ManifestProblem::DuplicateServer { .. } => "server",
            ManifestProblem::DuplicateId { .. } => "id",
            ManifestProblem::DuplicateFile { .. } => "file",
            ManifestProblem::UnknownMember { .. } => "unknown-member",
            ManifestProblem::RepeatedMember { .. } => "repeated-member",
            ManifestProblem::DirectMessageSize { .. } => "dm-size",
            ManifestProblem::FileContent { .. } => "file-content",
            ManifestProblem::Base64 { .. } => "base64",
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        categories,
        std::collections::BTreeSet::from([
            "subject",
            "kind",
            "conversation",
            "server",
            "id",
            "file",
            "unknown-member",
            "repeated-member",
            "dm-size",
            "file-content",
            "base64"
        ])
    );
}

#[test]
fn manifest_rejects_unknown_fields_at_each_authored_level() {
    for document in [
        "servers: []\noops: 1",
        "servers: [{id: acme, oops: 1}]",
        "servers: [{id: acme, users: [{id: ua, name: A, oops: 1}]}]",
        "servers: [{id: acme, channels: [{id: ca, name: A, members: [], oops: 1}]}]",
        "servers: [{id: acme, directMessages: [{id: da, members: [], oops: 1}]}]",
        "servers: [{id: acme, files: [{name: a, snippet: a, oops: 1}]}]",
        "servers: [{id: acme, files: [{name: a, image: {mime: image/png, base64: '', oops: 1}}]}]",
    ] {
        assert!(
            matches!(
                Fauxlack::from_manifest(document),
                Err(ManifestError::Decode(_))
            ),
            "{document}"
        );
    }
    Fauxlack::from_manifest("servers: [{id: acme, users: [{id: ua, name: A}, {id: ub, name: B}], directMessages: [{id: da, members: [ua, ub]}, {id: db, members: [ua, ub]}]}]").expect("distinct DMs may share members");
}

#[tokio::test]
async fn completed_answers_follow_completion_order_and_progress_is_not_a_transcript_entry() {
    let chat = Fauxlack::from_manifest(TEAM).unwrap();
    let server = chat.server("acme");
    assert_eq!(server.channel_name("cteam"), Some("team"));
    let root = server.user("ualice").post("cteam", "start").await.unwrap();
    let thread = root.thread();
    let progress = server
        .publish(Draft {
            author: "udekopon",
            container: "cteam",
            thread: Some(root.id()),
            text: "working",
            files: Vec::new(),
            publication: Publication::Progress,
        })
        .await
        .unwrap();
    assert_eq!(thread.transcript().authors(), ["ualice"]);
    assert!(server.progress(progress.id(), "edited", Progress::Edit));
    let second = server
        .publish(Draft {
            author: "udekopon",
            container: "cteam",
            thread: Some(root.id()),
            text: "first completed",
            files: Vec::new(),
            publication: Publication::Final,
        })
        .await
        .unwrap();
    assert_eq!(
        thread
            .next_from("udekopon", Duration::from_secs(1))
            .await
            .id,
        second.id()
    );
    assert!(server.progress(
        progress.id(),
        "second completed",
        Progress::Finalize(Vec::new())
    ));
    assert_eq!(
        thread
            .next_from("udekopon", Duration::from_secs(1))
            .await
            .id,
        progress.id()
    );
    assert!(!server.progress(progress.id(), "duplicate", Progress::Finalize(Vec::new())));
    let deleted = server
        .publish(Draft {
            author: "udekopon",
            container: "cteam",
            thread: Some(root.id()),
            text: "never completed",
            files: Vec::new(),
            publication: Publication::Progress,
        })
        .await
        .unwrap();
    assert!(server.progress(deleted.id(), "", Progress::Delete));
    assert_eq!(
        thread
            .transcript()
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        ["start", "second completed", "first completed"]
    );
    assert!(matches!(
        server.user("ucarol").post("dalice", "no").await,
        Err(PostError::NotAMember)
    ));
    assert!(matches!(
        server.user("ualice").post("missing", "no").await,
        Err(PostError::UnknownContainer)
    ));
    assert!(matches!(
        server
            .user("ualice")
            .post_with("cteam", "no", &["missing"])
            .await,
        Err(PostError::UnknownFile)
    ));
}

#[tokio::test]
async fn delivery_backpressures_at_64_and_preserves_post_order() {
    let chat = Fauxlack::from_manifest(TEAM).unwrap();
    let server = chat.server("acme");
    let (_, mut receiver) = server.attach("udekopon").unwrap();
    let alice = server.user("ualice");
    for number in 0..64 {
        alice.post("cteam", &number.to_string()).await.unwrap();
    }
    let mut blocked = std::pin::pin!(alice.post("cteam", "64"));
    assert!(futures_util::poll!(&mut blocked).is_pending());
    assert_eq!(receiver.recv().await.unwrap().text, "0");
    blocked.await.unwrap();
    for number in 1..65 {
        assert_eq!(receiver.recv().await.unwrap().text, number.to_string());
    }
}

#[tokio::test]
async fn progress_driver_and_reply_observe_completed_answers_and_generated_files() {
    let chat = Fauxlack::from_manifest(TEAM).unwrap();
    let server = chat.server("acme");
    let root = server.user("ualice").post("cteam", "start").await.unwrap();
    let thread = root.thread();
    let mut transport = crate::transport::fauxlack::FauxlackTransport::new(
        "chat".into(),
        server.clone(),
        "udekopon".into(),
    );
    transport.connect().await.unwrap();
    let driver = transport.driver();
    let progress = driver.progress().unwrap();
    let target = LivenessTarget::Fauxlack {
        server: "acme".into(),
        container: "cteam".into(),
        thread: Some(root.id().into()),
    };
    let message = progress
        .post(&target, &ProgressText::for_test("working"), false)
        .await
        .unwrap();
    assert_eq!(thread.transcript().authors(), ["ualice"]);
    let bytes = b"\x89PNG\r\n\x1a\nkitty pixels";
    progress
        .finalize(&message, &OutboundReply::text("progress done"))
        .await
        .unwrap();
    assert_eq!(
        thread
            .next_from("udekopon", Duration::from_secs(1))
            .await
            .text,
        "progress done"
    );
    driver
        .reply(
            &crate::transport::ReplyTarget::Fauxlack {
                server: "acme".into(),
                container: "cteam".into(),
                thread: Some(root.id().into()),
            },
            OutboundReply::with_images(
                "done",
                vec![dekopon_agent::attachment::GeneratedImage::from_png(bytes.to_vec()).unwrap()],
            ),
        )
        .await
        .unwrap();
    let answer = thread.next_from("udekopon", Duration::from_secs(1)).await;
    assert_eq!(answer.files[0].bytes, bytes);
    assert_eq!(answer.files[0].mime, "image/png");
    assert!(
        progress
            .post(
                &LivenessTarget::Local { connection: 1 },
                &ProgressText::for_test("no"),
                false
            )
            .await
            .is_err()
    );
}
