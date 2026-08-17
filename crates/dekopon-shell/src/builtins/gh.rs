//! The `gh` builtin: GitHub-CLI-shaped subcommands over granted `gh.*` capabilities.
//!
//! Like `curl`, this is a flag parser, not a client. It owns a fixed subcommand vocabulary with
//! `gh`-CLI spellings, maps each one onto exactly one `gh.*` capability, and hands the assembled
//! input to [`BuiltinContext::invoke_capability`] — the same path `cap <id> {...}` takes, with the
//! same budget, deadline, denial, and telemetry behavior. Which of those capabilities exist is a
//! policy decision made elsewhere; a subcommand whose capability is not granted in this session
//! reports the missing capability by name with exit code 127 rather than guessing.
//!
//! Flags that would change what a command *means* — `--json`, `--jq`, `--web`, `--checkout` — are
//! rejected by name instead of accepted as no-ops, exactly as `curl` rejects `-L`. Output is
//! always the capability's structured JSON value; filter it with the `jq` builtin.
//!
//! The builtin is sugar only. Every `gh.*` capability remains directly invocable as a command word
//! (`gh.pull-request.read --owner o --repo r --number 7`) with no builtin involved.

use serde_json::{Map, Value};

use super::{Builtin, BuiltinContext, CommandFailure, CommandResult, unsupported_flag};

/// The GitHub subcommand surface.
pub(crate) struct Gh;

const USAGE: &str = "gh: usage: gh <pr|repo|content|issue|branch|commit|user> <subcommand> \
                     [arguments]; every command maps to one granted gh.* capability";

impl Builtin for Gh {
    fn name(&self) -> &'static str {
        "gh"
    }

    fn run(
        &self,
        context: &mut BuiltinContext<'_>,
        arguments: &[String],
        _input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let Some((area, rest)) = arguments.split_first() else {
            return Err(CommandFailure::usage(USAGE));
        };
        if area == "api" {
            // Deliberate refusal rather than an unimplemented gap: a path-level passthrough would
            // collapse per-capability policy into "everything the credential can reach".
            return Err(CommandFailure::usage(
                "gh: `gh api` is not available: raw API passthrough would bypass per-capability \
                 authorization; use the granted gh.* capabilities (see `cap --list`)",
            ));
        }
        let Some((verb, rest)) = rest.split_first() else {
            return Err(CommandFailure::usage(USAGE));
        };
        let options = Options::parse(rest)?;

        let call = build_call(area, verb, options)?;
        let missing = call
            .requires
            .iter()
            .find(|capability| !context.invoker.is_granted(capability));
        if let Some(capability) = missing {
            // The same exit code an unknown command word gets, but with the exact capability an
            // operator must grant, so "not configured" is never mistaken for "does not exist".
            return Err(CommandFailure::Status {
                message: format!(
                    "gh: {area} {verb} requires capability {capability}, which is not granted in \
                     this session"
                ),
                status: crate::ExitCode::NOT_FOUND,
            });
        }
        context.invoke_capability(call.requires[0], Value::Object(call.input))
    }
}

/// One resolved subcommand: the capability it invokes and the input it assembled.
struct Call {
    /// The capability this subcommand dispatches to; index 0 is invoked.
    requires: &'static [&'static str],
    input: Map<String, Value>,
}

/// Which review event `gh pr review` submits. Exactly one must be chosen.
#[derive(Clone, Copy, Eq, PartialEq)]
enum ReviewEvent {
    Approve,
    Comment,
    RequestChanges,
}

/// The complete flag vocabulary, parsed before any subcommand logic runs.
///
/// Parsing everything up front keeps rejection messages uniform, and each subcommand then refuses
/// the flags that do not apply to it — a flag silently ignored is a flag that lied.
#[derive(Default)]
struct Options {
    repo: Option<(String, String)>,
    body: Option<String>,
    state: Option<String>,
    author: Option<String>,
    page: Option<u64>,
    per_page: Option<u64>,
    git_ref: Option<String>,
    expected_head_sha: Option<String>,
    no_patch: bool,
    review_event: Option<ReviewEvent>,
    merge_method: Option<&'static str>,
    positionals: Vec<String>,
}

/// Flags real `gh` accepts that this builtin refuses by name, with the reason.
const REJECTED_FLAGS: &[(&str, &str)] = &[
    ("--web", "there is no browser to open"),
    (
        "--json",
        "output is always a structured JSON value already; filter it with the jq builtin",
    ),
    ("--jq", "pipe the output to the jq builtin instead"),
    ("--template", "format the JSON output with jq instead"),
    ("--checkout", "there is no working tree to check out into"),
    ("--editor", "there is no editor; pass text with --body"),
    ("--fill", "there is no commit context to fill from"),
];

impl Options {
    fn parse(arguments: &[String]) -> Result<Self, CommandFailure> {
        let mut options = Self::default();
        let mut index = 0;
        while index < arguments.len() {
            let argument = arguments[index].as_str();
            if let Some((flag, reason)) = REJECTED_FLAGS
                .iter()
                .find(|(flag, _)| argument == *flag || argument.starts_with(&format!("{flag}=")))
            {
                return Err(CommandFailure::usage(format!(
                    "gh: {flag} is not supported: {reason}"
                )));
            }
            match argument {
                "-R" | "--repo" => {
                    let value = take_value(arguments, &mut index, argument)?;
                    options.repo = Some(parse_repo(&value)?);
                }
                "-b" | "--body" => {
                    options.body = Some(take_value(arguments, &mut index, argument)?);
                }
                "--state" => {
                    options.state = Some(take_value(arguments, &mut index, argument)?);
                }
                "--author" => {
                    options.author = Some(take_value(arguments, &mut index, argument)?);
                }
                "--page" => {
                    options.page = Some(take_number(arguments, &mut index, argument)?);
                }
                "--per-page" => {
                    options.per_page = Some(take_number(arguments, &mut index, argument)?);
                }
                "--ref" => {
                    options.git_ref = Some(take_value(arguments, &mut index, argument)?);
                }
                "--expected-head-sha" => {
                    options.expected_head_sha = Some(take_value(arguments, &mut index, argument)?);
                }
                "--no-patch" => {
                    options.no_patch = true;
                    index += 1;
                }
                "--approve" => {
                    options.set_review_event(ReviewEvent::Approve)?;
                    index += 1;
                }
                "--comment" => {
                    options.set_review_event(ReviewEvent::Comment)?;
                    index += 1;
                }
                "--request-changes" => {
                    options.set_review_event(ReviewEvent::RequestChanges)?;
                    index += 1;
                }
                "--squash" | "--merge" | "--rebase" => {
                    let method = &argument[2..];
                    if options.merge_method.is_some() {
                        return Err(CommandFailure::usage(
                            "gh: choose exactly one of --squash, --merge, or --rebase",
                        ));
                    }
                    options.merge_method = Some(match method {
                        "squash" => "squash",
                        "rebase" => "rebase",
                        _ => "merge",
                    });
                    index += 1;
                }
                flag if flag.starts_with('-') && flag.len() > 1 => {
                    return Err(unsupported_flag("gh", flag));
                }
                positional => {
                    options.positionals.push(positional.to_owned());
                    index += 1;
                }
            }
        }
        Ok(options)
    }

    fn set_review_event(&mut self, event: ReviewEvent) -> Result<(), CommandFailure> {
        if self.review_event.is_some() {
            return Err(CommandFailure::usage(
                "gh: choose exactly one of --approve, --comment, or --request-changes",
            ));
        }
        self.review_event = Some(event);
        Ok(())
    }

    /// Consumes the repository, which every repository-scoped subcommand requires explicitly.
    ///
    /// There is no git working tree here, so nothing can be inferred: a missing `-R` is a usage
    /// error naming the exact form, never a guess.
    fn require_repo(&mut self, command: &str) -> Result<(String, String), CommandFailure> {
        self.repo.take().ok_or_else(|| {
            CommandFailure::usage(format!(
                "gh: {command} requires -R owner/repo; there is no repository context to infer"
            ))
        })
    }

    /// Consumes the single expected positional argument.
    fn require_positional(&mut self, command: &str, what: &str) -> Result<String, CommandFailure> {
        if self.positionals.len() > 1 {
            return Err(CommandFailure::usage(format!(
                "gh: {command} takes exactly one {what} argument"
            )));
        }
        self.positionals.pop().ok_or_else(|| {
            CommandFailure::usage(format!("gh: {command} requires a {what} argument"))
        })
    }

    fn reject_positionals(&self, command: &str) -> Result<(), CommandFailure> {
        if let Some(extra) = self.positionals.first() {
            return Err(CommandFailure::usage(format!(
                "gh: {command} takes no positional argument {extra:?}"
            )));
        }
        Ok(())
    }
}

/// Builds the capability call for one `gh <area> <verb>` spelling.
#[allow(clippy::too_many_lines)]
fn build_call(area: &str, verb: &str, mut options: Options) -> Result<Call, CommandFailure> {
    let command = format!("{area} {verb}");
    let mut input = Map::new();

    let call = match (area, verb) {
        ("pr", "list") => {
            options.reject_positionals(&command)?;
            let (owner, repo) = options.require_repo(&command)?;
            insert_repo(&mut input, owner, repo);
            insert_optional_text(&mut input, "state", options.state.take());
            insert_optional_text(&mut input, "author", options.author.take());
            insert_paging(&mut input, &mut options);
            Call {
                requires: &["gh.pull-request.list"],
                input,
            }
        }
        ("pr", "view") => {
            let number = require_number(&mut options, &command)?;
            let (owner, repo) = options.require_repo(&command)?;
            insert_repo(&mut input, owner, repo);
            input.insert("number".to_owned(), Value::from(number));
            Call {
                requires: &["gh.pull-request.read"],
                input,
            }
        }
        ("pr", "files") => {
            let number = require_number(&mut options, &command)?;
            let (owner, repo) = options.require_repo(&command)?;
            insert_repo(&mut input, owner, repo);
            input.insert("number".to_owned(), Value::from(number));
            insert_paging(&mut input, &mut options);
            if options.no_patch {
                options.no_patch = false;
                input.insert("includePatch".to_owned(), Value::Bool(false));
            }
            Call {
                requires: &["gh.pull-request.files"],
                input,
            }
        }
        ("pr", "diff") => {
            let number = require_number(&mut options, &command)?;
            let (owner, repo) = options.require_repo(&command)?;
            insert_repo(&mut input, owner, repo);
            input.insert("number".to_owned(), Value::from(number));
            Call {
                requires: &["gh.pull-request.diff"],
                input,
            }
        }
        // `status` is the primary spelling; `checks` is accepted because it is what real `gh`
        // calls the same question and both dispatch to the identical capability.
        ("pr", "status" | "checks") => {
            let number = require_number(&mut options, &command)?;
            let (owner, repo) = options.require_repo(&command)?;
            insert_repo(&mut input, owner, repo);
            input.insert("number".to_owned(), Value::from(number));
            Call {
                requires: &["gh.pull-request.status"],
                input,
            }
        }
        ("pr", "reviews") => {
            let number = require_number(&mut options, &command)?;
            let (owner, repo) = options.require_repo(&command)?;
            insert_repo(&mut input, owner, repo);
            input.insert("number".to_owned(), Value::from(number));
            Call {
                requires: &["gh.pull-request.reviews"],
                input,
            }
        }
        ("pr", "review") => {
            let number = require_number(&mut options, &command)?;
            let (owner, repo) = options.require_repo(&command)?;
            let Some(event) = options.review_event.take() else {
                return Err(CommandFailure::usage(
                    "gh: pr review requires exactly one of --approve, --comment, or \
                     --request-changes",
                ));
            };
            insert_repo(&mut input, owner, repo);
            input.insert("number".to_owned(), Value::from(number));
            let capability: &'static [&'static str] = match event {
                ReviewEvent::Approve => {
                    insert_optional_text(&mut input, "body", options.body.take());
                    insert_optional_text(
                        &mut input,
                        "expectedHeadSha",
                        options.expected_head_sha.take(),
                    );
                    &["gh.pull-request.approve"]
                }
                ReviewEvent::Comment => {
                    input.insert(
                        "body".to_owned(),
                        Value::String(require_body(&mut options, "--comment")?),
                    );
                    &["gh.pull-request.comment"]
                }
                ReviewEvent::RequestChanges => {
                    input.insert(
                        "body".to_owned(),
                        Value::String(require_body(&mut options, "--request-changes")?),
                    );
                    &["gh.pull-request.request-changes"]
                }
            };
            Call {
                requires: capability,
                input,
            }
        }
        ("pr", "merge") => {
            let number = require_number(&mut options, &command)?;
            let (owner, repo) = options.require_repo(&command)?;
            insert_repo(&mut input, owner, repo);
            input.insert("number".to_owned(), Value::from(number));
            if let Some(method) = options.merge_method.take() {
                input.insert("mergeMethod".to_owned(), Value::String(method.to_owned()));
            }
            insert_optional_text(
                &mut input,
                "expectedHeadSha",
                options.expected_head_sha.take(),
            );
            Call {
                requires: &["gh.pull-request.merge"],
                input,
            }
        }
        ("repo", "view") => {
            // `gh repo view owner/repo` is the natural spelling; `-R` works too.
            let (owner, repo) = if let Some(repo) = options.repo.take() {
                options.reject_positionals(&command)?;
                repo
            } else {
                parse_repo(&options.require_positional(&command, "owner/repo")?)?
            };
            insert_repo(&mut input, owner, repo);
            Call {
                requires: &["gh.repo.read"],
                input,
            }
        }
        ("content", "view") => {
            let path = options.require_positional(&command, "path")?;
            let (owner, repo) = options.require_repo(&command)?;
            insert_repo(&mut input, owner, repo);
            input.insert("path".to_owned(), Value::String(path));
            insert_optional_text(&mut input, "ref", options.git_ref.take());
            Call {
                requires: &["gh.content.read"],
                input,
            }
        }
        ("issue", "view") => {
            let number = require_number(&mut options, &command)?;
            let (owner, repo) = options.require_repo(&command)?;
            insert_repo(&mut input, owner, repo);
            input.insert("number".to_owned(), Value::from(number));
            Call {
                requires: &["gh.issue.read"],
                input,
            }
        }
        ("issue", "list") => {
            options.reject_positionals(&command)?;
            let (owner, repo) = options.require_repo(&command)?;
            insert_repo(&mut input, owner, repo);
            insert_optional_text(&mut input, "state", options.state.take());
            insert_paging(&mut input, &mut options);
            Call {
                requires: &["gh.issue.list"],
                input,
            }
        }
        ("issue", "comments") => {
            let number = require_number(&mut options, &command)?;
            let (owner, repo) = options.require_repo(&command)?;
            insert_repo(&mut input, owner, repo);
            input.insert("number".to_owned(), Value::from(number));
            Call {
                requires: &["gh.issue-comments.read"],
                input,
            }
        }
        ("issue", "comment") => {
            let number = require_number(&mut options, &command)?;
            let (owner, repo) = options.require_repo(&command)?;
            insert_repo(&mut input, owner, repo);
            input.insert("number".to_owned(), Value::from(number));
            input.insert(
                "body".to_owned(),
                Value::String(require_body(&mut options, "issue comment")?),
            );
            Call {
                requires: &["gh.issue.comment"],
                input,
            }
        }
        ("branch", "view") => {
            let branch = options.require_positional(&command, "branch")?;
            let (owner, repo) = options.require_repo(&command)?;
            insert_repo(&mut input, owner, repo);
            input.insert("branch".to_owned(), Value::String(branch));
            Call {
                requires: &["gh.branch.read"],
                input,
            }
        }
        ("commit", "view") => {
            let reference = options.require_positional(&command, "ref")?;
            let (owner, repo) = options.require_repo(&command)?;
            insert_repo(&mut input, owner, repo);
            input.insert("ref".to_owned(), Value::String(reference));
            Call {
                requires: &["gh.commit.read"],
                input,
            }
        }
        ("user", "view") => {
            let login = options.require_positional(&command, "login")?;
            input.insert("login".to_owned(), Value::String(login));
            Call {
                requires: &["gh.user.read"],
                input,
            }
        }
        _ => {
            return Err(CommandFailure::usage(format!(
                "gh: unknown command {command:?}; supported: pr \
                 list|view|files|diff|status|reviews|review|merge, repo view, content view, issue \
                 view|list|comments|comment, branch view, commit view, user view"
            )));
        }
    };

    reject_leftovers(&command, &options)?;
    Ok(call)
}

/// Refuses any parsed flag the chosen subcommand did not consume.
///
/// Accepting `gh pr diff 7 -R o/r --approve` while ignoring `--approve` would let a script believe
/// it reviewed something. Every consumed option was `take()`n out, so anything still present is a
/// flag this subcommand does not have.
fn reject_leftovers(command: &str, options: &Options) -> Result<(), CommandFailure> {
    let leftover = [
        (options.body.is_some(), "--body"),
        (options.state.is_some(), "--state"),
        (options.author.is_some(), "--author"),
        (options.page.is_some(), "--page"),
        (options.per_page.is_some(), "--per-page"),
        (options.git_ref.is_some(), "--ref"),
        (options.expected_head_sha.is_some(), "--expected-head-sha"),
        (options.no_patch, "--no-patch"),
        (options.review_event.is_some(), "a review event flag"),
        (options.merge_method.is_some(), "a merge method flag"),
        (options.repo.is_some(), "-R"),
    ]
    .into_iter()
    .find_map(|(present, flag)| present.then_some(flag));
    if let Some(flag) = leftover {
        return Err(CommandFailure::usage(format!(
            "gh: {command} does not accept {flag}"
        )));
    }
    Ok(())
}

fn insert_repo(input: &mut Map<String, Value>, owner: String, repo: String) {
    input.insert("owner".to_owned(), Value::String(owner));
    input.insert("repo".to_owned(), Value::String(repo));
}

fn insert_optional_text(input: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        input.insert(key.to_owned(), Value::String(value));
    }
}

fn insert_paging(input: &mut Map<String, Value>, options: &mut Options) {
    if let Some(page) = options.page.take() {
        input.insert("page".to_owned(), Value::from(page));
    }
    if let Some(per_page) = options.per_page.take() {
        input.insert("perPage".to_owned(), Value::from(per_page));
    }
}

fn require_number(options: &mut Options, command: &str) -> Result<u64, CommandFailure> {
    let positional = options.require_positional(command, "number")?;
    let number = positional.parse::<u64>().map_err(|_| {
        CommandFailure::usage(format!(
            "gh: {command}: {positional:?} is not a pull-request or issue number"
        ))
    })?;
    if number == 0 {
        return Err(CommandFailure::usage(format!(
            "gh: {command}: numbers start at 1"
        )));
    }
    Ok(number)
}

fn require_body(options: &mut Options, what: &str) -> Result<String, CommandFailure> {
    options
        .body
        .take()
        .ok_or_else(|| CommandFailure::usage(format!("gh: {what} requires --body text")))
}

/// Splits `owner/repo`, structurally only; deeper grammar checks belong to the provider.
fn parse_repo(value: &str) -> Result<(String, String), CommandFailure> {
    let mut parts = value.splitn(2, '/');
    let owner = parts.next().unwrap_or_default();
    let repo = parts.next().unwrap_or_default();
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return Err(CommandFailure::usage(format!(
            "gh: repository {value:?} must be formatted as owner/repo"
        )));
    }
    Ok((owner.to_owned(), repo.to_owned()))
}

fn take_value(
    arguments: &[String],
    index: &mut usize,
    flag: &str,
) -> Result<String, CommandFailure> {
    let Some(value) = arguments.get(*index + 1) else {
        return Err(CommandFailure::usage(format!(
            "gh: {flag} requires a value"
        )));
    };
    *index += 2;
    Ok(value.clone())
}

fn take_number(arguments: &[String], index: &mut usize, flag: &str) -> Result<u64, CommandFailure> {
    let value = take_value(arguments, index, flag)?;
    value.parse::<u64>().map_err(|_| {
        CommandFailure::usage(format!(
            "gh: {flag} requires a positive number, not {value:?}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{
        CapabilityCallResult, CapabilityInvoker, ExitCode,
        builtins::{CommandFailure, test_support::run_builtin_with_invoker},
    };

    use super::Gh;

    /// Grants the full gh surface and records every dispatch.
    #[derive(Default)]
    struct RecordingInvoker {
        calls: std::cell::RefCell<Vec<(String, Value)>>,
    }

    const GRANTED: &[&str] = &[
        "gh.content.read",
        "gh.pull-request.list",
        "gh.pull-request.read",
        "gh.pull-request.files",
        "gh.pull-request.diff",
        "gh.pull-request.status",
        "gh.pull-request.reviews",
        "gh.pull-request.approve",
        "gh.pull-request.comment",
        "gh.pull-request.request-changes",
        "gh.pull-request.merge",
        "gh.repo.read",
        "gh.branch.read",
        "gh.commit.read",
        "gh.issue.read",
        "gh.issue.list",
        "gh.issue-comments.read",
        "gh.issue.comment",
        "gh.user.read",
    ];

    impl CapabilityInvoker for RecordingInvoker {
        fn granted(&self) -> Vec<String> {
            GRANTED.iter().map(|it| (*it).to_owned()).collect()
        }

        fn invoke(&self, capability: &str, input: Value) -> CapabilityCallResult {
            self.calls
                .borrow_mut()
                .push((capability.to_owned(), input.clone()));
            CapabilityCallResult::Succeeded(json!({"ok": true}))
        }
    }

    fn dispatch(arguments: &[&str]) -> (String, Value) {
        let invoker = RecordingInvoker::default();
        run_builtin_with_invoker(&Gh, arguments, &invoker).expect("gh dispatches");
        let calls = invoker.calls.borrow();
        assert_eq!(calls.len(), 1, "exactly one capability call");
        calls[0].clone()
    }

    fn refuse(arguments: &[&str]) -> CommandFailure {
        let invoker = RecordingInvoker::default();
        let failure =
            run_builtin_with_invoker(&Gh, arguments, &invoker).expect_err("gh must refuse");
        assert!(
            invoker.calls.borrow().is_empty(),
            "a refused command must not dispatch"
        );
        failure
    }

    #[test]
    fn every_subcommand_maps_to_its_capability_and_input() {
        let cases: &[(&[&str], &str, Value)] = &[
            (
                &[
                    "pr",
                    "list",
                    "-R",
                    "o/r",
                    "--state",
                    "open",
                    "--author",
                    "cpetersen",
                ],
                "gh.pull-request.list",
                json!({"owner": "o", "repo": "r", "state": "open", "author": "cpetersen"}),
            ),
            (
                &["pr", "view", "7", "-R", "o/r"],
                "gh.pull-request.read",
                json!({"owner": "o", "repo": "r", "number": 7}),
            ),
            (
                &["pr", "files", "7", "-R", "o/r", "--page", "2", "--no-patch"],
                "gh.pull-request.files",
                json!({"owner": "o", "repo": "r", "number": 7, "page": 2, "includePatch": false}),
            ),
            (
                &["pr", "diff", "7", "-R", "o/r"],
                "gh.pull-request.diff",
                json!({"owner": "o", "repo": "r", "number": 7}),
            ),
            (
                &["pr", "status", "7", "-R", "o/r"],
                "gh.pull-request.status",
                json!({"owner": "o", "repo": "r", "number": 7}),
            ),
            (
                &["pr", "checks", "7", "-R", "o/r"],
                "gh.pull-request.status",
                json!({"owner": "o", "repo": "r", "number": 7}),
            ),
            (
                &["pr", "reviews", "7", "-R", "o/r"],
                "gh.pull-request.reviews",
                json!({"owner": "o", "repo": "r", "number": 7}),
            ),
            (
                &[
                    "pr",
                    "review",
                    "7",
                    "-R",
                    "o/r",
                    "--approve",
                    "--expected-head-sha",
                    "abc123",
                ],
                "gh.pull-request.approve",
                json!({"owner": "o", "repo": "r", "number": 7, "expectedHeadSha": "abc123"}),
            ),
            (
                &["pr", "review", "7", "-R", "o/r", "--comment", "-b", "hm"],
                "gh.pull-request.comment",
                json!({"owner": "o", "repo": "r", "number": 7, "body": "hm"}),
            ),
            (
                &[
                    "pr",
                    "review",
                    "7",
                    "-R",
                    "o/r",
                    "--request-changes",
                    "-b",
                    "no",
                ],
                "gh.pull-request.request-changes",
                json!({"owner": "o", "repo": "r", "number": 7, "body": "no"}),
            ),
            (
                &["pr", "merge", "7", "-R", "o/r", "--squash"],
                "gh.pull-request.merge",
                json!({"owner": "o", "repo": "r", "number": 7, "mergeMethod": "squash"}),
            ),
            (
                &["repo", "view", "o/r"],
                "gh.repo.read",
                json!({"owner": "o", "repo": "r"}),
            ),
            (
                &[
                    "content",
                    "view",
                    "src/lib.rs",
                    "-R",
                    "o/r",
                    "--ref",
                    "main",
                ],
                "gh.content.read",
                json!({"owner": "o", "repo": "r", "path": "src/lib.rs", "ref": "main"}),
            ),
            (
                &["issue", "view", "9", "-R", "o/r"],
                "gh.issue.read",
                json!({"owner": "o", "repo": "r", "number": 9}),
            ),
            (
                &["issue", "list", "-R", "o/r", "--per-page", "5"],
                "gh.issue.list",
                json!({"owner": "o", "repo": "r", "perPage": 5}),
            ),
            (
                &["issue", "comments", "9", "-R", "o/r"],
                "gh.issue-comments.read",
                json!({"owner": "o", "repo": "r", "number": 9}),
            ),
            (
                &["issue", "comment", "9", "-R", "o/r", "-b", "done"],
                "gh.issue.comment",
                json!({"owner": "o", "repo": "r", "number": 9, "body": "done"}),
            ),
            (
                &["branch", "view", "main", "-R", "o/r"],
                "gh.branch.read",
                json!({"owner": "o", "repo": "r", "branch": "main"}),
            ),
            (
                &["commit", "view", "abc123", "-R", "o/r"],
                "gh.commit.read",
                json!({"owner": "o", "repo": "r", "ref": "abc123"}),
            ),
            (
                &["user", "view", "cpetersen"],
                "gh.user.read",
                json!({"login": "cpetersen"}),
            ),
        ];

        for (arguments, capability, input) in cases {
            let (called, sent) = dispatch(arguments);
            assert_eq!(called, *capability, "{arguments:?}");
            assert_eq!(sent, *input, "{arguments:?}");
        }
    }

    #[test]
    fn a_missing_repository_is_a_usage_error_naming_the_form() {
        let CommandFailure::Status { message, status } = refuse(&["pr", "view", "7"]) else {
            panic!("usage failures stay recoverable");
        };
        assert_eq!(status, ExitCode::SYNTAX);
        assert!(message.contains("-R owner/repo"), "{message}");
    }

    #[test]
    fn review_requires_exactly_one_event() {
        assert!(matches!(
            refuse(&["pr", "review", "7", "-R", "o/r"]),
            CommandFailure::Status { .. }
        ));
        let failure = refuse(&["pr", "review", "7", "-R", "o/r", "--approve", "--comment"]);
        let CommandFailure::Status { message, .. } = failure else {
            panic!("event conflicts stay recoverable");
        };
        assert!(message.contains("exactly one"), "{message}");
    }

    #[test]
    fn comment_and_request_changes_require_a_body() {
        for event in ["--comment", "--request-changes"] {
            let CommandFailure::Status { message, .. } =
                refuse(&["pr", "review", "7", "-R", "o/r", event])
            else {
                panic!("missing body stays recoverable");
            };
            assert!(message.contains("--body"), "{message}");
        }
    }

    #[test]
    fn lying_flags_are_rejected_with_guidance() {
        for flag in ["--web", "--json", "--jq", "--template", "--checkout"] {
            let CommandFailure::Status { message, status } =
                refuse(&["pr", "view", "7", "-R", "o/r", flag])
            else {
                panic!("rejected flags stay recoverable");
            };
            assert_eq!(status, ExitCode::SYNTAX);
            assert!(message.contains(flag), "{message}");
        }
    }

    #[test]
    fn gh_api_is_refused_as_a_policy_bypass() {
        let CommandFailure::Status { message, .. } = refuse(&["api", "/repos/o/r/pulls"]) else {
            panic!("gh api stays recoverable");
        };
        assert!(message.contains("per-capability"), "{message}");
    }

    #[test]
    fn flags_a_subcommand_does_not_have_are_refused_not_ignored() {
        let CommandFailure::Status { message, .. } =
            refuse(&["pr", "diff", "7", "-R", "o/r", "--approve"])
        else {
            panic!("leftover flags stay recoverable");
        };
        assert!(message.contains("does not accept"), "{message}");
    }

    #[test]
    fn an_ungranted_capability_reports_127_and_names_it() {
        struct NothingGranted;
        impl CapabilityInvoker for NothingGranted {
            fn granted(&self) -> Vec<String> {
                Vec::new()
            }
            fn invoke(&self, _capability: &str, _input: Value) -> CapabilityCallResult {
                panic!("an ungranted command must not dispatch");
            }
        }

        let failure =
            run_builtin_with_invoker(&Gh, &["pr", "view", "7", "-R", "o/r"], &NothingGranted)
                .expect_err("ungranted capabilities refuse");
        let CommandFailure::Status { message, status } = failure else {
            panic!("grant refusals stay recoverable");
        };
        assert_eq!(status, ExitCode::NOT_FOUND);
        assert!(message.contains("gh.pull-request.read"), "{message}");
    }

    #[test]
    fn a_denied_capability_keeps_the_denied_exit_code() {
        struct Denies;
        impl CapabilityInvoker for Denies {
            fn granted(&self) -> Vec<String> {
                vec!["gh.pull-request.approve".to_owned()]
            }
            fn invoke(&self, _capability: &str, _input: Value) -> CapabilityCallResult {
                CapabilityCallResult::Denied {
                    reason: "policy".to_owned(),
                }
            }
        }

        let failure = run_builtin_with_invoker(
            &Gh,
            &["pr", "review", "7", "-R", "o/r", "--approve"],
            &Denies,
        )
        .expect_err("denials propagate");
        let CommandFailure::Status { status, .. } = failure else {
            panic!("denials stay recoverable");
        };
        assert_eq!(status, ExitCode::DENIED);
    }

    #[test]
    fn unknown_subcommands_list_the_supported_surface() {
        let CommandFailure::Status { message, .. } = refuse(&["pr", "create", "-R", "o/r"]) else {
            panic!("unknown subcommands stay recoverable");
        };
        assert!(message.contains("supported"), "{message}");

        assert!(matches!(refuse(&[]), CommandFailure::Status { .. }));
        assert!(matches!(refuse(&["pr"]), CommandFailure::Status { .. }));
    }

    #[test]
    fn malformed_repositories_and_numbers_are_usage_errors() {
        assert!(matches!(
            refuse(&["pr", "view", "7", "-R", "just-a-name"]),
            CommandFailure::Status { .. }
        ));
        assert!(matches!(
            refuse(&["pr", "view", "seven", "-R", "o/r"]),
            CommandFailure::Status { .. }
        ));
        assert!(matches!(
            refuse(&["pr", "view", "0", "-R", "o/r"]),
            CommandFailure::Status { .. }
        ));
    }
}
