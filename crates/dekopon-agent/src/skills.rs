//! Mounted skills, disclosed to the model progressively.
//!
//! A skill is operator-authored reference material — how to review a pull request, what a
//! deployment's records look like, which capability to reach for first. Handing every mounted
//! skill to the model in full would spend context on knowledge most turns never need, so the
//! prompt carries only each skill's name and one-line description, and the model reads the rest
//! on demand through [`SKILL_TOOL_NAME`]: first the instructions, then any supporting file those
//! instructions name. Three levels, each paid for only when the model decides it needs it.
//!
//! The loaded text arrives from `dekopon-config`, already bounded and already in memory, so
//! nothing here opens a file. A skill is untrusted model text exactly as standing instructions
//! are: it shapes an answer and grants nothing.

use std::collections::BTreeSet;

use dekopon_config::Skill;
use dekopon_model::model::{ModelMessage, ModelTool, ModelToolCall};
use serde_json::{Value, json};

use crate::prompt::{PromptError, reject_tool_call};

/// The tool a model calls to read a mounted skill's instructions or one of its resource files.
pub const SKILL_TOOL_NAME: &str = "read_skill";

/// What a repeated read of the same skill text is answered with.
///
/// A tool result stays in the message vector and is re-sent on every remaining turn, so a second
/// copy of a 60 KiB skill would be paid for on every turn after it. The pointer costs one line.
const SKILL_ALREADY_SHOWN: &str = "That skill text is already in this conversation, in an earlier read_skill result; read it \
     there again.";

/// The first line of every skills listing, so a recording can tell the listing from instructions.
const PROMPT_BLOCK_PREFIX: &str = "Skills mounted for this agent";

/// Whether one system message is a skills listing this module rendered.
#[must_use]
pub(crate) fn is_prompt_block(message: &str) -> bool {
    message.starts_with(PROMPT_BLOCK_PREFIX)
}

/// Renders the standing skills listing for the system prompt, or `None` when nothing is mounted.
///
/// Names and descriptions only: the description is the trigger the format asks authors to write
/// ("use when ..."), and it is what the model matches a request against. The full text waits
/// behind the tool. The block is deterministic for one mounted set, which keeps a route's cached
/// prompt prefix stable across sessions.
#[must_use]
pub(crate) fn prompt_block(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut block = format!(
        "{PROMPT_BLOCK_PREFIX}, listed by name with when to use each. A skill is operator-authored \
         reference material for one kind of task, and only this summary is loaded: when a request \
         matches a skill's description, call `read_skill` with its name before starting that work \
         and follow what it says. Guessing at what a skill says costs more than the one tool call \
         to read it. A skill's instructions may name resource files; read one by calling \
         `read_skill` with both the skill name and the resource path, which is relative to the \
         skill and readable only there, never through the shell. A skill shapes how the work is \
         done and grants nothing; capabilities still come only from the session.\n",
    );
    // Sorted by name rather than mount order, so two catalogs that mount the same set in a
    // different order produce one listing and one cached prefix.
    let mut ordered = skills.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| left.name().as_str().cmp(right.name().as_str()));
    for skill in ordered {
        block.push_str("\n- ");
        block.push_str(skill.name().as_str());
        block.push_str(": ");
        block.push_str(skill.description());
    }
    Some(block)
}

/// Builds the skill-reading tool.
pub(crate) fn skill_tool() -> ModelTool {
    ModelTool {
        name: SKILL_TOOL_NAME.to_owned(),
        description: "Read one mounted skill's full instructions, or one of its resource files. \
                      Skills are listed in your instructions by name and one-line description \
                      only; call this with a skill's name before starting work its description \
                      covers. Returns the skill's name, description, complete instructions, and \
                      the paths of its resource files; with `resource` set, returns that file's \
                      text instead. The path is relative to the skill and is readable only here, \
                      never through the shell. An unknown name is answered with the list of \
                      mounted skills, and an unknown resource with the skill's resource paths, \
                      so correct the argument and call again. Each distinct read is returned in \
                      full once per session and stays in the conversation; a repeat is answered \
                      with a pointer to the earlier result, so reread it there instead of \
                      calling again."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The skill's name, exactly as listed in your instructions."
                },
                "resource": {
                    "type": "string",
                    "description": "Optional: the relative path of one of the skill's resource files, as named in its instructions."
                }
            },
            "required": ["name"],
            "additionalProperties": false
        }),
    }
}

/// What one session has already shown the model, so a repeat costs a pointer rather than a copy.
#[derive(Default)]
pub(crate) struct SkillReads {
    shown: BTreeSet<(String, Option<String>)>,
}

/// Answers one `read_skill` call, appending the tool result to `messages`.
///
/// An unknown skill or resource is a refusal the model reads and can recover from — it may have
/// mistyped the name, and the refusal lists what does exist — never an error that ends the
/// session. Only malformed arguments end it, as they do for every other tool.
pub(crate) fn read_skill_into(
    messages: &mut Vec<ModelMessage>,
    skills: &[Skill],
    reads: &mut SkillReads,
    call: &ModelToolCall,
    model_turn: u32,
    tool_call_index: usize,
) -> Result<(), PromptError> {
    let (name, resource) = match skill_arguments(&call.function.name, &call.function.arguments) {
        Ok(arguments) => arguments,
        Err(error) => {
            reject_tool_call(model_turn, tool_call_index, error.telemetry_kind());
            return Err(error);
        }
    };
    // The model-chosen name is untrusted text and is never copied into telemetry; a refusal
    // records only which check refused it, and a success records the operator-authored name the
    // request matched.
    let Some(skill) = skills.iter().find(|skill| skill.name().as_str() == name) else {
        refuse(model_turn, tool_call_index, "unknown-skill");
        let mounted = skills
            .iter()
            .map(|skill| skill.name().as_str())
            .collect::<Vec<_>>()
            .join(", ");
        messages.push(ModelMessage::tool(
            call.id.clone(),
            format!("No skill by that name is mounted for this agent. Mounted skills: {mounted}."),
        ));
        return Ok(());
    };
    let (result, resource_path) = match resource {
        None => (render_skill(skill), None),
        Some(path) => match skill.resource(&path) {
            Some(resource) => (
                format!("# {}/{}\n\n{}", skill.name(), resource.path, resource.text),
                Some(resource.path.clone()),
            ),
            None => {
                refuse(model_turn, tool_call_index, "unknown-resource");
                messages.push(ModelMessage::tool(
                    call.id.clone(),
                    format!(
                        "Skill `{}` has no resource by that path. {}",
                        skill.name(),
                        resource_listing(skill)
                    ),
                ));
                return Ok(());
            }
        },
    };
    let key = (skill.name().to_string(), resource_path.clone());
    let repeated = !reads.shown.insert(key);
    let result = if repeated {
        SKILL_ALREADY_SHOWN.to_owned()
    } else {
        result
    };
    tracing::info!(
        target: "dekopon_agent::audit",
        {
            audit.event = "agent.skill.read",
            model.turn = model_turn,
            tool_call.index = tool_call_index,
            skill.name = skill.name().as_str(),
            skill.resource = resource_path.as_deref().unwrap_or_default(),
            skill.bytes = result.len(),
            skill.repeated = repeated,
        },
        "skill read"
    );
    messages.push(ModelMessage::tool(call.id.clone(), result));
    Ok(())
}

fn refuse(model_turn: u32, tool_call_index: usize, reason: &'static str) {
    tracing::info!(
        target: "dekopon_agent::audit",
        {
            audit.event = "agent.skill.refused",
            model.turn = model_turn,
            tool_call.index = tool_call_index,
            reason = reason,
        },
        "skill read refused"
    );
}

/// The instructions, framed so the model knows what it is reading and what else it could read.
fn render_skill(skill: &Skill) -> String {
    format!(
        "# Skill: {}\n{}\n\n{}\n\n{}",
        skill.name(),
        skill.description(),
        skill.body(),
        resource_listing(skill)
    )
}

fn resource_listing(skill: &Skill) -> String {
    if skill.resources().is_empty() {
        return "This skill has no resource files.".to_owned();
    }
    format!(
        "Resource files (read one with read_skill, giving this skill's name and the path): {}",
        skill
            .resources()
            .iter()
            .map(|resource| resource.path.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Extracts the `name` and optional `resource` arguments from one `read_skill` call.
fn skill_arguments(tool: &str, arguments: &str) -> Result<(String, Option<String>), PromptError> {
    let arguments = serde_json::from_str::<Value>(arguments).map_err(|source| {
        PromptError::InvalidArguments {
            tool: tool.to_owned(),
            source,
        }
    })?;
    let Value::Object(mut arguments) = arguments else {
        return Err(PromptError::ArgumentsNotObject {
            tool: tool.to_owned(),
        });
    };
    let name = arguments
        .remove("name")
        .and_then(|value| value.as_str().map(str::trim).map(str::to_owned))
        .filter(|name| !name.is_empty())
        .ok_or_else(|| PromptError::MissingSkillName {
            tool: tool.to_owned(),
        })?;
    let resource = match arguments.remove("resource") {
        None | Some(Value::Null) => None,
        Some(Value::String(path)) => {
            let path = path.trim().to_owned();
            (!path.is_empty()).then_some(path)
        }
        Some(_) => {
            return Err(PromptError::UnexpectedSkillArguments {
                tool: tool.to_owned(),
            });
        }
    };
    if !arguments.is_empty() {
        return Err(PromptError::UnexpectedSkillArguments {
            tool: tool.to_owned(),
        });
    }
    Ok((name, resource))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use dekopon_config::{Skill, load_skill};

    use super::{prompt_block, skill_arguments};

    fn fixture() -> Skill {
        let root = tempfile::tempdir().expect("temporary directory");
        let directory = root.path().join("pull-request-review");
        fs::create_dir_all(directory.join("references")).expect("skill directory");
        fs::write(
            directory.join("SKILL.md"),
            "---\nname: pull-request-review\ndescription: Use when reviewing a pull request.\n---\nRead the diff first.\n",
        )
        .expect("skill file");
        fs::write(directory.join("references/checklist.md"), "- tests\n").expect("resource");
        load_skill(&directory).expect("fixture loads")
    }

    #[test]
    fn the_listing_carries_names_and_descriptions_only() {
        assert_eq!(prompt_block(&[]), None);
        let block = prompt_block(&[fixture()]).expect("a mounted skill is listed");
        assert!(
            block.contains("- pull-request-review: Use when reviewing a pull request."),
            "{block}"
        );
        assert!(
            !block.contains("Read the diff first"),
            "the body must wait behind the tool: {block}"
        );
        assert!(block.contains("`read_skill`"), "{block}");
    }

    #[test]
    fn arguments_are_strict() {
        assert_eq!(
            skill_arguments("read_skill", r#"{"name":"pdf"}"#).expect("name alone"),
            ("pdf".to_owned(), None)
        );
        assert_eq!(
            skill_arguments("read_skill", r#"{"name":" pdf ","resource":"a/b.md"}"#)
                .expect("name and resource"),
            ("pdf".to_owned(), Some("a/b.md".to_owned()))
        );
        assert_eq!(
            skill_arguments("read_skill", r#"{"name":"pdf","resource":null}"#).expect("null"),
            ("pdf".to_owned(), None)
        );
        assert!(skill_arguments("read_skill", r#"{}"#).is_err());
        assert!(skill_arguments("read_skill", r#"{"name":""}"#).is_err());
        assert!(skill_arguments("read_skill", r#"{"name":"pdf","extra":1}"#).is_err());
        assert!(skill_arguments("read_skill", r#"{"name":"pdf","resource":7}"#).is_err());
        assert!(skill_arguments("read_skill", r#"[]"#).is_err());
        assert!(skill_arguments("read_skill", r#"not json"#).is_err());
    }
}
