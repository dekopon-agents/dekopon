//! The reusable agent session layer shared by Dekopon's embedding binaries.
//!
//! `dekopon-run` drives one prompt session from a CLI; `dekopond` drives many from chat transports. Both need the same four pieces, and this crate is where they live so there is one
//! authoritative copy:
//!
//! - [`prompt::run_prompt`] — the bounded model tool loop offering one sandboxed scripting tool,
//!   with [`prompt::run_prompt_with_history`] running that same loop as the continuation of a
//!   bounded [`prompt::History`], [`prompt::SessionInputs`] optionally carrying cooperative
//!   cancellation for transport-owned Stop controls or a request-scoped no-reply decision, and
//!   [`prompt::run_prompt_with_history_and_options`] adding the request-scoped routing metadata a
//!   caller uses to point one conversation's turns at one provider cache lane;
//! - [`ShellRuntime`] — the [`prompt::ScriptRuntime`] that runs each model-authored script on a
//!   fresh `dekopon-shell` interpreter under a session-wide capability budget;
//! - [`SessionInvoker`] — capability dispatch that prefers a local read-only leg and falls through
//!   to a broker leg;
//! - [`BrokerLeg`] — a synchronous [`CapabilityInvoker`] facade over the asynchronous
//!   [`BrokerClient`], for sessions that run on a blocking task.
//!
//! Nothing here holds authority. The broker leg submits identity-free proposals and reports back
//! whatever the broker decided; this crate never interprets policy, resolves credentials, or
//! constructs authorization state, and it deliberately depends only on the client half of the
//! broker protocol.

#![forbid(unsafe_code)]

use std::time::Duration;

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
    Attestation, BrokerClient, ChatMemorySurface, ClientError, ERROR_UNAUTHENTICATED,
    InvocationOutcome, InvocationRequest,
};
#[cfg(unix)]
use dekopon_core::{CapabilityId, IdentifierError, InvocationId, TraceId};
#[cfg(unix)]
use dekopon_shell::CapabilityDescription;
use dekopon_shell::{
    CapabilityCallResult, CapabilityInvoker, Interpreter, Limits as ShellLimits, ScriptOutcome,
};
use serde_json::Value;
#[cfg(unix)]
use thiserror::Error;

use crate::{meta::EffectiveCapabilityView, prompt::ScriptRuntime};

pub mod improvement;
pub mod meta;
pub mod prompt;
pub mod replay;
pub mod skills;

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

    fn command_words(&self) -> Vec<String> {
        self.invoker.command_words()
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

    fn resolve_command(
        &self,
        word: &str,
        argv: &[String],
    ) -> Option<Result<(String, Value), String>> {
        // Same precedence as `invoke`: whichever leg owns the word rewrites it. A word both legs
        // claim cannot happen — the broker refuses to start on a duplicate, and direct mode loads
        // its own registry through the same check.
        self.direct
            .resolve_command(word, argv)
            .or_else(|| self.broker.as_ref()?.resolve_command(word, argv))
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

/// Failure to open a session's broker leg.
#[cfg(unix)]
#[derive(Debug, Error)]
pub enum BrokerLegError {
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
}

#[cfg(unix)]
impl BrokerLeg {
    /// Connects one session's broker leg, snapshotting its capability set.
    ///
    /// The snapshot happens here, on the async side, for two reasons. It lets `cap --list` answer
    /// without a round trip per script, and it turns "the daemon is not running" into one clear
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
        let (capabilities, command_words, chat_memory) =
            client.session_surface(attestation.clone()).await?;
        Self::build(
            client,
            trace_prefix,
            capabilities,
            command_words,
            attestation,
            chat_memory,
        )
    }

    fn build(
        client: BrokerClient,
        trace_prefix: &str,
        available: Vec<dekopon_broker_protocol::AvailableCapability>,
        command_words: Vec<String>,
        attestation: Option<Attestation>,
        chat_memory: Option<ChatMemorySurface>,
    ) -> Result<Self, BrokerLegError> {
        let (capabilities, effective_capabilities) = snapshot(available)?;
        let namespaces = capabilities.keys().map(|id| namespace_of(id)).collect();
        Ok(Self {
            client,
            runtime: tokio::runtime::Handle::current(),
            capabilities,
            effective_capabilities,
            command_words: command_words.into_iter().collect(),
            namespaces,
            identifiers: IdSequence::new(trace_prefix)
                .map_err(BrokerLegError::SessionIdentifier)?,
            attestation,
            chat_memory,
        })
    }

    /// Returns this session's trusted, subject-specific effective capability classification.
    ///
    /// This is the same fresh broker answer that backs `cap --list`. It contains no policy source,
    /// policy identifier, subject, principal, constraint, or credential metadata.
    #[must_use]
    pub fn effective_capabilities(&self) -> Vec<EffectiveCapabilityView> {
        self.effective_capabilities.clone()
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

    fn resolve_command(
        &self,
        word: &str,
        argv: &[String],
    ) -> Option<Result<(String, Value), String>> {
        // Same visibility check the capability path makes, and for the same reason: the broker
        // decides refusals, this only avoids spending a round trip on a word no provider owns.
        if !self.command_words.contains(word) {
            return None;
        }
        // Safe for the reason `invoke` documents: this runs on a `spawn_blocking` thread.
        let resolved = self.runtime.block_on(async {
            self.client
                .resolve_command(self.attestation.clone(), word.to_owned(), argv.to_vec())
                .await
        });
        match resolved {
            Ok(Ok((capability, input))) => Some(Ok((capability.to_string(), input))),
            Ok(Err(message)) => Some(Err(message)),
            Err(error) => Some(Err(format!("{word}: {error}"))),
        }
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

pub(crate) fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
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
        };

        use dekopon_broker_protocol::{
            BrokerClient, BrokerRequest, ERROR_UNAUTHENTICATED, FrameLimits, InvocationOutcome,
            InvocationResult, RequestEnvelope, ResponseEnvelope, read_frame, write_frame,
        };
        use dekopon_capability::DecisionReference;
        use dekopon_core::{AgentId, ExternalSubject};
        use dekopon_shell::{CapabilityCallResult, CapabilityDescription, CapabilityInvoker};
        use serde_json::json;
        use tokio::{net::UnixListener, sync::mpsc};

        use crate::{Attestation, BrokerLeg, IdSequence, meta::EffectiveCapabilityView};

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
                .map(|id| crate::namespace_of(id))
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
                identifiers: IdSequence::new("dekopon-agent-test").expect("session identifiers"),
                attestation,
                chat_memory: None,
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
            let identifiers = IdSequence::new("dekopon-agent-test").expect("session identifiers");
            let first = identifiers.next_invocation().expect("first identifier");
            let second = identifiers.next_invocation().expect("second identifier");

            assert_ne!(first, second);
            let trace = identifiers.trace().as_str();
            assert!(first.as_str().starts_with(trace), "{first} vs {trace}");
            assert!(second.as_str().starts_with(trace), "{second} vs {trace}");
            assert!(trace.starts_with("dekopon-agent-test-"), "{trace}");

            // Two sessions in the same process must not share a key space either.
            let other = IdSequence::new("dekopon-agent-test").expect("second session identifiers");
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

        #[test]
        fn a_duplicated_capability_identifier_is_a_malformed_broker_answer() {
            // Last-wins here would leave `cap --list` and `inspect_agent_config` describing
            // different sessions: the map keeps one entry per identifier and the effective view
            // keeps every entry it was handed. Every repeat is named at once, the way the rest of
            // the workspace reports conflicts.
            let error = crate::snapshot(vec![
                available("http-probe.fetch"),
                available("echo.echo"),
                available("http-probe.fetch"),
                available("echo.echo"),
            ])
            .expect_err("a duplicate identifier is refused");

            assert!(
                matches!(
                    &error,
                    crate::BrokerLegError::DuplicateCapabilities { capabilities }
                        if capabilities == "echo.echo, http-probe.fetch"
                ),
                "{error}"
            );
        }

        #[test]
        fn a_distinct_capability_set_indexes_both_views() {
            let (descriptions, effective) =
                crate::snapshot(vec![available("http-probe.fetch"), available("echo.echo")])
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
