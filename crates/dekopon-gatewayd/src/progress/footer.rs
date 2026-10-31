use std::time::Duration;

use crate::transport::{TextUnit, split_message};

#[derive(Clone, Debug)]
pub(crate) struct Footer(String);

impl Footer {
    pub(crate) fn new(models: &[String], turns: u32, elapsed: Duration) -> Self {
        let steps = if turns == 1 { "step" } else { "steps" };
        Self(format!(
            "{} · {turns} {steps} · {} s",
            models.join(" + "),
            elapsed.as_secs()
        ))
    }

    pub(crate) fn line(&self) -> &str {
        &self.0
    }

    pub(crate) fn chunks(
        text: &str,
        suffix: Option<&str>,
        max: usize,
        unit: TextUnit,
    ) -> Vec<String> {
        let mut chunks = if text.trim().is_empty() && suffix.is_some() {
            Vec::new()
        } else {
            split_message(text, max, unit)
        };
        if let Some(suffix) = suffix {
            let count = |value: &str| match unit {
                TextUnit::Utf16 => value.encode_utf16().count(),
                TextUnit::Scalar => value.chars().count(),
            };
            if let Some(last) = chunks.last_mut()
                && count(last) + 2 + count(suffix) <= max
            {
                last.push_str("\n\n");
                last.push_str(suffix);
            } else {
                chunks.push(suffix.to_owned());
            }
        }
        chunks
    }

    pub(crate) fn without_discord_footer(text: &str) -> &str {
        let (answer, line) = if let Some(pair) = text.rsplit_once("\n\n-# ") {
            pair
        } else if let Some(line) = text.strip_prefix("-# ") {
            ("", line)
        } else {
            return text;
        };
        let Some((models, rest)) = line.split_once(" · ") else {
            return text;
        };
        let Some((steps, seconds)) = rest.split_once(" · ") else {
            return text;
        };
        let Some((count, noun)) = steps.split_once(' ') else {
            return text;
        };
        let Some(seconds) = seconds.strip_suffix(" s") else {
            return text;
        };
        let Ok(count) = count.parse::<u32>() else {
            return text;
        };
        if !models.is_empty()
            && !models.contains(['\r', '\n'])
            && noun == if count == 1 { "step" } else { "steps" }
            && seconds.parse::<u64>().is_ok()
        {
            answer
        } else {
            text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_footer_names_models_and_uses_singular_steps_and_whole_seconds() {
        assert_eq!(
            Footer::new(&["model-a".into()], 1, Duration::from_millis(7999)).line(),
            "model-a · 1 step · 7 s"
        );
        assert_eq!(
            Footer::new(
                &["model-b".into(), "model-a".into()],
                3,
                Duration::from_secs(12)
            )
            .line(),
            "model-b + model-a · 3 steps · 12 s"
        );
    }

    #[test]
    fn a_footer_stays_whole_after_the_last_service_sized_answer_chunk() {
        let suffix = "-# model · 1 step · 7 s";
        let chunks = Footer::chunks(&"😀".repeat(20), Some(suffix), 40, TextUnit::Utf16);
        assert_eq!(chunks, ["😀".repeat(20), suffix.to_owned()]);
        assert_eq!(
            Footer::chunks("", Some(suffix), 40, TextUnit::Utf16),
            [suffix]
        );
    }

    #[test]
    fn platform_recall_removes_only_a_complete_discord_footer() {
        assert_eq!(
            Footer::without_discord_footer("answer\n\n-# model · 1 step · 7 s"),
            "answer"
        );
        assert_eq!(
            Footer::without_discord_footer("-# model · 1 step · 7 s"),
            ""
        );
        for text in [
            "answer\n\n-# a conclusion",
            "answer\n\n-# model · 1 steps · 7 s",
            "answer\n\n-# model · 1 step · later s",
        ] {
            assert_eq!(Footer::without_discord_footer(text), text);
        }
    }
}
