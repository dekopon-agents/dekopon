//! A sandboxed, bash-flavored scripting language whose commands dispatch to Dekopon capabilities.
//!
//! Capability calls go through [`CapabilityInvoker`]. `jq` runs in a worker executable supplied by
//! the embedder, with an address-space limit on Linux; the worker is killed on deadline or cancel.
//!
//! # What this is for
//!
//! Exposing one model-facing tool schema per provider capability bloats a system prompt and forces
//! a model into many small round trips. A single scripting tool lets a model express a multi-step
//! plan — loops, conditionals, functions, JSON handling — in one tool call. The "commands" in that
//! script are builtins and the command words loaded providers contribute, each of which proposes a
//! capability invocation; `jq` alone uses a worker process.
//!
//! # Safety model
//!
//! The shell is a native tree-walking evaluator, with bounds enforced in [`limits`]:
//!
//! - a step budget covering statements, loop iterations, function calls, arithmetic nodes, and
//!   values pulled from a `jq` filter,
//! - a shell-function recursion depth cap,
//! - independent output byte and line ceilings with head-and-tail truncation,
//! - a wall-clock deadline, re-read on every step and around every capability call,
//! - a capability-invocation ceiling that is deliberately separate from the step budget,
//! - a cumulative ceiling on the value bytes a script may materialize, which is what bounds memory
//!   for a script that is cheap in steps and expensive in bytes. `jq` also has a worker address-space
//!   limit on Linux.
//!
//! One bound is *not* in [`limits`], because it applies before any budget exists: the parser caps
//! grammar nesting depth at a fixed ceiling. Parsing is recursive and runs on the native stack, so
//! without it a few kilobytes of nested `$( $( ... ) )` aborts the host process instead of
//! returning a [`ScriptOutcome`].
//!
//! The variable namespace is seeded only from the script's own assignments. This interpreter never
//! reads the host process environment — including through `jq`, whose standard library exports an
//! `env` filter that is deliberately not linked.
//!
//! A `jq` filter that does not yield is killed with its worker at the deadline or on cancel.
//!
//! # Observability
//!
//! Each script run opens one `shell.script` span carrying the totals for the whole run, and every
//! command word inside it opens a `shell.command` span — no events — carrying the command word
//! whoever wrote it, its resolution kind, its argv, the value piped into it, what it produced, its
//! exit code, and a stable outcome label. The argv, stdin, and output are each bounded by
//! `dekopon_core::bounded_attribute` beside a byte total. A trace therefore reads as the ordered
//! list of commands a script actually executed rather than as one opaque "a script ran" entry.
//!
//! A model-authored `while` loop can execute tens of thousands of command words inside one tool
//! call, and every one of them gets its span; the `shell.script` span's counters give the totals in
//! constant size beside them.
//!
//! This crate depends on `tracing` and nothing else for that. It knows no exporter, no collector,
//! and no telemetry protocol; the embedding binary's own subscriber decides where these go. The
//! dependency does not compromise the synchronous design constraint below — `tracing` imposes no
//! async runtime and is routinely used from fully synchronous code — but it does mean spans may
//! leave the process, so `interp::telemetry` documents exactly which fields a command carries.
//!
//! # Example
//!
//! ```
//! use dekopon_shell::{CapabilityCallResult, CapabilityInvoker, CommandRun, Interpreter, Limits};
//! use serde_json::{Value, json};
//!
//! /// One provider word, `probe`, whose `upper --text <s>` proposes `cli-probe.upper`.
//! struct Fixture;
//!
//! impl CapabilityInvoker for Fixture {
//!     fn granted(&self) -> Vec<String> {
//!         vec!["cli-probe.upper".to_owned()]
//!     }
//!
//!     fn command_words(&self) -> Vec<String> {
//!         vec!["probe".to_owned()]
//!     }
//!
//!     fn run_command(
//!         &self,
//!         word: &str,
//!         argv: &[String],
//!         _stdin: Option<&str>,
//!     ) -> Option<CommandRun> {
//!         if word != "probe" {
//!             return None;
//!         }
//!         Some(match argv {
//!             [command, flag, text] if command == "upper" && flag == "--text" => {
//!                 CommandRun::Proposed {
//!                     capability: "cli-probe.upper".to_owned(),
//!                     input: json!({ "text": text }),
//!                     secret_use: None,
//!                     report: None,
//!                 }
//!             }
//!             _ => CommandRun::Failed {
//!                 message: "usage: probe upper --text <text>".to_owned(),
//!             },
//!         })
//!     }
//!
//!     fn invoke(&self, proposal: dekopon_shell::CommandProposal) -> CapabilityCallResult {
//!         let dekopon_shell::CommandProposal { capability, input, secret_use, .. } = proposal;
//!         if secret_use.is_some() {
//!             return dekopon_shell::secret_use_unsupported();
//!         }
//!         assert_eq!(capability, "cli-probe.upper");
//!         let text = input["text"].as_str().unwrap_or_default().to_uppercase();
//!         CapabilityCallResult::Succeeded(json!({ "text": text }))
//!     }
//! }
//!
//! let outcome = Interpreter::new(Limits::default())
//!     .run("probe upper --text hi | cat", &Fixture);
//! assert_eq!(outcome.exit_code.get(), 0);
//! assert_eq!(outcome.output, r#"{"text":"HI"}"#);
//! ```

#![cfg_attr(test, allow(clippy::unwrap_used))]
#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
use std::{collections::BTreeMap, sync::Arc, time::Duration};

use serde_json::Value;

mod ast;
mod builtins;
mod dispatch;
mod interp;
mod jq_worker;
mod lexer;
pub use jq_worker::{run_jq_worker_if_requested, set_jq_worker_executable};
pub mod limits;
mod parser;
mod pipe;
mod proposal;
pub use proposal::{CommandProposal, CommandReport, CommandReportOutcome};
mod tree;
pub use tree::{CallBudget, RetainedBytes, TreeContext};
pub mod value;

pub use limits::{
    DEFAULT_MAX_CAPABILITY_CALLS, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_MAX_OUTPUT_LINES,
    DEFAULT_MAX_RECURSION_DEPTH, DEFAULT_MAX_STEPS, DEFAULT_MAX_VALUE_BYTES, DEFAULT_TIMEOUT,
    Limits,
};

use dekopon_core::{ProviderFailureDetail, SecretUseProposal};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityDescription {
    pub capability: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CapabilityCallResult {
    Succeeded(Value),
    Denied {
        reason: String,
    },
    Failed {
        error: String,
        /// The provider's own code and message are rendered after the classification, not instead
        /// of it, since a class alone can't tell a model whether to retry, wait, or stop.
        detail: Option<ProviderFailureDetail>,
    },
    NotFound,
}

#[derive(Debug)]
pub enum CommandRun {
    Proposed {
        capability: String,
        input: Value,
        /// The secret reference is intent, never authority: the broker authorizes it separately
        /// from the capability, and an invoker with no broker behind it must refuse it rather than
        /// treat it as approved.
        secret_use: Option<SecretUseProposal>,
        report: Option<CommandReport>,
    },
    Rendered {
        stdout: String,
        stderr: String,
        status: u8,
    },
    Failed {
        message: String,
    },
    /// A run that failed before the provider could answer is reported like an errored capability,
    /// not a usage error, since telling the model to fix its argv would be wrong.
    Errored {
        /// This names the cause of failure and must never be a filesystem path.
        message: String,
    },
    Denied {
        reason: String,
    },
}

pub trait CapabilityInvoker: Send + Sync {
    fn cancelled(&self) -> bool {
        false
    }

    fn granted(&self) -> Vec<String>;

    fn is_granted(&self, capability: &str) -> bool {
        self.granted().iter().any(|granted| granted == capability)
    }

    fn command_words(&self) -> Vec<String> {
        Vec::new()
    }

    fn command_word_help(&self) -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    fn has_command_word(&self, word: &str) -> bool {
        self.command_words()
            .iter()
            .any(|candidate| candidate == word)
    }

    /// Running a command word grants nothing: any proposal it makes is invoked through the same
    /// budget, denial, and telemetry path as every other capability call.
    fn run_command(&self, word: &str, argv: &[String], stdin: Option<&str>) -> Option<CommandRun> {
        let _ = (word, argv, stdin);
        None
    }

    fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
        let _ = capability;
        None
    }

    fn note(&self, text: &str, eta: Option<Duration>) {
        let _ = (text, eta);
    }

    fn script_finished(&self) {}

    fn invoke(&self, proposal: CommandProposal) -> CapabilityCallResult;
}

#[must_use]
pub fn secret_use_unsupported() -> CapabilityCallResult {
    CapabilityCallResult::Denied {
        reason: "secret references require a broker-backed capability".to_owned(),
    }
}

impl<T: CapabilityInvoker + ?Sized> CapabilityInvoker for Arc<T> {
    fn cancelled(&self) -> bool {
        self.as_ref().cancelled()
    }

    fn granted(&self) -> Vec<String> {
        self.as_ref().granted()
    }

    fn is_granted(&self, capability: &str) -> bool {
        self.as_ref().is_granted(capability)
    }

    fn command_words(&self) -> Vec<String> {
        self.as_ref().command_words()
    }

    fn command_word_help(&self) -> BTreeMap<String, String> {
        self.as_ref().command_word_help()
    }

    fn has_command_word(&self, word: &str) -> bool {
        self.as_ref().has_command_word(word)
    }

    fn run_command(&self, word: &str, argv: &[String], stdin: Option<&str>) -> Option<CommandRun> {
        self.as_ref().run_command(word, argv, stdin)
    }

    fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
        self.as_ref().describe(capability)
    }

    fn note(&self, text: &str, eta: Option<Duration>) {
        self.as_ref().note(text, eta);
    }

    fn script_finished(&self) {
        self.as_ref().script_finished();
    }

    fn invoke(&self, proposal: CommandProposal) -> CapabilityCallResult {
        self.as_ref().invoke(proposal)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ExitCode(u8);

impl ExitCode {
    pub const SUCCESS: Self = Self(0);
    pub const FAILURE: Self = Self(1);
    pub const SYNTAX: Self = Self(2);
    pub const TIMEOUT: Self = Self(124);
    pub const CANCELLED: Self = Self(130);
    pub const DENIED: Self = Self(126);
    pub const NOT_FOUND: Self = Self(127);

    #[must_use]
    pub fn from_script_exit(status: i64) -> Self {
        Self(u8::try_from(status.rem_euclid(256)).unwrap_or(0))
    }

    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    #[must_use]
    pub const fn from_capability_result(result: &CapabilityCallResult) -> Self {
        match result {
            CapabilityCallResult::Succeeded(_) => Self::SUCCESS,
            CapabilityCallResult::Failed { .. } => Self::FAILURE,
            CapabilityCallResult::Denied { .. } => Self::DENIED,
            CapabilityCallResult::NotFound => Self::NOT_FOUND,
        }
    }
}

impl From<ExitCode> for u8 {
    fn from(code: ExitCode) -> Self {
        code.0
    }
}

impl From<u8> for ExitCode {
    fn from(code: u8) -> Self {
        Self(code)
    }
}

impl std::fmt::Display for ExitCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptOutcome {
    pub output: String,
    pub exit_code: ExitCode,
    pub truncated: bool,
    pub capability_calls: u32,
    pub steps: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Interpreter {
    limits: Limits,
}

impl Interpreter {
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self { limits }
    }

    #[must_use]
    pub fn limits(&self) -> Limits {
        self.limits
    }

    pub fn run(&self, script: &str, invoker: &dyn CapabilityInvoker) -> ScriptOutcome {
        self.run_with_tree(
            script,
            invoker,
            &TreeContext::new(
                self.limits,
                CallBudget::new(self.limits.max_capability_calls),
            ),
        )
    }

    pub fn run_with_tree(
        &self,
        script: &str,
        invoker: &dyn CapabilityInvoker,
        tree: &TreeContext,
    ) -> ScriptOutcome {
        interp::run_with_tree(script, None, invoker, self.limits, tree)
    }

    pub fn run_with_prev(
        &self,
        script: &str,
        prev: &str,
        invoker: &dyn CapabilityInvoker,
    ) -> ScriptOutcome {
        interp::run(script, Some(prev), invoker, self.limits)
    }
}

pub fn run(script: &str, invoker: &dyn CapabilityInvoker) -> ScriptOutcome {
    Interpreter::new(Limits::default()).run(script, invoker)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU32, Ordering},
        },
        time::{Duration, Instant},
    };

    use dekopon_core::{SecretDrn, SecretUseProposal};
    use serde_json::{Value, json};

    use super::{
        CallBudget, CapabilityCallResult, CapabilityDescription, CapabilityInvoker, CommandRun,
        ExitCode, Interpreter, Limits, TreeContext,
    };

    #[derive(Default)]
    struct RecordingInvoker {
        secret_uses: Mutex<Vec<Option<SecretUseProposal>>>,
        scripts_finished: AtomicU32,
        tree: Option<TreeContext>,
        retained: Mutex<Vec<u64>>,
    }

    impl CapabilityInvoker for RecordingInvoker {
        fn granted(&self) -> Vec<String> {
            vec!["cli-probe.upper".to_owned()]
        }

        fn is_granted(&self, capability: &str) -> bool {
            capability == "gh.pr-view" || capability == "gh-extra"
        }

        fn command_words(&self) -> Vec<String> {
            vec!["gh".to_owned()]
        }

        fn command_word_help(&self) -> BTreeMap<String, String> {
            BTreeMap::from([("gh".to_owned(), "gh: recorded help".to_owned())])
        }

        fn has_command_word(&self, word: &str) -> bool {
            matches!(word, "gh-extra" | "retained" | "render")
        }

        fn run_command(
            &self,
            word: &str,
            argv: &[String],
            stdin: Option<&str>,
        ) -> Option<CommandRun> {
            if matches!(word, "retained" | "render") {
                let stdout = if word == "retained" {
                    self.retained
                        .lock()
                        .unwrap()
                        .push(self.tree.as_ref()?.value_bytes());
                    String::new()
                } else {
                    "abcdefgh".to_owned()
                };
                return Some(CommandRun::Rendered {
                    stdout,
                    stderr: String::new(),
                    status: 0,
                });
            }
            Some(CommandRun::Proposed {
                capability: word.to_owned(),
                input: json!({ "argv": argv, "stdin": stdin }),
                secret_use: None,
                report: None,
            })
        }

        fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
            Some(CapabilityDescription {
                capability: capability.to_owned(),
                description: "recorded".to_owned(),
            })
        }

        fn invoke(&self, proposal: super::CommandProposal) -> CapabilityCallResult {
            let input = proposal.input;
            let secret_use = proposal.secret_use;
            self.secret_uses
                .lock()
                .expect("recorded secret uses")
                .push(secret_use);
            CapabilityCallResult::Succeeded(input)
        }

        fn script_finished(&self) {
            self.scripts_finished.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct Cancellation(Arc<AtomicBool>);

    impl CapabilityInvoker for Cancellation {
        fn cancelled(&self) -> bool {
            self.0.load(Ordering::Relaxed)
        }
        fn granted(&self) -> Vec<String> {
            Vec::new()
        }
        fn invoke(&self, _: super::CommandProposal) -> CapabilityCallResult {
            CapabilityCallResult::NotFound
        }
    }

    #[test]
    fn a_cancelled_loop_stops_before_exhausting_steps() {
        let cancelled = Arc::new(AtomicBool::new(true));
        let limits = Limits {
            max_steps: 100_000,
            ..Limits::default()
        };
        let outcome =
            Interpreter::new(limits).run("while true; do :; done", &Cancellation(cancelled));
        assert_eq!(outcome.exit_code, ExitCode::CANCELLED);
        assert_eq!(
            super::interp::telemetry::fatal_outcome(&super::builtins::FatalError::Limit(
                super::limits::LimitExceeded::Cancelled
            )),
            "cancelled"
        );
        assert!(outcome.steps < 100);
    }

    #[test]
    fn a_sleep_wakes_for_cancellation_without_waiting_for_the_deadline() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let flip = Arc::clone(&cancelled);
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            flip.store(true, Ordering::Relaxed);
        });
        let start = Instant::now();
        let outcome = Interpreter::new(Limits {
            timeout: Duration::from_secs(310),
            ..Limits::default()
        })
        .run("sleep 300", &Cancellation(cancelled));
        worker.join().expect("cancel worker");
        assert_eq!(outcome.exit_code, ExitCode::CANCELLED);
        assert!(start.elapsed() < Duration::from_millis(1500));
    }

    #[test]
    fn interpreters_in_one_tree_share_calls_and_deadline() {
        let limits = Limits {
            timeout: Duration::from_millis(80),
            max_capability_calls: 1,
            ..Limits::default()
        };
        let tree = TreeContext::new(limits, CallBudget::new(1));
        let invoker = RecordingInvoker::default();
        let interpreter = Interpreter::new(limits);
        assert_eq!(
            interpreter
                .run_with_tree("gh-extra one", &invoker, &tree)
                .capability_calls,
            1
        );
        let second_interpreter = Interpreter::new(limits);
        let second = second_interpreter.run_with_tree("gh-extra two", &invoker, &tree);
        assert_eq!(second.exit_code, ExitCode::SYNTAX);
        assert_eq!(tree.calls().used(), 1);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            second_interpreter
                .run_with_tree("echo late", &invoker, &tree)
                .exit_code,
            ExitCode::TIMEOUT
        );
    }

    #[test]
    fn replacements_and_scope_drops_refund_retained_bytes() {
        let limits = Limits {
            max_value_bytes: 128,
            ..Limits::default()
        };
        let tree = TreeContext::new(limits, CallBudget::new(1));
        let script = "f() { local y=abcdefgh; }; x=abcdefgh; ".to_owned()
            + &"x=ijklmnop; f; echo abcdefgh > buf; ".repeat(20);
        let outcome =
            Interpreter::new(limits).run_with_tree(&script, &RecordingInvoker::default(), &tree);
        assert_eq!(outcome.exit_code, ExitCode::SUCCESS, "{}", outcome.output);
        assert_eq!(tree.value_bytes(), 0);
    }

    #[test]
    fn substitution_arguments_keep_their_charges_until_the_command_finishes() {
        let invoker = RecordingInvoker::default();
        let limits = Limits {
            max_value_bytes: 64,
            ..Limits::default()
        };
        let tree = TreeContext::new(limits, CallBudget::new(4));
        let word = "\"$(printf 12345678901234567890123456789012)\"";
        let interpreter = Interpreter::new(limits);
        let failed = interpreter.run_with_tree(&format!("gh-extra {word} {word}"), &invoker, &tree);
        assert_eq!(failed.exit_code, ExitCode::SYNTAX, "{}", failed.output);
        assert!(invoker.secret_uses.lock().unwrap().is_empty());
        assert_eq!(tree.value_bytes(), 0);
        let script = format!("gh-extra {word}; gh-extra {word}");
        let next = interpreter.run_with_tree(&script, &invoker, &tree);
        assert_eq!(next.exit_code, ExitCode::SUCCESS, "{}", next.output);
        assert_eq!(tree.value_bytes(), 0);
    }

    #[test]
    fn substitution_refunds_trailing_line_feeds_before_the_next_capture() {
        let limits = Limits {
            max_value_bytes: 41,
            ..Limits::default()
        };
        let tree = TreeContext::new(limits, CallBudget::new(1));
        let invoker = RecordingInvoker {
            tree: Some(tree.clone()),
            ..RecordingInvoker::default()
        };
        let outcome = Interpreter::new(limits).run_with_tree(
            "v=$(printf 'x\\n\\n'); retained; w=$(printf 12345678); retained",
            &invoker,
            &tree,
        );
        assert_eq!(outcome.exit_code, ExitCode::SUCCESS, "{outcome:?}");
        assert_eq!(*invoker.retained.lock().unwrap(), vec![17, 41]);
        assert_eq!(tree.value_bytes(), 0);
    }

    #[test]
    fn redirect_keeps_source_charge_while_copying_to_a_buffer() {
        let limits = Limits {
            max_value_bytes: 15,
            ..Limits::default()
        };
        let tree = TreeContext::new(limits, CallBudget::new(1));
        let invoker = RecordingInvoker {
            tree: Some(tree.clone()),
            ..RecordingInvoker::default()
        };
        let outcome = Interpreter::new(limits).run_with_tree("render > buf", &invoker, &tree);
        assert_ne!(outcome.exit_code, ExitCode::SUCCESS, "{outcome:?}");
        assert!(outcome.output.contains("bytes of values"), "{outcome:?}");
        assert_eq!(tree.value_bytes(), 0);
    }

    #[test]
    fn storage_adopts_moved_charges_and_refunds_unset_and_replacement() {
        for (maximum, script, expected) in [
            (
                32,
                "x=$(printf abcdefgh); retained; x=a; retained; unset x; retained",
                vec![24, 17, 0],
            ),
            (
                31,
                "printf abcdefgh > buf; retained; : > buf; retained; render >> buf; retained; : > buf; retained",
                vec![8, 0, 8, 0],
            ),
            (
                48,
                "render > buf; retained; render >> buf; retained; : > buf; retained",
                vec![8, 16, 0],
            ),
            (32, "cat <<EOF\n$(printf abcdefgh)\nEOF\nretained", vec![0]),
        ] {
            let limits = Limits {
                max_value_bytes: maximum,
                ..Limits::default()
            };
            let tree = TreeContext::new(limits, CallBudget::new(1));
            let invoker = RecordingInvoker {
                tree: Some(tree.clone()),
                ..RecordingInvoker::default()
            };
            let outcome = Interpreter::new(limits).run_with_tree(script, &invoker, &tree);
            assert_eq!(outcome.exit_code, ExitCode::SUCCESS, "{}", outcome.output);
            assert_eq!(*invoker.retained.lock().unwrap(), expected, "{script}");
            assert_eq!(tree.value_bytes(), 0);
        }
    }

    fn proposal() -> SecretUseProposal {
        SecretUseProposal::HttpBearer {
            secret: "drn:com.xrl:secret:prod:api/token"
                .parse::<SecretDrn>()
                .expect("canonical DRN"),
        }
    }

    #[test]
    fn an_arc_hands_a_secret_use_proposal_to_the_invoker_behind_it_unchanged() {
        let inner = Arc::new(RecordingInvoker::default());
        let shared: Arc<dyn CapabilityInvoker> = Arc::clone(&inner) as Arc<dyn CapabilityInvoker>;

        assert_eq!(
            shared.invoke(super::CommandProposal::new(
                "http-probe.fetch",
                json!({"url": "https://x"}),
                None
            )),
            CapabilityCallResult::Succeeded(json!({"url": "https://x"}))
        );
        assert_eq!(
            shared.invoke(super::CommandProposal::new(
                "http-probe.fetch",
                json!({}),
                Some(proposal())
            )),
            CapabilityCallResult::Succeeded(json!({}))
        );

        assert_eq!(
            *inner.secret_uses.lock().expect("recorded secret uses"),
            vec![None, Some(proposal())],
            "the pointer altered a proposal on its way to the invoker behind it"
        );
    }

    #[test]
    fn an_arc_forwards_the_defaulted_methods_instead_of_inheriting_their_defaults() {
        let inner = Arc::new(RecordingInvoker::default());
        let shared: Arc<dyn CapabilityInvoker> = Arc::clone(&inner) as Arc<dyn CapabilityInvoker>;

        shared.script_finished();
        assert_eq!(inner.scripts_finished.load(Ordering::Relaxed), 1);

        assert_eq!(shared.granted(), vec!["cli-probe.upper".to_owned()]);
        assert!(shared.is_granted("gh.pr-view"));
        assert_eq!(shared.command_words(), vec!["gh".to_owned()]);
        assert_eq!(
            shared.command_word_help(),
            BTreeMap::from([("gh".to_owned(), "gh: recorded help".to_owned())])
        );
        assert!(shared.has_command_word("gh-extra"));
        let Some(CommandRun::Proposed {
            capability,
            input,
            secret_use,
            report,
        }) = shared.run_command("gh", &["pr".to_owned()], Some("piped"))
        else {
            panic!("expected proposal")
        };
        assert_eq!(capability, "gh");
        assert_eq!(input, json!({"argv": ["pr"], "stdin": "piped"}));
        assert!(secret_use.is_none());
        assert!(report.is_none());
        assert_eq!(
            shared.describe("gh.pr-view").map(|it| it.description),
            Some("recorded".to_owned())
        );
    }

    struct ProposingInvoker {
        secret_use: Option<SecretUseProposal>,
        invocations: Mutex<Vec<(String, Value, Option<SecretUseProposal>)>>,
    }

    impl ProposingInvoker {
        fn proposing(secret_use: Option<SecretUseProposal>) -> Self {
            Self {
                secret_use,
                invocations: Mutex::new(Vec::new()),
            }
        }
    }

    impl CapabilityInvoker for ProposingInvoker {
        fn granted(&self) -> Vec<String> {
            vec!["http-probe.fetch".to_owned()]
        }

        fn command_words(&self) -> Vec<String> {
            vec!["httpprobe".to_owned()]
        }

        fn run_command(
            &self,
            word: &str,
            argv: &[String],
            stdin: Option<&str>,
        ) -> Option<CommandRun> {
            (word == "httpprobe").then(|| CommandRun::Proposed {
                capability: "http-probe.fetch".to_owned(),
                input: json!({ "argv": argv, "stdin": stdin }),
                secret_use: self.secret_use.clone(),
                report: None,
            })
        }

        fn invoke(&self, proposal: super::CommandProposal) -> CapabilityCallResult {
            let capability = proposal.capability;
            let input = proposal.input;
            let secret_use = proposal.secret_use;
            self.invocations
                .lock()
                .expect("recorded invocations")
                .push((capability.to_owned(), input, secret_use));
            CapabilityCallResult::Succeeded(json!({"status": 200}))
        }
    }

    #[test]
    fn a_provider_proposal_hands_its_secret_use_to_invoke() {
        for secret_use in [Some(proposal()), None] {
            let invoker = ProposingInvoker::proposing(secret_use.clone());
            let outcome = Interpreter::new(Limits::default())
                .run("httpprobe fetch --url https://x", &invoker);
            assert_eq!(outcome.exit_code, ExitCode::SUCCESS, "{}", outcome.output);
            assert_eq!(outcome.output, r#"{"status":200}"#);
            assert_eq!(outcome.capability_calls, 1);
            assert_eq!(
                *invoker.invocations.lock().expect("recorded invocations"),
                vec![(
                    "http-probe.fetch".to_owned(),
                    json!({"argv": ["fetch", "--url", "https://x"], "stdin": null}),
                    secret_use,
                )],
                "the proposal's secret use did not reach invoke unchanged"
            );
        }
    }

    #[test]
    fn exit_codes_follow_the_documented_mapping() {
        assert_eq!(ExitCode::SUCCESS.get(), 0);
        assert_eq!(ExitCode::FAILURE.get(), 1);
        assert_eq!(ExitCode::SYNTAX.get(), 2);
        assert_eq!(ExitCode::TIMEOUT.get(), 124);
        assert_eq!(ExitCode::DENIED.get(), 126);
        assert_eq!(ExitCode::NOT_FOUND.get(), 127);
    }

    #[test]
    fn capability_results_map_onto_distinct_codes() {
        assert_eq!(
            ExitCode::from_capability_result(&CapabilityCallResult::Succeeded(
                serde_json::Value::Null
            )),
            ExitCode::SUCCESS
        );
        assert_eq!(
            ExitCode::from_capability_result(&CapabilityCallResult::Failed {
                error: "boom".to_owned(),
                detail: None
            }),
            ExitCode::FAILURE
        );
        assert_eq!(
            ExitCode::from_capability_result(&CapabilityCallResult::Denied {
                reason: "policy".to_owned()
            }),
            ExitCode::DENIED
        );
        assert_eq!(
            ExitCode::from_capability_result(&CapabilityCallResult::NotFound),
            ExitCode::NOT_FOUND
        );
    }

    #[test]
    fn script_exit_wraps_like_bash() {
        assert_eq!(ExitCode::from_script_exit(0).get(), 0);
        assert_eq!(ExitCode::from_script_exit(7).get(), 7);
        assert_eq!(ExitCode::from_script_exit(256).get(), 0);
        assert_eq!(ExitCode::from_script_exit(257).get(), 1);
        assert_eq!(ExitCode::from_script_exit(-1).get(), 255);
    }
}
