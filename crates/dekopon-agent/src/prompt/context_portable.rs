use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Representation {
    pub routine_id: String,
    pub version: String,
    pub source_record_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part: Option<String>,
    pub kind: String,
    pub captured_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Exchange {
    pub tool: String,
    pub request: Vec<Representation>,
    pub result: Vec<Representation>,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<i32>,
}

impl Representation {
    #[must_use]
    pub fn original(text: &str, version: &str) -> Self {
        Self {
            routine_id: "original".into(),
            version: version.into(),
            source_record_id: 0,
            part: None,
            kind: "original".into(),
            captured_bytes: text.len() as u64,
            text: Some(text.into()),
        }
    }

    #[must_use]
    pub fn prefix(text: &str, max_bytes: usize, routine_id: &str, version: &str) -> Self {
        Self {
            routine_id: routine_id.into(),
            version: version.into(),
            source_record_id: 0,
            part: None,
            kind: "prefix".into(),
            captured_bytes: text.len() as u64,
            text: Some(prefix_text(text, max_bytes)),
        }
    }

    #[must_use]
    pub fn omitted_bytes(&self) -> u64 {
        self.captured_bytes
            .saturating_sub(self.text.as_ref().map_or(0, |text| text.len() as u64))
    }
}

pub fn prefix_text(text: &str, bytes: usize) -> String {
    text[..text.floor_char_boundary(bytes.min(text.len()))].to_owned()
}

pub fn eviction_count(
    lengths: impl IntoIterator<Item = usize>,
    max_turns: usize,
    max_bytes: usize,
) -> usize {
    let lengths: Vec<_> = lengths.into_iter().collect();
    let mut dropped = lengths.len().saturating_sub(max_turns);
    let mut bytes = lengths[dropped..]
        .iter()
        .copied()
        .fold(0usize, usize::saturating_add);
    while bytes > max_bytes && dropped < lengths.len() {
        bytes = bytes.saturating_sub(lengths[dropped]);
        dropped += 1;
    }
    dropped
}

pub fn fit_projected(
    existing: impl IntoIterator<Item = usize>,
    new_bytes: usize,
    max_turns: usize,
    max_bytes: usize,
) -> Option<usize> {
    if max_turns == 0 || new_bytes > max_bytes {
        return None;
    }
    Some(eviction_count(
        existing.into_iter().chain(std::iter::once(new_bytes)),
        max_turns,
        max_bytes,
    ))
}

#[derive(Clone, Copy)]
pub struct BodySource<'a> {
    pub user: Option<&'a str>,
    pub answer: Option<&'a str>,
    pub source_eligibility: bool,
    pub representations: &'a [Representation],
    pub exchanges: &'a [Exchange],
}

pub fn render_body(source: BodySource<'_>, id: &str, version: &str) -> Option<String> {
    let matches = |rep: &&Representation| rep.routine_id == id && rep.version == version;
    let mut reps = source.representations.iter().filter(matches).peekable();
    let original = reps.peek()?.kind == "original";
    let mut text = if original {
        if !source.source_eligibility {
            return None;
        }
        match (&source.user, &source.answer) {
            (Some(user), Some(answer)) => format!("{user}\n{answer}"),
            (Some(user), None) => user.to_string(),
            (None, _) => String::new(),
        }
    } else {
        reps.filter_map(|rep| rep.text.as_deref()).collect()
    };
    if text.is_empty() && (!original || source.exchanges.is_empty()) {
        return None;
    }
    for exchange in source.exchanges {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!(
            "[tool: {}, outcome: {}, exit: {:?}]",
            exchange.tool, exchange.outcome, exchange.exit_status
        ));
        for (label, parts) in [("request", &exchange.request), ("result", &exchange.result)] {
            let content: String = parts
                .iter()
                .filter(matches)
                .filter_map(|rep| rep.text.as_deref())
                .collect();
            if !content.is_empty() {
                text.push_str(&format!("\n[{label}] {content}"));
            }
        }
    }
    Some(text)
}

pub struct EnvelopeSource<'a> {
    pub record_id: u64,
    pub body: BodySource<'a>,
    pub original_id: Option<&'a str>,
    pub eligible: &'a [String],
}

pub struct ProjectedTurn {
    pub user: String,
    pub answer: Option<String>,
}

pub fn render_envelope(source: EnvelopeSource<'_>, id: &str, text: &str) -> ProjectedTurn {
    let label = format!(
        "[memory: {}]\n",
        serde_json::json!({
            "recordId": source.record_id,
            "representationId": id,
            "eligibleRepresentationIds": source.eligible,
            "omittedBytes": source.body.representations.iter()
                .find(|rep| rep.routine_id == id && rep.text.is_some())
                .map_or(0, Representation::omitted_bytes),
        })
    );
    if Some(id) == source.original_id
        && source.body.exchanges.is_empty()
        && let Some(user) = source.body.user
    {
        ProjectedTurn {
            user: format!("{label}{user}"),
            answer: source.body.answer.map(str::to_owned),
        }
    } else {
        ProjectedTurn {
            user: format!("{label}{text}"),
            answer: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_stops_at_unicode_byte_boundary() {
        let rep = Representation::prefix("🌿 café", 7, "short", "v1");
        assert_eq!(rep.text.as_deref(), Some("🌿 ca"));
        assert_eq!(rep.captured_bytes, 10);
        assert_eq!(rep.omitted_bytes(), 3);
        assert_eq!(prefix_text("🌿 café", 7), "🌿 ca");
    }

    #[test]
    fn body_requires_matching_version_and_source_eligibility() {
        let reps = [Representation::original("a\nb", "v1")];
        let source = BodySource {
            user: Some("a"),
            answer: Some("b"),
            source_eligibility: true,
            representations: &reps,
            exchanges: &[],
        };
        assert_eq!(
            render_body(source, "original", "v1").as_deref(),
            Some("a\nb")
        );
        assert_eq!(render_body(source, "original", "stale"), None);
        assert_eq!(
            render_body(
                BodySource {
                    source_eligibility: false,
                    ..source
                },
                "original",
                "v1"
            ),
            None
        );
        let eligible = vec!["original".to_owned()];
        let projected = render_envelope(
            EnvelopeSource {
                record_id: 9_007_199_254_740_993,
                body: source,
                original_id: Some("original"),
                eligible: &eligible,
            },
            "original",
            "a\nb",
        );
        assert_eq!(projected.answer.as_deref(), Some("b"));
        let (label, user) = projected.user.split_once('\n').expect("label then user");
        assert_eq!(user, "a");
        let label: serde_json::Value = serde_json::from_str(
            label
                .strip_prefix("[memory: ")
                .expect("memory label")
                .strip_suffix(']')
                .expect("label end"),
        )
        .expect("JSON label");
        assert_eq!(
            label,
            serde_json::json!({"recordId": 9_007_199_254_740_993u64, "representationId": "original", "eligibleRepresentationIds": ["original"], "omittedBytes": 0})
        );
    }

    #[test]
    fn tool_only_original_has_no_leading_newline() {
        let reps = [Representation::original("", "v1")];
        let exchanges = [Exchange {
            tool: "echo".into(),
            request: vec![Representation::original("ask", "v1")],
            result: vec![Representation::original("reply", "v1")],
            outcome: "ok".into(),
            exit_status: Some(0),
        }];
        let source = BodySource {
            user: None,
            answer: None,
            source_eligibility: true,
            representations: &reps,
            exchanges: &exchanges,
        };
        assert_eq!(
            render_body(source, "original", "v1").as_deref(),
            Some("[tool: echo, outcome: ok, exit: Some(0)]\n[request] ask\n[result] reply")
        );
    }

    #[test]
    fn fit_evicts_oldest_whole_records_and_omits_oversized() {
        assert_eq!(fit_projected([5, 5], 5, 3, 10), Some(1));
        assert_eq!(fit_projected([5, 5], 11, 3, 10), None);
    }
}
