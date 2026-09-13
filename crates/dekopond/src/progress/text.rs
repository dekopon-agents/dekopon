//! What a person waiting on one message may be shown, and the operator strings it is built from.
//!
//! Nothing here can carry a prompt, a script, a capability argument, a provider result, or model
//! text: a [`ProgressText`] is assembled inside this module from an operator's template and the
//! session's own numbers, and the one value that comes from outside — a provider-authored command
//! word — is bounded and marker-terminated before it is written.

use std::time::Duration;

use serde::Deserialize;

use crate::config::TemplateOverrides;

/// Longest provider-authored command word one progress line may show.
///
/// A capability id comes from a provider manifest rather than from a model, so this is a rendering
/// bound rather than a trust boundary: a long word would push the sentence past a transport's own
/// ceiling and cost the edit, not leak anything.
const MAX_WORD_CHARS: usize = 32;
/// What a shortened command word ends with, so a reader knows the line cut it.
const WORD_MARKER: char = '…';

/// The default verb while nothing more specific is known.
pub(crate) const DEFAULT_WORKING: &str = "Working on it…";
/// The default verb while one capability call runs.
pub(crate) const DEFAULT_TOOL: &str = "Running {word}…";
/// The default keep-alive line; its number is fresh by construction, because the only time this
/// renders is on a tick.
pub(crate) const DEFAULT_KEEP_ALIVE: &str = "Still working ({elapsed_s} s)…";

/// Text a progress surface may show.
///
/// Constructed only inside this module in every shipped build, which is what makes "no model text
/// reaches a progress message" a property of the type rather than a rule somebody has to remember.
/// Drivers receive `&ProgressText` and call [`Self::as_str`]; `for_test` is the test-only escape
/// hatch, and it is compiled out of everything that runs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProgressText(String);

impl ProgressText {
    /// Private on purpose: see the type's own documentation.
    fn new(text: String) -> Self {
        Self(text)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// The one way a test outside this module gets a value of this type.
    ///
    /// A driver test has to hand its `ProgressMessage` implementation a line and then assert what
    /// the transport did with it, and rendering one through [`Templates`] there would test the
    /// template engine a second time rather than the driver. Test-only so the production
    /// guarantee — every progress line is assembled here, from operator templates and the
    /// session's own numbers — is unchanged.
    #[cfg(test)]
    pub(crate) fn for_test(text: &str) -> Self {
        Self::new(text.to_owned())
    }
}

/// How much a route's progress surface says.
///
/// Chosen per agent because the same event stream serves a family Discord and an operations
/// channel, and the numbers that help the second are noise in the first. Every driver renders
/// every level; a level a transport cannot show is a no-op there rather than an error.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ProgressDetail {
    /// Typing, native status, and the reaction only: no message is posted or edited.
    Off,
    /// Verbs only, with elapsed seconds on keep-alive ticks where the number is fresh.
    #[default]
    Plain,
    /// The verbs plus turn, capability-call, and elapsed counters on every edit.
    Detailed,
}

/// Which template one string is.
///
/// Named rather than positional so a refusal says which line an operator has to fix, and so the
/// allowed placeholders are stated once per field instead of once per validation site.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TemplateField {
    Working,
    Tool,
    KeepAlive,
    Stopped,
    Failed,
}

impl TemplateField {
    /// The authored key this field is written under.
    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Tool => "tool",
            Self::KeepAlive => "keepAlive",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }

    /// Placeholders this field can actually render.
    ///
    /// A terminal line renders none: it is written after the session stopped, where a turn counter
    /// describes nothing a reader can act on.
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

/// One template an operator wrote that this daemon cannot render.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TemplateProblem {
    pub field: TemplateField,
    /// The placeholder as written, without its braces.
    pub placeholder: String,
}

/// The numbers a progress line may be built from.
///
/// Deliberately a plain record of counters: there is no field a prompt, an argument, or a provider
/// result could be put in.
#[derive(Clone, Debug, Default)]
pub(crate) struct RenderState {
    pub turn: u32,
    pub of: u32,
    pub calls: u32,
    pub calls_max: u32,
    pub elapsed: Duration,
    /// The capability whose call is running, already bounded for display.
    pub word: Option<String>,
}

impl RenderState {
    /// Records the running capability, bounded and marker-terminated.
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

/// Operator strings for everything a progress surface says.
///
/// Defaults ship in the binary, so a deployment that writes no `templates:` block gets the
/// sentences below and an operator who wants their agent to speak differently overrides one field
/// without restating the rest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Templates {
    working: String,
    tool: String,
    keep_alive: String,
    stopped: String,
    failed: String,
}

impl Templates {
    /// Builds one set from the authored overrides, reporting **every** unrenderable placeholder.
    ///
    /// Every problem rather than the first, because an operator who rewrote all five lines against
    /// the wrong placeholder names should fix five and restart once. The templates come back even
    /// when problems do — an unrenderable placeholder is a literal, not a parse failure — and the
    /// configuration they belong to is refused by the caller that collected the problems.
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

    /// The line for a session that is working with no capability call in flight.
    pub(crate) fn working(&self, detail: ProgressDetail, state: &RenderState) -> ProgressText {
        self.render(&self.working, detail, state)
    }

    /// The line for a session inside one capability call.
    pub(crate) fn tool(&self, detail: ProgressDetail, state: &RenderState) -> ProgressText {
        self.render(&self.tool, detail, state)
    }

    /// The line one keep-alive tick writes.
    pub(crate) fn keep_alive(&self, detail: ProgressDetail, state: &RenderState) -> ProgressText {
        self.render(&self.keep_alive, detail, state)
    }

    /// The fixed sentence a cancelled session ends with.
    pub(crate) fn stopped(&self) -> &str {
        &self.stopped
    }

    /// The fixed sentence a failed session ends with.
    pub(crate) fn failed(&self) -> &str {
        &self.failed
    }

    /// Appends the counters at `detailed`, and nothing at `plain`.
    ///
    /// The counters are gateway-authored rather than templated because they are route budgets with
    /// one true rendering; an operator chooses *whether* to see them, not how they are spelled.
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

/// Takes the authored line or this daemon's own, recording every placeholder it cannot render.
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

/// Every `{placeholder}` in `template` that `field` has no value for.
fn unrenderable(field: TemplateField, template: &str) -> Vec<TemplateProblem> {
    placeholders(template)
        .into_iter()
        .filter(|name| !field.placeholders().contains(&name.as_str()))
        .map(|placeholder| TemplateProblem { field, placeholder })
        .collect()
}

/// The `{name}` tokens in one template, in the order they appear.
///
/// An unterminated `{` is not a placeholder and is left alone: it is a brace the operator wrote,
/// and refusing a configuration over one would be refusing a literal.
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

/// Substitutes every placeholder this state has a value for.
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
            // Validation refused every placeholder no field can render, so this is a brace pair an
            // operator wrote as literal text.
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
