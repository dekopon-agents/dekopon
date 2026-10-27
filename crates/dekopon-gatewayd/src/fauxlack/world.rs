use std::{collections::BTreeMap, sync::Arc, time::Duration};

use dekopon_broker_protocol::ConversationKind;
use dekopon_core::ExternalSubject;
use parking_lot::Mutex;
use thiserror::Error;
use tokio::sync::{Semaphore, mpsc, watch};

use crate::{config::ConfigProblem, transport::TransportIdentity};

#[derive(Clone)]
pub struct Fauxlack(Arc<World>);
struct World {
    servers: BTreeMap<String, Server>,
}
#[derive(Clone)]
pub struct Server(Arc<ServerData>);
struct ServerData {
    id: String,
    facts: Facts,
    state: Mutex<State>,
    posting: Semaphore,
    changed: watch::Sender<u64>,
}
pub(crate) struct Facts {
    pub users: BTreeMap<String, Person>,
    pub containers: BTreeMap<String, Container>,
    pub files: BTreeMap<String, File>,
}
pub(crate) struct Person {
    pub name: String,
    pub bot: bool,
    pub subject: ExternalSubject,
}
pub(crate) struct Container {
    pub name: Option<String>,
    pub kind: ConversationKind,
    pub members: Vec<String>,
}
#[derive(Default)]
struct State {
    next_message: u64,
    next_file: usize,
    completion: u64,
    messages: BTreeMap<u64, Row>,
    files: BTreeMap<String, File>,
    attachments: BTreeMap<String, mpsc::Sender<Message>>,
}
struct Row {
    message: Message,
    state: MessageState,
}
enum MessageState {
    Posted,
    Progress,
    Final(u64),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct File {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}
#[derive(Clone, Debug)]
pub struct Message {
    pub id: String,
    pub author: String,
    pub container: String,
    pub thread: Option<String>,
    pub text: String,
    pub mentions: Vec<String>,
    pub files: Vec<File>,
}
#[derive(Clone)]
pub struct User {
    server: Server,
    id: String,
}
#[derive(Clone)]
pub struct Posted {
    message: Message,
    thread: Thread,
}
#[derive(Clone)]
pub struct Thread {
    server: Server,
    container: String,
    root: Option<String>,
    cursor: Arc<Mutex<u64>>,
}
pub struct Transcript {
    pub messages: Vec<Message>,
}
impl Transcript {
    pub fn authors(&self) -> Vec<&str> {
        self.messages
            .iter()
            .map(|message| message.author.as_str())
            .collect()
    }
}
#[derive(Debug, Error, Eq, PartialEq)]
pub enum PostError {
    #[error("user is not a member")]
    NotAMember,
    #[error("unknown container")]
    UnknownContainer,
    #[error("unknown file")]
    UnknownFile,
}

impl std::fmt::Debug for Fauxlack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fauxlack")
            .field("servers", &self.0.servers.keys())
            .finish()
    }
}
impl Fauxlack {
    pub(crate) fn new(facts: BTreeMap<String, Facts>) -> Self {
        let (changed, _) = watch::channel(0);
        let servers = facts
            .into_iter()
            .map(|(id, facts)| {
                let state = State {
                    next_file: facts.files.len(),
                    files: facts
                        .files
                        .values()
                        .map(|file| (file.id.clone(), file.clone()))
                        .collect(),
                    ..State::default()
                };
                (
                    id.clone(),
                    Server(Arc::new(ServerData {
                        id,
                        facts,
                        state: Mutex::new(state),
                        posting: Semaphore::new(1),
                        changed: changed.clone(),
                    })),
                )
            })
            .collect();
        Self(Arc::new(World { servers }))
    }
    pub fn server(&self, id: &str) -> Server {
        self.0
            .servers
            .get(id)
            .unwrap_or_else(|| panic!("unknown fauxlack server {id}"))
            .clone()
    }
    pub(crate) fn validate(
        &self,
        name: &str,
        server: &str,
        bot: &str,
        problems: &mut Vec<ConfigProblem>,
    ) {
        match self.0.servers.get(server) {
            None => problems.push(ConfigProblem::FauxlackServer {
                name: name.into(),
                server: server.into(),
            }),
            Some(server) => match server.0.facts.users.get(bot) {
                None => problems.push(ConfigProblem::FauxlackBot {
                    name: name.into(),
                    bot: bot.into(),
                }),
                Some(user) if !user.bot => problems.push(ConfigProblem::FauxlackNotBot {
                    name: name.into(),
                    bot: bot.into(),
                }),
                Some(_) => {}
            },
        }
    }
}
impl Server {
    pub fn user(&self, id: &str) -> User {
        assert!(
            self.0.facts.users.contains_key(id),
            "unknown fauxlack user {id}"
        );
        User {
            server: self.clone(),
            id: id.into(),
        }
    }
    pub async fn attached(&self, bot: &str) {
        let mut changed = self.0.changed.subscribe();
        let waiting = async {
            loop {
                if self.0.state.lock().attachments.contains_key(bot) {
                    return;
                }
                changed.changed().await.expect("world is owned by server");
            }
        };
        tokio::time::timeout(Duration::from_secs(30), waiting)
            .await
            .expect("fauxlack bot attached");
    }
    pub fn transcript(&self) -> Transcript {
        self.snapshot(|_| true)
    }
    pub fn channel_name(&self, id: &str) -> Option<&str> {
        self.container(id)?.name.as_deref()
    }
    pub(crate) fn id(&self) -> &str {
        &self.0.id
    }
    pub(crate) fn person(&self, id: &str) -> Option<&Person> {
        self.0.facts.users.get(id)
    }
    pub(crate) fn container(&self, id: &str) -> Option<&Container> {
        self.0.facts.containers.get(id)
    }
    pub(crate) fn attach(&self, bot: &str) -> Option<(TransportIdentity, mpsc::Receiver<Message>)> {
        let user = self.person(bot)?;
        if !user.bot {
            return None;
        }
        let (sender, receiver) = mpsc::channel(64);
        let mut state = self.0.state.lock();
        if state.attachments.contains_key(bot) {
            return None;
        }
        state.attachments.insert(bot.into(), sender);
        drop(state);
        self.bump();
        Some((
            TransportIdentity {
                user_id: Some(bot.into()),
                handle: Some(user.name.clone()),
            },
            receiver,
        ))
    }
    pub(crate) fn detach(&self, bot: &str) {
        self.0.state.lock().attachments.remove(bot);
        self.bump();
    }
    fn bump(&self) {
        self.0.changed.send_modify(|revision| *revision += 1);
    }
    fn snapshot(&self, filter: impl Fn(&Message) -> bool) -> Transcript {
        Transcript {
            messages: self
                .0
                .state
                .lock()
                .messages
                .values()
                .filter_map(|row| {
                    let visible = match row.state {
                        MessageState::Posted | MessageState::Final(_) => true,
                        MessageState::Progress => false,
                    };
                    (visible && filter(&row.message)).then(|| row.message.clone())
                })
                .collect(),
        }
    }
    pub(crate) async fn publish(
        &self,
        author: &str,
        container: &str,
        thread: Option<&str>,
        text: &str,
        files: Vec<File>,
        publication: Publication,
    ) -> Result<Posted, PostError> {
        let facts = self
            .container(container)
            .ok_or(PostError::UnknownContainer)?;
        if !facts.members.iter().any(|member| member == author) {
            return Err(PostError::NotAMember);
        }
        let _admission = self
            .0
            .posting
            .acquire()
            .await
            .map_err(|_| PostError::UnknownContainer)?;
        let (posted, deliveries) = {
            let mut state = self.0.state.lock();
            state.next_message += 1;
            let number = state.next_message;
            let id = format!("m{number}");
            let root = match facts.kind {
                ConversationKind::Channel | ConversationKind::Thread => {
                    Some(thread.unwrap_or(&id).to_owned())
                }
                ConversationKind::DirectMessage | ConversationKind::GroupDirectMessage => None,
            };
            let observer = Thread {
                server: self.clone(),
                container: container.into(),
                root: root.clone(),
                cursor: Arc::new(Mutex::new(state.completion)),
            };
            let message = Message {
                id,
                author: author.into(),
                container: container.into(),
                thread: root,
                text: text.into(),
                mentions: mentions(text),
                files,
            };
            let kind = match publication {
                Publication::Final => {
                    state.completion += 1;
                    MessageState::Final(state.completion)
                }
                Publication::Posted => MessageState::Posted,
                Publication::Progress => MessageState::Progress,
            };
            state.messages.insert(
                number,
                Row {
                    message: message.clone(),
                    state: kind,
                },
            );
            let deliveries = state
                .attachments
                .iter()
                .filter(|(bot, _)| facts.members.contains(bot))
                .map(|(_, sender)| sender.clone())
                .collect::<Vec<_>>();
            (
                Posted {
                    message,
                    thread: observer,
                },
                deliveries,
            )
        };
        self.bump();
        for sender in deliveries {
            // A detached reader owns no delivery; bounded sends otherwise backpressure this post.
            if sender.send(posted.message.clone()).await.is_err() {
                self.bump();
            }
        }
        Ok(posted)
    }
    pub(crate) fn files(&self, names: &[&str]) -> Result<Vec<File>, PostError> {
        names
            .iter()
            .map(|name| {
                self.0
                    .facts
                    .files
                    .get(*name)
                    .cloned()
                    .ok_or(PostError::UnknownFile)
            })
            .collect()
    }
    pub(crate) fn read_file(&self, id: &str) -> Option<File> {
        self.0.state.lock().files.get(id).cloned()
    }
    pub(crate) fn store_files(&self, files: Vec<(String, String, Vec<u8>)>) -> Vec<File> {
        let mut state = self.0.state.lock();
        let files = files
            .into_iter()
            .map(|(name, mime, bytes)| {
                state.next_file += 1;
                let file = File {
                    id: format!("f{}", state.next_file),
                    name,
                    mime,
                    bytes,
                };
                state.files.insert(file.id.clone(), file.clone());
                file
            })
            .collect();
        drop(state);
        self.bump();
        files
    }
    pub(crate) fn progress(&self, id: &str, text: &str, operation: Progress) -> bool {
        let Some(number) = id
            .strip_prefix('m')
            .and_then(|number| number.parse::<u64>().ok())
        else {
            return false;
        };
        let mut state = self.0.state.lock();
        if !state.messages.contains_key(&number) {
            return false;
        }
        match operation {
            Progress::Delete => {
                state.messages.remove(&number);
            }
            Progress::Edit => {
                if let Some(row) = state.messages.get_mut(&number) {
                    row.message.text = text.into();
                    row.state = MessageState::Progress;
                }
            }
            Progress::Finalize(files) => {
                let already = state
                    .messages
                    .get(&number)
                    .is_some_and(|row| match row.state {
                        MessageState::Final(_) => true,
                        MessageState::Posted | MessageState::Progress => false,
                    });
                if already {
                    return false;
                }
                state.completion += 1;
                let completion = state.completion;
                if let Some(row) = state.messages.get_mut(&number) {
                    row.message.text = text.into();
                    row.message.files = files;
                    row.state = MessageState::Final(completion);
                }
            }
        }
        drop(state);
        self.bump();
        true
    }
}
pub(crate) enum Publication {
    Posted,
    Progress,
    Final,
}
pub(crate) enum Progress {
    Edit,
    Delete,
    Finalize(Vec<File>),
}
impl User {
    pub async fn post(&self, container: &str, text: &str) -> Result<Posted, PostError> {
        self.post_with(container, text, &[]).await
    }
    pub async fn post_with(
        &self,
        container: &str,
        text: &str,
        files: &[&str],
    ) -> Result<Posted, PostError> {
        self.server
            .publish(
                &self.id,
                container,
                None,
                text,
                self.server.files(files)?,
                Publication::Posted,
            )
            .await
    }
    pub async fn reply(&self, posted: &Posted, text: &str) -> Result<Posted, PostError> {
        if !Arc::ptr_eq(&self.server.0, &posted.thread.server.0) {
            return Err(PostError::UnknownContainer);
        }
        self.server
            .publish(
                &self.id,
                &posted.message.container,
                posted.message.thread.as_deref(),
                text,
                Vec::new(),
                Publication::Posted,
            )
            .await
    }
}
impl Posted {
    pub fn id(&self) -> &str {
        &self.message.id
    }
    pub fn thread(&self) -> Thread {
        self.thread.clone()
    }
}
impl Thread {
    pub fn transcript(&self) -> Transcript {
        self.server
            .snapshot(|message| message.container == self.container && message.thread == self.root)
    }
    pub async fn next_from(&self, bot: &str, timeout: Duration) -> Message {
        let mut changed = self.server.0.changed.subscribe();
        let waiting = async {
            loop {
                {
                    let mut cursor = self.cursor.lock();
                    let state = self.server.0.state.lock();
                    let next = state
                        .messages
                        .values()
                        .filter_map(|row| match row.state {
                            MessageState::Final(sequence)
                                if sequence > *cursor
                                    && row.message.author == bot
                                    && row.message.container == self.container
                                    && row.message.thread == self.root =>
                            {
                                Some((sequence, &row.message))
                            }
                            MessageState::Final(_)
                            | MessageState::Posted
                            | MessageState::Progress => None,
                        })
                        .min_by_key(|(sequence, _)| *sequence);
                    if let Some((sequence, message)) = next {
                        *cursor = sequence;
                        return message.clone();
                    }
                }
                changed.changed().await.expect("world is owned by thread");
            }
        };
        tokio::time::timeout(timeout, waiting)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "fauxlack answer timed out: {:#?}",
                    self.transcript().messages
                )
            })
    }
}
fn mentions(text: &str) -> Vec<String> {
    let mut mentions = Vec::new();
    let mut rest = text;
    while let Some((_, after)) = rest.split_once("<@") {
        let Some((id, after)) = after.split_once('>') else {
            break;
        };
        if !id.is_empty() && !id.contains(['<', '@', ' ']) {
            mentions.push(id.into());
        }
        rest = after;
    }
    mentions
}
