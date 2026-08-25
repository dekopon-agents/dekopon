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
//!     fn invoke(&self, capability: &str, input: Value) -> CapabilityCallResult {
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

    /// Rewrites one provider command word's argv into a capability proposal.
    ///
    /// `Some(Ok((capability, input)))` names the capability to invoke; `Some(Err(message))` is a
    /// usage error the provider chose to report; `None` means no loaded provider owns the word.
    ///
    /// The rewrite grants nothing: what comes back is a proposal, invoked on exactly the path a
    /// direct capability word takes, with the same budget, denial, and telemetry behavior.
    fn resolve_command(
        &self,
        word: &str,
        argv: &[String],
    ) -> Option<Result<(String, Value), String>> {
        let _ = (word, argv);
        None
    }

    /// Returns model-facing metadata for one capability, when the implementation has any.
    fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
        let _ = capability;
        None
    }

    /// Invokes one capability synchronously.
    ///
    /// This is deliberately synchronous: this crate carries no async runtime dependency, and the
    /// calling binary's model tool loop is untouched. An implementation that is asynchronous
    /// underneath bridges here itself, which is what `dekopon-run` does from its blocking task.
    fn invoke(&self, capability: &str, input: Value) -> CapabilityCallResult;

    /// Invokes one capability with optional typed secret-use intent.
    ///
    /// Direct and test invokers keep the safe default: a proposal selecting a DRN is refused. Only
    /// a broker-backed leg may override this and forward the typed field for fresh authorization.
    fn invoke_with_secret_use(
        &self,
        capability: &str,
        input: Value,
        secret_use: Option<SecretUseProposal>,
    ) -> CapabilityCallResult {
        match secret_use {
            None => self.invoke(capability, input),
            Some(_) => CapabilityCallResult::Denied {
                reason: "secret references require a broker-backed capability".to_owned(),
            },
        }
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
    use super::{CapabilityCallResult, ExitCode};

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
