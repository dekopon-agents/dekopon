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
    BrokerInvocationOutput, BrokerProviderRegistry,
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

/// Re-exported so a test never mixes two semver-incompatible `dekopon-*` versions.
///
/// A provider repository pins its guest SDK exactly (`=0.10.0`, say) while taking this testkit as a
/// development dependency at a different version. Both then exist in one dependency graph, and
/// `CapabilityId` from one is not `CapabilityId` from the other. Importing the types above from
/// here rather than from the guest SDK keeps a test on one side of that line.
pub mod prelude {
    pub use super::{
        BrokerInvocationOutput, CapabilityId, FakeBroker, FakeBrokerError, ProviderId,
        StorageAccess, StorageEvidence, StorageInterface,
    };
}

/// Namespace key used for every temporary root this crate creates.
///
/// Fixed rather than random so a failing test reproduces byte for byte. It authenticates paths
/// inside a `TempDir` that is deleted when the test ends, and protects nothing else.
const TEST_NAMESPACE_KEY: &str = concat!(
    "apiVersion: dekopon.dev/storage-key/v1alpha1\n",
    "key: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
);

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
    /// The component path does not exist.
    ///
    /// Its own variant because it is overwhelmingly the first failure a new provider test hits:
    /// the component is a build artifact, usually `.gitignore`d, and absent until `build.sh` runs.
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
    /// Creating the temporary root, or writing the namespace key, failed.
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

/// Chat-shaped scope material every storage grant is derived from.
///
/// [`StorageNamespace::Chat`] is the only namespace the storage host will grant — every other
/// value is refused with `PermissionDenied` — so a provider with nothing to do with chat still
/// needs a transport, channel, and conversation. These defaults are that ceremony, pre-filled.
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
            // `ContinuityPolicy`'s own default is `AuthorityBound`, which mints a fresh
            // non-reusing generation whenever the effective authority commitment changes. This
            // harness holds the authority surface fixed, so today the two policies address the
            // same namespace and nothing observable separates them. `Stable` is the default
            // anyway: it is the policy that survives an authority change, so a harness that grows
            // one later keeps addressing one namespace instead of silently starting over.
            continuity: ContinuityPolicy::Stable,
            scope: Scope::default(),
            // Left unset so `build` can derive them from the host limits in force. An
            // authorization may narrow a host ceiling but never widen one, so a hardcoded default
            // here would be a second definition of a number this crate does not own — and one
            // that fails every invocation the moment the two disagree.
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

    /// Points Wasmtime at a persistent compilation cache.
    ///
    /// Cranelift is the whole of a cold start. On a large component this is the difference
    /// between a test suite that runs and one nobody waits for.
    #[must_use]
    pub fn compile_cache(mut self, directory: impl Into<PathBuf>) -> Self {
        self.host_options.compile_cache_dir = Some(directory.into());
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

    /// Narrows the per-invocation wall-clock ceiling.
    ///
    /// Defaults to the host's own `max_timeout`. Raising it above that is refused at invocation
    /// time, because an authorization may narrow a host ceiling but never widen one.
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
        // Canonicalized because the storage host compares the key path against the root to refuse
        // a key stored inside the tree it authenticates, and on macOS `/var` is a symlink.
        let directory = temporary.path().canonicalize()?;
        let root = directory.join("storage");
        let key = directory.join("storage-key.yaml");
        std::fs::write(&key, TEST_NAMESPACE_KEY)?;
        set_key_permissions(&key)?;

        let storage = match self.storage {
            Some(_) => Some(StorageHost::open(&root, &key, self.storage_limits)?),
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

/// A loaded provider component with a real storage host behind it.
///
/// One `FakeBroker` is one durable namespace. Invocations made against it see each other's
/// committed writes, which is the property most storage provider tests actually need to assert.
#[derive(Debug)]
pub struct FakeBroker {
    /// Owned so the storage root outlives every invocation; dropping this deletes the tree.
    ///
    /// Never read — the value exists for its `Drop`, which is what the leading underscore says.
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
        // Grants are minted per invocation and consumed by it, so every call needs a fresh
        // invocation id. The scope material around it stays fixed, which is what keeps successive
        // calls addressing one namespace.
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
            "testkit-trace".parse::<TraceId>()?,
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
            .invoke_with_storage(authorized, None, grant)
            .await
            .map_err(|failure| FakeBrokerError::Invocation(Box::new(failure)))
    }

    /// Returns the storage root on disk.
    ///
    /// [`StorageEvidence`] reports byte counts only as coarse powers-of-two buckets, so a test
    /// asserting an exact size — that a write-ahead log was truncated to zero, say — has to look
    /// at the tree. Note every path component is an HMAC token, so walk it rather than guessing
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

#[cfg(unix)]
fn set_key_permissions(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_key_permissions(_path: &Path) -> Result<(), std::io::Error> {
    // The storage host requires an owner-only key file and refuses to open a root without one, so
    // this crate is effectively Unix-only. Failing at `StorageHost::open` with its own diagnostic
    // is clearer than inventing one here.
    Ok(())
}
