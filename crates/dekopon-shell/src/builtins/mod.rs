//! Builtins are text-shaped (grep, sed, cut, sort, uniq, wc, base64: lines in and out) or
//! value-shaped (jq, cap, cat: JSON-native); every name here is reserved, so a provider declaring
//! one is refused at load rather than shadowed.

use std::collections::BTreeMap;

use dekopon_core::SecretUseProposal;
use serde_json::Value;

use crate::{
    CapabilityCallResult, CapabilityInvoker, ExitCode,
    limits::{Budget, LimitExceeded},
};

pub(crate) mod cap;
pub(crate) mod encode;
pub(crate) mod jq;
pub(crate) mod misc;
pub(crate) mod text;
pub(crate) mod xargs;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CommandResult {
    pub value: Value,
    pub status: ExitCode,
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

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CommandFailure {
    Status { message: String, status: ExitCode },
    Fatal(FatalError),
}

impl CommandFailure {
    pub(crate) fn usage(message: impl Into<String>) -> Self {
        Self::Status {
            message: message.into(),
            status: ExitCode::SYNTAX,
        }
    }

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

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum FatalError {
    Limit(LimitExceeded),
    Unsupported(String),
    /// Terminal rather than recoverable, because that is the point of the construct: reporting a
    /// status and continuing with an empty string would be exactly the silent wrongness this shell
    /// exists to refuse.
    Assertion(String),
}

pub(crate) struct BuiltinContext<'a> {
    pub invoker: &'a dyn CapabilityInvoker,
    pub budget: &'a mut Budget,
    /// These are named in-memory buffers only, written by redirects; a lookup here must never touch
    /// a real filesystem path.
    pub buffers: &'a mut BTreeMap<String, Value>,
}

impl BuiltinContext<'_> {
    /// Capability calls are wall-clock expensive but step-cheap (the default budget allows 32 in 96
    /// steps), so the deadline is re-read on both sides; step-counting alone let a script overrun
    /// its deadline by minutes.
    pub(crate) fn invoke_capability_with_secret_use(
        &mut self,
        capability: &str,
        input: Value,
        secret_use: Option<SecretUseProposal>,
    ) -> Result<CommandResult, CommandFailure> {
        self.budget.charge_capability_call()?;
        self.budget.check_deadline()?;
        let result = self.invoker.invoke(capability, input, secret_use);
        self.budget.check_deadline()?;
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
            CapabilityCallResult::Failed { error, detail } => {
                let message = match detail {
                    Some(detail) => format!("{capability}: failed: {error}: {detail}"),
                    None => format!("{capability}: failed: {error}"),
                };
                return Err(CommandFailure::Status { message, status });
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

pub(crate) trait Builtin {
    fn name(&self) -> &'static str;

    fn run(
        &self,
        context: &mut BuiltinContext<'_>,
        arguments: &[String],
        input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure>;
}

#[derive(Clone, Copy)]
pub(crate) enum BuiltinKind {
    Simple(&'static dyn Builtin),
    Xargs,
}

const REGISTRY: &[&dyn Builtin] = &[
    &jq::Jq,
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

pub(crate) fn lookup(name: &str) -> Option<BuiltinKind> {
    if name == xargs::NAME {
        return Some(BuiltinKind::Xargs);
    }
    REGISTRY
        .iter()
        .find(|builtin| builtin.name() == name)
        .map(|builtin| BuiltinKind::Simple(*builtin))
}

/// names() must stay derived from REGISTRY rather than hand-listed, or the drift test against
/// RESERVED_COMMAND_WORDS could pass while stale.
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

pub(crate) fn unsupported_flag(command: &str, flag: &str) -> CommandFailure {
    CommandFailure::usage(format!("{command}: option not yet supported: {flag}"))
}

#[cfg(test)]
pub(crate) mod test_support {

    use std::collections::BTreeMap;

    use serde_json::Value;

    use crate::{
        CapabilityCallResult, CapabilityInvoker,
        limits::{Budget, Limits},
    };

    use super::{Builtin, BuiltinContext, CommandFailure, CommandResult};

    pub(crate) struct NoCapabilities;

    impl CapabilityInvoker for NoCapabilities {
        fn granted(&self) -> Vec<String> {
            Vec::new()
        }

        fn invoke(
            &self,
            _capability: &str,
            _input: Value,
            _secret_use: Option<dekopon_core::SecretUseProposal>,
        ) -> CapabilityCallResult {
            CapabilityCallResult::NotFound
        }
    }

    pub(crate) fn run_builtin(
        builtin: &dyn Builtin,
        arguments: &[&str],
        input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let mut buffers = BTreeMap::new();
        run_builtin_with(builtin, arguments, input, Limits::default(), &mut buffers)
    }

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
        };
        let arguments = arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect::<Vec<_>>();
        builtin.run(&mut context, &arguments, None)
    }

    pub(crate) fn run_builtin_with(
        builtin: &dyn Builtin,
        arguments: &[&str],
        input: Option<Value>,
        limits: Limits,
        buffers: &mut BTreeMap<String, Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let invoker = NoCapabilities;
        let mut budget = Budget::start(limits);
        let mut context = BuiltinContext {
            invoker: &invoker,
            budget: &mut budget,
            buffers,
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
    use dekopon_core::SecretUseProposal;
    use serde_json::Value;

    use crate::{CapabilityCallResult, CapabilityInvoker, ExitCode, Interpreter, Limits};

    use super::{lookup, names, xargs};

    #[test]
    fn the_registry_covers_every_documented_builtin() {
        let expected = [
            "[",
            "base64",
            "cap",
            "cat",
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

    struct HttpGranted;

    impl CapabilityInvoker for HttpGranted {
        fn granted(&self) -> Vec<String> {
            vec!["http-probe.fetch".to_owned()]
        }

        fn invoke(
            &self,
            _capability: &str,
            _input: Value,
            secret_use: Option<SecretUseProposal>,
        ) -> CapabilityCallResult {
            if secret_use.is_some() {
                return crate::secret_use_unsupported();
            }
            CapabilityCallResult::Failed {
                error: "curl reached a capability".to_owned(),
                detail: None,
            }
        }
    }

    #[test]
    fn curl_is_not_a_builtin_and_exits_127_even_with_an_http_capability_granted() {
        assert!(lookup("curl").is_none());
        let outcome = Interpreter::new(Limits::default()).run("curl https://x", &HttpGranted);
        assert_eq!(outcome.exit_code, ExitCode::NOT_FOUND, "{}", outcome.output);
        assert_eq!(outcome.output, "dekopon-shell: curl: command not found");
        assert_eq!(outcome.capability_calls, 0);
    }
}
