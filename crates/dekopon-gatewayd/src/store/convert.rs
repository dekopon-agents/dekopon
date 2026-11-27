use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

use dekopon_agent::wake::WakeId;
use dekopon_broker_protocol::{ChatTransportKind, Conversation, TraceParent};
use dekopon_core::{AgentId, Subject, TransportId};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::Deserialize;

use super::{
    StoreError,
    record::{AnchorRow, ConversationKey, RecordVersion, Snapshot, WakeRow, WakeRowKind, WatchRow},
};
use crate::{
    memory::journal::FollowMarker, pull_request_links::PullRequestLinkTarget,
    transport::ReplyTarget, wake::DIGEST_HEADER,
};

const CONVERTED: &str = "converted";
const FOLLOW_EXTENSION: &str = "followed";
const MAX_FOLLOW_BYTES: u64 = 4096;

#[derive(Clone, Debug, Default)]
pub(crate) struct LegacyInputs {
    pub(crate) wakes: Option<PathBuf>,
    pub(crate) journal: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub(crate) struct Conversion {
    pub(crate) conversations: Vec<ConversationKey>,
    pub(crate) wakes: usize,
    pub(crate) follows: FollowConversion,
}

#[derive(Clone, Debug)]
pub(crate) enum FollowConversion {
    None,
    Retained { markers: usize },
}

impl LegacyInputs {
    pub(crate) fn convert(
        &self,
        transaction: &Transaction<'_>,
        now_ms: u64,
    ) -> Result<Option<Conversion>, StoreError> {
        let converted: Option<String> = transaction
            .query_row(
                "SELECT value FROM meta WHERE name = ?1",
                [CONVERTED],
                |row| row.get(0),
            )
            .optional()?;
        if converted.is_some() {
            return Ok(None);
        }
        let mut snapshots: BTreeMap<ConversationKey, Snapshot> = BTreeMap::new();
        let mut wakes = 0;
        if let Some(path) = &self.wakes {
            match File::open(path) {
                Ok(file) => {
                    for (index, line) in BufReader::new(file).lines().enumerate() {
                        let legacy = || StoreError::LegacyWakes {
                            path: path.clone(),
                            line: index + 1,
                        };
                        let (key, row) = serde_json::from_str::<Line>(&line?)
                            .ok()
                            .and_then(Line::row)
                            .ok_or_else(legacy)?;
                        snapshots.entry(key).or_default().wakes.push(row);
                        wakes += 1;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        for (key, snapshot) in &snapshots {
            transaction.execute(
                "INSERT INTO conversations (key, version, snapshot, updated_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(key) DO UPDATE SET snapshot = excluded.snapshot,
                    version = excluded.version, updated_ms = excluded.updated_ms",
                params![
                    key.to_string(),
                    RecordVersion::CURRENT.get(),
                    serde_json::to_string(snapshot).map_err(io::Error::from)?,
                    i64::try_from(now_ms).unwrap_or(i64::MAX),
                ],
            )?;
        }
        let follows = match &self.journal {
            Some(journal) => {
                let mut markers = 0;
                for (key, marker) in legacy_follows(journal)? {
                    transaction.execute(
                        "INSERT OR REPLACE INTO retained_follows (key, marker) VALUES (?1, ?2)",
                        params![
                            key,
                            serde_json::to_string(&marker).map_err(io::Error::from)?
                        ],
                    )?;
                    markers += 1;
                }
                FollowConversion::Retained { markers }
            }
            None => FollowConversion::None,
        };
        transaction.execute(
            "INSERT INTO meta (name, value) VALUES (?1, ?2)",
            params![CONVERTED, now_ms.to_string()],
        )?;
        Ok(Some(Conversion {
            conversations: snapshots.into_keys().collect(),
            wakes,
            follows,
        }))
    }

    pub(super) fn retire(&self) {
        if let Some(path) = &self.wakes
            && path.exists()
        {
            let aside = converted_path(path);
            if let Err(error) = fs::rename(path, &aside) {
                tracing::warn!(
                    event = "gateway_store_conversion_retire_failed",
                    error.kind = ?error.kind(),
                    "gateway.store.conversion.retire.failed"
                );
            }
        }
        if let Some(journal) = &self.journal
            && let Ok(entries) = fs::read_dir(journal)
        {
            for path in entries.flatten().map(|entry| entry.path()) {
                if follow_key(&path).is_some()
                    && let Err(error) = fs::remove_file(&path)
                    && error.kind() != io::ErrorKind::NotFound
                {
                    tracing::warn!(
                        event = "gateway_store_conversion_retire_failed",
                        error.kind = ?error.kind(),
                        "gateway.store.conversion.retire.failed"
                    );
                }
            }
        }
    }
}

fn converted_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".converted");
    path.with_file_name(name)
}

fn follow_key(path: &Path) -> Option<String> {
    if path
        .extension()
        .is_none_or(|extension| extension != FOLLOW_EXTENSION)
    {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    (stem.len() == 64 && stem.bytes().all(|byte| byte.is_ascii_hexdigit())).then(|| stem.to_owned())
}

fn legacy_follows(journal: &Path) -> Result<Vec<(String, FollowMarker)>, StoreError> {
    let entries = match fs::read_dir(journal) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut follows = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let Some(key) = follow_key(&path) else {
            continue;
        };
        let mut bytes = Vec::new();
        File::open(&path)?
            .take(MAX_FOLLOW_BYTES + 1)
            .read_to_end(&mut bytes)?;
        match serde_json::from_slice::<FollowMarker>(&bytes) {
            Ok(marker) if marker.at_ms < marker.until_ms => follows.push((key, marker)),
            Ok(_) | Err(_) => tracing::warn!(
                event = "gateway_follow",
                operation = "refused",
                reason = "corrupt",
                "gateway.follow"
            ),
        }
    }
    Ok(follows)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Line {
    id: u32,
    transport: String,
    kind: ChatTransportKind,
    subject: Subject,
    #[serde(default)]
    subject_id: Option<Subject>,
    conversation: Conversation,
    reply: ReplyTarget,
    agent: AgentId,
    #[serde(default)]
    scheduled_in: Option<TraceParent>,
    note: String,
    next_at_ms: u64,
    #[serde(default)]
    watch: Option<WatchRow>,
    #[serde(default)]
    subscription: Option<PullRequestLinkTarget>,
    #[serde(default, rename = "run_as")]
    run_as: Option<RunAs>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RunAs {
    Space,
}

impl Line {
    fn row(self) -> Option<(ConversationKey, WakeRow)> {
        let transport: TransportId = self.transport.parse().ok()?;
        let space = match (self.run_as, &self.subscription) {
            (Some(RunAs::Space), _) | (None, Some(_)) => true,
            (None, None) => false,
        };
        let kind = match (self.watch, self.subscription) {
            (None, None) => WakeRowKind::Once,
            (None, Some(target)) => WakeRowKind::Subscription { target },
            (Some(_), Some(_)) => return None,
            (Some(mut watch), None) => match watch.script.strip_prefix(DIGEST_HEADER) {
                Some(script) if space => {
                    watch.script = script.to_owned();
                    WakeRowKind::Digest(watch)
                }
                Some(_) | None => WakeRowKind::Watch(watch),
            },
        };
        let key = ConversationKey::of(&transport, &self.conversation);
        Some((
            key,
            WakeRow {
                id: WakeId(self.id),
                anchor: AnchorRow {
                    transport,
                    kind: self.kind,
                    subject: self.subject,
                    subject_id: self.subject_id,
                    conversation: self.conversation,
                    reply: self.reply,
                    agent: self.agent,
                    scheduled_in: self.scheduled_in,
                },
                note: self.note,
                next_at_ms: self.next_at_ms,
                kind,
                pending: None,
            },
        ))
    }
}
