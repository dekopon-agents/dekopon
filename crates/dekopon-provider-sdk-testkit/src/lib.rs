//! An in-process fake broker for testing Dekopon provider components.
//!
//! A provider's behavior only fully exists when its compiled component runs against a host. HTTP
//! providers can approximate that natively by injecting a `FnMut(Request) -> Result<Response,
//! HttpError>` transport, but storage providers cannot: `dekopon-provider-storage` exposes free
//! functions that call the WIT import directly, and off `wasm32` those bindings expand to
//! `unreachable!()`. This crate closes that gap by running the real component.
//!
//! It is a *fake broker*, not a fake host. Policy and the constraint catalog are skipped —
//! [`FakeBroker`] mints its own authorization through
//! [`AuthorizationGate`], which is the allow-all
//! equivalent, since Dekopon has no wildcard grant spelling. Everything below that line is real:
//! the same Wasmtime host, the same [`StorageHost`], and the same [`StorageLimits`] a deployment
//! runs. A quota a test trips here is a quota production would have tripped.
//!
//! ```no_run
//! # use dekopon_provider_sdk_testkit::{FakeBroker, StorageAccess, StorageInterface};
//! # use serde_json::json;
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let broker = FakeBroker::builder()
//!     .component("turso-sql-provider.wasm")
//!     .provider("turso")
//!     .storage(StorageInterface::DurableFiles, StorageAccess::ReadWrite)
//!     .build()
//!     .await?;
//!
//! broker
//!     .invoke("turso.exec", json!({"statements": ["CREATE TABLE t(a INTEGER)"]}))
//!     .await?;
//!
//! // A second call reaches the same durable namespace.
//! let rows = broker
//!     .invoke("turso.exec", json!({"statements": ["SELECT count(*) FROM t"]}))
//!     .await?;
//! # let _ = rows;
//! # Ok(())
//! # }
//! ```
//!
//! # Tests must use a multi-thread runtime
//!
//! The storage path dispatches to `tokio::task::spawn_blocking`; a current-thread runtime
//! deadlocks waiting for a namespace lease. Annotate tests with
//! `#[tokio::test(flavor = "multi_thread")]`.

#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use dekopon_capability::broker::AuthorizationGate;
use dekopon_storage_host::StorageGrantRequest;
use serde_json::Value;
use tempfile::TempDir;

pub use dekopon_broker_host::{
    BrokerHostError, BrokerHostLimits, BrokerHostOptions, BrokerInvocationFailure,
    BrokerInvocationOutput, BrokerProviderRegistry, CommandRunOutcome,
};
pub use dekopon_capability::{
    AuthorizationError, ExecutionConstraints, ProposedInvocation, StorageAccess,
    StorageConstraints, StorageInterface, StorageNamespace,
};
pub use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, IdentifierError, InvocationId, PrincipalId,
    ProviderId, SubjectError, TraceId,
};
pub use dekopon_storage_host::{
    ContinuityPolicy, StorageEvidence, StorageHost, StorageHostError, StorageLimits,
};

/// Re-exported here rather than from the guest SDK so a test does not accidentally mix two
/// semver-incompatible dekopon-* versions of the same type.
pub mod prelude {
    pub use super::{
        BrokerInvocationOutput, CapabilityId, CommandRunOutcome, FakeBroker, FakeBrokerError,
        ProviderId, StorageAccess, StorageEvidence, StorageInterface,
    };
}

/// Anything that can stop a fake invocation, kept distinguishable so a test can assert on cause.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FakeBrokerError {
    /// [`FakeBrokerBuilder::component`] was never called.
    #[error("no component path was configured")]
    NoComponent,
    /// [`FakeBrokerBuilder::provider`] was never called.
    #[error("no provider id was configured")]
    NoProvider,
    /// The component path does not exist, usually because it is an untracked build artifact that
    /// has not been built yet via build.sh.
    #[error("provider component {} does not exist; build it first", path.display())]
    ComponentMissing {
        /// The path that was configured.
        path: PathBuf,
    },
    /// A configured identifier is not a valid Dekopon identifier.
    #[error("invalid identifier: {0}")]
    Identifier(#[from] IdentifierError),
    /// The configured external subject is malformed.
    #[error("invalid external subject: {0}")]
    Subject(#[from] SubjectError),
    /// Creating the temporary root failed.
    #[error("preparing the temporary storage root failed: {0}")]
    Io(#[from] std::io::Error),
    /// The storage host refused to open the root or to mint a grant.
    #[error(transparent)]
    Storage(#[from] StorageHostError),
    /// The component failed to compile, load, or expose the requested capability.
    #[error(transparent)]
    Host(#[from] BrokerHostError),
    /// The synthesized authorization was itself invalid.
    #[error(transparent)]
    Authorization(#[from] AuthorizationError),
    /// The invocation ran and failed.
    #[error("invocation failed: {0}")]
    Invocation(#[source] Box<BrokerInvocationFailure>),
}

impl FakeBrokerError {
    /// Returns the provider-declared `(code, message)` when the guest returned a structured
    /// failure, rather than the host refusing the call before or after the guest ran.
    ///
    /// Asserting on the code is the difference between "the provider refused this for the reason
    /// it documents" and "something, somewhere, went wrong".
    #[must_use]
    pub fn provider_failure(&self) -> Option<(&str, &str)> {
        let Self::Invocation(failure) = self else {
            return None;
        };
        match failure.error.as_ref() {
            BrokerHostError::ProviderFailure { code, message, .. } => Some((code, message)),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct Scope {
    agent: String,
    subject: String,
    transport_kind: String,
    transport: String,
    channel: String,
    conversation: String,
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            agent: "testkit-agent".to_owned(),
            subject: "slack.t0123abc.u9xyz".to_owned(),
            transport_kind: "slack".to_owned(),
            transport: "testkit-transport".to_owned(),
            channel: "c0123abc".to_owned(),
            conversation: "c0123abc:1712345678.000100".to_owned(),
        }
    }
}

/// Builds a [`FakeBroker`]. Start with [`FakeBroker::builder`].
#[derive(Clone, Debug)]
pub struct FakeBrokerBuilder {
    component: Option<PathBuf>,
    provider: Option<String>,
    storage: Option<(StorageInterface, StorageAccess)>,
    storage_limits: StorageLimits,
    host_limits: BrokerHostLimits,
    host_options: BrokerHostOptions,
    continuity: ContinuityPolicy,
    scope: Scope,
    timeout_ms: Option<u64>,
    max_output_bytes: Option<u64>,
}

impl Default for FakeBrokerBuilder {
    fn default() -> Self {
        Self {
            component: None,
            provider: None,
            storage: None,
            storage_limits: StorageLimits::default(),
            host_limits: BrokerHostLimits::default(),
            host_options: BrokerHostOptions::default(),
            // Defaults to Stable, not the crate's own AuthorityBound default, because this harness
            // holds authority fixed, so Stable is the policy that keeps addressing one namespace if
            // that ever varies.
            continuity: ContinuityPolicy::Stable,
            scope: Scope::default(),
            // Left unset so build derives them from the host limits in force; a hardcoded default
            // here would duplicate a number this crate does not own and could drift out of sync.
            timeout_ms: None,
            max_output_bytes: None,
        }
    }
}

impl FakeBrokerBuilder {
    /// Sets the compiled component to load. Required.
    #[must_use]
    pub fn component(mut self, path: impl Into<PathBuf>) -> Self {
        self.component = Some(path.into());
        self
    }

    /// Sets the provider id the authorization binds to. Required, and must match the id in the
    /// component's own manifest or the host refuses the invocation.
    #[must_use]
    pub fn provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    /// Grants storage on one interface. Omit entirely for an import-free component.
    #[must_use]
    pub fn storage(mut self, interface: StorageInterface, access: StorageAccess) -> Self {
        self.storage = Some((interface, access));
        self
    }

    /// Overrides the storage limits. Defaults to [`StorageLimits::default()`], which is what a
    /// deployment runs — narrow these to test quota behavior, and prefer not to widen them.
    #[must_use]
    pub fn storage_limits(mut self, limits: StorageLimits) -> Self {
        self.storage_limits = limits;
        self
    }

    /// Overrides the Wasmtime host limits.
    #[must_use]
    pub fn host_limits(mut self, limits: BrokerHostLimits) -> Self {
        self.host_limits = limits;
        self
    }

    /// Uses immutable, boot-verified, mmap-backed compiled components from a trusted directory; do
    /// not modify mapped artifacts while a harness is alive, since errors fail loading with no
    /// fallback.
    #[must_use]
    pub fn compile_cache(mut self, directory: impl Into<PathBuf>) -> Self {
        self.host_options.cwasm_dir = Some(directory.into());
        self
    }

    /// Overrides the continuity policy. The default is [`ContinuityPolicy::Stable`].
    #[must_use]
    pub fn continuity(mut self, continuity: ContinuityPolicy) -> Self {
        self.continuity = continuity;
        self
    }

    /// Overrides the agent identity every proposal is attributed to.
    #[must_use]
    pub fn agent(mut self, agent: impl Into<String>) -> Self {
        self.scope.agent = agent.into();
        self
    }

    /// Overrides the external subject the storage namespace is scoped from.
    ///
    /// Two brokers built with different subjects address different namespaces, which is how a
    /// test proves isolation.
    #[must_use]
    pub fn subject(mut self, subject: impl Into<String>) -> Self {
        self.scope.subject = subject.into();
        self
    }

    /// Narrows the per-invocation wall-clock ceiling below the host's own max_timeout; raising it
    /// above that is refused, since an authorization may narrow a host ceiling but never widen it.
    #[must_use]
    pub const fn timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }

    /// Narrows the maximum serialized output one invocation may return.
    ///
    /// Defaults to the host's own `max_output_bytes`, and is bounded by it for the same reason.
    #[must_use]
    pub const fn max_output_bytes(mut self, max_output_bytes: u64) -> Self {
        self.max_output_bytes = Some(max_output_bytes);
        self
    }

    /// Creates the temporary root, opens the storage host, and compiles the component.
    ///
    /// # Errors
    ///
    /// Returns [`FakeBrokerError`] if required fields are unset, the component is missing or fails
    /// to compile, an identifier is invalid, or the storage root cannot be opened.
    pub async fn build(self) -> Result<FakeBroker, FakeBrokerError> {
        let component = self.component.ok_or(FakeBrokerError::NoComponent)?;
        if !component.exists() {
            return Err(FakeBrokerError::ComponentMissing { path: component });
        }
        let provider: ProviderId = self.provider.ok_or(FakeBrokerError::NoProvider)?.parse()?;

        let temporary = tempfile::tempdir()?;
        // Canonicalized because the storage host refuses a root reached through a symlinked
        // ancestor, and on macOS `/var` is a symlink.
        let root = temporary.path().canonicalize()?.join("storage");

        let storage = match self.storage {
            Some(_) => Some(StorageHost::open(&root, self.storage_limits)?),
            None => None,
        };
        let host_limits = self.host_limits;
        let registry = BrokerProviderRegistry::load_with_options(
            [component],
            host_limits.clone(),
            storage.clone(),
            &self.host_options,
        )
        .await?;

        Ok(FakeBroker {
            _temporary: temporary,
            root,
            registry,
            storage,
            storage_grant: self.storage,
            provider,
            agent: self.scope.agent.parse()?,
            subject: self.scope.subject.parse()?,
            transport_kind: self.scope.transport_kind,
            transport: self.scope.transport,
            channel: self.scope.channel,
            conversation: self.scope.conversation,
            continuity: self.continuity,
            timeout_ms: self.timeout_ms.unwrap_or_else(|| {
                host_limits
                    .max_timeout
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX)
            }),
            max_output_bytes: self
                .max_output_bytes
                .unwrap_or(host_limits.max_output_bytes as u64),
            invocations: AtomicU64::new(0),
        })
    }
}

/// One FakeBroker is one durable namespace with a real storage host behind it; invocations see each
/// other's completed per-call writes even after a failure, since there is no invocation-wide
/// rollback.
#[derive(Debug)]
pub struct FakeBroker {
    /// Kept only for its Drop impl, which deletes the temporary storage root; the field itself is
    /// never read.
    _temporary: TempDir,
    root: PathBuf,
    registry: BrokerProviderRegistry,
    storage: Option<StorageHost>,
    storage_grant: Option<(StorageInterface, StorageAccess)>,
    provider: ProviderId,
    agent: AgentId,
    subject: ExternalSubject,
    transport_kind: String,
    transport: String,
    channel: String,
    conversation: String,
    continuity: ContinuityPolicy,
    timeout_ms: u64,
    max_output_bytes: u64,
    invocations: AtomicU64,
}

impl FakeBroker {
    /// Starts building a fake broker.
    #[must_use]
    pub fn builder() -> FakeBrokerBuilder {
        FakeBrokerBuilder::default()
    }

    /// Invokes one capability and returns the provider's JSON output.
    ///
    /// # Errors
    ///
    /// Returns [`FakeBrokerError`] if the capability id is invalid, the grant cannot be minted, or
    /// the invocation fails. Use [`FakeBrokerError::provider_failure`] to distinguish a failure
    /// the provider itself declared from one the host imposed.
    pub async fn invoke(&self, capability: &str, input: Value) -> Result<Value, FakeBrokerError> {
        Ok(self.invoke_full(capability, input).await?.output)
    }

    /// Invokes one capability and returns the full output, including storage evidence.
    ///
    /// # Errors
    ///
    /// As [`FakeBroker::invoke`].
    pub async fn invoke_full(
        &self,
        capability: &str,
        input: Value,
    ) -> Result<BrokerInvocationOutput, FakeBrokerError> {
        let capability: CapabilityId = capability.parse()?;
        // Each invocation needs a fresh id since grants are minted and consumed per call; the scope
        // material around it stays fixed, which is what keeps successive calls in one namespace.
        let sequence = self
            .invocations
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let invocation: InvocationId = format!("testkit-invoke-{sequence}").parse()?;

        let grant = match (&self.storage, self.storage_grant) {
            (Some(storage), Some((interface, access))) => {
                Some(storage.grant(StorageGrantRequest::new(
                    invocation.clone(),
                    capability.clone(),
                    self.provider.clone(),
                    interface,
                    access,
                    StorageNamespace::Chat,
                    self.agent.clone(),
                    self.subject.clone(),
                    self.transport_kind.clone(),
                    self.transport.clone(),
                    self.channel.clone(),
                    self.conversation.clone(),
                    self.continuity,
                    b"testkit-authority".to_vec(),
                ))?)
            }
            _ => None,
        };

        let proposal = ProposedInvocation::new(
            invocation,
            capability,
            Actor::Agent {
                agent: self.agent.clone(),
            },
            // Fixed sixteen-ASCII-byte trace ID so a failure dump reads as the fixture it is rather
            // than as a real run someone has to hunt down.
            TraceId::new(*b"dekopon-testkit!").expect("the fixture bytes are not all zeroes"),
            input,
        );
        let authorized = AuthorizationGate::new().authorize(
            proposal,
            self.provider.clone(),
            "testkit-decision".to_owned(),
            "testkit-broker".parse::<PrincipalId>()?,
            "testkit-policy".to_owned(),
            self.constraints(),
        )?;

        self.registry
            .invoke_with_storage(authorized, None, grant, Default::default())
            .await
            .map_err(|failure| FakeBrokerError::Invocation(Box::new(failure)))
    }

    /// Runs one command word as the sandboxed shell would, returning what the guest declared;
    /// nothing is authorized here, so run the resulting proposal through invoke to execute it.
    pub async fn run_command(
        &self,
        word: &str,
        argv: &[String],
        stdin: Option<&str>,
    ) -> Result<CommandRunOutcome, FakeBrokerError> {
        Ok(self.registry.run_command(word, argv, stdin).await?)
    }

    /// Returns the storage root on disk; StorageEvidence counts bytes moved, not final file sizes,
    /// and every path component is an opaque SHA-256 token, so walk the tree rather than guessing
    /// names.
    #[must_use]
    pub fn storage_root(&self) -> &Path {
        &self.root
    }

    /// Returns the loaded registry, for assertions the harness does not wrap.
    #[must_use]
    pub const fn registry(&self) -> &BrokerProviderRegistry {
        &self.registry
    }

    fn constraints(&self) -> ExecutionConstraints {
        ExecutionConstraints {
            asset: None,
            timeout_ms: self.timeout_ms,
            max_output_bytes: self.max_output_bytes,
            http: None,
            storage: self
                .storage_grant
                .map(|(interface, access)| StorageConstraints {
                    interface,
                    access,
                    namespace: StorageNamespace::Chat,
                }),
            secret_use: None,
        }
    }
}
