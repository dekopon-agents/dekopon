use serde_json::json;

use super::{RedactionReason, redact, sanitize, sanitize_line};

#[test]
fn reports_every_secret_in_one_payload_rather_than_the_first() {
    // Two simultaneous problems, both reported: a payload that carried three secrets and reported
    // one would be read as a payload that carried one.
    let redacted = redact(&json!({
        "headers": {"authorization": "Bearer abcdefghijklmnopqrstuvwxyz"},
        "note": "the key is ghp_0123456789abcdefghijklmnopqrstuvwxyz",
        "body": {"apiKey": "sk-0123456789abcdefghijklmnop"},
        "safe": "an ordinary sentence"
    }));

    // Sorted before comparing, not because order is meaningless — the list follows the document, so
    // a pane can point at each redaction in the order it drew them — but because *document* order
    // for a JSON object is a `serde_json` build detail: `preserve_order` makes it insertion order
    // and its absence makes it sorted. What this test is about is that all three are reported.
    let mut paths: Vec<&str> = redacted
        .redactions
        .iter()
        .map(|redaction| redaction.path.as_str())
        .collect();
    paths.sort_unstable();
    assert_eq!(paths, ["body.apiKey", "headers.authorization", "note"]);
    assert!(!redacted.is_clean());
    assert_eq!(
        redacted.value["safe"],
        json!("an ordinary sentence"),
        "ordinary text must survive untouched"
    );
    for path in ["headers", "body"] {
        assert!(
            !redacted.value[path].to_string().contains("abcdefghijkl"),
            "{path} still carries its secret"
        );
    }
}

#[test]
fn distinguishes_the_rule_that_fired() {
    let redacted = redact(&json!({"token": "short", "note": "sk-0123456789abcdefghijklmnop"}));

    let mut reasons: Vec<(&str, RedactionReason)> = redacted
        .redactions
        .iter()
        .map(|redaction| (redaction.path.as_str(), redaction.reason))
        .collect();
    reasons.sort_unstable_by_key(|(path, _)| *path);
    assert_eq!(
        reasons,
        [
            ("note", RedactionReason::Shape),
            ("token", RedactionReason::Key)
        ],
        "a short value under a secret key is still redacted, by the key rule"
    );
}

#[test]
fn redacts_a_secret_key_holding_a_non_string() {
    let redacted = redact(&json!({"secret": 12_345_678_901_234_567_890_u64}));

    assert_eq!(redacted.redactions.len(), 1);
    assert!(redacted.value["secret"].is_string());
    assert!(
        !redacted.value["secret"]
            .as_str()
            .unwrap_or_default()
            .contains("12345")
    );
}

#[test]
fn walks_into_arrays_and_reports_indexed_paths() {
    let redacted = redact(&json!({"rows": [{"token": "aaaaaaaaaaaaaaaaaaaaaa"}, {"name": "ok"}]}));

    assert_eq!(
        redacted
            .redactions
            .iter()
            .map(|redaction| redaction.path.clone())
            .collect::<Vec<_>>(),
        ["rows.0.token"]
    );
}

#[test]
fn recognises_a_three_segment_jwt_anywhere() {
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r";
    let redacted = redact(&json!({"assertion": jwt}));

    assert_eq!(redacted.redactions.len(), 1);
    assert_eq!(redacted.redactions[0].reason, RedactionReason::Shape);
}

#[test]
fn leaves_dotted_ordinary_text_alone() {
    // Three dot-separated words are not a JWT; a redactor that thought so would mark half the
    // capability identifiers in this system.
    assert!(redact(&json!({"id": "gh.pull-request.read"})).is_clean());
    assert!(redact(&json!({"host": "api.github.com"})).is_clean());
}

#[test]
fn strips_control_sequences_but_keeps_structure() {
    let hostile = "title\u{1b}[2Koverwritten\u{9b}31m\u{202e}reversed\ttab\nline";
    let cleaned = sanitize(hostile);

    assert!(!cleaned.contains('\u{1b}'), "ESC survived");
    assert!(!cleaned.contains('\u{9b}'), "eight-bit CSI survived");
    assert!(!cleaned.contains('\u{202e}'), "bidi override survived");
    assert!(
        cleaned.contains('\t') && cleaned.contains('\n'),
        "tabs and newlines are the renderer's business"
    );
    assert!(cleaned.contains("title") && cleaned.contains("line"));
}

#[test]
fn line_rendering_collapses_whitespace_it_cannot_draw() {
    let line = sanitize_line("one\ttwo\nthree\u{1b}[A");
    assert_eq!(line, "one two three\u{fffd}[A");
}

#[test]
fn a_token_inside_a_sentence_loses_only_the_token() {
    // The sentence is the part an operator was reading. Replacing the whole value would hide the
    // provider's own explanation along with the secret it happened to quote.
    let redacted = redact(&json!({
        "message": "auth failed for ghp_0123456789abcdefghijklmnopqrstuvwxyz on api.github.com"
    }));

    let rendered = redacted.value["message"].as_str().expect("still a string");
    assert!(rendered.starts_with("auth failed for "), "got: {rendered}");
    assert!(rendered.ends_with(" on api.github.com"), "got: {rendered}");
    assert!(
        !rendered.contains("ghp_0123"),
        "the token survived: {rendered}"
    );
    assert_eq!(redacted.redactions.len(), 1);
}

#[test]
fn a_scheme_word_marks_the_run_after_it() {
    let redacted = redact(&json!({
        "header": "Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123"
    }));

    let rendered = redacted.value["header"].as_str().expect("still a string");
    assert!(
        rendered.contains("Bearer"),
        "the scheme is not the secret: {rendered}"
    );
    assert!(
        !rendered.contains("abcdefghijkl"),
        "the credential survived: {rendered}"
    );
}

#[test]
fn a_scheme_word_ended_by_punctuation_introduces_nothing() {
    // `Bearer, somethingquitelongindeed` is a list, not a credential presentation.
    assert!(redact(&json!({"note": "Bearer, somethingquitelongindeedhere"})).is_clean());
}

#[test]
fn short_runs_after_a_scheme_are_left_alone() {
    assert!(redact(&json!({"note": "bearer none"})).is_clean());
}
