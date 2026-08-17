//! The reusable agent session layer shared by Dekopon's embedding binaries.
//!
//! `dekopon-run` drives one prompt session from a CLI; `dekopond` drives many from chat transports. Both need the same four pieces, and this crate is where they live so there is one
//! authoritative copy:
//!
//! - [`prompt::run_prompt`] — the bounded model tool loop offering one sandboxed scripting tool,
//!   with [`prompt::run_prompt_with_history`] running that same loop as the continuation of a
//!   bounded [`prompt::History`] for transports whose next message continues a conversation, and
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
    collections::BTreeMap,
    collections::hash_map::RandomState,
    hash::{BuildHasher as _, Hasher as _},
    sync::atomic::{AtomicU32, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use dekopon_broker_protocol::TraceParent;
#[cfg(unix)]
use dekopon_broker_protocol::{
    BrokerClient, ClientError, ERROR_UNAUTHENTICATED, InvocationOutcome, InvocationRequest,
};
#[cfg(unix)]
use dekopon_core::{
    AgentId, CapabilityId, ExternalSubject, IdentifierError, InvocationId, TraceId,
};
#[cfg(unix)]
use dekopon_shell::CapabilityDescription;
use dekopon_shell::{
    CapabilityCallResult, CapabilityInvoker, Interpreter, Limits as ShellLimits, ScriptOutcome,
};
use serde_json::Value;
#[cfg(unix)]
use thiserror::Error;

use crate::prompt::ScriptRuntime;

pub mod prompt;

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

    fn describe(&self, capability: &str) -> Option<dekopon_shell::CapabilityDescription> {
        self.direct.describe(capability).or_else(|| {
            self.broker
                .as_ref()
                .and_then(|broker| broker.describe(capability))
        })
    }

    fn invoke(&self, capability: &str, input: Value) -> CapabilityCallResult {
        if self.direct.is_granted(capability) {
            return self.direct.invoke(capability, input);
        }
        match &self.broker {
            Some(broker) => broker.invoke(capability, input),
            None => CapabilityCallResult::NotFound,
        }
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
}

/// The on-behalf-of claim an attested leg attaches to every call it makes.
///
/// Held whole rather than as two loose fields because the pair is meaningless apart: a subject
/// without the agent orchestrating for it names no context the broker can resolve, and the broker
/// matches `via`-scoped rules on both.
#[cfg(unix)]
struct Attestation {
    subject: ExternalSubject,
    agent: AgentId,
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
    identifiers: IdSequence,
    /// `None` for a leg that speaks as its own connected peer, which is the original behavior.
    attestation: Option<Attestation>,
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
    pub async fn connect(client: BrokerClient, trace_prefix: &str) -> Result<Self, BrokerLegError> {
        let capabilities = snapshot(client.capabilities().await?);
        Self::build(client, trace_prefix, capabilities, None)
    }

    /// Connects a leg that proposes on behalf of one transport-authenticated external subject.
    ///
    /// A chat gateway holds no broker authority of its own: it knows which subject sent a message
    /// and which agent is answering, and the broker decides everything else. So the snapshot comes
    /// from `capabilitiesFor` rather than `capabilities` — what this leg reports as granted is what
    /// policy makes visible to the *attested* context, not to the daemon's own peer identity.
    ///
    /// An empty snapshot is a valid result rather than an error. It means "policy grants this
    /// subject nothing through this agent", which a gateway answers very differently from "the
    /// broker is unreachable"; deciding which of those to say is the caller's job.
    pub async fn connect_attested(
        client: BrokerClient,
        trace_prefix: &str,
        subject: ExternalSubject,
        agent: AgentId,
    ) -> Result<Self, BrokerLegError> {
        let capabilities = snapshot(
            client
                .capabilities_for(subject.clone(), agent.clone())
                .await?,
        );
        Self::build(
            client,
            trace_prefix,
            capabilities,
            Some(Attestation { subject, agent }),
        )
    }

    fn build(
        client: BrokerClient,
        trace_prefix: &str,
        capabilities: BTreeMap<String, CapabilityDescription>,
        attestation: Option<Attestation>,
    ) -> Result<Self, BrokerLegError> {
        Ok(Self {
            client,
            runtime: tokio::runtime::Handle::current(),
            capabilities,
            identifiers: IdSequence::new(trace_prefix)
                .map_err(BrokerLegError::SessionIdentifier)?,
            attestation,
        })
    }
}

/// Indexes a capability snapshot by identifier for the interpreter's lookups.
#[cfg(unix)]
fn snapshot(
    capabilities: Vec<dekopon_broker_protocol::AvailableCapability>,
) -> BTreeMap<String, CapabilityDescription> {
    capabilities
        .into_iter()
        .map(|available| {
            (
                available.capability.id.to_string(),
                CapabilityDescription {
                    capability: available.capability.id.to_string(),
                    description: available.capability.description,
                    input_schema: available.capability.input_schema,
                },
            )
        })
        .collect()
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

    fn invoke(&self, capability: &str, input: Value) -> CapabilityCallResult {
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
            input,
        };

        // Safe specifically because this runs on a `spawn_blocking` thread rather than a runtime
        // worker: `Handle::block_on` from a worker would deadlock the executor, and from the
        // blocking pool it is the ordinary bridge back into async code.
        let submitted = self.runtime.block_on(async {
            match &self.attestation {
                Some(attestation) => {
                    self.client
                        .invoke_for(
                            request,
                            attestation.subject.clone(),
                            attestation.agent.clone(),
                        )
                        .await
                }
                None => self.client.invoke(request).await,
            }
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
    /// The prefix must itself be a valid identifier component (lowercase, `.`/`-`/`_`), because
    /// the derived trace identifier is validated before use and a bad prefix fails here rather
    /// than on the first invocation.
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
    }

    impl FakeLeg {
        fn new(capability: &'static str, marker: &'static str) -> Self {
            Self {
                capability,
                marker,
                invoked: std::sync::Mutex::new(Vec::new()),
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

        fn invoke(&self, capability: &str, _input: Value) -> CapabilityCallResult {
            if capability != self.capability {
                return CapabilityCallResult::NotFound;
            }
            self.invoked
                .lock()
                .expect("invocation lock")
                .push(capability.to_owned());
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
            invoker.invoke("shared.capability", json!({})),
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
            invoker.invoke("http-probe.fetch", json!({})),
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
            invoker.invoke("http-probe.fetch", json!({})),
            CapabilityCallResult::NotFound
        );
    }

    #[cfg(unix)]
    mod broker_leg {
        use std::{collections::BTreeMap, os::unix::fs::PermissionsExt as _, path::Path};

        use dekopon_broker_protocol::{
            BrokerClient, BrokerRequest, ERROR_UNAUTHENTICATED, FrameLimits, InvocationOutcome,
            InvocationResult, RequestEnvelope, ResponseEnvelope, read_frame, write_frame,
        };
        use dekopon_capability::DecisionReference;
        use dekopon_core::{AgentId, ExternalSubject};
        use dekopon_shell::{CapabilityCallResult, CapabilityDescription, CapabilityInvoker};
        use serde_json::json;
        use tokio::{net::UnixListener, sync::mpsc};

        use crate::{Attestation, BrokerLeg, IdSequence};

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
            BrokerLeg {
                client: BrokerClient::new(socket, server_uid(), FrameLimits::default())
                    .expect("stub broker client"),
                runtime: tokio::runtime::Handle::current(),
                capabilities,
                identifiers: IdSequence::new("dekopon-agent-test").expect("session identifiers"),
                attestation,
            }
        }

        /// Runs one dispatch the way an embedding binary does: from a blocking thread, never a
        /// worker.
        async fn invoke(leg: BrokerLeg, capability: &'static str) -> CapabilityCallResult {
            tokio::task::spawn_blocking(move || leg.invoke(capability, json!({"uri": "http://x/"})))
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
            let BrokerRequest::InvokeFor {
                invocation,
                attestation,
            } = request.request
            else {
                panic!("an attested leg must send an invokeFor frame: {request:?}");
            };
            assert_eq!(attestation.subject.canonical(), SUBJECT);
            assert_eq!(attestation.agent.as_str(), "chat-agent");
            // The claim binds to the proposal it travels with; the broker rejects a mismatch as a
            // protocol error rather than deciding it as policy.
            assert_eq!(attestation.invocation, invocation.id);
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
    }
}
