use dekopon_process::{ExitCode, LimitExceeded};
use serde_json::Value;
use std::collections::BTreeSet;

use crate::{CapabilityInvoker, limits::Budget};

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
    pub retained: Vec<dekopon_process::RetainedBytes>,
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
    pub started_jobs: &'a [dekopon_process::JobId],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Builtin {
    Echo,
    Printf,
    Test,
    TestBracket,
    True,
    False,
    Progress,
    Sleep,
    Cat,
    Jobs,
    Wait,
    Kill,
    Cap,
    Head,
    Tail,
    Grep,
    Sed,
    Cut,
    Uniq,
    Wc,
    Sort,
    Jq,
    Base64,
    Xargs,
}

impl Builtin {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Echo => "echo",
            Self::Printf => "printf",
            Self::Test => "test",
            Self::TestBracket => "[",
            Self::True => "true",
            Self::False => "false",
            Self::Progress => "progress",
            Self::Sleep => "sleep",
            Self::Cat => "cat",
            Self::Jobs => "jobs",
            Self::Wait => "wait",
            Self::Kill => "kill",
            Self::Cap => "cap",
            Self::Head => "head",
            Self::Tail => "tail",
            Self::Grep => "grep",
            Self::Sed => "sed",
            Self::Cut => "cut",
            Self::Uniq => "uniq",
            Self::Wc => "wc",
            Self::Sort => "sort",
            Self::Jq => "jq",
            Self::Base64 => "base64",
            Self::Xargs => xargs::NAME,
        }
    }
    pub(crate) const fn help(self) -> &'static str {
        match self {
            Self::Echo => misc::ECHO_HELP,
            Self::Printf => "",
            Self::Test => misc::TEST_HELP,
            Self::TestBracket => misc::TEST_HELP,
            Self::True => "",
            Self::False => "",
            Self::Progress => "--eta",
            Self::Sleep => "",
            Self::Cat => "",
            Self::Jobs => {
                "List this person's jobs; -p prints ids only, -l keeps the normal listing."
            }
            Self::Wait => "Wait for this script's jobs, or for named job ids.",
            Self::Kill => "Cancel a job by id; signal options are accepted but all cancel.",
            Self::Cap => cap::USAGE,
            Self::Head => "-n N --lines N -N -c N --bytes N",
            Self::Tail => "-n N --lines N -N -n +N",
            Self::Grep => "-v -i -c -n -E -q -A N -B N -C N",
            Self::Sed => "-e -E",
            Self::Cut => "-d -f -c",
            Self::Uniq => "-c -d -u",
            Self::Wc => "-l -w -c",
            Self::Sort => "-r -n -u",
            Self::Jq => jq::HELP,
            Self::Base64 => encode::HELP,
            Self::Xargs => xargs::HELP,
        }
    }
}

pub(crate) fn lookup(name: &str) -> Option<Builtin> {
    match name {
        "echo" => Some(Builtin::Echo),
        "printf" => Some(Builtin::Printf),
        "test" => Some(Builtin::Test),
        "[" => Some(Builtin::TestBracket),
        "true" => Some(Builtin::True),
        "false" => Some(Builtin::False),
        "progress" => Some(Builtin::Progress),
        "sleep" => Some(Builtin::Sleep),
        "cat" => Some(Builtin::Cat),
        "jobs" => Some(Builtin::Jobs),
        "wait" => Some(Builtin::Wait),
        "kill" => Some(Builtin::Kill),
        "cap" => Some(Builtin::Cap),
        "head" => Some(Builtin::Head),
        "tail" => Some(Builtin::Tail),
        "grep" => Some(Builtin::Grep),
        "sed" => Some(Builtin::Sed),
        "cut" => Some(Builtin::Cut),
        "uniq" => Some(Builtin::Uniq),
        "wc" => Some(Builtin::Wc),
        "sort" => Some(Builtin::Sort),
        "jq" => Some(Builtin::Jq),
        "base64" => Some(Builtin::Base64),
        xargs::NAME => Some(Builtin::Xargs),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) fn names() -> Vec<&'static str> {
    let mut names = [
        Builtin::Echo,
        Builtin::Printf,
        Builtin::Test,
        Builtin::TestBracket,
        Builtin::True,
        Builtin::False,
        Builtin::Progress,
        Builtin::Sleep,
        Builtin::Cat,
        Builtin::Jobs,
        Builtin::Wait,
        Builtin::Kill,
        Builtin::Cap,
        Builtin::Head,
        Builtin::Tail,
        Builtin::Grep,
        Builtin::Sed,
        Builtin::Cut,
        Builtin::Uniq,
        Builtin::Wc,
        Builtin::Sort,
        Builtin::Jq,
        Builtin::Base64,
        Builtin::Xargs,
    ]
    .map(Builtin::name)
    .to_vec();
    names.sort_unstable();
    names
}

pub(crate) type Run =
    fn(&mut BuiltinContext<'_>, &[String], Option<Value>) -> Result<CommandResult, CommandFailure>;

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

pub(crate) fn parse_flags(
    mut grammar: clap::Command,
    arguments: &[String],
    name: &str,
    help: &str,
) -> Result<clap::ArgMatches, CommandFailure> {
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    grammar = grammar
        .no_binary_name(true)
        .disable_help_flag(true)
        .args_override_self(true);
    grammar
        .try_get_matches_from_mut(arguments)
        .map_err(|mut error| {
            if error.kind() == ErrorKind::UnknownArgument
                && let Some(ContextValue::String(word)) = error.get(ContextKind::InvalidArg)
            {
                return if word.starts_with('-') {
                    unsupported_flag(name, word, help)
                } else {
                    CommandFailure::usage(format!("{name}: unexpected argument {word:?}"))
                };
            }
            error = error.format(&mut grammar);
            CommandFailure::usage(format!("{name}: {}", error.render().to_string().trim_end()))
        })
}

pub(crate) fn help_result(name: &str, help: &str) -> CommandResult {
    let help = if help.is_empty() { "no flags" } else { help };
    CommandResult::value(Value::String(format!("{name}: {help}")))
}

#[cfg(test)]
mod clap_spike {
    use clap::{Arg, ArgAction, Command};

    use super::{CommandFailure, parse_flags};

    #[test]
    fn paired_values_keep_empty_and_hyphen_led_words() {
        let grammar = || {
            Command::new("jq").arg(
                Arg::new("arg")
                    .long("arg")
                    .num_args(2)
                    .allow_hyphen_values(true),
            )
        };
        for value in ["", "-hello", "--=x"] {
            let args = ["--arg", "s", value].map(str::to_owned);
            let matches = parse_flags(grammar(), &args, "jq", "").unwrap();
            assert_eq!(
                matches
                    .get_many::<String>("arg")
                    .unwrap()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                ["s", value]
            );
        }
    }

    #[test]
    fn option_terminator_and_attached_clusters_are_distinct() {
        let grammar = || {
            Command::new("grep")
                .arg(Arg::new("i").short('i').action(ArgAction::SetTrue))
                .arg(Arg::new("n").short('n').action(ArgAction::SetTrue))
                .arg(Arg::new("pattern").required(true))
        };
        for (words, expected) in [
            (vec!["-in", "-"], "-"),
            (vec!["-in", "--", "--=x"], "--=x"),
            (vec!["--", "-in"], "-in"),
        ] {
            let matches = parse_flags(
                grammar(),
                &words.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                "grep",
                "-i -n",
            )
            .unwrap();
            assert_eq!(matches.get_one::<String>("pattern").unwrap(), expected);
        }
        assert!(matches!(
            parse_flags(grammar(), &["--=x".into()], "grep", "-i -n"),
            Err(CommandFailure::Status { .. })
        ));
    }

    #[test]
    fn trailing_child_argv_is_not_parsed_and_bad_options_do_not_exit() {
        let grammar = || {
            Command::new("xargs")
                .arg(Arg::new("n").short('n').num_args(1))
                .arg(
                    Arg::new("child")
                        .required(true)
                        .num_args(1..)
                        .trailing_var_arg(true),
                )
        };
        let args = ["-n1", "grep", "-in", "P"].map(str::to_owned);
        let matches = parse_flags(grammar(), &args, "xargs", "-n").unwrap();
        assert_eq!(
            matches
                .get_many::<String>("child")
                .unwrap()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["grep", "-in", "P"]
        );
        let failure =
            parse_flags(Command::new("grep"), &["--bogus".into()], "grep", "-i").unwrap_err();
        assert!(matches!(
            failure,
            CommandFailure::Status {
                status: dekopon_process::ExitCode::SYNTAX,
                ..
            }
        ));
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use crate::interp::telemetry::StageObservation;
    pub(crate) fn cancel_after_first_step<T>(
        run: impl FnOnce(
            &mut dekopon_process::pipe::PipeReader<StageObservation>,
            &mut crate::limits::Budget,
        ) -> T,
    ) -> T {
        use dekopon_process::{CallBudget, CancelSignal, Limits, TreeContext};
        let (handle, signal) = CancelSignal::pair();
        let limits = Limits::default();
        let tree = TreeContext::new(limits, CallBudget::new(32), crate::test_threads())
            .with_cancel(vec![signal]);
        let (mut writer, mut reader) = dekopon_process::pipe::pipe(
            None,
            None,
            crate::interp::telemetry::stage_read,
            crate::interp::telemetry::stage_write,
        );
        assert_eq!(
            writer.write(b"x\n"),
            dekopon_process::pipe::WriteOutcome::Accepted
        );
        let [permit] = tree.shell_threads().reserve().unwrap();
        std::thread::scope(|scope| {
            permit
                .spawn(
                    scope,
                    dekopon_process::ThreadName::new("sh-cancel-test"),
                    || {
                        while tree.steps() == 0 {
                            std::thread::yield_now();
                        }
                        handle.cancel();
                        let _outcome = writer.write(b"x\n");
                    },
                )
                .unwrap();
            let mut budget = crate::limits::Budget::start_tree(limits, tree.clone());
            run(&mut reader, &mut budget)
        })
    }

    use serde_json::Value;

    use crate::{CapabilityCallResult, CapabilityInvoker, limits::Budget};
    use dekopon_process::Limits;

    use super::{BuiltinContext, CommandFailure, CommandResult, Run};

    pub(crate) struct NoCapabilities;

    impl CapabilityInvoker for NoCapabilities {
        fn granted(&self) -> Vec<String> {
            Vec::new()
        }

        fn invoke(
            &self,
            _: crate::CommandProposal,
            _streams: crate::Streams,
            _tree: &dekopon_process::TreeContext,
        ) -> CapabilityCallResult {
            CapabilityCallResult::NotFound
        }
    }

    pub(crate) fn run_builtin(
        builtin: Run,
        arguments: &[&str],
        input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        run_builtin_with(builtin, arguments, input, Limits::default())
    }

    pub(crate) fn run_builtin_with_invoker(
        builtin: Run,
        arguments: &[&str],
        invoker: &dyn CapabilityInvoker,
    ) -> Result<CommandResult, CommandFailure> {
        let mut budget = Budget::start(Limits::default());
        let mut context = BuiltinContext {
            invoker,
            budget: &mut budget,
            started_jobs: &[],
        };
        let arguments = arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect::<Vec<_>>();
        builtin(&mut context, &arguments, None)
    }

    pub(crate) fn run_builtin_with(
        builtin: Run,
        arguments: &[&str],
        input: Option<Value>,
        limits: Limits,
    ) -> Result<CommandResult, CommandFailure> {
        let invoker = NoCapabilities;
        let mut budget = Budget::start(limits);
        let mut context = BuiltinContext {
            invoker: &invoker,
            budget: &mut budget,
            started_jobs: &[],
        };
        let arguments = arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect::<Vec<_>>();
        builtin(&mut context, &arguments, input)
    }
}

#[cfg(test)]
mod tests {
    use crate::{CapabilityCallResult, CapabilityInvoker, Interpreter};
    use dekopon_process::ExitCode;
    use dekopon_process::Limits;

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
            let outcome = Interpreter::new(Limits::default(), crate::test_threads())
                .run(&format!("{name} --help"), &NoCapabilities);
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
        let run = |script: &str| {
            Interpreter::new(Limits::default(), crate::test_threads()).run(script, &NoCapabilities)
        };

        assert_eq!(
            run("grep --help").output,
            "grep: -v -i -c -n -E -q -A N -B N -C N"
        );
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
        let result = Interpreter::new(Limits::default(), crate::test_threads())
            .run("grep -o a <<EOF\na\nEOF", &NoCapabilities);
        assert_eq!(result.exit_code, ExitCode::SYNTAX);
        assert!(
            result.output.contains(
                "grep: option not yet supported: -o (supported: -v -i -c -n -E -q -A N -B N -C N)"
            ),
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
            _tree: &dekopon_process::TreeContext,
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
        let outcome = Interpreter::new(Limits::default(), crate::test_threads())
            .run("curl https://x", &HttpGranted);
        assert_eq!(outcome.exit_code, ExitCode::NOT_FOUND, "{}", outcome.output);
        assert_eq!(outcome.output, "dekopon-shell: curl: command not found");
        assert_eq!(outcome.capability_calls, 0);
    }
}

#[derive(Clone, Copy)]
pub(crate) enum RejectedCommand {
    Eval,
    Exec,
    Source,
    Dot,
    Trap,
    Fg,
    Bg,
    Declare,
    Export,
}
impl RejectedCommand {
    pub(crate) fn lookup(word: &str) -> Option<Self> {
        match word {
            "eval" => Some(Self::Eval),
            "exec" => Some(Self::Exec),
            "source" => Some(Self::Source),
            "." => Some(Self::Dot),
            "trap" => Some(Self::Trap),
            "fg" => Some(Self::Fg),
            "bg" => Some(Self::Bg),
            "declare" => Some(Self::Declare),
            "export" => Some(Self::Export),
            _ => None,
        }
    }
    #[cfg(test)]
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Eval => "eval",
            Self::Exec => "exec",
            Self::Source => "source",
            Self::Dot => ".",
            Self::Trap => "trap",
            Self::Fg => "fg",
            Self::Bg => "bg",
            Self::Declare => "declare",
            Self::Export => "export",
        }
    }
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Self::Eval => "`eval` is not supported; write the command out",
            Self::Exec => {
                "`exec` is excluded: this shell never replaces a process image and has no processes to replace"
            }
            Self::Source => "`source` is excluded: there is no filesystem to read scripts from",
            Self::Dot => "`.` (source) is excluded: there is no filesystem to read scripts from",
            Self::Trap => "`trap` is excluded: this shell has no signals",
            Self::Fg => {
                "`fg` is excluded: jobs run detached and report when they finish; use `wait %N`"
            }
            Self::Bg => {
                "`bg` is excluded: jobs run detached and report when they finish; use `wait %N`"
            }
            Self::Declare => {
                "`declare` is excluded: arrays and maps are real JSON values here, so `declare -A` has nothing to declare"
            }
            Self::Export => "`export` is excluded: there is no process environment to export into",
        }
    }
    #[cfg(test)]
    pub(crate) const ALL: &[Self] = &[
        Self::Eval,
        Self::Exec,
        Self::Source,
        Self::Dot,
        Self::Trap,
        Self::Fg,
        Self::Bg,
        Self::Declare,
        Self::Export,
    ];
}

pub(crate) enum Resolution {
    Function,
    Builtin(Builtin),
    ProviderCommand,
    Rejected(&'static str),
    NotFound,
}

pub(crate) fn resolve(
    word: &str,
    functions: &BTreeSet<String>,
    invoker: &dyn CapabilityInvoker,
) -> Resolution {
    if let Some(rejected) = RejectedCommand::lookup(word) {
        return Resolution::Rejected(rejected.reason());
    }
    if functions.contains(word) {
        return Resolution::Function;
    }
    if let Some(builtin) = lookup(word) {
        return Resolution::Builtin(builtin);
    }
    if invoker.has_command_word(word) {
        return Resolution::ProviderCommand;
    }
    Resolution::NotFound
}

#[cfg(test)]
mod resolution_tests;

#[derive(Clone, Copy)]
pub(crate) enum ControlWord {
    Break,
    Continue,
    Exit,
    Local,
    Read,
    Return,
    Set,
    Shift,
    Unset,
    Colon,
}
impl ControlWord {
    pub(crate) fn lookup(word: &str) -> Option<Self> {
        match word {
            "break" => Some(Self::Break),
            "continue" => Some(Self::Continue),
            "exit" => Some(Self::Exit),
            "local" => Some(Self::Local),
            "read" => Some(Self::Read),
            "return" => Some(Self::Return),
            "set" => Some(Self::Set),
            "shift" => Some(Self::Shift),
            "unset" => Some(Self::Unset),
            ":" => Some(Self::Colon),
            _ => None,
        }
    }
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Break => "break",
            Self::Continue => "continue",
            Self::Exit => "exit",
            Self::Local => "local",
            Self::Read => "read",
            Self::Return => "return",
            Self::Set => "set",
            Self::Shift => "shift",
            Self::Unset => "unset",
            Self::Colon => ":",
        }
    }
    #[cfg(test)]
    pub(crate) const ALL: &[Self] = &[
        Self::Break,
        Self::Continue,
        Self::Exit,
        Self::Local,
        Self::Read,
        Self::Return,
        Self::Set,
        Self::Shift,
        Self::Unset,
        Self::Colon,
    ];
}

#[cfg(test)]
impl Builtin {
    pub(crate) fn parse_text(
        self,
        arguments: &[String],
    ) -> Result<(text::stream::Operation, Option<String>), CommandFailure> {
        match self {
            Self::Grep => text::stream::grep(arguments),
            Self::Sed => text::stream::sed(arguments),
            Self::Echo
            | Self::Printf
            | Self::Test
            | Self::TestBracket
            | Self::True
            | Self::False
            | Self::Progress
            | Self::Sleep
            | Self::Cat
            | Self::Jobs
            | Self::Wait
            | Self::Kill
            | Self::Cap
            | Self::Head
            | Self::Tail
            | Self::Cut
            | Self::Uniq
            | Self::Wc
            | Self::Sort
            | Self::Jq
            | Self::Base64
            | Self::Xargs => panic!("not a grep/sed test"),
        }
    }
    pub(crate) fn run_text(
        self,
        arguments: &[String],
        reader: &mut dekopon_process::pipe::PipeReader<crate::interp::telemetry::StageObservation>,
        budget: &mut Budget,
        emit: impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
    ) -> Result<dekopon_process::ExitCode, CommandFailure> {
        match self {
            Self::Grep | Self::Sed => {
                text::stream::run_parsed(self.parse_text(arguments)?.0, reader, budget, emit)
            }
            Self::Cut => {
                text::extra::run_parsed(text::extra::cut(arguments)?.0, reader, budget, emit)
            }
            Self::Uniq => {
                text::extra::run_parsed(text::extra::uniq(arguments)?.0, reader, budget, emit)
            }
            Self::Wc => {
                text::extra::run_parsed(text::extra::wc(arguments)?.0, reader, budget, emit)
            }
            Self::Sort => {
                text::extra::run_parsed(text::extra::sort(arguments)?.0, reader, budget, emit)
            }
            Self::Echo
            | Self::Printf
            | Self::Test
            | Self::TestBracket
            | Self::True
            | Self::False
            | Self::Progress
            | Self::Sleep
            | Self::Cat
            | Self::Jobs
            | Self::Wait
            | Self::Kill
            | Self::Cap
            | Self::Head
            | Self::Tail
            | Self::Jq
            | Self::Base64
            | Self::Xargs => panic!("not a streaming text test"),
        }
    }
}
