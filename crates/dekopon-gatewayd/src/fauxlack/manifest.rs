use std::collections::{BTreeMap, BTreeSet};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use dekopon_broker_protocol::{ChatTransportKind, Conversation, ConversationKind};
use dekopon_core::{Subject, SubjectError, SubjectKind};
use serde::Deserialize;
use thiserror::Error;

use super::world::{Container, Facts, Fauxlack, File, Person};

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("invalid fauxlack manifest: {0}")]
    Decode(#[source] serde_yaml::Error),
    #[error("invalid fauxlack manifest: {problems:?}")]
    Invalid { problems: Vec<ManifestProblem> },
}

#[derive(Debug, Error)]
pub enum ManifestProblem {
    #[error("{server}/{id}: {source}")]
    Subject {
        server: String,
        id: String,
        #[source]
        source: SubjectError,
    },
    #[error("{server}/{id}: noncanonical conversation")]
    Conversation { server: String, id: String },
    #[error("duplicate server {id}")]
    DuplicateServer { id: String },
    #[error("{server}: duplicate object {id}")]
    DuplicateId { server: String, id: String },
    #[error("{server}: duplicate file {name}")]
    DuplicateFile { server: String, name: String },
    #[error("{server}/{container}: unknown member {user}")]
    UnknownMember {
        server: String,
        container: String,
        user: String,
    },
    #[error("{server}/{container}: repeated DM member {user}")]
    RepeatedMember {
        server: String,
        container: String,
        user: String,
    },
    #[error("{server}/{id}: DM needs at least two members")]
    DirectMessageSize { server: String, id: String },
    #[error("{server}/{name}: file needs exactly one of canvas, snippet or image")]
    FileContent { server: String, name: String },
    #[error("{server}/{name}: {source}")]
    Base64 {
        server: String,
        name: String,
        #[source]
        source: base64::DecodeError,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    servers: Vec<Server>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Server {
    id: String,
    #[serde(default)]
    users: Vec<User>,
    #[serde(default)]
    channels: Vec<Channel>,
    #[serde(default)]
    direct_messages: Vec<DirectMessage>,
    #[serde(default)]
    files: Vec<Attachment>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct User {
    id: String,
    name: String,
    #[serde(default)]
    bot: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Channel {
    id: String,
    name: String,
    members: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectMessage {
    id: String,
    members: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Attachment {
    name: String,
    #[serde(default)]
    canvas: Option<String>,
    #[serde(default)]
    snippet: Option<String>,
    #[serde(default)]
    image: Option<Image>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Image {
    mime: String,
    base64: String,
}

fn subject(
    server: &str,
    id: &str,
    kind: SubjectKind,
    problems: &mut Vec<ManifestProblem>,
) -> Option<Subject> {
    Subject::new(kind, server, id)
        .map_err(|source| {
            problems.push(ManifestProblem::Subject {
                server: server.into(),
                id: id.into(),
                source,
            });
        })
        .ok()
}

fn container(
    server: &Server,
    id: &str,
    members: &[String],
    kind: ConversationKind,
    problems: &mut Vec<ManifestProblem>,
) {
    let sender = subject(&server.id, "u0", SubjectKind::FauxlackUser, problems);
    let conversation = Conversation {
        kind,
        container: Some(server.id.clone()),
        id: id.into(),
        thread: match kind {
            ConversationKind::Channel | ConversationKind::Thread => Some("m1".into()),
            ConversationKind::DirectMessage | ConversationKind::GroupDirectMessage => None,
        },
    };
    if let Some(sender) = sender
        && !conversation.is_canonical_for(ChatTransportKind::Fauxlack, &sender)
    {
        problems.push(ManifestProblem::Conversation {
            server: server.id.clone(),
            id: id.into(),
        });
    }
    for member in members {
        if !server.users.iter().any(|user| user.id == *member) {
            problems.push(ManifestProblem::UnknownMember {
                server: server.id.clone(),
                container: id.into(),
                user: member.clone(),
            });
        }
    }
}

impl Fauxlack {
    pub fn from_manifest(text: &str) -> Result<Self, ManifestError> {
        let manifest: Manifest = serde_yaml::from_str(text).map_err(ManifestError::Decode)?;
        let mut problems = Vec::new();
        let mut servers = BTreeMap::new();
        for server in manifest.servers {
            subject(&server.id, "u0", SubjectKind::FauxlackUser, &mut problems);
            if servers.contains_key(&server.id) {
                problems.push(ManifestProblem::DuplicateServer {
                    id: server.id.clone(),
                });
            }
            let mut ids = BTreeSet::new();
            for id in server
                .users
                .iter()
                .map(|u| &u.id)
                .chain(server.channels.iter().map(|c| &c.id))
                .chain(server.direct_messages.iter().map(|d| &d.id))
            {
                if !ids.insert(id) {
                    problems.push(ManifestProblem::DuplicateId {
                        server: server.id.clone(),
                        id: id.clone(),
                    });
                }
            }
            let mut users = BTreeMap::new();
            for user in &server.users {
                if let Some(subject) = subject(
                    &server.id,
                    &user.id,
                    SubjectKind::FauxlackUser,
                    &mut problems,
                ) {
                    users.insert(
                        user.id.clone(),
                        Person {
                            name: user.name.clone(),
                            bot: user.bot,
                            subject,
                        },
                    );
                }
            }
            let mut containers = BTreeMap::new();
            for channel in &server.channels {
                subject(
                    &server.id,
                    &channel.id,
                    SubjectKind::FauxlackChannel,
                    &mut problems,
                );
                container(
                    &server,
                    &channel.id,
                    &channel.members,
                    ConversationKind::Channel,
                    &mut problems,
                );
                containers.insert(
                    channel.id.clone(),
                    Container {
                        name: Some(channel.name.clone()),
                        kind: ConversationKind::Channel,
                        members: channel.members.clone(),
                    },
                );
            }
            for dm in &server.direct_messages {
                if dm.members.len() < 2 {
                    problems.push(ManifestProblem::DirectMessageSize {
                        server: server.id.clone(),
                        id: dm.id.clone(),
                    });
                }
                let mut seen = BTreeSet::new();
                for member in &dm.members {
                    if !seen.insert(member) {
                        problems.push(ManifestProblem::RepeatedMember {
                            server: server.id.clone(),
                            container: dm.id.clone(),
                            user: member.clone(),
                        });
                    }
                }
                let kind = if dm.members.len() > 2 {
                    ConversationKind::GroupDirectMessage
                } else {
                    ConversationKind::DirectMessage
                };
                container(&server, &dm.id, &dm.members, kind, &mut problems);
                containers.insert(
                    dm.id.clone(),
                    Container {
                        name: None,
                        kind,
                        members: dm.members.clone(),
                    },
                );
            }
            let files = files(&server.id, server.files, &mut problems);
            servers.insert(
                server.id,
                Facts {
                    users,
                    containers,
                    files,
                },
            );
        }
        if problems.is_empty() {
            Ok(Self::new(servers))
        } else {
            Err(ManifestError::Invalid { problems })
        }
    }
}

fn files(
    server: &str,
    attachments: Vec<Attachment>,
    problems: &mut Vec<ManifestProblem>,
) -> BTreeMap<String, File> {
    let mut files = BTreeMap::new();
    let mut file_names = BTreeSet::new();
    for attachment in attachments {
        let id = format!("f{}", files.len() + 1);
        if !file_names.insert(attachment.name.clone()) {
            problems.push(ManifestProblem::DuplicateFile {
                server: server.into(),
                name: attachment.name.clone(),
            });
        }
        let count = usize::from(attachment.canvas.is_some())
            + usize::from(attachment.snippet.is_some())
            + usize::from(attachment.image.is_some());
        if count != 1 {
            problems.push(ManifestProblem::FileContent {
                server: server.into(),
                name: attachment.name.clone(),
            });
        }
        let (mime, bytes) = match (attachment.canvas, attachment.snippet, attachment.image) {
            (Some(text), None, None) => ("text/markdown".into(), text.into_bytes()),
            (None, Some(text), None) => ("text/plain".into(), text.into_bytes()),
            (_, _, Some(image)) => match STANDARD.decode(image.base64) {
                Ok(bytes) => (image.mime, bytes),
                Err(source) => {
                    problems.push(ManifestProblem::Base64 {
                        server: server.into(),
                        name: attachment.name.clone(),
                        source,
                    });
                    continue;
                }
            },
            _ => continue,
        };
        files.insert(
            attachment.name.clone(),
            File {
                id,
                name: attachment.name,
                mime,
                bytes,
            },
        );
    }

    files
}
