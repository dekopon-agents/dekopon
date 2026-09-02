//! The `clap` layer over [`Provider::run_command`](crate::Provider::run_command).
//!
//! Hand-rolled argv handling is the baseline contract: a provider may match on argv slices and
//! return a proposal, a rendered page, or a usage error with no parser at all. This module is the
//! encouraged layer on top of it. A provider declares its command tree as a [`clap::Command`]
//! (by hand or through `#[derive(Parser)]` on the re-exported [`clap`]) and
//! [`run_command`] does what the upstream tool's `main` would: `--help` and `--version` render on
//! stdout at status 0, an unknown subcommand or a missing argument renders clap's own usage error
//! on stderr at status 2, and a well-formed argv reaches a dispatch closure that assembles the
//! proposal.
//!
//! The SDK's clap is built without `color` and without `env`, so rendered text is plain — no
//! escape sequence ever reaches the model — and no argument can read a process environment the
//! guest does not have. The tree is built on every call: a command word runs in a fresh store with
//! a fuel bound, so there is no process-lifetime `static` to construct it into, and the bound is
//! what caps the work.
//!
//! # Coupling the tree to the manifest
//!
//! Keep each capability identifier in one `const` used by both `manifest()` and the dispatch
//! closure, so renaming a capability is a compile error rather than an exit code a model discovers
//! mid-session; a fixture test that walks every dispatch target and finds it in the manifest closes
//! the remaining gap.
//!
//! ```rust,ignore
//! use dekopon_provider_sdk::clap::{Arg, ArgMatches, Command};
//! use dekopon_provider_sdk::{CommandInvocation, CommandRun, ProviderError, cli};
//!
//! const PR_READ: &str = "gh.pull-request.read";
//!
//! fn tree() -> Command {
//!     Command::new("gh").version("0.1.0").subcommand_required(true).subcommand(
//!         Command::new("pr").subcommand_required(true).subcommand(
//!             Command::new("view").arg(Arg::new("number").required(true)),
//!         ),
//!     )
//! }
//!
//! fn dispatch(matches: ArgMatches, _stdin: Option<&str>) -> Result<CommandInvocation, ProviderError> {
//!     match matches.subcommand() {
//!         Some(("pr", pr)) => match pr.subcommand() {
//!             Some(("view", view)) => Ok(CommandInvocation {
//!                 capability: PR_READ.parse().expect("static capability ID"),
//!                 input: serde_json::json!({ "number": view.get_one::<String>("number") }),
//!             }),
//!             _ => Err(ProviderError::new("usage", "gh pr view <NUMBER>")),
//!         },
//!         _ => Err(ProviderError::new("usage", "gh pr <COMMAND>")),
//!     }
//! }
//!
//! fn run_command(argv: &[String], stdin: Option<&str>) -> Result<CommandRun, ProviderError> {
//!     cli::run_command(tree(), argv, stdin, dispatch)
//! }
//! ```

use clap::{ArgMatches, Command};

use crate::{CommandInvocation, CommandRun, ProviderError};

/// Exit status reported when clap's own exit code does not fit the shell's `u8`.
///
/// clap only ever reports `0` (help, version) or `2` (usage), so this is the usage status it would
/// have reported anyway, kept as a named fallback rather than a silent truncation.
const USAGE_STATUS: u8 = 2;

/// Parses `argv` against `command` and either renders clap's answer or dispatches the matches.
///
/// `argv` holds the arguments after the command word, exactly as
/// [`Provider::run_command`](crate::Provider::run_command) receives them, so the tree is parsed
/// with [`Command::no_binary_name`]. clap prefixes a subcommand's usage line with the parent's
/// binary name, which nothing sets when no binary name is parsed, so a tree without an explicit
/// [`Command::bin_name`] gets its own name as one: `gh pr view` renders `Usage: gh pr view
/// <NUMBER>` rather than `Usage: view <NUMBER>`. The outcome follows clap's own classification of
/// what it produced:
///
/// - help and a version (`--help`, `-h`, `--version`, `-V`, or the `help` subcommand) are
///   [`CommandRun::Rendered`] on stdout at status 0;
/// - any usage error — an unknown subcommand, a missing or repeated argument, a value the parser
///   refused — is [`CommandRun::Rendered`] on stderr at clap's exit status, 2;
/// - a well-formed argv is handed to `dispatch` with the piped `stdin`, and its proposal becomes
///   [`CommandRun::Proposal`].
///
/// Rendered text is plain: the SDK's clap has no `color` feature, so no escape sequence is ever
/// produced. Nothing here reads the process environment, prints, or exits; the guest has none of
/// those.
///
/// # Errors
///
/// Returns whatever `dispatch` returns as the provider's own error: the decline reported to the
/// model as a usage error. Parsing itself never fails this function; clap's errors are rendered.
pub fn run_command<F>(
    command: Command,
    argv: &[String],
    stdin: Option<&str>,
    dispatch: F,
) -> Result<CommandRun, ProviderError>
where
    F: FnOnce(ArgMatches, Option<&str>) -> Result<CommandInvocation, ProviderError>,
{
    let command = match command.get_bin_name() {
        Some(_) => command,
        None => {
            let name = command.get_name().to_owned();
            command.bin_name(name)
        }
    };
    match command.no_binary_name(true).try_get_matches_from(argv) {
        Ok(matches) => dispatch(matches, stdin).map(CommandRun::Proposal),
        Err(error) => {
            let text = error.render().to_string();
            if error.use_stderr() {
                let status = u8::try_from(error.exit_code()).unwrap_or(USAGE_STATUS);
                Ok(CommandRun::rendered_error(text, status))
            } else {
                Ok(CommandRun::rendered(text, 0))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::{Arg, ArgMatches, Command};
    use serde_json::json;

    use super::run_command;
    use crate::{CommandInvocation, CommandRun, ProviderError};

    const PR_READ: &str = "gh.pull-request.read";

    fn tree() -> Command {
        Command::new("gh")
            .version("0.1.0")
            .subcommand_required(true)
            .subcommand(
                Command::new("pr")
                    .about("Work with pull requests")
                    .subcommand_required(true)
                    .subcommand(
                        Command::new("view")
                            .about("View one pull request")
                            .arg(Arg::new("number").required(true)),
                    ),
            )
    }

    fn dispatch(
        matches: ArgMatches,
        stdin: Option<&str>,
    ) -> Result<CommandInvocation, ProviderError> {
        let Some(("pr", pr)) = matches.subcommand() else {
            return Err(ProviderError::new("usage", "gh pr <COMMAND>"));
        };
        let Some(("view", view)) = pr.subcommand() else {
            return Err(ProviderError::new("usage", "gh pr view <NUMBER>"));
        };
        Ok(CommandInvocation {
            capability: PR_READ.parse().expect("static capability ID"),
            input: json!({
                "number": view.get_one::<String>("number").expect("required by the tree"),
                "stdin": stdin,
            }),
        })
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    fn rendered(argv: &[String]) -> (String, String, u8) {
        let run = run_command(tree(), argv, None, dispatch).expect("clap answers are rendered");
        let CommandRun::Rendered {
            stdout,
            stderr,
            status,
        } = run
        else {
            panic!("expected rendered text for {argv:?}, got {run:?}");
        };
        (stdout, stderr, status)
    }

    #[test]
    fn help_renders_on_stdout_at_status_zero() {
        for flag in ["--help", "-h"] {
            let (stdout, stderr, status) = rendered(&argv(&[flag]));
            assert_eq!(status, 0, "{flag}");
            assert!(
                stdout.starts_with("Usage: gh <COMMAND>"),
                "{flag}: {stdout:?}"
            );
            assert!(stdout.contains("  pr  "), "{flag}: {stdout:?}");
            assert!(stderr.is_empty(), "{flag}: {stderr:?}");
        }
        let (stdout, _, status) = rendered(&argv(&["pr", "view", "--help"]));
        assert_eq!(status, 0);
        assert!(stdout.starts_with("View one pull request"), "{stdout:?}");
    }

    #[test]
    fn version_renders_on_stdout_at_status_zero() {
        for flag in ["--version", "-V"] {
            let (stdout, stderr, status) = rendered(&argv(&[flag]));
            assert_eq!(status, 0, "{flag}");
            assert_eq!(stdout, "gh 0.1.0\n", "{flag}");
            assert!(stderr.is_empty(), "{flag}: {stderr:?}");
        }
    }

    #[test]
    fn an_unknown_subcommand_is_a_usage_error_on_stderr_at_status_two() {
        let (stdout, stderr, status) = rendered(&argv(&["bogus"]));
        assert_eq!(status, 2);
        assert!(stdout.is_empty(), "{stdout:?}");
        assert!(
            stderr.starts_with("error: unrecognized subcommand 'bogus'"),
            "{stderr:?}"
        );
        assert!(stderr.contains("\nUsage: gh <COMMAND>\n"), "{stderr:?}");
    }

    #[test]
    fn a_missing_argument_is_a_usage_error_on_stderr_at_status_two() {
        let (stdout, stderr, status) = rendered(&argv(&["pr", "view"]));
        assert_eq!(status, 2);
        assert!(stdout.is_empty(), "{stdout:?}");
        assert!(stderr.starts_with("error: "), "{stderr:?}");
        assert!(stderr.contains("<number>"), "{stderr:?}");
    }

    /// Without a binary name to parse, clap would print `Usage: view <number>`; the word the
    /// model typed is what the usage line has to start with.
    #[test]
    fn a_subcommands_usage_line_is_prefixed_with_the_command_word() {
        let (_, stderr, _) = rendered(&argv(&["pr", "view"]));
        assert!(
            stderr.contains("\nUsage: gh pr view <number>\n"),
            "{stderr:?}"
        );
        let (stdout, _, _) = rendered(&argv(&["pr", "--help"]));
        assert!(stdout.contains("\nUsage: gh pr <COMMAND>\n"), "{stdout:?}");
        let (stdout, _, _) = rendered(&argv(&["--help"]));
        assert!(stdout.starts_with("Usage: gh <COMMAND>\n"), "{stdout:?}");
    }

    #[test]
    fn a_subcommand_dispatches_with_its_matches() {
        let run = run_command(tree(), &argv(&["pr", "view", "7"]), None, dispatch)
            .expect("a well-formed argv proposes");
        assert_eq!(
            run,
            CommandRun::proposal(
                PR_READ.parse().expect("static capability ID"),
                json!({"number": "7", "stdin": null})
            )
        );
    }

    #[test]
    fn stdin_reaches_the_dispatch_closure_unchanged() {
        let piped = "line one\n  indented\ttabbed\n";
        let run = run_command(tree(), &argv(&["pr", "view", "7"]), Some(piped), dispatch)
            .expect("a well-formed argv proposes");
        assert_eq!(
            run,
            CommandRun::proposal(
                PR_READ.parse().expect("static capability ID"),
                json!({"number": "7", "stdin": piped})
            )
        );
    }

    #[test]
    fn no_rendered_text_contains_an_escape_byte() {
        for words in [
            &["--help"][..],
            &["--version"][..],
            &["bogus"][..],
            &["pr", "view"][..],
            &["pr", "--help"][..],
        ] {
            let (stdout, stderr, _) = rendered(&argv(words));
            assert!(!stdout.contains('\u{1b}'), "{words:?}: {stdout:?}");
            assert!(!stderr.contains('\u{1b}'), "{words:?}: {stderr:?}");
        }
    }

    #[test]
    fn a_dispatch_error_is_returned_as_the_providers_own_error() {
        let error = run_command(tree(), &argv(&["pr", "view", "7"]), None, |_, _| {
            Err(ProviderError::new(
                "not-a-number",
                "pull request numbers are digits",
            ))
        })
        .expect_err("the closure's decline is the provider's");
        assert_eq!(error.code(), "not-a-number");
        assert_eq!(error.message(), "pull request numbers are digits");
    }
}
