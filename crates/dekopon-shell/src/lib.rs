//! A sandboxed, bash-flavored scripting language whose commands dispatch to Dekopon capabilities.
//!
//! This crate is a pure interpreter library. It has no notion of Wasmtime, provider components, the
//! broker, HTTP, the filesystem, or the process environment. Everything a script can reach outside
//! its own value space goes through one seam, [`CapabilityInvoker`], which the embedding binary
//! implements.
//!
//! # What this is for
//!
//! Exposing one model-facing tool schema per provider capability bloats a system prompt and forces
//! a model into many small round trips. A single scripting tool lets a model express a multi-step
//! plan — loops, conditionals, functions, JSON handling — in one tool call. The "commands" in that
//! script are builtins and the command words loaded providers contribute, each of which proposes a
//! capability invocation; none is an operating-system process.
//!
//! # Safety model
//!
//! There is no operating-system sandbox here. This is a native tree-walking evaluator, so every
//! bound is hand-built in [`limits`]:
//!
//! - a step budget covering statements, loop iterations, function calls, arithmetic nodes, and
//!   values pulled from a `jq` filter,
//! - a shell-function recursion depth cap,
//! - independent output byte and line ceilings with head-and-tail truncation,
//! - a wall-clock deadline, re-read on every step and around every capability call,
//! - a capability-invocation ceiling that is deliberately separate from the step budget,
//! - a cumulative ceiling on the value bytes a script may materialize, which is what bounds memory
//!   for a script that is cheap in steps and expensive in bytes.
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
//! One residual is stated rather than hidden: a `jq` filter that never produces an output cannot be
//! stopped cooperatively, so one that outlives its script keeps a thread busy. See
//! [`abandoned_filter_workers`].
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
//!                 }
//!             }
//!             _ => CommandRun::Failed {
//!                 message: "usage: probe upper --text <text>".to_owned(),
//!             },
//!         })
//!     }
//!
//!     fn invoke(
//!         &self,
//!         capability: &str,
//!         input: Value,
//!         secret_use: Option<dekopon_core::SecretUseProposal>,
//!     ) -> CapabilityCallResult {
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
//!     .run("probe upper --text hi | jq -r .text", &Fixture);
//! assert_eq!(outcome.exit_code.get(), 0);
//! assert_eq!(outcome.output, "HI");
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
use std::sync::Arc;

use serde_json::Value;

mod ast;
mod builtins;
mod dispatch;
mod interp;
mod lexer;
pub mod limits;
mod parser;
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

#[derive(Clone, Debug, PartialEq)]
pub enum CommandRun {
    Proposed {
        capability: String,
        input: Value,
        /// The secret reference is intent, never authority: the broker authorizes it separately
        /// from the capability, and an invoker with no broker behind it must refuse it rather than
        /// treat it as approved.
        secret_use: Option<SecretUseProposal>,
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

pub trait CapabilityInvoker {
    fn granted(&self) -> Vec<String>;

    fn is_granted(&self, capability: &str) -> bool {
        self.granted().iter().any(|granted| granted == capability)
    }

    fn command_words(&self) -> Vec<String> {
        Vec::new()
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

    fn script_finished(&self) {}

    fn invoke(
        &self,
        capability: &str,
        input: Value,
        secret_use: Option<SecretUseProposal>,
    ) -> CapabilityCallResult;
}

#[must_use]
pub fn secret_use_unsupported() -> CapabilityCallResult {
    CapabilityCallResult::Denied {
        reason: "secret references require a broker-backed capability".to_owned(),
    }
}

impl<T: CapabilityInvoker + ?Sized> CapabilityInvoker for Arc<T> {
    fn granted(&self) -> Vec<String> {
        self.as_ref().granted()
    }

    fn is_granted(&self, capability: &str) -> bool {
        self.as_ref().is_granted(capability)
    }

    fn command_words(&self) -> Vec<String> {
        self.as_ref().command_words()
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

    fn script_finished(&self) {
        self.as_ref().script_finished();
    }

    fn invoke(
        &self,
        capability: &str,
        input: Value,
        secret_use: Option<SecretUseProposal>,
    ) -> CapabilityCallResult {
        self.as_ref().invoke(capability, input, secret_use)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ExitCode(u8);

impl ExitCode {
    pub const SUCCESS: Self = Self(0);
    pub const FAILURE: Self = Self(1);
    pub const SYNTAX: Self = Self(2);
    pub const TIMEOUT: Self = Self(124);
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
        interp::run(script, invoker, self.limits)
    }
}

pub fn run(script: &str, invoker: &dyn CapabilityInvoker) -> ScriptOutcome {
    Interpreter::new(Limits::default()).run(script, invoker)
}

/// A non-terminating jq filter has no interruption point, so its worker thread is abandoned and
/// spins forever after the deadline, permanently costing this process a CPU core.
#[must_use]
pub fn abandoned_filter_workers() -> usize {
    builtins::jq::abandoned_workers()
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    };

    use dekopon_core::{SecretDrn, SecretUseProposal};
    use serde_json::{Value, json};

    use super::{
        CapabilityCallResult, CapabilityDescription, CapabilityInvoker, CommandRun, ExitCode,
        Interpreter, Limits,
    };

    #[derive(Default)]
    struct RecordingInvoker {
        secret_uses: Mutex<Vec<Option<SecretUseProposal>>>,
        scripts_finished: AtomicU32,
    }

    impl CapabilityInvoker for RecordingInvoker {
        fn granted(&self) -> Vec<String> {
            vec!["cli-probe.upper".to_owned()]
        }

        fn is_granted(&self, capability: &str) -> bool {
            capability == "gh.pr-view"
        }

        fn command_words(&self) -> Vec<String> {
            vec!["gh".to_owned()]
        }

        fn has_command_word(&self, word: &str) -> bool {
            word == "gh-extra"
        }

        fn run_command(
            &self,
            word: &str,
            argv: &[String],
            stdin: Option<&str>,
        ) -> Option<CommandRun> {
            Some(CommandRun::Proposed {
                capability: word.to_owned(),
                input: json!({ "argv": argv, "stdin": stdin }),
                secret_use: None,
            })
        }

        fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
            Some(CapabilityDescription {
                capability: capability.to_owned(),
                description: "recorded".to_owned(),
            })
        }

        fn invoke(
            &self,
            _capability: &str,
            input: Value,
            secret_use: Option<SecretUseProposal>,
        ) -> CapabilityCallResult {
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
            shared.invoke("http-probe.fetch", json!({"url": "https://x"}), None),
            CapabilityCallResult::Succeeded(json!({"url": "https://x"}))
        );
        assert_eq!(
            shared.invoke("http-probe.fetch", json!({}), Some(proposal())),
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
        assert!(shared.has_command_word("gh-extra"));
        assert_eq!(
            shared.run_command("gh", &["pr".to_owned()], Some("piped")),
            Some(CommandRun::Proposed {
                capability: "gh".to_owned(),
                input: json!({"argv": ["pr"], "stdin": "piped"}),
                secret_use: None,
            })
        );
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
            })
        }

        fn invoke(
            &self,
            capability: &str,
            input: Value,
            secret_use: Option<SecretUseProposal>,
        ) -> CapabilityCallResult {
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
