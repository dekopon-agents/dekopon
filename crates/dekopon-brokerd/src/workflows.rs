use std::{
    collections::BTreeMap,
    fmt,
    num::NonZeroU32,
    path::{Path, PathBuf},
};

use dekopon_broker::ConstraintSet;
use dekopon_broker_host::BrokerHostError;
use dekopon_broker_protocol::{
    Attestation, ChatScopeClaim, ChatTransportKind, CommandRunOutcome, Conversation,
    ConversationKind, Trigger,
};
use dekopon_core::{
    AgentId, CapabilityId, ExternalSubject, FileHygieneError, PrincipalId, ProviderId, TransportId,
    error_chain,
    fragments::{self, FragmentError},
};
use semver::Version;
use serde::Deserialize;
use thiserror::Error;

use crate::{Ready, config, provider_manager, server::MappedPeer};

const SLACK_THREAD: &str = "1700000000.000100";

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct EssentialWorkflows {
    #[serde(rename = "apiVersion")]
    _api_version: ApiVersion,
    #[serde(rename = "kind")]
    _kind: Kind,
    agent: AgentId,
    subject: ExternalSubject,
    via: PrincipalId,
    scope: Scope,
    workflows: BTreeMap<String, Vec<Step>>,
}

#[derive(Deserialize)]
enum ApiVersion {
    #[serde(rename = "dekopon.dev/workflow/v1alpha1")]
    V1Alpha1,
}

#[derive(Deserialize)]
enum Kind {
    EssentialWorkflows,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Scope {
    transport: TransportId,
    transport_kind: ChatTransportKind,
    conversation: ConversationKind,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    step: String,
    argv: Argv,
    capability: CapabilityId,
    #[serde(default)]
    requests: Option<NonZeroU32>,
}

#[derive(Deserialize)]
#[serde(try_from = "Vec<String>")]
struct Argv {
    word: String,
    arguments: Vec<String>,
}

impl TryFrom<Vec<String>> for Argv {
    type Error = &'static str;

    fn try_from(mut argv: Vec<String>) -> Result<Self, Self::Error> {
        if argv.is_empty() {
            return Err("argv names no command word");
        }
        let word = argv.remove(0);
        Ok(Self {
            word,
            arguments: argv,
        })
    }
}

#[derive(Debug, Error)]
pub enum WorkflowFileError {
    #[error("could not read the essential workflows directory")]
    Directory(#[source] FragmentError),
    #[error("could not read an essential workflows file")]
    Read(#[source] FileHygieneError),
    #[error("essential workflows directory {path} holds no *.yaml file")]
    Empty { path: PathBuf },
    #[error("essential workflows file {path} does not decode")]
    Decode {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("essential workflows file {path} has an empty workflow map or step list")]
    Incomplete { path: PathBuf },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Check {
    Parse,
    Grant,
    Budget,
}

impl fmt::Display for Check {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Parse => "parse",
            Self::Grant => "grant",
            Self::Budget => "budget",
        })
    }
}

#[derive(Debug, Error)]
#[error("{workflow}: {step}: {}: {refused}", refused.check())]
pub struct StepRefusal {
    pub workflow: String,
    pub step: String,
    pub refused: Refused,
}

#[derive(Debug)]
pub enum Refused {
    Parse(Box<ParseRefusal>),
    Grant(GrantRefusal),
    Budget { declared: NonZeroU32, maximum: u32 },
}

#[derive(Debug)]
pub struct ParseRefusal {
    pub word: String,
    pub failure: ParseFailure,
    pub provider: Option<ProviderIdentity>,
}

#[derive(Debug)]
pub enum ParseFailure {
    Host(BrokerHostError),
    Rendered { status: u8, usage: String },
    Failed { code: String },
    Proposed { capability: CapabilityId },
}

#[derive(Debug)]
pub enum GrantRefusal {
    UnknownVia {
        via: PrincipalId,
    },
    AmbiguousVia {
        via: PrincipalId,
        identities: usize,
    },
    Refused {
        reason: &'static str,
        policy_ids: Vec<String>,
    },
}

#[derive(Debug)]
pub struct ProviderIdentity {
    id: ProviderId,
    version: Option<Version>,
    sha256: String,
}

impl Refused {
    #[must_use]
    pub const fn check(&self) -> Check {
        match self {
            Self::Parse(_) => Check::Parse,
            Self::Grant(_) => Check::Grant,
            Self::Budget { .. } => Check::Budget,
        }
    }
}

impl fmt::Display for Refused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(parse) => {
                let ParseRefusal {
                    word,
                    failure,
                    provider,
                } = &**parse;
                formatter.write_str(word)?;
                if let Some(provider) = provider {
                    write!(formatter, " ({provider})")?;
                }
                match failure {
                    ParseFailure::Host(error) => write!(formatter, ": {}", error_chain(error)),
                    ParseFailure::Rendered { status, usage } => {
                        write!(formatter, " exited {status}: {}", usage.trim_end())
                    }
                    ParseFailure::Failed { code } => write!(formatter, " failed: {code}"),
                    ParseFailure::Proposed { capability } => {
                        write!(formatter, " proposed {capability}")
                    }
                }
            }
            Self::Grant(GrantRefusal::UnknownVia { via }) => {
                write!(formatter, "no candidate identity has principal {via}")
            }
            Self::Grant(GrantRefusal::AmbiguousVia { via, identities }) => {
                write!(
                    formatter,
                    "{identities} candidate identities have principal {via}"
                )
            }
            Self::Grant(GrantRefusal::Refused { reason, policy_ids }) if policy_ids.is_empty() => {
                formatter.write_str(reason)
            }
            Self::Grant(GrantRefusal::Refused { reason, policy_ids }) => {
                write!(formatter, "{reason} by {}", policy_ids.join(", "))
            }
            Self::Budget { declared, maximum } => write!(
                formatter,
                "{declared} requests against constraints.http.maxRequests {maximum}"
            ),
        }
    }
}

impl fmt::Display for ProviderIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.version {
            Some(version) => write!(formatter, "provider {} {version}", self.id),
            None => write!(formatter, "provider {} unversioned", self.id),
        }?;
        write!(formatter, " sha256:{}", self.sha256)
    }
}

pub(crate) async fn load(
    directory: &Path,
    uid: u32,
) -> Result<Vec<EssentialWorkflows>, WorkflowFileError> {
    let paths =
        fragments::scan_directory(directory, uid, "yaml").map_err(WorkflowFileError::Directory)?;
    if paths.is_empty() {
        return Err(WorkflowFileError::Empty {
            path: directory.to_path_buf(),
        });
    }
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let bytes = config::read_not_world_writable(&path, uid, config::HARD_MAX_CONFIG_BYTES)
            .await
            .map_err(WorkflowFileError::Read)?;
        // Through a Value, whose mappings refuse a duplicate key; a typed map keeps the last.
        let file = serde_yaml::from_slice::<serde_yaml::Value>(&bytes)
            .and_then(serde_yaml::from_value::<EssentialWorkflows>)
            .map_err(|source| WorkflowFileError::Decode {
                path: path.clone(),
                source,
            })?;
        if file.workflows.is_empty() || file.workflows.values().any(Vec::is_empty) {
            return Err(WorkflowFileError::Incomplete { path });
        }
        files.push(file);
    }
    Ok(files)
}

pub(crate) async fn versions(lock: Option<&Path>, uid: u32) -> BTreeMap<ProviderId, Version> {
    let Some(lock) = lock else {
        return BTreeMap::new();
    };
    match provider_manager::load_lock(lock, uid).await {
        Ok(lock) => lock
            .providers
            .into_iter()
            .filter_map(|provider| {
                provider
                    .resolved_version
                    .map(|version| (provider.provider_id, version))
            })
            .collect(),
        Err(error) => {
            tracing::warn!(
                error = %error_chain(&error),
                "workflow refusals name every provider unversioned: the provider lock is unreadable"
            );
            BTreeMap::new()
        }
    }
}

pub(crate) async fn run(
    ready: &Ready,
    files: &[EssentialWorkflows],
    versions: &BTreeMap<ProviderId, Version>,
) -> Vec<StepRefusal> {
    let mut refusals = Vec::new();
    for file in files {
        let claim = Attestation {
            subject: file.subject.clone(),
            agent: file.agent.clone(),
            scope: Some(ChatScopeClaim {
                participant: None,
                transport: file.scope.transport.clone(),
                kind: file.scope.transport_kind,
                conversation: conversation(
                    file.scope.transport_kind,
                    file.scope.conversation,
                    &file.subject,
                ),
                delivery: None,
                trigger: Trigger::Message,
            }),
            invocation: None,
        };
        for (workflow, steps) in &file.workflows {
            for step in steps {
                if let Err(refused) = run_step(ready, &claim, step, versions, &file.via).await {
                    refusals.push(StepRefusal {
                        workflow: workflow.clone(),
                        step: step.step.clone(),
                        refused,
                    });
                }
            }
        }
    }
    refusals
}

async fn run_step(
    ready: &Ready,
    claim: &Attestation,
    step: &Step,
    versions: &BTreeMap<ProviderId, Version>,
    via: &PrincipalId,
) -> Result<(), Refused> {
    let peer = attesting_peer(ready, via).map_err(Refused::Grant)?;
    let broker = &ready.broker;
    let (context, refusal) = broker.resolve_context(&peer.context, peer.attestor.as_ref(), claim);
    if let Some(refusal) = refusal {
        return Err(grant(refusal));
    }
    let Argv { word, arguments } = &step.argv;
    let parse = |failure| {
        Refused::Parse(Box::new(ParseRefusal {
            word: word.clone(),
            failure,
            provider: provider(ready, word, versions),
        }))
    };
    let proposed = match broker
        .run_command(
            &peer.context,
            peer.attestor.as_ref(),
            Some(claim),
            word,
            arguments,
            false,
        )
        .await
    {
        Ok(CommandRunOutcome::Proposed { capability, .. }) if capability == step.capability => {
            Ok(())
        }
        Ok(CommandRunOutcome::Proposed { capability, .. }) => {
            Err(ParseFailure::Proposed { capability })
        }
        Ok(CommandRunOutcome::Rendered {
            stdout,
            stderr,
            status,
        }) => Err(ParseFailure::Rendered {
            status,
            usage: if stderr.is_empty() { stdout } else { stderr },
        }),
        Ok(CommandRunOutcome::Failed { error }) => Err(ParseFailure::Failed { code: error.code }),
        Err(error) => Err(ParseFailure::Host(error)),
    };
    proposed.map_err(parse)?;
    let decision = broker
        .authorize_invocation(&context, &step.capability)
        .map_err(grant)?;
    match step.requests {
        Some(declared) => {
            let maximum = max_requests(&decision.set);
            if maximum < declared.get() {
                return Err(Refused::Budget { declared, maximum });
            }
            Ok(())
        }
        None => Ok(()),
    }
}

fn attesting_peer<'ready>(
    ready: &'ready Ready,
    via: &PrincipalId,
) -> Result<&'ready MappedPeer, GrantRefusal> {
    let mut peers = ready
        .identities
        .values()
        .filter(|peer| peer.context.principal() == via);
    match (peers.next(), peers.count()) {
        (Some(peer), 0) => Ok(peer),
        (Some(_), others) => Err(GrantRefusal::AmbiguousVia {
            via: via.clone(),
            identities: others + 1,
        }),
        (None, _) => Err(GrantRefusal::UnknownVia { via: via.clone() }),
    }
}

fn grant(refusal: dekopon_broker::Refusal) -> Refused {
    Refused::Grant(GrantRefusal::Refused {
        reason: refusal.reason,
        policy_ids: refusal.policy_ids,
    })
}

fn max_requests(set: &ConstraintSet) -> u32 {
    set.constraints
        .http
        .as_ref()
        .map_or(0, |http| http.max_requests)
}

fn provider(
    ready: &Ready,
    word: &str,
    versions: &BTreeMap<ProviderId, Version>,
) -> Option<ProviderIdentity> {
    ready
        .registry
        .loaded_provider_metadata()
        .find(|loaded| {
            loaded
                .manifest
                .command_words
                .iter()
                .any(|known| known == word)
        })
        .map(|loaded| ProviderIdentity {
            version: versions.get(&loaded.manifest.id).cloned(),
            id: loaded.manifest.id,
            sha256: loaded.artifact_sha256,
        })
}

// Canonical for the kind and subject; a Slack DM carries the thread an Agent DM has, so a template
// that needs it is a miss on an unthreaded DM rather than a false red on an Agent DM.
fn conversation(
    transport: ChatTransportKind,
    kind: ConversationKind,
    subject: &ExternalSubject,
) -> Conversation {
    let part = |value: &str| Some(value.to_owned());
    let team = || part(subject.tenant().unwrap_or("t0"));
    let (container, id, thread) = match (transport, kind) {
        (ChatTransportKind::Slack, ConversationKind::DirectMessage) => {
            (team(), "d0".to_owned(), part(SLACK_THREAD))
        }
        (ChatTransportKind::Slack, ConversationKind::GroupDirectMessage) => {
            (team(), "g0".to_owned(), part(SLACK_THREAD))
        }
        (ChatTransportKind::Slack, ConversationKind::Channel | ConversationKind::Thread) => {
            (team(), "c0".to_owned(), part(SLACK_THREAD))
        }
        (
            ChatTransportKind::Discord,
            ConversationKind::DirectMessage | ConversationKind::GroupDirectMessage,
        ) => (None, "1".to_owned(), None),
        (ChatTransportKind::Discord, ConversationKind::Channel) => {
            (part("1"), "2".to_owned(), None)
        }
        (ChatTransportKind::Discord, ConversationKind::Thread) => {
            (part("1"), "2".to_owned(), part("3"))
        }
        (
            ChatTransportKind::Telegram,
            ConversationKind::DirectMessage | ConversationKind::GroupDirectMessage,
        ) => (None, subject.subject().to_owned(), None),
        (ChatTransportKind::Telegram, ConversationKind::Channel) => (None, "-1".to_owned(), None),
        (ChatTransportKind::Telegram, ConversationKind::Thread) => {
            (None, "-1".to_owned(), part("1"))
        }
        (
            ChatTransportKind::Whatsapp,
            ConversationKind::DirectMessage
            | ConversationKind::GroupDirectMessage
            | ConversationKind::Channel
            | ConversationKind::Thread,
        ) => (part("1:1"), subject.subject().to_owned(), None),
        (
            ChatTransportKind::Github,
            ConversationKind::DirectMessage
            | ConversationKind::GroupDirectMessage
            | ConversationKind::Channel
            | ConversationKind::Thread,
        ) => (
            subject.tenant().map(str::to_owned),
            format!(
                "{}.{}",
                subject.tenant().unwrap_or_default(),
                subject.subject()
            ),
            part("1"),
        ),
        (
            ChatTransportKind::Local,
            ConversationKind::DirectMessage
            | ConversationKind::GroupDirectMessage
            | ConversationKind::Channel
            | ConversationKind::Thread,
        ) => (None, "workflow-check".to_owned(), None),
    };
    Conversation {
        kind,
        container,
        id,
        thread,
    }
}
