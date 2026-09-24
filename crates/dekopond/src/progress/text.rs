//! This text carries only operator templates and session counters; a provider-authored command word
//! is the sole outside input, and it is bounded and marker-terminated before use.

use std::time::Duration;

use serde::Deserialize;

use crate::config::TemplateOverrides;

/// This is a rendering bound, not a trust boundary: the word comes from a provider manifest, not a
/// model, and the limit just stops it blowing a transport's message ceiling.
const MAX_WORD_CHARS: usize = 32;
const WORD_MARKER: char = '…';

pub(crate) const DEFAULT_WORKING: &str = "Working on it…";
pub(crate) const DEFAULT_TOOL: &str = "Running {word}…";
pub(crate) const DEFAULT_KEEP_ALIVE: &str = "Still working ({elapsed_s} s)…";

/// The private field makes it a property of the type, not a rule to remember, that no model text
/// ever reaches a progress message; the test-only escape hatch is compiled out of shipped builds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProgressText(String);

impl ProgressText {
    fn new(text: String) -> Self {
        Self(text)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn for_test(text: &str) -> Self {
        Self::new(text.to_owned())
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ProgressDetail {
    Off,
    #[default]
    Plain,
    Detailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TemplateField {
    Working,
    Tool,
    KeepAlive,
    Stopped,
    Failed,
}

impl TemplateField {
    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Tool => "tool",
            Self::KeepAlive => "keepAlive",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }

    const fn placeholders(self) -> &'static [&'static str] {
        const IN_FLIGHT: &[&str] = &["turn", "of", "calls", "calls_max", "elapsed_s"];
        const WITH_WORD: &[&str] = &["word", "turn", "of", "calls", "calls_max", "elapsed_s"];
        match self {
            Self::Working | Self::KeepAlive => IN_FLIGHT,
            Self::Tool => WITH_WORD,
            Self::Stopped | Self::Failed => &[],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TemplateProblem {
    pub field: TemplateField,
    pub placeholder: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RenderState {
    pub turn: u32,
    pub of: u32,
    pub calls: u32,
    pub calls_max: u32,
    pub elapsed: Duration,
    pub word: Option<String>,
}

impl RenderState {
    pub(crate) fn set_word(&mut self, word: &str) {
        let mut bounded: String = word.chars().take(MAX_WORD_CHARS).collect();
        if word.chars().nth(MAX_WORD_CHARS).is_some() {
            bounded.push(WORD_MARKER);
        }
        self.word = Some(bounded);
    }

    fn value(&self, placeholder: &str) -> Option<String> {
        Some(match placeholder {
            "word" => self.word.clone().unwrap_or_default(),
            "turn" => self.turn.to_string(),
            "of" => self.of.to_string(),
            "calls" => self.calls.to_string(),
            "calls_max" => self.calls_max.to_string(),
            "elapsed_s" => self.elapsed.as_secs().to_string(),
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Templates {
    working: String,
    tool: String,
    keep_alive: String,
    stopped: String,
    failed: String,
}

impl Templates {
    pub(crate) fn resolve(
        overrides: &TemplateOverrides,
        stopped_default: &str,
        failed_default: &str,
    ) -> (Self, Vec<TemplateProblem>) {
        let mut problems = Vec::new();
        let templates = Self {
            working: checked(
                TemplateField::Working,
                overrides.working.as_deref(),
                DEFAULT_WORKING,
                &mut problems,
            ),
            tool: checked(
                TemplateField::Tool,
                overrides.tool.as_deref(),
                DEFAULT_TOOL,
                &mut problems,
            ),
            keep_alive: checked(
                TemplateField::KeepAlive,
                overrides.keep_alive.as_deref(),
                DEFAULT_KEEP_ALIVE,
                &mut problems,
            ),
            stopped: checked(
                TemplateField::Stopped,
                overrides.stopped.as_deref(),
                stopped_default,
                &mut problems,
            ),
            failed: checked(
                TemplateField::Failed,
                overrides.failed.as_deref(),
                failed_default,
                &mut problems,
            ),
        };
        (templates, problems)
    }

    pub(crate) fn working(&self, detail: ProgressDetail, state: &RenderState) -> ProgressText {
        self.render(&self.working, detail, state)
    }

    pub(crate) fn tool(&self, detail: ProgressDetail, state: &RenderState) -> ProgressText {
        self.render(&self.tool, detail, state)
    }

    pub(crate) fn keep_alive(&self, detail: ProgressDetail, state: &RenderState) -> ProgressText {
        self.render(&self.keep_alive, detail, state)
    }

    pub(crate) fn stopped(&self) -> &str {
        &self.stopped
    }

    pub(crate) fn failed(&self) -> &str {
        &self.failed
    }

    fn render(&self, template: &str, detail: ProgressDetail, state: &RenderState) -> ProgressText {
        let mut rendered = expand(template, state);
        if detail == ProgressDetail::Detailed {
            rendered.push_str(&format!(
                " · turn {} of {} · {} of {} calls · {} s",
                state.turn,
                state.of,
                state.calls,
                state.calls_max,
                state.elapsed.as_secs()
            ));
        }
        ProgressText::new(rendered)
    }
}

fn checked(
    field: TemplateField,
    authored: Option<&str>,
    fallback: &str,
    problems: &mut Vec<TemplateProblem>,
) -> String {
    let text = authored.unwrap_or(fallback).to_owned();
    problems.extend(unrenderable(field, &text));
    text
}

fn unrenderable(field: TemplateField, template: &str) -> Vec<TemplateProblem> {
    placeholders(template)
        .into_iter()
        .filter(|name| !field.placeholders().contains(&name.as_str()))
        .map(|placeholder| TemplateProblem { field, placeholder })
        .collect()
}

fn placeholders(template: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            break;
        };
        found.push(after[..close].to_owned());
        rest = &after[close + 1..];
    }
    found
}

fn expand(template: &str, state: &RenderState) -> String {
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            break;
        };
        rendered.push_str(&rest[..open]);
        let name = &after[..close];
        match state.value(name) {
            Some(value) => rendered.push_str(&value),
            None => {
                rendered.push('{');
                rendered.push_str(name);
                rendered.push('}');
            }
        }
        rest = &after[close + 1..];
    }
    rendered.push_str(rest);
    rendered
}
