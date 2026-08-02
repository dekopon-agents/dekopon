//! The fixed builtin table and the context builtins run against.
//!
//! Every builtin name in this table is separator-free: it contains no `.`, `-`, or `_`. Capability
//! fallback in [`crate::dispatch`] only fires for words that *do* contain a separator, so a builtin
//! and a capability can never collide on the same bare word. That is the collision-avoidance
//! mechanism, not a coincidence, and [`tests::builtin_names_can_never_collide_with_capabilities`]
//! asserts it.
//!
//! Builtins are either **text-shaped** or **value-shaped**:
//!
//! - text-shaped (`grep`, `sed`, `cut`, `sort`, `uniq`, `wc`, `base64`) accept a raw string or a
//!   JSON array of lines, newline-joining arrays on the way in and re-coercing line lists to arrays
//!   on the way out, so `curl ... | grep foo | wc -l` reads like bash;
//! - value-shaped (`jq`, `cap`, `curl`, `cat`) stay JSON-native end to end.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::{
    CapabilityCallResult, CapabilityInvoker, ExitCode,
    limits::{Budget, LimitExceeded},
};

pub(crate) mod cap;
pub(crate) mod curl;
pub(crate) mod encode;
pub(crate) mod jq;
pub(crate) mod misc;
pub(crate) mod text;
pub(crate) mod xargs;

/// What one command produced.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CommandResult {
    /// The structured value the command produced.
    pub value: Value,
    /// The command's exit status.
    pub status: ExitCode,
    /// Whether emitting this value should omit the usual trailing newline.
    pub suppress_newline: bool,
}

impl CommandResult {
    pub(crate) fn value(value: Value) -> Self {
        Self {
            value,
            status: ExitCode::SUCCESS,
            suppress_newline: false,
        }
    }

    pub(crate) fn status(status: ExitCode) -> Self {
        Self {
            value: Value::Null,
            status,
            suppress_newline: false,
        }
    }

    pub(crate) fn without_newline(mut self) -> Self {
        self.suppress_newline = true;
        self
    }
}

/// Why a command did not produce a result.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CommandFailure {
    /// Recoverable: the message is written to output, the status is recorded, the script continues.
    Status {
        /// Diagnostic written to the combined output.
        message: String,
        /// Exit status recorded in `$?`.
        status: ExitCode,
    },
    /// Terminal: the script stops and the interpreter reports this exit code.
    Fatal(FatalError),
}

impl CommandFailure {
    /// A usage or argument error. Exit code `2`, matching a shell syntax failure.
    pub(crate) fn usage(message: impl Into<String>) -> Self {
        Self::Status {
            message: message.into(),
            status: ExitCode::SYNTAX,
        }
    }

    /// A runtime failure inside a builtin. Exit code `1`.
    pub(crate) fn failed(message: impl Into<String>) -> Self {
        Self::Status {
            message: message.into(),
            status: ExitCode::FAILURE,
        }
    }
}

impl From<LimitExceeded> for CommandFailure {
    fn from(limit: LimitExceeded) -> Self {
        Self::Fatal(FatalError::Limit(limit))
    }
}

impl From<LimitExceeded> for FatalError {
    fn from(limit: LimitExceeded) -> Self {
        Self::Limit(limit)
    }
}

/// A failure that stops the whole script.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum FatalError {
    /// A sandbox bound was exhausted.
    Limit(LimitExceeded),
    /// The script reached a construct this shell deliberately excludes.
    Unsupported(String),
}

/// Everything a builtin may touch.
pub(crate) struct BuiltinContext<'a> {
    /// The capability seam.
    pub invoker: &'a dyn CapabilityInvoker,
    /// Per-execution counters.
    pub budget: &'a mut Budget,
    /// Named in-memory buffers written by `>` and `>>`; never real paths.
    pub buffers: &'a mut BTreeMap<String, Value>,
    /// The capability `curl` assembles requests for, when the embedder configured one.
    pub curl_capability: Option<&'a str>,
}

impl BuiltinContext<'_> {
    /// Charges the capability-call budget and invokes one capability.
    pub(crate) fn invoke_capability(
        &mut self,
        capability: &str,
        input: Value,
    ) -> Result<CommandResult, CommandFailure> {
        self.budget.charge_capability_call()?;
        let result = self.invoker.invoke(capability, input);
        let status = ExitCode::from_capability_result(&result);
        Ok(match result {
            CapabilityCallResult::Succeeded(output) => CommandResult {
                value: output,
                status,
                suppress_newline: false,
            },
            CapabilityCallResult::Denied { reason } => {
                return Err(CommandFailure::Status {
                    message: format!("{capability}: denied: {reason}"),
                    status,
                });
            }
            CapabilityCallResult::Failed { error } => {
                return Err(CommandFailure::Status {
                    message: format!("{capability}: failed: {error}"),
                    status,
                });
            }
            CapabilityCallResult::NotFound => {
                return Err(CommandFailure::Status {
                    message: format!("{capability}: capability not found"),
                    status,
                });
            }
        })
    }
}

/// One builtin command.
pub(crate) trait Builtin {
    /// The separator-free name this builtin is dispatched by.
    fn name(&self) -> &'static str;

    /// Runs the builtin with already-expanded argv and the piped input value, if any.
    fn run(
        &self,
        context: &mut BuiltinContext<'_>,
        arguments: &[String],
        input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure>;
}

/// How a builtin is executed.
#[derive(Clone, Copy)]
pub(crate) enum BuiltinKind {
    /// Runs entirely inside the builtin.
    Simple(&'static dyn Builtin),
    /// `xargs` maps a command over a list, so the interpreter runs it re-entrantly.
    Xargs,
}

/// The complete builtin registry, in dispatch order.
const REGISTRY: &[&dyn Builtin] = &[
    &jq::Jq,
    &curl::Curl,
    &misc::Sleep,
    &text::Grep,
    &text::Sed,
    &text::Cut,
    &text::Sort,
    &text::Uniq,
    &text::Wc,
    &encode::Base64,
    &misc::Echo,
    &misc::Printf,
    &misc::Test,
    &misc::TestBracket,
    &misc::True,
    &misc::False,
    &misc::Cat,
    &cap::Cap,
];

/// Looks one command word up in the builtin table.
pub(crate) fn lookup(name: &str) -> Option<BuiltinKind> {
    if name == xargs::NAME {
        return Some(BuiltinKind::Xargs);
    }
    REGISTRY
        .iter()
        .find(|builtin| builtin.name() == name)
        .map(|builtin| BuiltinKind::Simple(*builtin))
}

/// Returns every builtin name, for the namespace-disjointness invariant tests.
#[cfg(test)]
pub(crate) fn names() -> Vec<&'static str> {
    let mut names = REGISTRY
        .iter()
        .map(|builtin| builtin.name())
        .collect::<Vec<_>>();
    names.push(xargs::NAME);
    names.sort_unstable();
    names
}

/// Rejects a flag this shell does not implement, rather than accepting it as a no-op.
pub(crate) fn unsupported_flag(command: &str, flag: &str) -> CommandFailure {
    CommandFailure::usage(format!("{command}: option not yet supported: {flag}"))
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Helpers letting each builtin be unit-tested without standing up the whole interpreter.

    use std::collections::BTreeMap;

    use serde_json::Value;

    use crate::{
        CapabilityCallResult, CapabilityInvoker,
        limits::{Budget, Limits},
    };

    use super::{Builtin, BuiltinContext, CommandFailure, CommandResult};

    /// An invoker that grants nothing, for builtins that never reach the capability seam.
    pub(crate) struct NoCapabilities;

    impl CapabilityInvoker for NoCapabilities {
        fn granted(&self) -> Vec<String> {
            Vec::new()
        }

        fn invoke(&self, _capability: &str, _input: Value) -> CapabilityCallResult {
            CapabilityCallResult::NotFound
        }
    }

    /// Runs one builtin under default limits with no capabilities and no buffers.
    pub(crate) fn run_builtin(
        builtin: &dyn Builtin,
        arguments: &[&str],
        input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let mut buffers = BTreeMap::new();
        run_builtin_with(
            builtin,
            arguments,
            input,
            Limits::default(),
            None,
            &mut buffers,
        )
    }

    /// Runs one builtin against a caller-supplied invoker.
    pub(crate) fn run_builtin_with_invoker(
        builtin: &dyn Builtin,
        arguments: &[&str],
        invoker: &dyn CapabilityInvoker,
    ) -> Result<CommandResult, CommandFailure> {
        let mut budget = Budget::start(Limits::default());
        let mut buffers = BTreeMap::new();
        let mut context = BuiltinContext {
            invoker,
            budget: &mut budget,
            buffers: &mut buffers,
            curl_capability: None,
        };
        let arguments = arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect::<Vec<_>>();
        builtin.run(&mut context, &arguments, None)
    }

    /// Runs one builtin with explicit limits, curl capability, and buffer store.
    pub(crate) fn run_builtin_with(
        builtin: &dyn Builtin,
        arguments: &[&str],
        input: Option<Value>,
        limits: Limits,
        curl_capability: Option<&str>,
        buffers: &mut BTreeMap<String, Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let invoker = NoCapabilities;
        let mut budget = Budget::start(limits);
        let mut context = BuiltinContext {
            invoker: &invoker,
            budget: &mut budget,
            buffers,
            curl_capability,
        };
        let arguments = arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect::<Vec<_>>();
        builtin.run(&mut context, &arguments, input)
    }
}

#[cfg(test)]
mod tests {
    use super::{lookup, names, xargs};

    #[test]
    fn builtin_names_can_never_collide_with_capabilities() {
        // Capability fallback only fires for words containing `.`, `-`, or `_`. Keeping every
        // builtin name separator-free is what makes that disjointness total rather than likely.
        for name in names() {
            assert!(
                !name.contains(['.', '-', '_']),
                "builtin {name:?} contains a capability-identifier separator"
            );
            assert!(
                name.parse::<dekopon_core::CapabilityId>().is_err()
                    || !name.contains(['.', '-', '_']),
                "builtin {name:?} must not be reachable through capability fallback"
            );
        }
    }

    #[test]
    fn the_registry_covers_every_documented_builtin() {
        let expected = [
            "[",
            "base64",
            "cap",
            "cat",
            "curl",
            "cut",
            "echo",
            "false",
            "grep",
            "jq",
            "printf",
            "sed",
            "sleep",
            "sort",
            "test",
            "true",
            "uniq",
            "wc",
            xargs::NAME,
        ];
        let mut expected = expected.to_vec();
        expected.sort_unstable();
        assert_eq!(names(), expected);
        for name in expected {
            assert!(lookup(name).is_some(), "{name} must resolve");
        }
        assert!(lookup("definitely-not-a-builtin").is_none());
    }
}
