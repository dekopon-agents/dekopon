use std::collections::BTreeMap;

use serde_json::Value;

use crate::{
    CapabilityInvoker, ExitCode,
    limits::{Budget, LimitExceeded},
};

pub(crate) mod cap;
pub(crate) mod encode;
pub(crate) mod jobs;
pub(crate) mod jq;
pub(crate) mod misc;
pub(crate) mod text;
pub(crate) mod xargs;

#[derive(Debug)]
pub(crate) struct CommandResult {
    pub value: Value,
    pub status: ExitCode,
    pub suppress_newline: bool,
    pub retained: Vec<crate::RetainedBytes>,
}

impl CommandResult {
    pub(crate) fn value(value: Value) -> Self {
        Self {
            value,
            status: ExitCode::SUCCESS,
            suppress_newline: false,
            retained: Vec::new(),
        }
    }

    pub(crate) fn status(status: ExitCode) -> Self {
        Self {
            value: Value::Null,
            status,
            suppress_newline: false,
            retained: Vec::new(),
        }
    }

    pub(crate) fn without_newline(mut self) -> Self {
        self.suppress_newline = true;
        self
    }

    pub(crate) fn lines(lines: Vec<String>) -> Self {
        if lines.is_empty() {
            return Self::status(ExitCode::SUCCESS);
        }
        Self::value(Value::String(lines.join("\n")))
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
    pub buffers: &'a BTreeMap<String, Vec<u8>>,
    pub started_jobs: &'a [crate::JobId],
}

pub(crate) trait Builtin {
    fn name(&self) -> &'static str;

    fn help(&self) -> &'static str;

    fn copies_stdin(&self) -> bool {
        false
    }

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
    Lines(text::lines::LineCommand),
    TextStream(text::stream::TextStream),
    Extra(text::extra::ExtraStream),
    Base64,
    Jq,
    Xargs,
}

const REGISTRY: &[&dyn Builtin] = &[
    &jobs::Jobs,
    &jobs::Wait,
    &jobs::Kill,
    &misc::Sleep,
    &misc::Progress,
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
    if let Some(command) = text::lines::LineCommand::lookup(name) {
        return Some(BuiltinKind::Lines(command));
    }
    if let Some(command) = match name {
        "grep" => Some(text::stream::TextStream::Grep),
        "sed" => Some(text::stream::TextStream::Sed),
        _ => None,
    } {
        return Some(BuiltinKind::TextStream(command));
    }
    if let Some(command) = match name {
        "cut" => Some(text::extra::ExtraStream::Cut),
        "uniq" => Some(text::extra::ExtraStream::Uniq),
        "wc" => Some(text::extra::ExtraStream::Wc),
        "sort" => Some(text::extra::ExtraStream::Sort),
        _ => None,
    } {
        return Some(BuiltinKind::Extra(command));
    }
    if name == "jq" {
        return Some(BuiltinKind::Jq);
    }
    if name == "base64" {
        return Some(BuiltinKind::Base64);
    }
    if name == xargs::NAME {
        return Some(BuiltinKind::Xargs);
    }
    REGISTRY
        .iter()
        .find(|builtin| builtin.name() == name)
        .map(|builtin| BuiltinKind::Simple(*builtin))
}

#[cfg(test)]
pub(crate) fn names() -> Vec<&'static str> {
    let mut names = REGISTRY
        .iter()
        .map(|builtin| builtin.name())
        .collect::<Vec<_>>();
    names.extend([
        "head", "tail", "base64", "grep", "sed", "cut", "uniq", "wc", "sort", "jq",
    ]);
    names.push(xargs::NAME);
    names.sort_unstable();
    names
}

pub(crate) fn unsupported_flag(command: &str, flag: &str, supported: &str) -> CommandFailure {
    let suffix = if supported.is_empty() {
        String::new()
    } else {
        format!(" (supported: {supported})")
    };
    CommandFailure::usage(format!(
        "{command}: option not yet supported: {flag}{suffix}"
    ))
}

pub(crate) fn help_result(name: &str, help: &str) -> CommandResult {
    let help = if help.is_empty() { "no flags" } else { help };
    CommandResult::value(Value::String(format!("{name}: {help}")))
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
            _: crate::CommandProposal,
            _streams: crate::Streams,
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
            started_jobs: &[],
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
        buffers: &mut BTreeMap<String, Vec<u8>>,
    ) -> Result<CommandResult, CommandFailure> {
        let invoker = NoCapabilities;
        let mut budget = Budget::start(limits);
        let mut context = BuiltinContext {
            invoker: &invoker,
            budget: &mut budget,
            buffers,
            started_jobs: &[],
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
    use crate::{CapabilityCallResult, CapabilityInvoker, ExitCode, Interpreter, Limits};

    use super::{lookup, names, test_support::NoCapabilities, xargs};

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
            "head",
            "jobs",
            "jq",
            "kill",
            "printf",
            "progress",
            "sed",
            "sleep",
            "sort",
            "tail",
            "test",
            "true",
            "uniq",
            "wait",
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

    #[test]
    fn every_builtin_answers_help_at_exit_zero_before_its_own_usage_rules_apply() {
        for name in names() {
            let outcome =
                Interpreter::new(Limits::default()).run(&format!("{name} --help"), &NoCapabilities);
            assert_eq!(outcome.exit_code, ExitCode::SUCCESS, "{name}: {outcome:?}");
            assert!(
                outcome.output.starts_with(&format!("{name}: ")),
                "{name}: {}",
                outcome.output
            );
        }
    }

    #[test]
    fn help_names_the_exact_flags_a_few_representative_builtins_accept() {
        let run = |script: &str| Interpreter::new(Limits::default()).run(script, &NoCapabilities);

        assert_eq!(run("grep --help").output, "grep: -v -i -c -n -E");
        assert_eq!(run("true --help").output, "true: no flags");
        assert_eq!(run("cat --help").output, "cat: no flags");
        assert_eq!(run("progress --help").output, "progress: --eta");
        assert_eq!(
            run(&format!("{} --help", xargs::NAME)).output,
            "xargs: -I -n"
        );
        assert_eq!(
            run("[ --help").output,
            "[: ! -z -n = == != < > -eq -ne -lt -le -gt -ge"
        );
    }

    #[test]
    fn unsupported_flag_names_the_accepted_subset() {
        let result =
            Interpreter::new(Limits::default()).run("grep -q a <<EOF\na\nEOF", &NoCapabilities);
        assert_eq!(result.exit_code, ExitCode::SYNTAX);
        assert!(
            result
                .output
                .contains("grep: option not yet supported: -q (supported: -v -i -c -n -E)"),
            "{result:?}"
        );
    }

    struct HttpGranted;

    impl CapabilityInvoker for HttpGranted {
        fn granted(&self) -> Vec<String> {
            vec!["http-probe.fetch".to_owned()]
        }

        fn invoke(
            &self,
            proposal: crate::CommandProposal,
            _streams: crate::Streams,
        ) -> CapabilityCallResult {
            if proposal.secret_use.is_some() {
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
