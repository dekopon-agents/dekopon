use super::*;
use crate::transport::{MAX_INBOUND_TEXT_BYTES, bound_inbound};

fn asset(id: u64, name: &str, mime: &str, size: Option<u64>) -> AssetRef {
    AssetRef {
        id,
        name: name.into(),
        mime: mime.into(),
        size,
        source: Some(AssetSourceRef::Slack {
            file_id: "F-private".into(),
            url: "https://files.slack.com/private/bearer-source".into(),
        }),
        arrived: SystemTime::UNIX_EPOCH,
        encoding: AssetEncoding::Identity,
        sent: false,
    }
}

fn inventory(count: u64) -> Registered {
    Registered {
        inventory: (1..=count)
            .map(|id| asset(id, &format!("file-{id}.png"), "image/png", Some(1024)))
            .collect(),
        arrived: vec![count],
    }
}

#[test]
fn a_forty_attachment_conversation_lists_all_forty() {
    let note = reference_note(
        &inventory(40),
        true,
        TextReader::AssetCat,
        MAX_INBOUND_TEXT_BYTES,
    );
    assert_eq!(
        (note.listed, note.omitted, note.bytes),
        (40, 0, note.text.len())
    );
    for id in 1..=40 {
        assert!(note.text.contains(&format!("chat-asset:{id} —")));
    }
}

#[test]
fn a_note_that_would_cross_the_inbound_limit_omits_the_oldest_and_keeps_the_new_arrival() {
    let text = "x".repeat(15 * 1024);
    let note = reference_note(
        &inventory(200),
        true,
        TextReader::AssetCat,
        MAX_INBOUND_TEXT_BYTES - text.len() - 2,
    );
    let prompt = format!("{text}\n\n{}", note.text);
    assert_eq!(bound_inbound(&prompt), prompt);
    assert!(
        note.text
            .contains("chat-asset:200 — file-200.png (image/png, 1 KB) — attached to this message")
    );
    assert!(note.text.contains(&format!(
        "{} older files, chat-asset:1 to chat-asset:{},",
        note.omitted, note.omitted
    )));
    let listed = note
        .text
        .lines()
        .filter_map(|line| line.trim().strip_prefix("chat-asset:"))
        .map(|line| line.split(' ').next().unwrap().parse::<usize>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(listed, ((note.omitted + 1)..=200).rev().collect::<Vec<_>>());
}

#[test]
fn a_code_file_is_read_with_asset_cat() {
    let registered = Registered {
        inventory: vec![asset(1, "notes.py", "text/x-python", Some(3072))],
        arrived: vec![],
    };
    let note = reference_note(
        &registered,
        true,
        TextReader::AssetCat,
        MAX_INBOUND_TEXT_BYTES,
    );
    assert!(note.text.contains(
        "chat-asset:1 — notes.py (text/x-python, 3 KB) — read with `asset cat chat-asset:1`"
    ));
}

#[test]
fn without_asset_cat_granted_a_text_file_is_command_only() {
    let registered = Registered {
        inventory: vec![asset(1, "notes.py", "text/x-python", None)],
        arrived: vec![],
    };
    assert_eq!(
        NoteLine::for_asset(&registered.inventory[0], true, TextReader::Absent),
        NoteLine::CommandOnly
    );
    let note = reference_note(
        &registered,
        true,
        TextReader::Absent,
        MAX_INBOUND_TEXT_BYTES,
    );
    assert!(
        note.text
            .contains("not readable here; a command can take it as chat-asset:1")
    );
    assert!(!note.text.contains("asset cat"));
}

#[test]
fn an_application_yaml_file_is_command_only() {
    let file = asset(1, "config.yaml", "application/yaml", None);
    assert_eq!(
        NoteLine::for_asset(&file, true, TextReader::AssetCat),
        NoteLine::CommandOnly
    );
}

#[test]
fn the_cat_predicate_matches_provider_asset_030_operations_cat() {
    for (mime, expected) in [
        ("text/x-python", true),
        ("text/yaml", true),
        ("application/ld+json", true),
        ("Text/Plain; charset=utf-8", true),
        ("application/yaml", false),
        ("image/svg+xml", false),
        ("application/xml", true),
        ("application/javascript", true),
        ("application/x-www-form-urlencoded", true),
        ("application/soap+xml", true),
        ("invalid", false),
    ] {
        assert_eq!(
            cat_reads(mime),
            expected,
            "provider a1da68db src/operations.rs:183-190: {mime}"
        );
    }
    assert_eq!(
        NoteLine::for_asset(
            &asset(1, "plain.txt", "text/plain", None),
            true,
            TextReader::AssetCat
        ),
        NoteLine::Shown
    );
    assert_eq!(
        NoteLine::for_asset(
            &asset(1, "plain.txt", "Text/Plain; charset=utf-8", None),
            true,
            TextReader::AssetCat
        ),
        NoteLine::Cat
    );
}

#[test]
fn the_whole_note_uses_the_frozen_line_templates() {
    let mut hidden = asset(5, "report.pdf", "application/pdf", None);
    hidden.source = None;
    let lines = [
        (
            NoteLine::Shown,
            asset(6, "chart.png", "image/png", Some(88 * 1024)),
            true,
        ),
        (NoteLine::Invisible, hidden, false),
        (
            NoteLine::CommandOnly,
            asset(4, "recording.mov", "video/quicktime", Some(43307008)),
            false,
        ),
        (
            NoteLine::ImagesOff,
            asset(3, "photo.jpg", "image/jpeg", Some(1259000)),
            false,
        ),
        (
            NoteLine::Cat,
            asset(2, "notes.py", "text/x-python", Some(3072)),
            false,
        ),
        (
            NoteLine::Shown,
            asset(1, "screenshot.png", "image/png", Some(214 * 1024)),
            false,
        ),
    ];
    let mut text = NOTE_HEADER.to_owned();
    for (kind, asset, arrived) in lines {
        text.push_str(&kind.render(&asset, arrived));
    }
    text.push_str(NOTE_FOOTER);
    text.push(']');
    assert_eq!(
        text,
        "[gateway: files in this conversation\n  chat-asset:6 — chart.png (image/png, 88 KB) — attached to this message\n  report.pdf (application/pdf, size unknown) — the gateway cannot see this file at all\n  chat-asset:4 — recording.mov (video/quicktime, 41.3 MB) — not readable here; a command can take it as chat-asset:4\n  chat-asset:3 — photo.jpg (image/jpeg, 1.2 MB) — this agent's model cannot be shown images\n  chat-asset:2 — notes.py (text/x-python, 3 KB) — read with `asset cat chat-asset:2`\n  chat-asset:1 — screenshot.png (image/png, 214 KB)\n  Call fetch_chat_asset with the number to look at an image or document.]"
    );
}

#[test]
fn a_note_with_a_slack_source_contains_only_safe_metadata() {
    let note = reference_note(
        &inventory(2),
        true,
        TextReader::AssetCat,
        MAX_INBOUND_TEXT_BYTES,
    );
    for private in [
        "source",
        "F-private",
        "https://files.slack.com",
        "bearer-source",
    ] {
        assert!(!note.text.contains(private));
    }
}

#[test]
fn a_tiny_note_shrinks_before_it_drops() {
    let registered = inventory(200);
    let line = NoteLine::Shown.render(&registered.inventory[199], true);
    let omission = "\n  199 older files, chat-asset:1 to chat-asset:199, are not listed for space; each number still works.";
    let budget = line.len() + omission.len();
    let note = reference_note(&registered, true, TextReader::AssetCat, budget);
    assert_eq!(note.text, format!("{line}{omission}"));
    assert_eq!((note.listed, note.omitted), (1, 199));
    let dropped = reference_note(&registered, true, TextReader::AssetCat, 1);
    assert_eq!(
        (
            dropped.text.as_str(),
            dropped.listed,
            dropped.omitted,
            dropped.bytes
        ),
        ("", 0, 200, 0)
    );
}
