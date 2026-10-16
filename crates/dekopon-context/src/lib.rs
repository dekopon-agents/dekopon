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

fn append_exchanges(
    output: &mut String,
    exchanges: &[Exchange],
    mut selected: impl FnMut(&Representation) -> bool,
) {
    for exchange in exchanges {
        output.push_str(&format!(
            "\n[tool: {}, outcome: {}, exit: {:?}]",
            exchange.tool, exchange.outcome, exchange.exit_status
        ));
        for (label, parts) in [("request", &exchange.request), ("result", &exchange.result)] {
            let content: String = parts
                .iter()
                .filter(|rep| selected(rep))
                .filter_map(|rep| rep.text.as_deref())
                .collect();
            if !content.is_empty() {
                output.push_str(&format!("\n[{label}] {content}"));
            }
        }
    }
}

pub fn original_envelope_bytes(user: &str, answer: Option<&str>, exchanges: &[Exchange]) -> usize {
    if exchanges.is_empty() {
        return user.len().saturating_add(answer.map_or(0, str::len));
    }
    let mut output = user.to_owned();
    if let Some(answer) = answer {
        output.push('\n');
        output.push_str(answer);
    }
    append_exchanges(&mut output, exchanges, |rep| rep.kind == "original");
    output.len()
}

pub struct EnvelopeSource<'a> {
    pub user: Option<&'a str>,
    pub answer: Option<&'a str>,
    pub representations: &'a [Representation],
    pub exchanges: &'a [Exchange],
}

pub fn render_envelope(
    source: EnvelopeSource<'_>,
    id: &str,
    version: &str,
    is_original: bool,
) -> Option<String> {
    let EnvelopeSource {
        user,
        answer,
        representations,
        exchanges,
    } = source;
    let text: String = representations
        .iter()
        .filter(|rep| rep.routine_id == id && rep.version == version && rep.text.is_some())
        .map(|rep| rep.text.as_deref().unwrap_or_default())
        .collect();
    if (is_original && user.is_none()) || (!is_original && text.is_empty()) {
        return None;
    }
    let mut output = match (user, answer) {
        (Some(user), Some(answer)) if is_original => format!("{user}\n{answer}"),
        (Some(user), None) if is_original => user.to_owned(),
        _ => text.clone(),
    };
    append_exchanges(&mut output, exchanges, |rep| {
        rep.routine_id == id && rep.version == version
    });
    if !is_original {
        output = format!(
            "[memory: routine={id}, omitted={} bytes]\n{output}",
            representations
                .iter()
                .find(|rep| rep.routine_id == id)
                .map_or(0, Representation::omitted_bytes)
        );
    }
    Some(output)
}

pub mod replay;

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
    fn envelope_preserves_user_assistant_and_tool_order() {
        let exchange = Exchange {
            tool: "echo".into(),
            request: vec![Representation::original("ask", "v1")],
            result: vec![Representation::original("reply", "v1")],
            outcome: "ok".into(),
            exit_status: Some(0),
        };
        let output = render_envelope(
            EnvelopeSource {
                user: Some("user"),
                answer: Some("assistant"),
                representations: &[],
                exchanges: std::slice::from_ref(&exchange),
            },
            "original",
            "v1",
            true,
        );
        assert_eq!(
            output.as_deref(),
            Some(
                "user\nassistant\n[tool: echo, outcome: ok, exit: Some(0)]\n[request] ask\n[result] reply"
            )
        );
        assert_eq!(
            original_envelope_bytes("user", Some("assistant"), std::slice::from_ref(&exchange)),
            output.expect("original envelope exists").len()
        );
    }

    #[test]
    fn original_without_exchanges_retains_historical_fit_count() {
        let text = render_envelope(
            EnvelopeSource {
                user: Some("a"),
                answer: Some("b"),
                representations: &[],
                exchanges: &[],
            },
            "original",
            "v1",
            true,
        )
        .expect("original envelope exists");
        assert_eq!(text, "a\nb");
        assert_eq!(original_envelope_bytes("a", Some("b"), &[]), 2);
    }

    #[test]
    fn fit_evicts_oldest_whole_records_and_omits_oversized() {
        assert_eq!(fit_projected([5, 5], 5, 3, 10), Some(1));
        assert_eq!(fit_projected([5, 5], 11, 3, 10), None);
    }
}
