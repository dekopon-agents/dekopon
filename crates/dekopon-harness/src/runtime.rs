//! Unprivileged shell dispatch and direct-first broker adapters.

#[cfg(unix)]
use std::{
    collections::hash_map::RandomState,
    collections::{BTreeMap, BTreeSet},
    hash::{BuildHasher as _, Hasher as _},
    sync::atomic::{AtomicU32, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use dekopon_broker_protocol::TraceParent;
#[cfg(unix)]
use dekopon_broker_protocol::{
    Attestation, BrokerClient, ChatMemorySurface, ClientError, CommandRunOutcome,
    ERROR_UNAUTHENTICATED, InvocationOutcome, InvocationRequest,
};
#[cfg(unix)]
use dekopon_core::{CapabilityId, IdentifierError, InvocationId, TraceId};
use dekopon_process::ProcessOutcome;
#[cfg(unix)]
use dekopon_process::{CancelSignal, ProcessMetadata, ProcessRun, process_fn};
#[cfg(unix)]
use dekopon_shell::CapabilityDescription;
use dekopon_shell::{
    CapabilityCallResult, CapabilityInvoker, CommandRun, Interpreter, Limits as ShellLimits,
    ScriptOutcome,
};
use serde_json::Value;
#[cfg(unix)]
use thiserror::Error;

use crate::{
    bootstrap::{BootstrapError, CapabilitySnapshot},
    meta::EffectiveCapabilityView,
};

/// Script execution boundary consumed by the prompt loop.
///
/// This deliberately returns no `Result`. A script failure — a parse error, an exhausted budget, a
/// capability that policy refused — is a script *outcome*, and the model reads it and recovers the
/// same way it would from a non-zero exit code in a terminal. Only a broken session aborts the
/// loop.
pub trait ScriptRuntime {
    /// Fresh host/broker surface check before inference or reuse of retained context.
    ///
    /// This is a *disclosure* gate, not an authorization one, and it belongs at the turn
    /// boundaries: before a model request and before a completion is disclosed. Dispatch-time
    /// authority is the broker's — every `invoke` is authorized there under the current policy and
    /// the live epoch — so running this per capability call bought no authority and cost one broker
    /// round trip per call. A command word needs no check of its own for a different reason:
    /// `runCommand` is deliberately ungated and grants nothing, and the proposal it returns is
    /// authorized on the `invoke` path.
    fn check_freshness(&self) -> Result<(), dekopon_shell::FreshnessError> {
        Ok(())
    }
    /// Whether actual dispatch observations, rather than a legacy script total, own the budget.
    fn observes_executions(&self) -> bool {
        false
    }
    /// Runs one model-authored script, invoking at most `max_capability_calls` capabilities.
    ///
    /// The ceiling is supplied per call rather than fixed at construction because the prompt loop
    /// spends one session-wide budget across every script it runs.
    fn run_script(&self, script: &str, max_capability_calls: u32) -> ScriptOutcome;

    /// Reads the same scoped in-memory metadata used by `cap --list` and `cap --describe`.
    ///
    /// This must not execute a script, invoke a provider, or discover capabilities through a model.
    /// A runtime with no live capability surface returns an explicitly empty snapshot.
    fn capability_snapshot(&self) -> Result<CapabilitySnapshot, BootstrapError>;

    /// Observe actual dispatch, not a script's narrative. Non-dispatch replay/mock runtimes may
    /// return recorded scripts without claiming new capability executions.
    fn run_script_observed(
        &self,
        script: &str,
        maximum: u32,
        _journal: &crate::checkpoint::ExecutionJournal,
    ) -> ScriptOutcome {
        self.run_script(script, maximum)
    }
}

/// Runs each model-authored script on the interpreter under this session's dispatch.
pub struct ShellRuntime<I> {
    /// Capability dispatch for every command word a script runs.
    pub invoker: I,
    /// Per-script interpreter bounds; the capability ceiling is narrowed per call.
    pub limits: ShellLimits,
    /// The capability `curl` assembles requests for, when the session has one.
    pub curl_capability: Option<String>,
}

impl<I: CapabilityInvoker> ScriptRuntime for ShellRuntime<I> {
    fn check_freshness(&self) -> Result<(), dekopon_shell::FreshnessError> {
        self.invoker.check_freshness()
    }
    fn observes_executions(&self) -> bool {
        true
    }
    fn run_script_observed(
        &self,
        script: &str,
        maximum: u32,
        journal: &crate::checkpoint::ExecutionJournal,
    ) -> ScriptOutcome {
        let limits = ShellLimits {
            max_capability_calls: self.limits.max_capability_calls.min(maximum),
            ..self.limits
        };
        Interpreter::new(limits)
            .with_curl_capability(self.curl_capability.clone())
            .run(
                script,
                &ObservedInvoker {
                    inner: &self.invoker,
                    journal,
                },
            )
    }

    fn run_script(&self, script: &str, max_capability_calls: u32) -> ScriptOutcome {
        // A fresh interpreter per script, but not a fresh budget: the prompt loop spends one
        // capability allowance across the whole session, so this script gets whatever the earlier
        // ones left. Exhausting it trips the interpreter's own ceiling, with the message and exit
        // code the interpreter already established, rather than inventing a second way to say
        // "no".
        let limits = ShellLimits {
            max_capability_calls: self.limits.max_capability_calls.min(max_capability_calls),
            ..self.limits
        };
        Interpreter::new(limits)
            .with_curl_capability(self.curl_capability.clone())
            .run(script, &self.invoker)
    }

    fn capability_snapshot(&self) -> Result<CapabilitySnapshot, BootstrapError> {
        CapabilitySnapshot::from_invoker(&self.invoker)
    }
}

/// Dispatches a script's commands to direct-mode providers first and a broker second.
///
/// The order is not arbitrary. A direct component call is local, synchronous, and unauthorized by
/// construction — the linker is import-free, so the component cannot reach anything. Preferring it
/// keeps every capability that *can* run without a broker transition doing exactly that, and
/// leaves the broker leg for what direct mode provably cannot reach: anything performing I/O.
pub struct SessionInvoker<D> {
    /// The local, read-only leg consulted first.
    pub direct: D,
    /// The broker-backed leg consulted for everything direct mode cannot serve.
    pub broker: Option<Box<dyn CapabilityInvoker + Send>>,
}

impl<D: CapabilityInvoker> CapabilityInvoker for SessionInvoker<D> {
    fn check_freshness(&self) -> Result<(), dekopon_shell::FreshnessError> {
        self.direct.check_freshness()?;
        if let Some(broker) = &self.broker {
            broker.check_freshness()?;
        }
        Ok(())
    }
    fn granted(&self) -> Vec<String> {
        let mut granted = self.direct.granted();
        if let Some(broker) = &self.broker {
            granted.extend(broker.granted());
        }
        granted.sort_unstable();
        granted.dedup();
        granted
    }

    fn is_granted(&self, capability: &str) -> bool {
        self.direct.is_granted(capability)
            || self
                .broker
                .as_ref()
                .is_some_and(|broker| broker.is_granted(capability))
    }

    // Both membership queries ask each leg rather than merging the legs' lists and searching the
    // merge. Dispatch asks them per command word, so building, extending, sorting, and deduping
    // two `Vec<String>` here made a loop of a thousand commands do it a thousand times.
    fn grants_namespace(&self, namespace: &str) -> bool {
        self.direct.grants_namespace(namespace)
            || self
                .broker
                .as_ref()
                .is_some_and(|broker| broker.grants_namespace(namespace))
    }

    fn has_command_word(&self, word: &str) -> bool {
        self.direct.has_command_word(word)
            || self
                .broker
                .as_ref()
                .is_some_and(|broker| broker.has_command_word(word))
    }

    fn describe(&self, capability: &str) -> Option<dekopon_shell::CapabilityDescription> {
        self.direct.describe(capability).or_else(|| {
            self.broker
                .as_ref()
                .and_then(|broker| broker.describe(capability))
        })
    }

    fn invoke(
        &self,
        capability: &str,
        input: Value,
        secret_use: Option<dekopon_core::SecretUseProposal>,
    ) -> CapabilityCallResult {
        // A DRN reaches only the broker leg, and only for a capability that leg already holds.
        // The direct leg is read-only and import-free; it has no authorizer to prove the use
        // against, so a proposal naming one is refused rather than run without it.
        if secret_use.is_some() {
            return match &self.broker {
                Some(broker) if broker.is_granted(capability) => {
                    broker.invoke(capability, input, secret_use)
                }
                _ => dekopon_shell::secret_use_unsupported(),
            };
        }
        if self.direct.is_granted(capability) {
            return self.direct.invoke(capability, input, None);
        }
        match &self.broker {
            Some(broker) => broker.invoke(capability, input, None),
            None => CapabilityCallResult::NotFound,
        }
    }

    fn command_words(&self) -> Vec<String> {
        let mut words = self.direct.command_words();
        if let Some(broker) = &self.broker {
            words.extend(broker.command_words());
        }
        words.sort_unstable();
        words.dedup();
        words
    }

    fn run_command(&self, word: &str, argv: &[String], stdin: Option<&str>) -> Option<CommandRun> {
        // Same precedence as `invoke`: whichever leg owns the word runs it. A word both legs
        // claim cannot happen — the broker refuses to start on a duplicate, and direct mode loads
        // its own registry through the same check.
        self.direct
            .run_command(word, argv, stdin)
            .or_else(|| self.broker.as_ref()?.run_command(word, argv, stdin))
    }
}

// The shell seam deliberately knows nothing about broker evidence. A scoped synchronous slot
// carries typed detail across that seam on the same blocking thread, never across tasks or jobs.
// The outer invoker observes every actual dispatch, including a gateway's lone broker leg.
#[derive(Clone)]
struct DispatchDetail {
    provenance: crate::history::ExecutionProvenance,
    invocation: Option<String>,
    evidence: Vec<String>,
    outcome: crate::history::ExecutionOutcome,
}
thread_local! { static DISPATCH_DETAIL: std::cell::RefCell<Option<DispatchDetail>> = const { std::cell::RefCell::new(None) }; }
fn dispatch_detail(detail: DispatchDetail) {
    DISPATCH_DETAIL.with(|slot| *slot.borrow_mut() = Some(detail));
}

struct ObservedInvoker<'a, I> {
    inner: &'a I,
    journal: &'a crate::checkpoint::ExecutionJournal<'a>,
}
impl<I: CapabilityInvoker> CapabilityInvoker for ObservedInvoker<'_, I> {
    fn granted(&self) -> Vec<String> {
        self.inner.granted()
    }
    fn is_granted(&self, c: &str) -> bool {
        self.inner.is_granted(c)
    }
    fn grants_namespace(&self, n: &str) -> bool {
        self.inner.grants_namespace(n)
    }
    fn command_words(&self) -> Vec<String> {
        self.inner.command_words()
    }
    fn has_command_word(&self, w: &str) -> bool {
        self.inner.has_command_word(w)
    }
    fn describe(&self, c: &str) -> Option<dekopon_shell::CapabilityDescription> {
        self.inner.describe(c)
    }
    fn run_command(&self, w: &str, args: &[String], stdin: Option<&str>) -> Option<CommandRun> {
        if self.journal.cancelled() {
            return Some(CommandRun::Denied {
                reason: "session-cancelled".to_owned(),
            });
        }
        self.inner.run_command(w, args, stdin)
    }
    fn invoke(
        &self,
        capability: &str,
        input: Value,
        secret: Option<dekopon_core::SecretUseProposal>,
    ) -> CapabilityCallResult {
        use crate::history::{ExecutionOutcome as EO, ExecutionProvenance as EP};
        // No freshness check here, deliberately, and none in `run_command` either. The broker
        // authorizes this dispatch under its own live policy and epoch a few microseconds from now;
        // a client-side refetch immediately before it adds a full round trip per capability call
        // and decides nothing the broker is not about to decide. Freshness is a disclosure gate and
        // runs at the turn boundaries in `session.rs`.
        if self.journal.cancelled() {
            return CapabilityCallResult::Denied {
                reason: "session-cancelled".to_owned(),
            };
        }
        let sequence = match self.journal.reserve(capability) {
            Ok(sequence) => sequence,
            Err(error) => {
                self.journal.failure(error);
                return CapabilityCallResult::Denied {
                    reason: error.to_string(),
                };
            }
        };
        if let Some(activity) = &self.journal.activity {
            activity.emit(
                sequence,
                capability,
                crate::activity::ActivityPhase::Submitted,
                None,
            );
        }
        DISPATCH_DETAIL.with(|slot| slot.borrow_mut().take());
        let result = self.inner.invoke(capability, input, secret);
        let detail = DISPATCH_DETAIL.with(|slot| slot.borrow_mut().take());
        let (outcome, text) = match &result {
            CapabilityCallResult::Succeeded(output) => (EO::Succeeded, output.to_string()),
            CapabilityCallResult::Failed { error } => (EO::Failed, error.clone()),
            CapabilityCallResult::Denied { reason } => (EO::Denied, reason.clone()),
            CapabilityCallResult::NotFound => (EO::NotExecuted, "capability not found".to_owned()),
        };
        let detail = detail.unwrap_or(DispatchDetail {
            provenance: EP::DirectReadOnly,
            invocation: None,
            evidence: Vec::new(),
            outcome,
        });
        if let Some(activity) = &self.journal.activity {
            activity.emit(
                sequence,
                capability,
                crate::activity::ActivityPhase::Finished,
                Some(detail.outcome),
            );
        }
        if let Err(error) = self.journal.observe(sequence, |record| {
            record.provenance = detail.provenance;
            record.invocation = detail.invocation;
            record.evidence = detail.evidence;
            record.outcome = detail.outcome;
            record.result = Some(crate::checkpoint::result_excerpt(&text));
        }) {
            self.journal.failure(error);
        }
        result
    }
}

/// Trace context to send with a broker proposal, if this process is exporting one.
///
/// `None` is the ordinary state when export is disabled: the broker then records its own root
/// span rather than a child of a trace nothing will ever receive.
#[must_use]
pub fn current_trace_parent() -> Option<TraceParent> {
    let parts = dekopon_telemetry::current_trace_context()?;
    // A context the SDK considers valid can still be rejected here (all-zero identifiers), and a
    // malformed parent is worse than none: it would attach broker spans to a trace that does not
    // exist. Dropping it degrades correlation instead of corrupting it.
    TraceParent::new(parts.trace_id, parts.span_id, parts.flags).ok()
}

/// Maps a provider's own command-run outcome onto the shell's seam type.
///
/// One definition for both legs: the direct host and the broker answer with the same
/// [`CommandRunOutcome`], and the shell reads the same [`CommandRun`] from either. The provider's
/// stable failure `code` stays with the operator; the model reads the message, as it always has.
#[must_use]
pub fn command_run_from_outcome(outcome: CommandRunOutcome) -> CommandRun {
    match outcome {
        CommandRunOutcome::Proposed { capability, input } => CommandRun::Proposed {
            capability: capability.to_string(),
            input,
        },
        CommandRunOutcome::Rendered {
            stdout,
            stderr,
            status,
        } => CommandRun::Rendered {
            stdout,
            stderr,
            status,
        },
        CommandRunOutcome::Failed { error } => CommandRun::Failed {
            message: error.message,
        },
    }
}

/// Records a command-word run whose caller was dropped while its process node was still joined.
///
/// The audit record carries only fixed categories — the leg, the outcome, an error kind — never
/// the word, the argv, the piped value, or the text a provider rendered. The complete cause of a
/// failure goes out as an ordinary error event at the same site, so it is recorded exactly once
/// and never inside the audit stream, where a provider path or broker text does not belong.
/// `error_type` names the stable kind of the leg's own error, since each leg fails differently.
pub fn report_unobserved_command_run<E: std::error::Error + 'static>(
    leg: &'static str,
    outcome: ProcessOutcome<CommandRun, E>,
    error_type: fn(&E) -> &'static str,
) {
    match outcome {
        ProcessOutcome::Completed(Ok(_run)) => {
            tracing::warn!(
                target: "dekopon_harness::audit",
                {
                    audit.event = "agent.command.unobserved",
                    command.leg = leg,
                    outcome = "succeeded",
                    error.type = "none",
                },
                "unobserved command run completed"
            );
        }
        ProcessOutcome::Completed(Err(error)) => {
            tracing::error!(
                target: "dekopon_harness::audit",
                {
                    audit.event = "agent.command.unobserved",
                    command.leg = leg,
                    outcome = "operation-error",
                    error.type = error_type(&error),
                },
                "unobserved command run failed"
            );
            tracing::error!(
                command.leg = leg,
                error = %dekopon_core::error_chain(&error),
                "unobserved command run failed"
            );
        }
        ProcessOutcome::TaskFailed(error) => {
            let (outcome, error_type) = if error.is_cancelled() {
                ("cancelled", "task-cancelled")
            } else {
                ("task-failed", "task-panicked")
            };
            tracing::error!(
                target: "dekopon_harness::audit",
                {
                    audit.event = "agent.command.unobserved",
                    command.leg = leg,
                    outcome = outcome,
                    error.type = error_type,
                },
                "unobserved command run task failed"
            );
            tracing::error!(
                command.leg = leg,
                error = %error,
                "unobserved command run task failed"
            );
        }
    }
}

/// Failure to open a session's broker leg.
#[cfg(unix)]
#[derive(Debug, Error)]
pub enum BrokerLegError {
    /// The complete scoped surface cannot fit safely in request-one context.
    #[error(transparent)]
    Bootstrap(#[from] BootstrapError),
    /// The broker could not be reached or refused the capability snapshot.
    #[error(transparent)]
    Client(#[from] ClientError),
    /// A unique session identifier could not be derived.
    #[error("could not derive a unique identifier for this broker session")]
    SessionIdentifier(#[source] IdentifierError),
    /// The broker's capability snapshot named the same capability more than once.
    #[error("the broker answered with duplicate capability identifiers: {capabilities}")]
    DuplicateCapabilities {
        /// Every repeated identifier, in identifier order.
        capabilities: String,
    },
}

/// The broker half of a session's capability dispatch.
///
/// This is a client of `dekopon-brokerd`'s authorization path, never a participant in it: it
/// submits a proposal and reports back whatever the broker decided. Nothing here interprets policy,
/// and nothing here can mint authorization. An attested leg additionally *claims* an external
/// subject, which is still not authority: the broker honors the claim only under an owner-configured
/// attestor grant, and it alone maps that subject to a principal.
#[cfg(unix)]
pub struct BrokerLeg {
    client: BrokerClient,
    runtime: tokio::runtime::Handle,
    capabilities: BTreeMap<String, CapabilityDescription>,
    /// Trusted, credential-free classification for this exact effective capability set.
    effective_capabilities: Vec<EffectiveCapabilityView>,
    /// Command words loaded providers contribute, snapshotted with the capability set.
    ///
    /// Snapshotted for the same reason the capabilities are: dispatch consults this on every
    /// command word a script runs, and a round trip per word would make the interpreter's cost
    /// depend on the network rather than on the script. A set rather than a list for the same
    /// reason again: that consultation is a membership test, and a script running thousands of
    /// commands asks it thousands of times.
    command_words: BTreeSet<String>,
    /// Provider namespaces this leg holds a grant in, derived from the capability set.
    ///
    /// Only [`CapabilityInvoker::grants_namespace`] reads it, on the path where a word was refused
    /// — the answer that separates "the model typed nonsense" from "the model keeps reaching for
    /// something we never granted".
    namespaces: BTreeSet<String>,
    identifiers: IdSequence,
    /// `None` for a leg that speaks as its own connected peer, which is the original behavior.
    attestation: Option<Attestation>,
    /// Broker-derived optional all-three durable-memory surface for this exact chat scope.
    chat_memory: Option<ChatMemorySurface>,
    surface_epoch: dekopon_core::SurfaceEpoch,
    /// Per-component commitment to the surface this leg was built on, for freshness comparison.
    digest: SurfaceDigest,
    /// What cancels a command-word run in flight: [`CancelSignal::never`] until an embedder ties
    /// it to its own session with [`BrokerLeg::with_cancel_signal`].
    cancel: CancelSignal,
    /// The bounded model-facing projection of this leg's surface, built once when the leg was.
    ///
    /// Construction validates it anyway, so keeping it costs nothing and saves the embedder a
    /// second pass that would describe, serialize and sort every granted capability again for a
    /// projection identical to this one.
    snapshot: CapabilitySnapshot,
}

/// One commitment per component of a session surface, so a change names the component it hit.
///
/// Five digests rather than one. A single digest answers "did anything change" and nothing else,
/// and the five causes are different incidents — a restarted broker, a redeployed provider, a
/// narrowed policy. Digests rather than the values themselves because this is compared on the hot
/// path: the previous check rebuilt the whole indexed catalog, cloning every input schema, to run a
/// deep equality it then threw away.
#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct SurfaceDigest {
    epoch: [u8; 32],
    descriptions: [u8; 32],
    effective: [u8; 32],
    command_words: [u8; 32],
    chat_memory: [u8; 32],
}

#[cfg(unix)]
impl SurfaceDigest {
    /// The first component that differs, in the order a change is most likely to explain the rest.
    fn changed(&self, fresh: &Self) -> Option<dekopon_shell::SurfaceChange> {
        use dekopon_shell::SurfaceChange;
        // Epoch first: a restarted broker changes everything downstream of it, and reporting one of
        // those consequences instead would send an operator looking for a policy edit.
        for (mine, theirs, change) in [
            (&self.epoch, &fresh.epoch, SurfaceChange::Epoch),
            (
                &self.descriptions,
                &fresh.descriptions,
                SurfaceChange::Descriptions,
            ),
            (
                &self.effective,
                &fresh.effective,
                SurfaceChange::EffectiveViews,
            ),
            (
                &self.command_words,
                &fresh.command_words,
                SurfaceChange::CommandWords,
            ),
            (
                &self.chat_memory,
                &fresh.chat_memory,
                SurfaceChange::ChatMemory,
            ),
        ] {
            if mine != theirs {
                return Some(change);
            }
        }
        None
    }
}

/// Feeds one length-prefixed field into a digest.
///
/// Length-prefixed rather than delimiter-joined because two of these fields are provider-supplied
/// text: a description containing the delimiter must not be able to spell a neighbour's value and
/// leave the digest unchanged.
#[cfg(unix)]
fn digest_field(hasher: &mut sha2::Sha256, bytes: &[u8]) {
    use sha2::Digest as _;
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

/// Commits to one fetched broker surface without building either indexed view.
#[cfg(unix)]
fn surface_digest(
    available: &[dekopon_broker_protocol::AvailableCapability],
    command_words: &[String],
    chat_memory: Option<&ChatMemorySurface>,
    epoch: &dekopon_core::SurfaceEpoch,
) -> SurfaceDigest {
    use sha2::{Digest as _, Sha256};
    // Sorted by identifier, and words sorted and deduplicated, because neither indexed view depends
    // on the order the broker listed them in: one is a `BTreeMap` and the other is sorted. Hashing
    // the raw order would report a reordered answer as a changed surface.
    let mut order = (0..available.len()).collect::<Vec<_>>();
    order.sort_by(|&left, &right| {
        available[left]
            .capability
            .id
            .as_str()
            .cmp(available[right].capability.id.as_str())
    });
    let mut descriptions = Sha256::new();
    let mut effective = Sha256::new();
    for index in order {
        let entry = &available[index];
        let id = entry.capability.id.as_str();
        // A duplicate identifier is hashed twice and so cannot match the distinct set this leg was
        // built on; `snapshot` refuses one outright at construction.
        digest_field(&mut descriptions, id.as_bytes());
        digest_field(&mut descriptions, entry.capability.description.as_bytes());
        digest_field(
            &mut descriptions,
            &serde_json::to_vec(&entry.capability.input_schema)
                .expect("a fetched input schema is JSON"),
        );
        digest_field(&mut effective, id.as_bytes());
        digest_field(&mut effective, entry.provider.as_str().as_bytes());
        digest_field(&mut effective, entry.capability.description.as_bytes());
        digest_field(
            &mut effective,
            entry.capability.effect.to_string().as_bytes(),
        );
        digest_field(&mut effective, entry.capability.risk.to_string().as_bytes());
        digest_field(
            &mut effective,
            entry.capability.idempotency.to_string().as_bytes(),
        );
    }
    let mut words = Sha256::new();
    for word in command_words.iter().collect::<BTreeSet<_>>() {
        digest_field(&mut words, word.as_bytes());
    }
    let mut memory = Sha256::new();
    match chat_memory {
        Some(surface) => {
            digest_field(&mut memory, b"present");
            digest_field(&mut memory, &surface.max_lookback_turns.to_be_bytes());
            digest_field(&mut memory, surface.prompt_note.as_bytes());
        }
        None => digest_field(&mut memory, b"absent"),
    }
    let mut startup = Sha256::new();
    digest_field(&mut startup, epoch.as_str().as_bytes());
    SurfaceDigest {
        epoch: startup.finalize().into(),
        descriptions: descriptions.finalize().into(),
        effective: effective.finalize().into(),
        command_words: words.finalize().into(),
        chat_memory: memory.finalize().into(),
    }
}

#[cfg(unix)]
impl BrokerLeg {
    /// Connects one session's broker leg, snapshotting its capability set.
    ///
    /// The snapshot happens here, on the async side, for two reasons. It lets `cap --list` answer
    /// and request-one bootstrap answer without another round trip, and it turns "the daemon is not running" into one clear
    /// startup failure instead of a capability that inexplicably reports "command not found"
    /// halfway through a script a model already committed to.
    ///
    /// `trace_prefix` names the embedding surface (for example `dekopon-run-prompt`) and becomes
    /// the leading component of the session's trace and invocation identifiers, so every call a
    /// session made is recoverable from the broker's audit log by prefix.
    ///
    /// `attestation` is `None` for a leg that speaks as its own connected peer, which is the
    /// original behavior. A chat gateway holds no broker authority of its own: it knows which
    /// subject sent a message and which agent is answering, and the broker decides everything
    /// else. So an attested leg's snapshot is what policy makes visible to the *attested* context
    /// rather than to the daemon's own peer identity, and a claim carrying a chat scope is the
    /// only leg that can see durable memory.
    ///
    /// An empty snapshot is a valid result rather than an error. It means "policy grants this
    /// subject nothing through this agent", which a gateway answers very differently from "the
    /// broker is unreachable"; deciding which of those to say is the caller's job.
    pub async fn connect(
        client: BrokerClient,
        trace_prefix: &str,
        attestation: Option<Attestation>,
    ) -> Result<Self, BrokerLegError> {
        let (capabilities, command_words, chat_memory, surface_epoch) =
            client.session_surface(attestation.clone()).await?;
        Self::build(
            client,
            trace_prefix,
            capabilities,
            command_words,
            attestation,
            chat_memory,
            surface_epoch,
        )
    }

    fn build(
        client: BrokerClient,
        trace_prefix: &str,
        available: Vec<dekopon_broker_protocol::AvailableCapability>,
        command_words: Vec<String>,
        attestation: Option<Attestation>,
        chat_memory: Option<ChatMemorySurface>,
        surface_epoch: dekopon_core::SurfaceEpoch,
    ) -> Result<Self, BrokerLegError> {
        let digest = surface_digest(
            &available,
            &command_words,
            chat_memory.as_ref(),
            &surface_epoch,
        );
        let (capabilities, effective_capabilities) = snapshot(available)?;
        let namespaces = capabilities.keys().map(|id| namespace_of(id)).collect();
        let mut leg = Self {
            client,
            runtime: tokio::runtime::Handle::current(),
            capabilities,
            effective_capabilities,
            command_words: command_words.into_iter().collect(),
            namespaces,
            digest,
            identifiers: IdSequence::new(trace_prefix)
                .map_err(BrokerLegError::SessionIdentifier)?,
            attestation,
            chat_memory,
            surface_epoch,
            cancel: CancelSignal::never(),
            snapshot: CapabilitySnapshot::empty(),
        };
        leg.snapshot = CapabilitySnapshot::from_invoker(&leg)?;
        Ok(leg)
    }

    /// Ties every command-word run this leg makes to the embedder's cancellation.
    ///
    /// A run in flight when `signal` is requested is aborted at its next await and joined before
    /// the leg answers the script with `session-cancelled`; the gateway fires it from a native
    /// Stop. Without it a run is cancellable in contract only, which is what `dekopon-run` gets.
    #[must_use]
    pub fn with_cancel_signal(mut self, signal: CancelSignal) -> Self {
        self.cancel = signal;
        self
    }

    /// The bounded model-facing projection of this leg's capability surface.
    ///
    /// Built and validated when the leg connected, so an embedder that needs the same projection —
    /// the gateway hands one to the session engine per message — reads it here instead of building
    /// a second, identical one out of the same descriptions.
    #[must_use]
    pub fn capability_snapshot(&self) -> &CapabilitySnapshot {
        &self.snapshot
    }

    /// Returns this session's trusted, subject-specific effective capability classification.
    ///
    /// This is the same fresh broker answer that backs `cap --list`. It contains no policy source,
    /// policy identifier, subject, principal, constraint, or credential metadata.
    #[must_use]
    pub fn effective_capabilities(&self) -> Vec<EffectiveCapabilityView> {
        self.effective_capabilities.clone()
    }

    /// Host-only broker startup epoch. It never enters capability metadata or model context.
    pub fn surface_epoch(&self) -> &dekopon_core::SurfaceEpoch {
        &self.surface_epoch
    }

    /// Returns the broker-derived memory note and lookback only when all three grants are effective.
    #[must_use]
    pub fn chat_memory_surface(&self) -> Option<&ChatMemorySurface> {
        self.chat_memory.as_ref()
    }

    /// This session's trace identifier, which every invocation it makes extends.
    ///
    /// It is the join key between an embedding surface's own telemetry and the broker's audit
    /// records for the same session.
    #[must_use]
    pub fn session_trace(&self) -> &TraceId {
        self.identifiers.trace()
    }
}

/// Returns the provider namespace one capability identifier belongs to.
///
/// A separator-free identifier is its own namespace, which is how the interpreter reads one too.
#[cfg(unix)]
fn namespace_of(capability: &str) -> String {
    capability
        .split_once('.')
        .map_or(capability, |(namespace, _)| namespace)
        .to_owned()
}

/// Indexes a capability snapshot for shell dispatch and its credential-free meta view.
///
/// A repeated identifier is a refusal rather than a last-wins overwrite, and every repeat is named
/// at once. The two views are built from one list, so tolerating a duplicate would make `cap
/// --list` and `inspect_agent_config` disagree about the same session — the broker refuses
/// duplicate routes at startup, and the client half must not quietly accept the shape the rest of
/// the system treats as fatal.
#[cfg(unix)]
fn snapshot(
    capabilities: Vec<dekopon_broker_protocol::AvailableCapability>,
) -> Result<
    (
        BTreeMap<String, CapabilityDescription>,
        Vec<EffectiveCapabilityView>,
    ),
    BrokerLegError,
> {
    let mut descriptions = BTreeMap::new();
    let mut duplicates = BTreeSet::new();
    let mut effective = Vec::with_capacity(capabilities.len());
    for available in capabilities {
        let id = available.capability.id.to_string();
        if descriptions.contains_key(&id) {
            duplicates.insert(id.clone());
        }
        effective.push(EffectiveCapabilityView {
            id: id.clone(),
            provider: available.provider.to_string(),
            description: available.capability.description.clone(),
            effect: available.capability.effect.to_string(),
            risk: available.capability.risk.to_string(),
            idempotency: available.capability.idempotency.to_string(),
        });
        descriptions.insert(
            id.clone(),
            CapabilityDescription {
                capability: id,
                description: available.capability.description,
                input_schema: available.capability.input_schema,
            },
        );
    }
    if !duplicates.is_empty() {
        return Err(BrokerLegError::DuplicateCapabilities {
            capabilities: duplicates.into_iter().collect::<Vec<_>>().join(", "),
        });
    }
    effective.sort_by(|left, right| left.id.cmp(&right.id));
    Ok((descriptions, effective))
}

#[cfg(unix)]
impl CapabilityInvoker for BrokerLeg {
    fn check_freshness(&self) -> Result<(), dekopon_shell::FreshnessError> {
        let (available, words, memory, epoch) = self
            .runtime
            .block_on(self.client.session_surface(self.attestation.clone()))
            .map_err(|error| {
                dekopon_shell::FreshnessError::Unavailable(dekopon_core::error_chain(&error))
            })?;
        // Digests, not a rebuilt catalog: this compares five 32-byte commitments instead of
        // re-indexing every capability and cloning every input schema to throw the result away.
        let fresh = surface_digest(&available, &words, memory.as_ref(), &epoch);
        match self.digest.changed(&fresh) {
            Some(change) => Err(dekopon_shell::FreshnessError::Changed(change)),
            None => Ok(()),
        }
    }
    fn granted(&self) -> Vec<String> {
        self.capabilities.keys().cloned().collect()
    }

    fn is_granted(&self, capability: &str) -> bool {
        self.capabilities.contains_key(capability)
    }

    fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
        self.capabilities.get(capability).cloned()
    }

    fn command_words(&self) -> Vec<String> {
        self.command_words.iter().cloned().collect()
    }

    fn has_command_word(&self, word: &str) -> bool {
        self.command_words.contains(word)
    }

    fn grants_namespace(&self, namespace: &str) -> bool {
        self.namespaces.contains(namespace)
    }

    fn run_command(&self, word: &str, argv: &[String], stdin: Option<&str>) -> Option<CommandRun> {
        // Same visibility check the capability path makes, and for the same reason: the broker
        // decides refusals, this only avoids spending a round trip on a word no provider owns.
        if !self.command_words.contains(word) {
            return None;
        }
        // The round trip is one cancellable process node: a gateway Stop aborts it at its next
        // await and the supervisor still joins it before this returns, so the leg never answers
        // while the request could still be in flight. The node owns its inputs for the whole run,
        // which is why the client (a path, a UID, and two bounds) is cloned into it.
        let client = self.client.clone();
        let attestation = self.attestation.clone();
        let (owned_word, argv, stdin) = (word.to_owned(), argv.to_vec(), stdin.map(str::to_owned));
        let operation = process_fn(
            ProcessMetadata::cancellable("broker-command", self.cancel.clone()),
            move || async move {
                client
                    .run_command(attestation, owned_word, argv, stdin)
                    .await
                    .map(command_run_from_outcome)
            },
        );
        // Safe for the reason `invoke` documents: this runs on a `spawn_blocking` thread.
        let outcome = self
            .runtime
            .block_on(ProcessRun::execute(operation, |outcome| {
                report_unobserved_command_run("broker", outcome, |error: &ClientError| {
                    error.kind().as_str()
                });
            }));
        Some(match outcome {
            ProcessOutcome::Completed(Ok(run)) => run,
            // A transport failure is not the provider declining: the model reads it as the broker
            // being unreachable rather than as a bad argv, and the cause travels with it; the
            // interpreter prefixes the word, so the message must not. No `ClientError` names the
            // socket path.
            ProcessOutcome::Completed(Err(error)) => CommandRun::Errored {
                message: dekopon_core::error_chain(&error),
            },
            ProcessOutcome::TaskFailed(error) if error.is_cancelled() => CommandRun::Denied {
                reason: "session-cancelled".to_owned(),
            },
            ProcessOutcome::TaskFailed(error) => CommandRun::Errored {
                message: error.to_string(),
            },
        })
    }

    fn invoke(
        &self,
        capability: &str,
        input: Value,
        secret_use: Option<dekopon_core::SecretUseProposal>,
    ) -> CapabilityCallResult {
        let Ok(parsed) = capability.parse::<CapabilityId>() else {
            return CapabilityCallResult::NotFound;
        };
        // A visibility check, deliberately not an authorization one. Bare-word dispatch already
        // filters on `is_granted`, but the `cap <id>` escape hatch does not, so without this a
        // script could spend its whole capability budget probing the broker with guessed
        // identifiers. What this must never do is decide a *refusal*: anything policy makes
        // visible goes to the broker and comes back with the broker's own answer, including the
        // denials that only it can issue.
        if !self.capabilities.contains_key(capability) {
            return CapabilityCallResult::NotFound;
        }
        let Ok(id) = self.identifiers.next_invocation() else {
            return CapabilityCallResult::Failed {
                error: "could not derive a unique invocation identifier".to_owned(),
            };
        };
        let observed_invocation = id.to_string();
        let request = InvocationRequest {
            id,
            capability: parsed,
            trace: self.identifiers.trace().clone(),
            // Read on the blocking thread the session entered, so the broker parents its spans to
            // the script span that actually asked for this capability rather than to the session
            // root.
            trace_parent: current_trace_parent(),
            secret_use,
            input,
        };

        // Safe specifically because this runs on a `spawn_blocking` thread rather than a runtime
        // worker: `Handle::block_on` from a worker would deadlock the executor, and from the
        // blocking pool it is the ordinary bridge back into async code.
        let submitted = self
            .runtime
            .block_on(async { self.client.invoke(self.attestation.clone(), request).await });
        // Typed broker observations are captured before shell status conversion. In particular,
        // outcome-unknown must never be mis-recorded as the shell's non-retryable Denied status.
        use crate::history::{ExecutionOutcome as EO, ExecutionProvenance as EP};
        let (outcome, evidence) = match &submitted {
            Ok(result) => (
                match result.outcome {
                    InvocationOutcome::Succeeded => EO::Succeeded,
                    InvocationOutcome::Denied => EO::Denied,
                    InvocationOutcome::Failed => EO::Failed,
                },
                result
                    .evidence
                    .iter()
                    .take(16)
                    .map(|e| crate::history::Excerpt::new(&e.digest, 256).text)
                    .collect(),
            ),
            Err(ClientError::Remote { code, .. }) if code == ERROR_UNAUTHENTICATED => {
                (EO::Denied, Vec::new())
            }
            Err(error) if error.may_have_executed() => (EO::Unknown, Vec::new()),
            Err(_) => (EO::NotExecuted, Vec::new()),
        };
        dispatch_detail(DispatchDetail {
            provenance: EP::BrokerObserved,
            invocation: Some(observed_invocation),
            evidence,
            outcome,
        });
        match submitted {
            Ok(result) => match result.outcome {
                InvocationOutcome::Succeeded => {
                    CapabilityCallResult::Succeeded(result.output.unwrap_or(Value::Null))
                }
                // A refusal has to stay a refusal all the way to the script's exit code. The
                // interpreter maps `Denied` to 126 and `Failed` to 1, and a model that reads
                // "policy said no" as "the call errored" will retry something it must not retry.
                InvocationOutcome::Denied => CapabilityCallResult::Denied {
                    reason: result
                        .error
                        .unwrap_or_else(|| "authorization refused this invocation".to_owned()),
                },
                InvocationOutcome::Failed => CapabilityCallResult::Failed {
                    error: result
                        .error
                        .unwrap_or_else(|| "the broker reported a failed invocation".to_owned()),
                },
            },
            // An unmapped peer — or, for an attested leg, a refused attestation — is an
            // authorization refusal that never reached a decision record, so it arrives as a
            // transport-level code rather than a `Denied` outcome. It is still a refusal, and
            // collapsing it into a generic failure would tell a model to retry.
            Err(ClientError::Remote { code, message }) if code == ERROR_UNAUTHENTICATED => {
                CapabilityCallResult::Denied { reason: message }
            }
            // The proposal reached the broker and its outcome is unknown here: a client-side read
            // timeout cannot distinguish a `gh.issue.comment` that ran 29s against a 30s deadline
            // from one that never ran, and `outcome-unaudited` says outright that the effect may
            // have happened. `Failed` exits 1, which a model reads as "the call errored, try
            // again" — and a retry carries a fresh invocation identifier, so replay rejection
            // cannot catch the duplicate external effect. `Denied` (126) is the interpreter's only
            // non-retryable status, so an unaudited outcome takes it and says why.
            Err(error) if error.may_have_executed() => CapabilityCallResult::Denied {
                reason: format!(
                    "the broker did not record an outcome for this invocation and it may already \
                     have taken effect; do not resubmit it ({error})"
                ),
            },
            // Every `ClientError` renders without the socket path, so a script cannot learn where
            // the broker lives — the interpreter refuses to read the process environment, and this
            // is the one path that could otherwise leak `DEKOPON_BROKER_SOCKET` back into it.
            Err(error) => CapabilityCallResult::Failed {
                error: error.to_string(),
            },
        }
    }
}

/// Generates the trace and invocation identifiers one session needs.
///
/// The broker treats an invocation identifier as a durable replay-rejection key, so two calls must
/// never share one and a script that calls the same capability in a loop must not collide with
/// itself. Nothing in this workspace generates randomness, and a dependency is not worth 64 bits
/// of it, so the session prefix mixes an OS-seeded `RandomState` key with the process ID and a
/// wall-clock reading, and a monotonic counter makes collisions *within* a session impossible
/// rather than merely unlikely. Invocation identifiers extend the session trace, so every call a
/// session made is recoverable from the broker's audit log by prefix.
#[cfg(unix)]
pub struct IdSequence {
    trace: TraceId,
    next: AtomicU32,
}

#[cfg(unix)]
impl IdSequence {
    /// Derives one session's identifier space under `prefix`.
    ///
    /// The prefix must itself be a valid identifier component (lowercase, `.`/`-`/`_`), and short
    /// enough that the longest identifier derived from it still validates, because a bad prefix
    /// fails here rather than on the first invocation.
    pub fn new(prefix: &str) -> Result<Self, IdentifierError> {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        hasher.write_u128(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default(),
        );
        let trace = format!("{prefix}-{:016x}", hasher.finish()).parse::<TraceId>()?;
        // The longest invocation identifier this session could ever derive, checked now rather
        // than on the first call. A prefix can be short enough for a valid trace and still push
        // every identifier built from it past the length bound, which would leave a session that
        // constructed cleanly and then failed every capability call a model committed to.
        format!("{trace}-{}", u32::MAX).parse::<InvocationId>()?;
        Ok(Self {
            trace,
            next: AtomicU32::new(1),
        })
    }

    /// The session's trace identifier, shared by every invocation it makes.
    #[must_use]
    pub fn trace(&self) -> &TraceId {
        &self.trace
    }

    /// Derives the next invocation identifier in this session.
    pub fn next_invocation(&self) -> Result<InvocationId, IdentifierError> {
        let counter = self.next.fetch_add(1, Ordering::Relaxed);
        format!("{}-{counter}", self.trace).parse()
    }
}

#[cfg(test)]
mod tests {
    use dekopon_shell::{CapabilityCallResult, CapabilityDescription, CapabilityInvoker};
    use serde_json::{Value, json};

    use super::{SessionInvoker, current_trace_parent};

    /// Outside an exporting span there is no context to send, and a session must not invent one.
    #[test]
    fn trace_parent_is_absent_without_an_active_exporting_span() {
        assert!(current_trace_parent().is_none());
    }

    /// A leg that answers for a fixed capability set and records what it was asked to run.
    struct FakeLeg {
        capability: &'static str,
        marker: &'static str,
        invoked: std::sync::Mutex<Vec<String>>,
        /// Every secret-use field this leg was handed, in call order.
        secret_uses: std::sync::Mutex<Vec<Option<dekopon_core::SecretUseProposal>>>,
    }

    impl FakeLeg {
        fn new(capability: &'static str, marker: &'static str) -> Self {
            Self {
                capability,
                marker,
                invoked: std::sync::Mutex::new(Vec::new()),
                secret_uses: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl CapabilityInvoker for FakeLeg {
        fn granted(&self) -> Vec<String> {
            vec![self.capability.to_owned()]
        }

        fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
            (capability == self.capability).then(|| CapabilityDescription {
                capability: capability.to_owned(),
                description: self.marker.to_owned(),
                input_schema: json!({"type": "object"}),
            })
        }

        fn invoke(
            &self,
            capability: &str,
            _input: Value,
            secret_use: Option<dekopon_core::SecretUseProposal>,
        ) -> CapabilityCallResult {
            if capability != self.capability {
                return CapabilityCallResult::NotFound;
            }
            self.invoked
                .lock()
                .expect("invocation lock")
                .push(capability.to_owned());
            self.secret_uses
                .lock()
                .expect("invocation lock")
                .push(secret_use);
            CapabilityCallResult::Succeeded(json!({ "leg": self.marker }))
        }
    }

    #[test]
    fn direct_capabilities_are_preferred_over_the_broker() {
        // A capability reachable without a broker transition must never take one: the direct call
        // is local and unauthorized by construction, so routing it through the broker would add an
        // authorization decision, an audit record, and a round trip for no gain.
        let shared = Box::new(FakeLeg::new("shared.capability", "broker"));
        let invoker = SessionInvoker {
            direct: FakeLeg::new("shared.capability", "direct"),
            broker: Some(shared),
        };

        assert_eq!(
            invoker.invoke("shared.capability", json!({}), None),
            CapabilityCallResult::Succeeded(json!({"leg": "direct"}))
        );
    }

    #[test]
    fn capabilities_absent_from_direct_mode_fall_through_to_the_broker() {
        let invoker = SessionInvoker {
            direct: FakeLeg::new("echo.echo", "direct"),
            broker: Some(Box::new(FakeLeg::new("http-probe.fetch", "broker"))),
        };

        assert_eq!(
            invoker.invoke("http-probe.fetch", json!({}), None),
            CapabilityCallResult::Succeeded(json!({"leg": "broker"}))
        );
        assert!(invoker.is_granted("http-probe.fetch"));
        assert_eq!(
            invoker.granted(),
            vec!["echo.echo".to_owned(), "http-probe.fetch".to_owned()]
        );
        assert_eq!(
            invoker
                .describe("http-probe.fetch")
                .map(|it| it.description),
            Some("broker".to_owned())
        );
    }

    #[test]
    fn membership_queries_agree_with_the_lists_they_replace() {
        // Dispatch asks these per command word rather than merging both legs' lists and searching
        // the merge. They have to answer what searching the merge would have answered, from either
        // leg, including for a leg that overrides neither and is scanned by the default.
        let invoker = SessionInvoker {
            direct: FakeLeg::new("echo.echo", "direct"),
            broker: Some(Box::new(FakeLeg::new("http-probe.fetch", "broker"))),
        };

        for granted in invoker.granted() {
            let namespace = granted.split('.').next().expect("a namespace");
            assert!(invoker.grants_namespace(namespace), "{granted}");
        }
        assert!(invoker.grants_namespace("echo"));
        assert!(invoker.grants_namespace("http-probe"));
        assert!(!invoker.grants_namespace("gh"));
        assert!(!invoker.grants_namespace("ech"));

        // Neither `FakeLeg` contributes command words, so nothing is one.
        assert!(invoker.command_words().is_empty());
        assert!(!invoker.has_command_word("gh"));
    }

    #[test]
    fn a_session_without_a_broker_is_exactly_as_capable_as_direct_mode() {
        // Omitting the broker leg has to leave a session behaving as direct mode always did, so a
        // local demo or a CI run with no daemon is unaffected.
        let invoker = SessionInvoker {
            direct: FakeLeg::new("echo.echo", "direct"),
            broker: None,
        };

        assert_eq!(invoker.granted(), vec!["echo.echo".to_owned()]);
        assert!(!invoker.is_granted("http-probe.fetch"));
        assert_eq!(
            invoker.invoke("http-probe.fetch", json!({}), None),
            CapabilityCallResult::NotFound
        );
    }

    /// A DRN reaches the broker and nothing else, through the one invocation method.
    ///
    /// The composite used to answer this on a separate defaulted method while every wrapper around
    /// it forwarded the other one, so the field a `curl --user USER:${drn:...}` produced was
    /// dropped between the shell and this decision. There is one method now, and the deny for the
    /// direct leg is a branch inside it rather than a default a wrapper can inherit by accident.
    #[test]
    fn a_secret_use_proposal_reaches_only_a_broker_backed_capability() {
        let proposal = dekopon_core::SecretUseProposal::HttpBearer {
            secret: "drn:com.xrl:secret:prod:api/token"
                .parse::<dekopon_core::SecretDrn>()
                .expect("canonical DRN"),
        };
        let broker = Box::new(FakeLeg::new("http-probe.fetch", "broker"));
        let invoker = SessionInvoker {
            direct: FakeLeg::new("echo.echo", "direct"),
            broker: Some(broker),
        };

        assert_eq!(
            invoker.invoke("http-probe.fetch", json!({}), Some(proposal.clone())),
            CapabilityCallResult::Succeeded(json!({"leg": "broker"}))
        );

        // Deny-by-default on the direct leg: immediate mode has no authorizer, so a capability it
        // owns cannot carry a secret even though the call itself would succeed without one.
        assert_eq!(
            invoker.invoke("echo.echo", json!({}), Some(proposal)),
            dekopon_shell::secret_use_unsupported()
        );
        assert!(
            invoker
                .direct
                .secret_uses
                .lock()
                .expect("invocation lock")
                .is_empty(),
            "the direct leg was handed a proposal it cannot authorize"
        );
    }

    /// A leg whose command words and membership answers no trait default could produce.
    struct CommandLeg {
        word: &'static str,
        capability: &'static str,
    }

    impl CapabilityInvoker for CommandLeg {
        fn granted(&self) -> Vec<String> {
            Vec::new()
        }

        fn is_granted(&self, capability: &str) -> bool {
            capability == self.capability
        }

        fn grants_namespace(&self, namespace: &str) -> bool {
            self.capability
                .split('.')
                .next()
                .is_some_and(|candidate| candidate == namespace)
        }

        fn command_words(&self) -> Vec<String> {
            vec![self.word.to_owned()]
        }

        fn has_command_word(&self, word: &str) -> bool {
            word == self.word
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

    /// Command words and grants have to survive the composite, from either leg.
    ///
    /// `command_words` defaults to an empty list and `is_granted` to a scan of `granted`, so a
    /// composite that forgets either answers "command not found" for a word a provider
    /// contributed and refuses a capability a leg holds. Both legs here report a `granted` list
    /// that is empty or silent about what they answer for, so every assertion below fails against
    /// the defaults rather than coinciding with them.
    #[test]
    fn command_words_and_grants_survive_both_legs_rather_than_falling_back_to_the_defaults() {
        let invoker = SessionInvoker {
            direct: CommandLeg {
                word: "echo",
                capability: "echo.echo",
            },
            broker: Some(Box::new(CommandLeg {
                word: "gh",
                capability: "gh.pr-view",
            })),
        };

        assert_eq!(
            invoker.command_words(),
            vec!["echo".to_owned(), "gh".to_owned()],
            "a word a provider contributed became `command not found`"
        );
        assert!(invoker.has_command_word("echo"));
        assert!(invoker.has_command_word("gh"));
        assert!(!invoker.has_command_word("git"));

        assert!(invoker.granted().is_empty());
        assert!(invoker.is_granted("echo.echo"), "the direct leg holds it");
        assert!(invoker.is_granted("gh.pr-view"), "the broker leg holds it");
        assert!(!invoker.is_granted("gh.pr-merge"));
        assert!(invoker.grants_namespace("echo"));
        assert!(invoker.grants_namespace("gh"));
        assert!(!invoker.grants_namespace("git"));
    }

    #[cfg(unix)]
    mod broker_leg {
        use std::{
            collections::{BTreeMap, BTreeSet},
            os::unix::fs::PermissionsExt as _,
            path::Path,
            sync::atomic,
        };

        use dekopon_broker_protocol::{
            BrokerClient, BrokerRequest, CommandRunOutcome, ERROR_UNAUTHENTICATED, FrameLimits,
            InvocationOutcome, InvocationResult, RequestEnvelope, ResponseEnvelope, read_frame,
            write_frame,
        };
        use dekopon_capability::DecisionReference;
        use dekopon_core::{AgentId, ExternalSubject};
        use dekopon_process::CancelSignal;
        use dekopon_shell::{
            CapabilityCallResult, CapabilityDescription, CapabilityInvoker, CommandRun,
        };
        use serde_json::json;
        use tokio::{
            net::UnixListener,
            sync::{mpsc, oneshot},
        };

        use crate::{
            meta::EffectiveCapabilityView,
            runtime::{Attestation, BrokerLeg, IdSequence},
        };

        const CAPABILITY: &str = "http-probe.fetch";
        const SUBJECT: &str = "slack.t0123abc.u9xyz";

        fn server_uid() -> u32 {
            rustix::process::geteuid().as_raw()
        }

        fn result(outcome: InvocationOutcome, error: Option<&str>) -> InvocationResult {
            InvocationResult {
                invocation: "invoke-stub".parse().expect("valid invocation fixture"),
                decision: DecisionReference {
                    decision_id: "decision-stub".to_owned(),
                    authorized_by: "broker-stub".parse().expect("valid principal fixture"),
                    policy_revision: "policy-stub".to_owned(),
                },
                outcome,
                output: matches!(outcome, InvocationOutcome::Succeeded)
                    .then(|| json!({"status": 200})),
                error: error.map(str::to_owned),
                evidence: Vec::new(),
            }
        }

        /// Serves a fixed script of responses over a private Unix socket.
        ///
        /// A real socket rather than an in-memory duplex, because the client authenticates the
        /// server by socket ownership and peer UID before it writes a byte; a stub that skipped
        /// that would not be exercising the path an embedding binary actually takes.
        async fn stub_leg(directory: &Path, responses: Vec<ResponseEnvelope>) -> BrokerLeg {
            let (leg, _observed) = stub_leg_observing(directory, responses, None).await;
            leg
        }

        /// Serves `responses` and reports every request frame it decoded.
        ///
        /// The observation channel is what makes an attested test meaningful: the only difference
        /// between a direct and an attested leg is the frame it puts on the wire, so a test that
        /// checked the returned `CapabilityCallResult` alone would pass for both.
        async fn stub_leg_observing(
            directory: &Path,
            responses: Vec<ResponseEnvelope>,
            attestation: Option<Attestation>,
        ) -> (BrokerLeg, mpsc::UnboundedReceiver<RequestEnvelope>) {
            let socket = directory.join("broker.sock");
            let listener = UnixListener::bind(&socket).expect("bind stub broker");
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
                .expect("secure stub socket");
            let (observed, receiver) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                for response in responses {
                    let (mut stream, _) = listener.accept().await.expect("stub broker accepts");
                    let request =
                        read_frame::<_, RequestEnvelope>(&mut stream, FrameLimits::default())
                            .await
                            .expect("stub broker reads one request");
                    #[allow(
                        clippy::let_underscore_must_use,
                        reason = "`stub_leg` drops the observation receiver immediately, so a \
                                  closed channel is the ordinary case for every unobserved test"
                    )]
                    let _ = observed.send(request);
                    write_frame(&mut stream, &response, FrameLimits::default())
                        .await
                        .expect("stub broker writes one response");
                }
            });

            (leg_with(&socket, attestation), receiver)
        }

        fn leg_for(socket: &Path) -> BrokerLeg {
            leg_with(socket, None)
        }

        /// Accepts one request, reports it, and never answers until released: the shape of a
        /// broker still working on a run when the session is cancelled underneath it.
        async fn stub_leg_parked(
            directory: &Path,
        ) -> (
            BrokerLeg,
            mpsc::UnboundedReceiver<RequestEnvelope>,
            oneshot::Sender<()>,
        ) {
            let socket = directory.join("broker.sock");
            let listener = UnixListener::bind(&socket).expect("bind stub broker");
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
                .expect("secure stub socket");
            let (observed, receiver) = mpsc::unbounded_channel();
            let (release, released) = oneshot::channel::<()>();
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("stub broker accepts");
                let request = read_frame::<_, RequestEnvelope>(&mut stream, FrameLimits::default())
                    .await
                    .expect("stub broker reads one request");
                #[allow(
                    clippy::let_underscore_must_use,
                    reason = "the test may have finished observing before the stub reports"
                )]
                let _ = observed.send(request);
                // Hold the connection open, unanswered, until the test lets go of the sender: a
                // dropped sender releases the stub exactly as a sent signal would.
                #[allow(
                    clippy::let_underscore_must_use,
                    reason = "a dropped sender and a sent signal both mean the test is done"
                )]
                let _ = released.await;
                drop(stream);
            });
            (leg_for(&socket), receiver, release)
        }

        /// Runs one command word the way an embedding binary does: from a blocking thread.
        async fn run_word(
            leg: BrokerLeg,
            argv: &'static [&'static str],
            stdin: Option<&'static str>,
        ) -> Option<CommandRun> {
            tokio::task::spawn_blocking(move || {
                let argv = argv
                    .iter()
                    .map(|argument| (*argument).to_owned())
                    .collect::<Vec<_>>();
                leg.run_command("probe", &argv, stdin)
            })
            .await
            .expect("blocking dispatch completes")
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_rendered_run_reaches_the_script_with_its_status() {
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let rendered = CommandRunOutcome::Rendered {
                stdout: "Usage: probe <COMMAND>\n".to_owned(),
                stderr: String::new(),
                status: 0,
            };
            let (mut leg, mut observed) = stub_leg_observing(
                directory.path(),
                vec![ResponseEnvelope::command_run(rendered)],
                None,
            )
            .await;
            leg.command_words.insert("probe".to_owned());

            assert_eq!(
                run_word(leg, &["--help"], None).await,
                Some(CommandRun::Rendered {
                    stdout: "Usage: probe <COMMAND>\n".to_owned(),
                    stderr: String::new(),
                    status: 0,
                })
            );
            let request = observed.recv().await.expect("stub broker saw the run");
            assert_eq!(
                request.request,
                BrokerRequest::RunCommand {
                    attestation: None,
                    word: "probe".to_owned(),
                    argv: vec!["--help".to_owned()],
                    stdin: None,
                }
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn the_piped_value_travels_in_the_run_frame() {
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let proposed = CommandRunOutcome::Proposed {
                capability: "cli-probe.upper".parse().expect("valid capability fixture"),
                input: json!({"text": "hello"}),
            };
            let (mut leg, mut observed) = stub_leg_observing(
                directory.path(),
                vec![ResponseEnvelope::command_run(proposed)],
                None,
            )
            .await;
            leg.command_words.insert("probe".to_owned());

            assert_eq!(
                run_word(leg, &["upper", "-"], Some("hello")).await,
                Some(CommandRun::Proposed {
                    capability: "cli-probe.upper".to_owned(),
                    input: json!({"text": "hello"}),
                })
            );
            let request = observed.recv().await.expect("stub broker saw the run");
            assert!(
                matches!(
                    &request.request,
                    BrokerRequest::RunCommand { stdin: Some(piped), .. } if piped == "hello"
                ),
                "{request:?}"
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_cancel_signal_abandons_an_in_flight_run() {
            // The broker has the request and is not answering. A gateway Stop must not leave the
            // script parked on it: the node is aborted and joined, and the script reads the same
            // refusal the capability path gives a cancelled session.
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let (mut leg, mut observed, release) = stub_leg_parked(directory.path()).await;
            leg.command_words.insert("probe".to_owned());
            let (handle, signal) = CancelSignal::pair();
            let leg = leg.with_cancel_signal(signal);

            let run = tokio::task::spawn_blocking(move || {
                leg.run_command("probe", &["--help".to_owned()], None)
            });
            observed.recv().await.expect("the run reached the broker");
            handle.cancel();

            assert_eq!(
                run.await.expect("blocking dispatch completes"),
                Some(CommandRun::Denied {
                    reason: "session-cancelled".to_owned(),
                })
            );
            drop(release);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_transport_failure_names_its_cause_and_never_the_socket() {
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let socket = directory.path().join("dekopon-secret-broker.sock");
            let mut leg = leg_for(&socket);
            leg.command_words.insert("probe".to_owned());

            let Some(CommandRun::Errored { message }) = run_word(leg, &["--help"], None).await
            else {
                panic!("a missing broker socket is an infrastructure failure, not a decline");
            };
            assert!(
                message.starts_with("could not inspect broker socket: "),
                "{message}"
            );
            assert!(!message.contains("dekopon-secret-broker"), "{message}");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_word_no_provider_owns_never_reaches_the_broker() {
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let leg = leg_for(&directory.path().join("absent.sock"));

            assert_eq!(run_word(leg, &["--help"], None).await, None);
        }

        fn attestation() -> Attestation {
            Attestation {
                subject: SUBJECT
                    .parse::<ExternalSubject>()
                    .expect("canonical subject fixture"),
                agent: "chat-agent"
                    .parse::<AgentId>()
                    .expect("valid agent fixture"),
                scope: None,
                invocation: None,
            }
        }

        fn leg_with(socket: &Path, attestation: Option<Attestation>) -> BrokerLeg {
            let mut capabilities = BTreeMap::new();
            capabilities.insert(
                CAPABILITY.to_owned(),
                CapabilityDescription {
                    capability: CAPABILITY.to_owned(),
                    description: "Fetches one broker-authorized URI".to_owned(),
                    input_schema: json!({"type": "object"}),
                },
            );
            let namespaces = capabilities
                .keys()
                .map(|id| crate::runtime::namespace_of(id))
                .collect();
            BrokerLeg {
                client: BrokerClient::new(socket, server_uid(), FrameLimits::default())
                    .expect("stub broker client"),
                runtime: tokio::runtime::Handle::current(),
                capabilities,
                effective_capabilities: vec![EffectiveCapabilityView {
                    id: CAPABILITY.to_owned(),
                    provider: "http-probe".to_owned(),
                    description: "Fetches one broker-authorized URI".to_owned(),
                    effect: "read-only".to_owned(),
                    risk: "Low".to_owned(),
                    idempotency: "idempotent".to_owned(),
                }],
                command_words: BTreeSet::new(),
                namespaces,
                identifiers: IdSequence::new("dekopon-harness-test").expect("session identifiers"),
                attestation,
                chat_memory: None,
                surface_epoch: "fixture-epoch".parse().expect("fixture epoch"),
                // The same commitment `build` would make for this fixture surface, so a stub that
                // answers with the identical catalog is fresh and one that changes it is not.
                digest: crate::runtime::surface_digest(
                    &[available(CAPABILITY)],
                    &[],
                    None,
                    &"fixture-epoch".parse().expect("fixture epoch"),
                ),
                cancel: CancelSignal::never(),
                snapshot: crate::bootstrap::CapabilitySnapshot::empty(),
            }
        }

        /// Runs one dispatch the way an embedding binary does: from a blocking thread, never a
        /// worker.
        async fn invoke(leg: BrokerLeg, capability: &'static str) -> CapabilityCallResult {
            tokio::task::spawn_blocking(move || {
                leg.invoke(capability, json!({"uri": "http://x/"}), None)
            })
            .await
            .expect("blocking dispatch completes")
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_denied_invocation_stays_denied_all_the_way_to_the_exit_code() {
            // The interpreter maps `Denied` to 126 and `Failed` to 1. A model that reads "policy
            // refused this" as "the call errored" will retry something it must not retry, so this
            // distinction has to survive the whole trip back.
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let leg = stub_leg(
                directory.path(),
                vec![ResponseEnvelope::invocation(result(
                    InvocationOutcome::Denied,
                    Some("policy-denied"),
                ))],
            )
            .await;

            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Denied {
                    reason: "policy-denied".to_owned()
                }
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn an_unmapped_peer_is_a_denial_rather_than_an_infrastructure_failure() {
            // This refusal never reaches a decision record, so it arrives as a transport-level
            // code instead of a `Denied` outcome. It is still policy saying no.
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let leg = stub_leg(
                directory.path(),
                vec![ResponseEnvelope::error(
                    ERROR_UNAUTHENTICATED,
                    "peer is not mapped by broker policy",
                )],
            )
            .await;

            assert!(matches!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Denied { .. }
            ));
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_failed_invocation_carries_the_broker_reason_without_becoming_a_denial() {
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let leg = stub_leg(
                directory.path(),
                vec![ResponseEnvelope::invocation(result(
                    InvocationOutcome::Failed,
                    Some("provider trapped"),
                ))],
            )
            .await;

            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Failed {
                    error: "provider trapped".to_owned()
                }
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_successful_invocation_hands_provider_output_to_the_script() {
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let leg = stub_leg(
                directory.path(),
                vec![ResponseEnvelope::invocation(result(
                    InvocationOutcome::Succeeded,
                    None,
                ))],
            )
            .await;

            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Succeeded(json!({"status": 200}))
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn an_attested_leg_proposes_on_behalf_of_its_subject() {
            // The whole difference between the two legs is the frame, so assert on the frame. An
            // `invoke` here would be a gateway silently proposing as *itself*, which the broker
            // would answer under the daemon's own peer identity rather than the sender's.
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let (leg, mut observed) = stub_leg_observing(
                directory.path(),
                vec![ResponseEnvelope::invocation(result(
                    InvocationOutcome::Succeeded,
                    None,
                ))],
                Some(attestation()),
            )
            .await;

            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Succeeded(json!({"status": 200}))
            );

            let request = observed.recv().await.expect("stub broker saw one request");
            let BrokerRequest::Invoke {
                attestation: Some(attestation),
                invocation,
            } = request.request
            else {
                panic!("an attested leg must send an attested invoke frame: {request:?}");
            };
            assert_eq!(attestation.subject.canonical(), SUBJECT);
            assert_eq!(attestation.agent.as_str(), "chat-agent");
            // The claim binds to the proposal it travels with; the broker rejects a mismatch as a
            // protocol error rather than deciding it as policy.
            assert_eq!(attestation.invocation, Some(invocation.id));
            assert_eq!(invocation.capability.as_str(), CAPABILITY);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_refused_attestation_is_a_denial_rather_than_an_infrastructure_failure() {
            // A gateway whose grant does not cover this subject's namespace gets the same
            // transport-level code an unmapped peer gets. It is policy saying no, and a model that
            // reads it as "the call errored" will retry something it must not retry.
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let (leg, _observed) = stub_leg_observing(
                directory.path(),
                vec![ResponseEnvelope::error(
                    ERROR_UNAUTHENTICATED,
                    "attestation refused: no attestor authority for this subject",
                )],
                Some(attestation()),
            )
            .await;

            assert!(matches!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Denied { .. }
            ));
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_direct_leg_still_proposes_without_any_identity_claim() {
            // The original behavior has to stay byte-for-byte: adding an attested mode must not
            // start attaching claims to sessions that never asked for one.
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let (leg, mut observed) = stub_leg_observing(
                directory.path(),
                vec![ResponseEnvelope::invocation(result(
                    InvocationOutcome::Succeeded,
                    None,
                ))],
                None,
            )
            .await;

            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Succeeded(json!({"status": 200}))
            );

            let request = observed.recv().await.expect("stub broker saw one request");
            assert!(
                matches!(request.request, BrokerRequest::Invoke { .. }),
                "{request:?}"
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn capabilities_outside_the_session_never_reach_the_broker() {
            // No stub server at all: if this dispatched, the call would fail against a missing
            // socket instead of reporting the capability as absent.
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let leg = leg_for(&directory.path().join("absent.sock"));

            assert_eq!(
                invoke(leg, "totally.unknown").await,
                CapabilityCallResult::NotFound
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn transport_failures_never_disclose_where_the_broker_lives() {
            // The interpreter refuses to read the process environment precisely so a script cannot
            // learn about its host. This is the one path that could hand `DEKOPON_BROKER_SOCKET`
            // straight back to a model inside an error string.
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let socket = directory.path().join("dekopon-secret-broker.sock");
            let leg = leg_for(&socket);

            let CapabilityCallResult::Failed { error } = invoke(leg, CAPABILITY).await else {
                panic!("a missing broker socket is an infrastructure failure");
            };
            assert!(!error.contains("dekopon-secret-broker"), "{error}");
            assert!(!error.contains(&socket.display().to_string()), "{error}");
        }

        #[tokio::test]
        async fn invocation_identifiers_are_unique_and_extend_the_session_trace() {
            // The broker treats an invocation ID as a durable replay-rejection key, so a script
            // calling one capability in a loop must not collide with itself.
            let identifiers = IdSequence::new("dekopon-harness-test").expect("session identifiers");
            let first = identifiers.next_invocation().expect("first identifier");
            let second = identifiers.next_invocation().expect("second identifier");

            assert_ne!(first, second);
            let trace = identifiers.trace().as_str();
            assert!(first.as_str().starts_with(trace), "{first} vs {trace}");
            assert!(second.as_str().starts_with(trace), "{second} vs {trace}");
            assert!(trace.starts_with("dekopon-harness-test-"), "{trace}");

            // Two sessions in the same process must not share a key space either.
            let other =
                IdSequence::new("dekopon-harness-test").expect("second session identifiers");
            assert_ne!(identifiers.trace(), other.trace());
        }

        #[tokio::test]
        async fn an_invalid_trace_prefix_fails_at_construction() {
            // The prefix reaches identifier validation verbatim, so a bad one must fail here
            // rather than on the first invocation a model already committed to.
            assert!(IdSequence::new("Not A Prefix").is_err());
        }

        #[tokio::test]
        async fn a_prefix_only_the_derived_invocations_outgrow_fails_at_construction_too() {
            // A prefix can leave room for the trace and none for what the trace derives: 235
            // characters plus the separator and 16 hexadecimal digits is a 252-character trace,
            // one under the 253-byte identifier bound, while every invocation identifier built
            // from it is 11 characters longer. Accepting this would hand back a session whose
            // every capability call fails with "could not derive a unique invocation identifier".
            let prefix = "a".repeat(235);
            let trace = format!("{prefix}-{:016x}", 0_u64);
            assert_eq!(trace.len(), 252, "the trace itself must still be valid");
            assert!(trace.parse::<dekopon_core::TraceId>().is_ok());

            let Err(error) = IdSequence::new(&prefix) else {
                panic!("a prefix whose derived identifiers are too long must fail construction");
            };
            assert!(
                matches!(error, dekopon_core::IdentifierError::TooLong { .. }),
                "{error}"
            );
        }

        fn available(id: &str) -> dekopon_broker_protocol::AvailableCapability {
            serde_json::from_value(json!({
                "provider": "http-probe",
                "capability": {
                    "id": id,
                    "description": "Fetches one broker-authorized URI",
                    "effect": "read-only",
                    "risk": "Low",
                    "idempotency": "idempotent",
                    "inputSchema": {"type": "object"}
                }
            }))
            .expect("capability fixture decodes")
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn ordinary_safe_yields_fence_changed_or_uncertain_broker_surface_after_evidence() {
            use crate::{
                bootstrap::SessionBootstrap,
                history::History,
                session::{PromptError, PromptLimits, SessionEngine},
            };
            use dekopon_model::model::{
                AssistantTurn, ChatModel, ModelError, ModelFunctionCall, ModelMessage, ModelTool,
                ModelToolCall,
            };
            struct Model(std::sync::atomic::AtomicUsize);
            impl ChatModel for Model {
                fn complete(
                    &self,
                    _: &[ModelMessage],
                    _: &[ModelTool],
                    recorder: &dyn dekopon_model::usage::AttemptRecorder,
                ) -> Result<AssistantTurn, ModelError> {
                    assert_eq!(
                        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                        0,
                        "no subsequent inference"
                    );
                    let attempt = recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
                    recorder.observe(
                        attempt,
                        dekopon_model::usage::UsageObservation::from_json(
                            &json!({"input_tokens":7}),
                            false,
                        ),
                    )?;
                    Ok(AssistantTurn {
                        content: None,
                        tool_calls: vec![ModelToolCall {
                            id: "same-id".into(),
                            kind: "function".into(),
                            function: ModelFunctionCall {
                                name: "bash".into(),
                                arguments: json!({"script":CAPABILITY}).to_string(),
                            },
                        }],
                        usage: None,
                        replay_items: vec![],
                    })
                }
            }
            for uncertain in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                // Two surface answers, one invocation, then the change: the turn's pre-model check
                // and its post-completion disclosure check, which are the only two freshness
                // points in a turn. Neither dispatch nor the per-tool-call step refetches the
                // surface — the broker authorizes every invocation against its live policy.
                let mut responses = (0..2)
                    .map(|_| {
                        ResponseEnvelope::capabilities(
                            vec![available(CAPABILITY)],
                            vec![],
                            "fixture-epoch".parse().unwrap(),
                        )
                    })
                    .collect::<Vec<_>>();
                responses.push(ResponseEnvelope::invocation(result(
                    InvocationOutcome::Succeeded,
                    None,
                )));
                responses.push(if uncertain {
                    ResponseEnvelope::error(ERROR_UNAUTHENTICATED, "revoked")
                } else {
                    ResponseEnvelope::capabilities(vec![], vec![], "new-epoch".parse().unwrap())
                });
                let (leg, mut received) =
                    stub_leg_observing(directory.path(), responses, None).await;
                tokio::task::spawn_blocking(move || {
                    let runtime = crate::runtime::ShellRuntime {
                        invoker: leg,
                        limits: dekopon_shell::Limits::default(),
                        curl_capability: None,
                    };
                    let mut history = History::default();
                    let error = SessionEngine::new(&Model(Default::default()), &runtime)
                        .run(
                            SessionBootstrap::new(
                                "request",
                                PromptLimits {
                                    max_steps: 3,
                                    max_capability_calls: 3,
                                },
                                "fixture",
                            ),
                            &mut history,
                        )
                        .unwrap_err();
                    let PromptError::Interrupted { checkpoint, source } = error else {
                        panic!("must fence checkpoint")
                    };
                    assert_eq!(source, crate::checkpoint::CheckpointError::ScopeChanged);
                    assert_eq!(checkpoint.record.executions.len(), 1);
                    assert_eq!(
                        checkpoint.record.executions[0].outcome,
                        crate::history::ExecutionOutcome::Succeeded
                    );
                    assert_eq!(checkpoint.state.accounting.calls.len(), 1);
                    assert!(checkpoint.record.generated.is_none());
                })
                .await
                .unwrap();
                let mut count = 0;
                while received.try_recv().is_ok() {
                    count += 1;
                }
                assert_eq!(count, 4);
            }
        }

        /// A stub broker that answers by request kind rather than from a fixed queue, counting the
        /// `Capabilities` requests a session makes.
        ///
        /// It answers an unbounded number of them on purpose: the property under test is that the
        /// count does *not* grow with the number of capabilities a script drives, and a queue whose
        /// length encodes the expected answer would make the test restate its own expectation.
        async fn counting_stub(directory: &Path) -> (BrokerLeg, std::sync::Arc<CountedRequests>) {
            let socket = directory.join("broker.sock");
            let listener = UnixListener::bind(&socket).expect("bind stub broker");
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
                .expect("secure stub socket");
            let counts = std::sync::Arc::new(CountedRequests::default());
            let observed = std::sync::Arc::clone(&counts);
            tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let Ok(request) =
                        read_frame::<_, RequestEnvelope>(&mut stream, FrameLimits::default()).await
                    else {
                        return;
                    };
                    let response = match request.request {
                        dekopon_broker_protocol::BrokerRequest::Capabilities { .. } => {
                            observed.surfaces.fetch_add(1, atomic::Ordering::SeqCst);
                            ResponseEnvelope::capabilities(
                                vec![available(CAPABILITY)],
                                vec![],
                                "fixture-epoch".parse().expect("fixture epoch"),
                            )
                        }
                        _ => {
                            observed.invocations.fetch_add(1, atomic::Ordering::SeqCst);
                            ResponseEnvelope::invocation(result(InvocationOutcome::Succeeded, None))
                        }
                    };
                    if write_frame(&mut stream, &response, FrameLimits::default())
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            });
            (leg_with(&socket, None), counts)
        }

        #[derive(Default)]
        struct CountedRequests {
            surfaces: atomic::AtomicUsize,
            invocations: atomic::AtomicUsize,
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn freshness_round_trips_do_not_grow_with_the_capabilities_a_script_drives() {
            use crate::{
                bootstrap::SessionBootstrap,
                history::History,
                session::{PromptLimits, SessionEngine},
            };
            use dekopon_model::model::{
                AssistantTurn, ChatModel, ModelError, ModelFunctionCall, ModelMessage, ModelTool,
                ModelToolCall,
            };
            /// One bash tool call running `calls` capabilities, then a plain answer.
            struct Model {
                calls: usize,
                turn: atomic::AtomicUsize,
            }
            impl ChatModel for Model {
                fn complete(
                    &self,
                    _: &[ModelMessage],
                    _: &[ModelTool],
                    recorder: &dyn dekopon_model::usage::AttemptRecorder,
                ) -> Result<AssistantTurn, ModelError> {
                    let attempt = recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
                    recorder.observe(
                        attempt,
                        dekopon_model::usage::UsageObservation::from_json(
                            &json!({"input_tokens":7}),
                            false,
                        ),
                    )?;
                    if self.turn.fetch_add(1, atomic::Ordering::SeqCst) > 0 {
                        return Ok(AssistantTurn {
                            content: Some("done".into()),
                            tool_calls: vec![],
                            usage: None,
                            replay_items: vec![],
                        });
                    }
                    let script = vec![format!("{CAPABILITY} > /dev/null"); self.calls].join("\n");
                    Ok(AssistantTurn {
                        content: None,
                        tool_calls: vec![ModelToolCall {
                            id: "call-1".into(),
                            kind: "function".into(),
                            function: ModelFunctionCall {
                                name: "bash".into(),
                                arguments: json!({ "script": script }).to_string(),
                            },
                        }],
                        usage: None,
                        replay_items: vec![],
                    })
                }
            }

            let mut observed = Vec::new();
            for calls in [1_usize, 3] {
                let directory = tempfile::tempdir().expect("temporary broker directory");
                let (leg, counts) = counting_stub(directory.path()).await;
                tokio::task::spawn_blocking(move || {
                    let runtime = crate::runtime::ShellRuntime {
                        invoker: leg,
                        limits: dekopon_shell::Limits::default(),
                        curl_capability: None,
                    };
                    let model = Model {
                        calls,
                        turn: atomic::AtomicUsize::new(0),
                    };
                    let mut history = History::default();
                    SessionEngine::new(&model, &runtime)
                        .run(
                            SessionBootstrap::new(
                                "request",
                                PromptLimits {
                                    max_steps: 4,
                                    max_capability_calls: 8,
                                },
                                "fixture",
                            ),
                            &mut history,
                        )
                        .expect("the session completes");
                })
                .await
                .expect("blocking session completes");
                observed.push((
                    counts.surfaces.load(atomic::Ordering::SeqCst),
                    counts.invocations.load(atomic::Ordering::SeqCst),
                ));
            }

            assert_eq!(observed[0].1, 1, "one capability, one dispatch");
            assert_eq!(observed[1].1, 3, "three capabilities, three dispatches");
            // The whole point: freshness is a turn-boundary disclosure gate, so tripling the
            // capabilities a script drives must not triple the broker traffic the session makes.
            assert_eq!(
                observed[0].0, observed[1].0,
                "freshness round trips scaled with capability invocations: {observed:?}"
            );
        }

        #[test]
        fn each_kind_of_surface_change_reports_its_own_cause() {
            use dekopon_broker_protocol::ChatMemorySurface;
            use dekopon_shell::SurfaceChange;
            let epoch = || "fixture-epoch".parse().expect("fixture epoch");
            let memory = ChatMemorySurface {
                max_lookback_turns: 4,
                prompt_note: "note".to_owned(),
            };
            let words = vec!["probe".to_owned(), "echo".to_owned()];
            let base = crate::runtime::surface_digest(
                &[available(CAPABILITY), available("echo.echo")],
                &words,
                Some(&memory),
                &epoch(),
            );

            // The same surface with the capabilities and the command words listed in the other
            // order: not a change. Both indexed views and the word list are order-independent, so
            // reporting a reordered answer as a fence would stop live sessions for nothing.
            let reordered = crate::runtime::surface_digest(
                &[available("echo.echo"), available(CAPABILITY)],
                &["echo".to_owned(), "probe".to_owned(), "echo".to_owned()],
                Some(&memory),
                &epoch(),
            );
            assert_eq!(base.changed(&reordered), None);

            let mut redescribed = available(CAPABILITY);
            redescribed.capability.description = "Fetches something else".to_owned();
            let redescribed = [redescribed, available("echo.echo")];
            let mut reclassified = available(CAPABILITY);
            reclassified.provider = "other-probe".parse().expect("provider fixture");
            let reclassified = [reclassified, available("echo.echo")];
            let unchanged = [available(CAPABILITY), available("echo.echo")];
            let quieter = ChatMemorySurface {
                max_lookback_turns: 2,
                prompt_note: memory.prompt_note.clone(),
            };
            for (fresh, expected) in [
                (
                    crate::runtime::surface_digest(
                        &unchanged,
                        &words,
                        Some(&memory),
                        &"restarted-epoch".parse().expect("fixture epoch"),
                    ),
                    SurfaceChange::Epoch,
                ),
                (
                    crate::runtime::surface_digest(&redescribed, &words, Some(&memory), &epoch()),
                    SurfaceChange::Descriptions,
                ),
                (
                    crate::runtime::surface_digest(&reclassified, &words, Some(&memory), &epoch()),
                    SurfaceChange::EffectiveViews,
                ),
                (
                    crate::runtime::surface_digest(&unchanged, &[], Some(&memory), &epoch()),
                    SurfaceChange::CommandWords,
                ),
                (
                    crate::runtime::surface_digest(&unchanged, &words, Some(&quieter), &epoch()),
                    SurfaceChange::ChatMemory,
                ),
            ] {
                assert_eq!(base.changed(&fresh), Some(expected));
                let error = dekopon_shell::FreshnessError::Changed(expected);
                assert!(
                    error.to_string().contains(expected.as_str()),
                    "{error} does not name its cause"
                );
            }
        }

        #[test]
        fn a_duplicated_capability_identifier_is_a_malformed_broker_answer() {
            // Last-wins here would leave `cap --list` and `inspect_agent_config` describing
            // different sessions: the map keeps one entry per identifier and the effective view
            // keeps every entry it was handed. Every repeat is named at once, the way the rest of
            // the workspace reports conflicts.
            let error = crate::runtime::snapshot(vec![
                available("http-probe.fetch"),
                available("echo.echo"),
                available("http-probe.fetch"),
                available("echo.echo"),
            ])
            .expect_err("a duplicate identifier is refused");

            assert!(
                matches!(
                    &error,
                    crate::runtime::BrokerLegError::DuplicateCapabilities { capabilities }
                        if capabilities == "echo.echo, http-probe.fetch"
                ),
                "{error}"
            );
        }

        #[test]
        fn a_distinct_capability_set_indexes_both_views() {
            let (descriptions, effective) = crate::runtime::snapshot(vec![
                available("http-probe.fetch"),
                available("echo.echo"),
            ])
            .expect("a distinct set is accepted");

            assert_eq!(descriptions.len(), 2);
            assert_eq!(
                effective
                    .iter()
                    .map(|view| view.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["echo.echo", "http-probe.fetch"]
            );
        }
    }
}

#[cfg(test)]
mod execution_observation_tests {
    use super::*;
    use crate::{
        bootstrap::SessionBootstrap,
        history::{ExecutionOutcome, ExecutionProvenance, History},
        session::{PromptLimits, SessionEngine},
    };
    use dekopon_model::model::{
        AssistantTurn, ChatModel, ModelError, ModelFunctionCall, ModelMessage, ModelTool,
        ModelToolCall,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Model;
    impl ChatModel for Model {
        fn complete(
            &self,
            _: &[ModelMessage],
            _: &[ModelTool],
            recorder: &dyn dekopon_model::usage::AttemptRecorder,
        ) -> Result<AssistantTurn, ModelError> {
            let attempt = recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
            let result: Result<AssistantTurn, ModelError> = {
                Ok(AssistantTurn {
                    content: None,
                    tool_calls: vec![ModelToolCall {
                        id: "batch".to_owned(),
                        kind: "function".to_owned(),
                        function: ModelFunctionCall {
                            name: "bash".to_owned(),
                            arguments: json!({"script":"test.read; test.read"}).to_string(),
                        },
                    }],
                    usage: None,
                    replay_items: Vec::new(),
                })
            };
            if let Ok(turn) = &result
                && let Some(usage) = turn.usage
            {
                recorder.observe(
                    attempt,
                    dekopon_model::usage::UsageObservation {
                        usage,
                        invalid: [false; 5],
                    },
                )?;
            }
            result
        }
    }
    struct UnknownBroker(AtomicUsize);
    impl CapabilityInvoker for UnknownBroker {
        fn granted(&self) -> Vec<String> {
            vec!["test.read".to_owned()]
        }
        fn describe(&self, _: &str) -> Option<dekopon_shell::CapabilityDescription> {
            Some(dekopon_shell::CapabilityDescription {
                capability: "test.read".to_owned(),
                description: "fixture".to_owned(),
                input_schema: json!({"type":"object"}),
            })
        }
        fn invoke(
            &self,
            _: &str,
            _: Value,
            _: Option<dekopon_core::SecretUseProposal>,
        ) -> CapabilityCallResult {
            self.0.fetch_add(1, Ordering::SeqCst);
            dispatch_detail(DispatchDetail {
                provenance: ExecutionProvenance::BrokerObserved,
                invocation: Some("invocation-fixture".to_owned()),
                evidence: vec!["sha256:fixture".to_owned()],
                outcome: ExecutionOutcome::Unknown,
            });
            CapabilityCallResult::Denied {
                reason: "unknown broker outcome, do not resubmit".to_owned(),
            }
        }
    }
    #[test]
    fn typed_unknown_broker_evidence_is_not_misrecorded_as_shell_denial_or_retried() {
        let runtime = ShellRuntime {
            invoker: UnknownBroker(AtomicUsize::new(0)),
            limits: ShellLimits::default(),
            curl_capability: None,
        };
        let mut history = History::default();
        let result = SessionEngine::new(&Model, &runtime).run(
            SessionBootstrap::new(
                "request",
                PromptLimits {
                    max_steps: 4,
                    max_capability_calls: 4,
                },
                "fixture",
            ),
            &mut history,
        );
        assert!(result.is_err());
        assert_eq!(runtime.invoker.0.load(Ordering::SeqCst), 1);
        let record = &history.turns()[0].executions[0];
        assert_eq!(record.outcome, ExecutionOutcome::Unknown);
        assert_eq!(record.provenance, ExecutionProvenance::BrokerObserved);
        assert_eq!(record.invocation.as_deref(), Some("invocation-fixture"));
        assert_eq!(record.evidence, vec!["sha256:fixture".to_owned()]);
        assert!(history.has_unknown_work());
    }
}
