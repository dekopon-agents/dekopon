use super::{Journal, JournalError, Line, duration_millis, millis, validated_lines};
use dekopon_agent::{
    memory_tools::MemoryTools,
    prompt::{Pin, ReadRef},
};
use std::{
    borrow::Cow,
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    time::SystemTime,
};

pub(crate) struct JournalMemoryTools<'a> {
    pub journal: &'a Journal,
    pub stem: String,
    pub grant: String,
    pub versions: HashMap<String, String>,
}
impl JournalMemoryTools<'_> {
    fn eligible(&self, line: &Line) -> bool {
        line.grant == self.grant
            && line.at_ms
                >= millis(SystemTime::now())
                    .saturating_sub(duration_millis(self.journal.age_for(&self.stem)))
    }
    fn representation<'a>(&self, line: &'a Line, id: &str) -> Option<Cow<'a, str>> {
        if line.source_eligibility
            && line.representations.iter().any(|rep| {
                rep.kind == "original"
                    && rep.routine_id == id
                    && self.versions.get(id) == Some(&rep.version)
            })
        {
            return line.user.as_deref().map(|user| {
                let mut text = user.to_owned();
                if let Some(answer) = &line.answer {
                    text.push('\n');
                    text.push_str(answer);
                }
                for exchange in &line.exchanges {
                    text.push_str(&format!(
                        "\n[tool: {}, outcome: {}, exit: {:?}]",
                        exchange.tool, exchange.outcome, exchange.exit_status
                    ));
                    for (label, parts) in
                        [("request", &exchange.request), ("result", &exchange.result)]
                    {
                        if let Some(part) = parts.iter().find(|rep| {
                            rep.kind == "original"
                                && rep.routine_id == id
                                && self.versions.get(id) == Some(&rep.version)
                                && rep.text.is_some()
                        }) {
                            text.push_str(&format!("\n[{label}] "));
                            text.push_str(part.text.as_deref().unwrap_or_default());
                        }
                    }
                }
                Cow::Owned(text)
            });
        }
        line.representations
            .iter()
            .chain(
                line.exchanges
                    .iter()
                    .flat_map(|exchange| exchange.request.iter().chain(&exchange.result)),
            )
            .find(|rep| rep.routine_id == id && self.versions.get(id) == Some(&rep.version))
            .and_then(|rep| rep.text.as_deref().map(Cow::Borrowed))
    }
    fn refusal(&self, line: Option<&Line>) -> String {
        line.map_or_else(
            || "record unavailable: expired, evicted or unknown".into(),
            |line| if !self.eligible(line) {
                "record unavailable: expired or changed grant".into()
            } else {
                format!("representation unavailable: missing, discarded or version mismatch; eligible representations: {}", line.availability.join(", "))
            },
        )
    }
    fn lookup(&self, record_id: u64) -> Result<Option<Line>, JournalError> {
        let mut found = None;
        validated_lines(&self.journal.path(&self.stem), |line| {
            #[cfg(test)]
            super::observe_live_decoded(1 + usize::from(found.is_some()));
            if line.record_id == record_id {
                found = Some(line);
            }
            Ok(())
        })?;
        Ok(found)
    }
    fn mutate(&self, mut update: impl FnMut(&mut Line) -> bool) -> Result<bool, JournalError> {
        let path = self.journal.path(&self.stem);
        let temporary = path.with_extension("memory-update");
        let result = (|| {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)?;
            let mut changed = false;
            validated_lines(&path, |mut line| {
                changed |= update(&mut line);
                file.write_all(&super::encode_line(&line)?)?;
                Ok(())
            })?;
            file.sync_all()?;
            drop(file);
            if changed {
                if let Some((agent, _)) = self.stem.split_once('/') {
                    let dir = self.journal.dir.join(agent);
                    let mut total = fs::metadata(&temporary)?.len();
                    for entry in fs::read_dir(dir)? {
                        let sibling = entry?.path();
                        if sibling != path && super::is_journal_file(&sibling) {
                            total = total.saturating_add(fs::metadata(sibling)?.len());
                        }
                    }
                    let cap = self
                        .journal
                        .agent_bytes
                        .get(agent)
                        .copied()
                        .unwrap_or(self.journal.max_bytes_per_agent);
                    if total > cap {
                        return Err(io::Error::other("memory update exceeds agent cap").into());
                    }
                }
                fs::rename(&temporary, &path)?;
            } else {
                fs::remove_file(&temporary)?;
            }
            Ok(changed)
        })();
        if result.is_err() {
            super::remove(&temporary);
        }
        result
    }
}
impl MemoryTools for JournalMemoryTools<'_> {
    fn read(&self, record_id: u64, representation_id: &str, range: Option<(u64, u64)>) -> String {
        let _lock = self.journal.lock.lock();
        let line = match self.lookup(record_id) {
            Ok(line) => line,
            Err(JournalError::Io {
                kind: io::ErrorKind::NotFound,
            }) => None,
            Err(_) => return "record unavailable: storage error".into(),
        };
        let mut outcome = "unavailable";
        let result = if let Some(line) = line.as_ref().filter(|line| self.eligible(line)) {
            if let Some(text) = self.representation(line, representation_id) {
                let selected = match range {
                    None if text.len() <= 16 * 1024 => Some(text.as_ref()),
                    Some((start, end)) => usize::try_from(start)
                        .ok()
                        .zip(usize::try_from(end).ok())
                        .and_then(|(start, end)| {
                            (start < end && end - start <= 16 * 1024).then_some((start, end))
                        })
                        .and_then(|(start, end)| text.get(start..end)),
                    None => None,
                };
                if let Some(selected) = selected {
                    outcome = "read";
                    selected.to_owned()
                } else {
                    outcome = "invalid-range";
                    format!(
                        "range unavailable: select at most 16384 UTF-8 bytes (0..{})",
                        text.len()
                    )
                }
            } else {
                self.refusal(Some(line))
            }
        } else {
            self.refusal(line.as_ref())
        };
        let version = self
            .versions
            .get(representation_id)
            .cloned()
            .unwrap_or_default();
        #[cfg(test)]
        let _held = line.as_ref().map(|_| super::DecodedHold::one());
        if line.as_ref().is_some_and(|line| self.eligible(line))
            && self
                .mutate(|line| {
                    if line.record_id != record_id {
                        return false;
                    }
                    line.reads.push(ReadRef {
                        source_record_id: record_id,
                        representation_id: representation_id.to_owned(),
                        version: version.clone(),
                        range,
                        outcome: outcome.into(),
                    });
                    true
                })
                .is_err()
        {
            return "record unavailable: storage error".into();
        }
        result
    }
    fn set_pin(&self, record_id: u64, representation_id: &str) -> String {
        let _lock = self.journal.lock.lock();
        let line = match self.lookup(record_id) {
            Ok(line) => line,
            Err(JournalError::Io {
                kind: io::ErrorKind::NotFound,
            }) => None,
            Err(_) => return "record unavailable: storage error".into(),
        };
        let Some(line) = line.as_ref().filter(|line| {
            self.eligible(line) && self.representation(line, representation_id).is_some()
        }) else {
            return self.refusal(line.as_ref());
        };
        let cost = self
            .representation(line, representation_id)
            .map_or(0, |text| text.len());
        #[cfg(test)]
        let _held = super::DecodedHold::one();
        match self.mutate(|line| {
            if line.record_id != record_id {
                return false;
            }
            line.pins.retain(|pin| pin.record_id != record_id);
            line.pins.push(Pin {
                record_id,
                representation_id: representation_id.to_owned(),
            });
            true
        }) {
            Ok(_) => format!(
                "pinned record {record_id} to {representation_id}; pinned text bytes: {cost}"
            ),
            Err(_) => "pin unavailable: storage error".into(),
        }
    }
    fn clear_pin(&self, record_id: Option<u64>) -> String {
        let _lock = self.journal.lock.lock();
        match self.mutate(|line| {
            if !self.eligible(line) {
                return false;
            }
            let before = line.pins.len();
            line.pins
                .retain(|pin| record_id.is_some_and(|id| pin.record_id != id));
            line.pins.len() != before
        }) {
            Ok(true) => "pin cleared".into(),
            Ok(false)
            | Err(JournalError::Io {
                kind: io::ErrorKind::NotFound,
            }) => "no eligible pin found".into(),
            Err(_) => "pin unavailable: storage error".into(),
        }
    }
}
