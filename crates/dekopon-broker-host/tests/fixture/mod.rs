#![allow(
    clippy::unwrap_used,
    clippy::disallowed_methods,
    clippy::disallowed_types,
    dead_code
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
    StorageConstraints, StorageInterface, StorageScope,
};
pub use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, IdentifierError, InvocationId, PrincipalId,
    ProviderId, SubjectError, TraceId,
};
pub use dekopon_storage_host::{ContinuityPolicy, StorageHost, StorageHostError, StorageLimits};

pub struct Stdout(std::thread::JoinHandle<Vec<u8>>);

impl Stdout {
    #[must_use]
    pub fn bytes(self) -> Vec<u8> {
        self.0.join().unwrap()
    }

    #[must_use]
    pub fn json(self) -> Value {
        serde_json::from_slice(&self.bytes()).unwrap()
    }
}

#[must_use]
pub fn piped_stdout() -> (dekopon_broker_host::asset::AssetInputs, Stdout) {
    let (host, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let capture = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut peer, &mut bytes).unwrap();
        bytes
    });
    let assets = dekopon_broker_host::asset::AssetInputs {
        streams: Some(dekopon_broker_host::Streams {
            stdin: None,
            stdout: host.into(),
        }),
        ..Default::default()
    };
    (assets, Stdout(capture))
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FixtureHostError {
    #[error("no component path was configured")]
    NoComponent,
    #[error("no provider id was configured")]
    NoProvider,
    #[error("provider component {} does not exist; build it first", path.display())]
    ComponentMissing { path: PathBuf },
    #[error("invalid identifier: {0}")]
    Identifier(#[from] IdentifierError),
    #[error("invalid external subject: {0}")]
    Subject(#[from] SubjectError),
    #[error("preparing the temporary storage root failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Storage(#[from] StorageHostError),
    #[error(transparent)]
    Host(#[from] BrokerHostError),
    #[error(transparent)]
    Authorization(#[from] AuthorizationError),
    #[error("invocation failed: {0}")]
    Invocation(#[source] Box<BrokerInvocationFailure>),
}

impl FixtureHostError {
    #[must_use]
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
    pub fn provider_failure(&self) -> Option<(u8, &str)> {
        let Self::Invocation(failure) = self else {
            return None;
        };
        match failure.error.as_ref() {
            BrokerHostError::ProviderFailure { status, stderr, .. } => Some((*status, stderr)),
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

#[derive(Clone, Debug)]
pub struct FixtureHostBuilder {
    component: Option<PathBuf>,
    provider: Option<String>,
    storage: Option<(StorageInterface, StorageAccess)>,
    storage_limits: StorageLimits,
    host_limits: BrokerHostLimits,
    host_options: BrokerHostOptions,
    continuity: ContinuityPolicy,
    scope: Scope,
    timeout_ms: Option<u64>,
}

impl Default for FixtureHostBuilder {
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
        }
    }
}

impl FixtureHostBuilder {
    #[must_use]
    pub fn component(mut self, path: impl Into<PathBuf>) -> Self {
        self.component = Some(path.into());
        self
    }

    #[must_use]
    pub fn provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

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

    #[must_use]
    pub fn host_limits(mut self, limits: BrokerHostLimits) -> Self {
        self.host_limits = limits;
        self
    }

    /// Do not modify mapped artifacts while the harness is alive; loading fails without fallback.
    #[must_use]
    pub fn compile_cache(mut self, directory: impl Into<PathBuf>) -> Self {
        self.host_options.cwasm_dir = Some(directory.into());
        self
    }

    #[must_use]
    pub fn continuity(mut self, continuity: ContinuityPolicy) -> Self {
        self.continuity = continuity;
        self
    }

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

    pub async fn build(self) -> Result<FixtureHost, FixtureHostError> {
        let component = self.component.ok_or(FixtureHostError::NoComponent)?;
        if !component.exists() {
            return Err(FixtureHostError::ComponentMissing { path: component });
        }
        let provider: ProviderId = self.provider.ok_or(FixtureHostError::NoProvider)?.parse()?;

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

        Ok(FixtureHost {
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
            invocations: AtomicU64::new(0),
        })
    }
}

/// One FixtureHost is one durable namespace with a real storage host behind it; invocations see each
/// other's completed per-call writes even after a failure, since there is no invocation-wide
/// rollback.
#[derive(Debug)]
pub struct FixtureHost {
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
    invocations: AtomicU64,
}

impl FixtureHost {
    #[must_use]
    pub fn builder() -> FixtureHostBuilder {
        FixtureHostBuilder::default()
    }

    pub async fn invoke(&self, capability: &str, input: Value) -> Result<Value, FixtureHostError> {
        Ok(self.invoke_full(capability, input).await?.1)
    }

    pub async fn invoke_full(
        &self,
        capability: &str,
        input: Value,
    ) -> Result<(BrokerInvocationOutput, Value), FixtureHostError> {
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
                    StorageScope::PrivateConversation,
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

        let (assets, stdout) = piped_stdout();
        let output = self
            .registry
            .invoke_with_storage(authorized, None, grant, assets)
            .await
            .map_err(|failure| FixtureHostError::Invocation(Box::new(failure)))?;
        Ok((output, stdout.json()))
    }

    /// A command proposal has no authority; pass it through invoke to execute it.
    pub async fn run_command(
        &self,
        word: &str,
        argv: &[String],
        stdin_piped: bool,
    ) -> Result<CommandRunOutcome, FixtureHostError> {
        Ok(self.registry.run_command(word, argv, stdin_piped).await?)
    }

    /// StorageEvidence counts bytes moved, not final file sizes; walk the opaque SHA-256 paths.
    #[must_use]
    pub fn storage_root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub const fn registry(&self) -> &BrokerProviderRegistry {
        &self.registry
    }

    fn constraints(&self) -> ExecutionConstraints {
        ExecutionConstraints {
            asset: None,
            timeout_ms: self.timeout_ms,
            http: None,
            storage: self
                .storage_grant
                .map(|(interface, access)| StorageConstraints {
                    interface,
                    access,
                    scope: StorageScope::PrivateConversation,
                    retention: Default::default(),
                }),
            secret_use: None,
        }
    }
}
