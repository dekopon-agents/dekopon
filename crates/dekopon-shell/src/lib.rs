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
//! script are capability invocations, not operating-system processes.
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
//! One bound is *not* in [`limits`], because it applies before any budget exists: [`parser`] caps
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
//! command word inside it opens a `shell.command` span — no events — carrying the command name
//! (from a fixed vocabulary, or `<withheld>`), its resolution kind, its argument count, its exit
//! code, and a stable outcome label. A trace therefore reads as the ordered list of commands a
//! script actually executed rather than as one opaque "a script ran" entry.
//!
//! A model-authored `while` loop can execute tens of thousands of command words inside one tool
//! call, so only the first few hundred spans are emitted at INFO; the rest drop to DEBUG, and the
//! `shell.script` span's counters keep the totals in constant size either way.
//!
//! This crate depends on `tracing` and nothing else for that. It knows no exporter, no collector,
//! and no telemetry protocol; the embedding binary's own subscriber decides where these go, the
//! same way `curl` here links no HTTP client and only assembles a request for one capability. The
//! dependency does not compromise the synchronous design constraint below — `tracing` imposes no
//! async runtime and is routinely used from fully synchronous code — but it does mean spans may
//! leave the process, so `interp::telemetry` documents exactly which fields a command may carry:
//! never an argument value, and never a model-authored command word.
//!
//! # Example
//!
//! ```
//! use dekopon_shell::{CapabilityCallResult, CapabilityInvoker, Interpreter, Limits};
//! use serde_json::{Value, json};
//!
//! struct Fixture;
//!
//! impl CapabilityInvoker for Fixture {
//!     fn granted(&self) -> Vec<String> {
//!         vec!["echo.echo".to_owned()]
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
//!         assert_eq!(capability, "echo.echo");
//!         CapabilityCallResult::Succeeded(input)
//!     }
//! }
//!
//! let outcome = Interpreter::new(Limits::default())
//!     .run("echo.echo --message hi | jq -r .message", &Fixture);
//! assert_eq!(outcome.exit_code.get(), 0);
//! assert_eq!(outcome.output, "hi");
//! ```

#![forbid(unsafe_code)]

use std::sync::Arc;

use serde_json::Value;

pub mod ast;
mod builtins;
mod dispatch;
mod interp;
pub mod lexer;
pub mod limits;
pub mod parser;
pub mod value;

pub use limits::{
    DEFAULT_ALLOW_CLOCK, DEFAULT_MAX_CAPABILITY_CALLS, DEFAULT_MAX_OUTPUT_BYTES,
    DEFAULT_MAX_OUTPUT_LINES, DEFAULT_MAX_RECURSION_DEPTH, DEFAULT_MAX_STEPS,
    DEFAULT_MAX_VALUE_BYTES, DEFAULT_TIMEOUT, Limits,
};
pub use parser::ParseError;

use dekopon_core::SecretUseProposal;

/// Model-facing metadata for one capability, used by `cap --describe`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityDescription {
    /// Canonical capability identifier.
    pub capability: String,
    /// Human-readable operation description.
    pub description: String,
    /// Object-shaped JSON Schema for the capability's input.
    pub input_schema: Value,
}

/// The outcome of one capability invocation.
///
/// The variants mirror the exit-code mapping in [`ExitCode`]: a capability that ran and failed is
/// materially different from one that policy refused, which is different again from one that does
/// not exist. Collapsing them would hide an authorization refusal behind a generic failure.
#[derive(Clone, Debug, PartialEq)]
pub enum CapabilityCallResult {
    /// The capability ran and produced output.
    Succeeded(Value),
    /// Authorization refused the invocation. The capability was found but not permitted.
    Denied {
        /// Why the invocation was refused.
        reason: String,
    },
    /// The capability ran and failed.
    Failed {
        /// Failure detail.
        error: String,
    },
    /// No such capability is reachable from this session.
    NotFound,
}

/// What a provider did with one of its command words.
///
/// A provider command word behaves like its own command-line program: it can turn an argv into a
/// capability proposal, print its own help or usage text, or refuse the argv outright. Those are
/// kept apart because the interpreter charges and reports them differently, and apart again from
/// a run that never reached the provider's answer, which is not a usage error however it failed.
#[derive(Clone, Debug, PartialEq)]
pub enum CommandRun {
    /// The provider proposed a capability, authorized and charged exactly like a direct call.
    Proposed {
        /// The capability identifier the provider chose.
        capability: String,
        /// The input it assembled from the argv and stdin.
        input: Value,
    },
    /// The provider produced text of its own — help, a version, a usage error — and chose the
    /// exit status; no capability call is charged.
    Rendered {
        /// Text for the script's stdout, exactly as the provider wrote it.
        stdout: String,
        /// Text for the script's diagnostic stream, exactly as the provider wrote it.
        stderr: String,
        /// The exit status the provider chose, `0` for help and `2` for a usage error by
        /// convention.
        status: u8,
    },
    /// The provider declined the argv; reported as a usage error at exit `2`.
    Failed {
        /// Why the provider declined.
        message: String,
    },
    /// The run itself failed before the provider could answer — the broker was unreachable, the
    /// host refused or trapped, the task did not complete — and is reported like a capability
    /// that ran and errored, at exit `1`. Telling the model to fix its argv would be wrong.
    Errored {
        /// What failed, naming its cause; never a path.
        message: String,
    },
    /// The run was refused before or during dispatch — the session was cancelled underneath it —
    /// and is reported like a refused capability, at exit `126`.
    Denied {
        /// Why the run was refused.
        reason: String,
    },
}

/// The boundary between this interpreter and the real world.
///
/// Implementations decide what a "capability" is: a direct Wasm component call, a broker proposal,
/// or a test fixture. This crate never learns which.
pub trait CapabilityInvoker {
    /// Returns every capability identifier currently available to invoke.
    fn granted(&self) -> Vec<String>;

    /// Reports whether one capability identifier is available, for dispatch-time lookup.
    ///
    /// The default scans [`CapabilityInvoker::granted`]; override it when a cheaper lookup exists.
    fn is_granted(&self, capability: &str) -> bool {
        self.granted().iter().any(|granted| granted == capability)
    }

    /// Reports whether this session holds any capability in one provider namespace.
    ///
    /// Asked only about a word that is *not* granted, to tell "the model typed nonsense" from "the
    /// model keeps reaching for something we never granted". The default scans
    /// [`CapabilityInvoker::granted`]; override it when a cheaper lookup exists.
    fn grants_namespace(&self, namespace: &str) -> bool {
        self.granted().iter().any(|granted| {
            granted
                .split('.')
                .next()
                .is_some_and(|candidate| candidate == namespace)
        })
    }

    /// Returns the command words loaded providers contribute, for dispatch and the prompt.
    ///
    /// Filtered by the embedder to providers this session already holds a grant on, so a principal
    /// with no `gh.*` grant never sees the word and never reaches its rewrite.
    fn command_words(&self) -> Vec<String> {
        Vec::new()
    }

    /// Reports whether one word is a command word a loaded provider contributed.
    ///
    /// This is the membership test [`CapabilityInvoker::is_granted`] already provides for
    /// capabilities, and it is asked of *every* command word a script executes — a loop running
    /// thousands of commands asks it thousands of times. The default builds and scans
    /// [`CapabilityInvoker::command_words`]; override it when a cheaper lookup exists, because
    /// materializing that list per command is what this exists to avoid.
    fn has_command_word(&self, word: &str) -> bool {
        self.command_words()
            .iter()
            .any(|candidate| candidate == word)
    }

    /// Runs one provider command word against its argv and the text piped into it.
    ///
    /// `stdin` is what the script piped in, already rendered to text by the shell's display rule
    /// (strings verbatim, other values as compact JSON); `None` when nothing was piped. `None`
    /// coming back means no loaded provider owns the word; otherwise the [`CommandRun`] says
    /// whether the provider proposed a capability, rendered text of its own, or declined.
    ///
    /// Running the word grants nothing: a proposal is invoked on exactly the path a direct
    /// capability word takes, with the same budget, denial, and telemetry behavior, and rendered
    /// text is charged only against the script's value and output ceilings.
    fn run_command(&self, word: &str, argv: &[String], stdin: Option<&str>) -> Option<CommandRun> {
        let _ = (word, argv, stdin);
        None
    }

    /// Returns model-facing metadata for one capability, when the implementation has any.
    fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
        let _ = capability;
        None
    }

    /// Invokes one capability synchronously, carrying the optional typed secret-use intent.
    ///
    /// One method rather than two. A defaulted `invoke_with_secret_use` used to sit beside a
    /// two-argument `invoke`, and every wrapper forwarded the argument it could see: three of them
    /// forwarded `invoke` and inherited the other's deny-by-default, so a DRN proposal a
    /// broker-backed console or gateway session made was refused inside the process that made it
    /// and never reached the broker. There is now one method to forward, and forgetting it is a
    /// compile error rather than a refusal nobody asked for.
    ///
    /// An invoker with no authorizer behind it answers a `Some` proposal with
    /// [`secret_use_unsupported`]. Dropping the field and running the call anyway is the one thing
    /// it must not do: the caller asked for a credential the callee cannot prove it may use.
    ///
    /// This is deliberately synchronous: this crate carries no async runtime dependency, and the
    /// calling binary's model tool loop is untouched. An implementation that is asynchronous
    /// underneath bridges here itself, which is what `dekopon-run` does from its blocking task.
    fn invoke(
        &self,
        capability: &str,
        input: Value,
        secret_use: Option<SecretUseProposal>,
    ) -> CapabilityCallResult;
}

/// The refusal an invoker with no authorizer behind it owes a secret-use proposal.
///
/// A public DRN names a secret only the broker may resolve. An invoker that cannot reach one — a
/// direct Wasm registry, an empty local leg, a test fixture — refuses rather than dropping the
/// field and running the call as though it had never been asked for.
#[must_use]
pub fn secret_use_unsupported() -> CapabilityCallResult {
    CapabilityCallResult::Denied {
        reason: "secret references require a broker-backed capability".to_owned(),
    }
}

/// Forwards every method to the shared invoker behind the pointer.
///
/// This is what lets one broker leg be held by a console's shell pane and handed to that session's
/// dispatch at the same time. It replaced a hand-written forwarder that had to be kept in step with
/// the trait by hand and was not.
impl<T: CapabilityInvoker + ?Sized> CapabilityInvoker for Arc<T> {
    fn granted(&self) -> Vec<String> {
        self.as_ref().granted()
    }

    fn is_granted(&self, capability: &str) -> bool {
        self.as_ref().is_granted(capability)
    }

    fn grants_namespace(&self, namespace: &str) -> bool {
        self.as_ref().grants_namespace(namespace)
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

    fn invoke(
        &self,
        capability: &str,
        input: Value,
        secret_use: Option<SecretUseProposal>,
    ) -> CapabilityCallResult {
        self.as_ref().invoke(capability, input, secret_use)
    }
}

/// A script exit code.
///
/// The mapping is fixed and mirrors the conventions a model already knows from bash and coreutils.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ExitCode(u8);

impl ExitCode {
    /// A capability call, builtin, or script completed successfully.
    pub const SUCCESS: Self = Self(0);
    /// A capability call ran and errored, or a builtin reported a runtime failure.
    pub const FAILURE: Self = Self(1);
    /// A shell parse error or an exhausted resource limit.
    pub const SYNTAX: Self = Self(2);
    /// The script exceeded its wall-clock deadline, matching coreutils `timeout(1)`.
    pub const TIMEOUT: Self = Self(124);
    /// A capability was found but authorization refused it, matching bash's "cannot execute".
    pub const DENIED: Self = Self(126);
    /// An unknown builtin, or a capability not granted to this session.
    pub const NOT_FOUND: Self = Self(127);

    /// Wraps a raw status, mirroring bash's `N mod 256` wraparound for `exit N`.
    #[must_use]
    pub fn from_script_exit(status: i64) -> Self {
        Self(u8::try_from(status.rem_euclid(256)).unwrap_or(0))
    }

    /// Returns the numeric exit code.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Maps one capability call outcome onto its exit code.
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

/// Everything one script execution produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptOutcome {
    /// Combined stdout and stderr, already truncated to the configured ceilings.
    pub output: String,
    /// The script's exit code.
    pub exit_code: ExitCode,
    /// Whether output was dropped to stay under the ceilings.
    pub truncated: bool,
    /// Capability invocations this script drove.
    pub capability_calls: u32,
    /// Evaluation steps this script charged.
    pub steps: u64,
}

/// A configured script interpreter.
#[derive(Clone, Debug, Default)]
pub struct Interpreter {
    limits: Limits,
    curl_capability: Option<String>,
}

impl Interpreter {
    /// Creates an interpreter under the given bounds.
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            curl_capability: None,
        }
    }

    /// Selects the capability the `curl` builtin assembles requests for.
    ///
    /// `curl` speaks no HTTP itself. It is a flag parser that produces the
    /// `{uri, method, headers, body}` shape and hands it to this one capability through the same
    /// [`CapabilityInvoker::invoke`] path every other command uses. When no capability is
    /// configured, `curl` reports "command not found" like any ungranted capability.
    #[must_use]
    pub fn with_curl_capability(mut self, capability: Option<String>) -> Self {
        self.curl_capability = capability;
        self
    }

    /// Returns the configured bounds.
    #[must_use]
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Parses and evaluates one script.
    ///
    /// This never returns an error: a script failure is a script outcome. Parse errors and limit
    /// trips are reported through [`ScriptOutcome::output`] and [`ScriptOutcome::exit_code`].
    pub fn run(&self, script: &str, invoker: &dyn CapabilityInvoker) -> ScriptOutcome {
        interp::run(
            script,
            invoker,
            self.limits,
            self.curl_capability.as_deref(),
        )
    }
}

/// Parses and evaluates one script under default bounds.
pub fn run(script: &str, invoker: &dyn CapabilityInvoker) -> ScriptOutcome {
    Interpreter::new(Limits::default()).run(script, invoker)
}

/// Returns how many abandoned `jq` filter workers are still running in this process.
///
/// jaq offers no interruption point, so a filter that produces no output at all — `jq 'def f: f;
/// f'` — cannot be stopped when its script's deadline passes. Its worker is abandoned and spins
/// until the process exits. A filter that produces output stops at its next one, so this counter
/// falls back to zero on its own; what stays is the non-terminating kind, and each one is a core
/// this process will never get back. `jq` refuses to start new filters once too many have
/// accumulated.
///
/// Only abandoned workers are counted, and only they are threads this process cannot reclaim. An
/// ordinary filter is served by the worker its thread already has, which is reused for the next
/// one and released when that thread exits.
///
/// A long-lived embedder should surface this as a gauge. A one-shot runner can ignore it: the
/// process is about to exit anyway.
#[must_use]
pub fn abandoned_filter_workers() -> usize {
    builtins::jq::abandoned_workers()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use dekopon_core::{SecretDrn, SecretUseProposal};
    use serde_json::{Value, json};

    use super::{
        CapabilityCallResult, CapabilityDescription, CapabilityInvoker, CommandRun, ExitCode,
    };

    /// An invoker that overrides every defaulted method with an answer the default cannot give.
    ///
    /// Each override answers for something absent from `granted` or `command_words`, so a caller
    /// that reached the trait default instead of this implementation answers `false`, `None`, or
    /// an empty list — which is exactly the shape of the defect this fixture exists to catch.
    #[derive(Default)]
    struct RecordingInvoker {
        secret_uses: Mutex<Vec<Option<SecretUseProposal>>>,
    }

    impl CapabilityInvoker for RecordingInvoker {
        fn granted(&self) -> Vec<String> {
            vec!["echo.echo".to_owned()]
        }

        fn is_granted(&self, capability: &str) -> bool {
            capability == "gh.pr-view"
        }

        fn grants_namespace(&self, namespace: &str) -> bool {
            namespace == "gh"
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
            })
        }

        fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
            Some(CapabilityDescription {
                capability: capability.to_owned(),
                description: "recorded".to_owned(),
                input_schema: json!({"type": "object"}),
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
    }

    fn proposal() -> SecretUseProposal {
        SecretUseProposal::HttpBearer {
            secret: "drn:com.xrl:secret:prod:api/token"
                .parse::<SecretDrn>()
                .expect("canonical DRN"),
        }
    }

    /// The pointer is what lets one broker leg be held by a shell pane and by that session's
    /// dispatch at once, so a proposal crossing it has to arrive whole.
    ///
    /// A hand-written forwarder here once dropped the secret-use argument it could not see, and
    /// the DRN a `curl --user USER:${drn:…}` produced was refused inside the process that made it.
    /// The `Arc` blanket replaced that forwarder; nothing but this test now holds it to the same
    /// standard, its last other consumer having left with `dekopon-tui`.
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

    /// Six of this trait's methods have defaults, and a forwarder that omits one silently answers
    /// with the default instead of the invoker's own answer.
    ///
    /// Every override here answers for something the default would have to say `false`, `None`, or
    /// "nothing" about, so an omission fails rather than coinciding.
    #[test]
    fn an_arc_forwards_the_defaulted_methods_instead_of_inheriting_their_defaults() {
        let shared: Arc<dyn CapabilityInvoker> = Arc::new(RecordingInvoker::default());

        assert_eq!(shared.granted(), vec!["echo.echo".to_owned()]);
        // Not in `granted`: the default scan would refuse it.
        assert!(shared.is_granted("gh.pr-view"));
        assert!(shared.grants_namespace("gh"));
        // The default is an empty list and, through it, an empty membership test.
        assert_eq!(shared.command_words(), vec!["gh".to_owned()]);
        assert!(shared.has_command_word("gh-extra"));
        // Both default to `None`.
        assert_eq!(
            shared.run_command("gh", &["pr".to_owned()], Some("piped")),
            Some(CommandRun::Proposed {
                capability: "gh".to_owned(),
                input: json!({"argv": ["pr"], "stdin": "piped"}),
            })
        );
        assert_eq!(
            shared.describe("gh.pr-view").map(|it| it.description),
            Some("recorded".to_owned())
        );
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
                error: "boom".to_owned()
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
