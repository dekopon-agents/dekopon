//! Required inference-attempt observations. This module owns no job totals or billing estimates.
use crate::model::{ModelError, ModelUsage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// One inference transmission, or an explicitly non-HTTP adapter operation (e.g. a fixture).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AttemptKind {
    Http,
    Adapter,
}

/// The five usage fields an observation reports, in [`ModelUsage::fields`] order.
///
/// One definition of the names. A warning that says a field went unknown uses the same word the
/// protocol's report spells `<field>_tokens`, so an operator reading the log knows which of the
/// five counts to stop trusting instead of guessing.
pub const USAGE_FIELD_NAMES: [&str; 5] = [
    "input",
    "cached_input",
    "output",
    "reasoning_output",
    "provider_total",
];

/// Which report an observation came from, so a terminal one supersedes an interim one.
///
/// A streaming response may report usage on `response.in_progress` and again on
/// `response.completed`. Those are the same transmission described twice, and only the last one is
/// the provider's own final answer; ranking them keeps an early estimate from competing with it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ObservationPrecedence {
    /// An event before the response completed. A terminal report replaces it.
    #[default]
    Interim,
    /// The response's own final usage report; nothing later supersedes it.
    Final,
}

/// Missing/malformed fields stay unknown independently of other reported fields.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageObservation {
    pub usage: ModelUsage,
    pub invalid: [bool; 5],
}
impl UsageObservation {
    /// Merges a second observation of the same attempt, marking every disagreeing field unknown.
    ///
    /// Two differing reports of one transmission describe work this process cannot re-measure, and
    /// a provider controls both of them: duplicate `"usage"` keys in one JSON object are legal, and
    /// a stream can contradict itself. Disagreement is therefore a fact about those fields, not a
    /// reason to stop counting the job — every field the two reports agreed on survives, and only
    /// what is genuinely in doubt becomes unknown. Returns the merged observation and which fields
    /// were in conflict.
    #[must_use]
    pub fn reconcile(self, other: Self) -> (Self, [bool; 5]) {
        let (mine, theirs) = (self.usage.fields(), other.usage.fields());
        let mut fields = [None; 5];
        let mut invalid = [false; 5];
        let mut conflicts = [false; 5];
        for (index, conflict) in conflicts.iter_mut().enumerate() {
            if mine[index] == theirs[index] && self.invalid[index] == other.invalid[index] {
                fields[index] = mine[index];
                invalid[index] = self.invalid[index];
            } else {
                // Unknown rather than zero, and invalid so no total can be computed from it.
                *conflict = true;
                invalid[index] = true;
            }
        }
        (
            Self {
                usage: ModelUsage::from_fields(fields),
                invalid,
            },
            conflicts,
        )
    }

    /// Normalize a usage object without letting a malformed sibling erase valid counts.
    pub fn from_json(value: &Value, chat_completions: bool) -> Self {
        let paths = if chat_completions {
            [
                "/prompt_tokens",
                "/prompt_tokens_details/cached_tokens",
                "/completion_tokens",
                "/completion_tokens_details/reasoning_tokens",
                "/total_tokens",
            ]
        } else {
            [
                "/input_tokens",
                "/input_tokens_details/cached_tokens",
                "/output_tokens",
                "/output_tokens_details/reasoning_tokens",
                "/total_tokens",
            ]
        };
        let mut fields = [None; 5];
        let mut invalid = [false; 5];
        for (i, path) in paths.iter().enumerate() {
            if let Some(v) = value.pointer(path).filter(|v| !v.is_null()) {
                fields[i] = v.as_u64();
                invalid[i] = fields[i].is_none();
            }
        }
        if !value.is_object() && !value.is_null() {
            invalid = [true; 5];
        }
        Self {
            usage: ModelUsage::from_fields(fields),
            invalid,
        }
    }
}

/// Recorder failure is terminal: no transmission may start after a failed reservation.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("inference accounting refused: {0}")]
pub struct AccountingError(pub &'static str);

/// Names every field a [`UsageObservation::reconcile`] found in conflict, for one log line.
#[must_use]
pub fn conflicting_fields(conflicts: [bool; 5]) -> String {
    USAGE_FIELD_NAMES
        .iter()
        .zip(conflicts)
        .filter_map(|(name, conflict)| conflict.then_some(*name))
        .collect::<Vec<_>>()
        .join(",")
}

/// Implemented by the harness for each logical call. Reserve before inference HTTP; auth refresh
/// is not inference. Observe usage before decoding content, even on unsuccessful responses.
///
/// A recorder never refuses an observation because it disagrees with an earlier one: provider text
/// decides what those two reports say, and a disagreement is a fact about the fields involved, not
/// grounds to fence a job. It reconciles them with [`UsageObservation::reconcile`] — or, when the
/// later report is [`ObservationPrecedence::Final`] and the earlier one was not, replaces it.
/// [`AccountingError`] stays what it always meant: no further transmission on this ledger is safe.
pub trait AttemptRecorder {
    fn begin(&self, kind: AttemptKind) -> Result<u32, AccountingError>;

    /// Records one usage report that is the adapter's own final answer for this attempt.
    ///
    /// An adapter that reports usage once — a JSON body, a fixture — calls only this.
    fn observe(&self, attempt: u32, usage: UsageObservation) -> Result<(), AccountingError>;

    /// Records one usage report whose rank within the response is known.
    ///
    /// A streaming adapter can report usage on an interim event and again on the terminal one, and
    /// only the terminal report is the provider's own final answer. The default ignores the rank
    /// and forwards to [`AttemptRecorder::observe`], which is right for a recorder that keeps no
    /// ledger. A recorder that does keep one overrides *this* method and defines
    /// [`AttemptRecorder::observe`] as this method with [`ObservationPrecedence::Final`], so the
    /// two entry points cannot disagree about the same attempt.
    fn observe_ranked(
        &self,
        attempt: u32,
        usage: UsageObservation,
        _precedence: ObservationPrecedence,
    ) -> Result<(), AccountingError> {
        self.observe(attempt, usage)
    }
}

/// One attempt an [`AttemptLog`] kept.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoggedAttempt {
    /// Whether the attempt was an HTTP transmission or an adapter operation.
    pub kind: AttemptKind,
    /// The reconciled usage this attempt reported, `None` when it reported none.
    pub observation: Option<UsageObservation>,
    /// Which report [`LoggedAttempt::observation`] came from.
    pub precedence: ObservationPrecedence,
}

/// A bounded in-memory recorder for a caller that keeps no ledger of its own.
///
/// [`crate::model::ChatModel::complete`] takes an [`AttemptRecorder`] because an inference
/// transmission nobody counted is spend nobody can account for. The harness supplies a checkpointed
/// one; a consumer with nothing to checkpoint — a one-shot script, an example, a test — uses this
/// and reads the observations back afterwards. It counts, it does not persist: dropping it loses
/// the record, which is exactly why a long-lived job implements the trait itself.
///
/// ```
/// use dekopon_model::usage::{AttemptKind, AttemptLog, AttemptRecorder, UsageObservation};
///
/// let log = AttemptLog::default();
/// let attempt = log.begin(AttemptKind::Http)?;
/// // `ChatModel::complete(&messages, &tools, &log)` observes into it; this stands in for that.
/// let usage = UsageObservation::from_json(
///     &serde_json::json!({"input_tokens": 12, "output_tokens": 3}),
///     false,
/// );
/// log.observe(attempt, usage)?;
///
/// let attempts = log.observations();
/// assert_eq!(attempts.len(), 1);
/// assert_eq!(attempts[0].observation.unwrap().usage.input_tokens, Some(12));
/// # Ok::<(), dekopon_model::usage::AccountingError>(())
/// ```
#[derive(Default)]
pub struct AttemptLog(std::sync::Mutex<Vec<LoggedAttempt>>);
impl AttemptLog {
    /// Every attempt begun on this log, in the order they were begun.
    #[must_use]
    pub fn observations(&self) -> Vec<LoggedAttempt> {
        self.0.lock().expect("attempt log lock").clone()
    }
}
impl AttemptRecorder for AttemptLog {
    fn begin(&self, kind: AttemptKind) -> Result<u32, AccountingError> {
        let mut log = self.0.lock().map_err(|e| {
            tracing::error!(cause_type = "attempt-log-lock", %e);
            AccountingError("poisoned")
        })?;
        if log.len() >= 2 {
            return Err(AccountingError("attempt limit"));
        }
        log.push(LoggedAttempt {
            kind,
            observation: None,
            precedence: ObservationPrecedence::Interim,
        });
        Ok(log.len() as u32)
    }
    fn observe(&self, attempt: u32, usage: UsageObservation) -> Result<(), AccountingError> {
        self.observe_ranked(attempt, usage, ObservationPrecedence::Final)
    }
    fn observe_ranked(
        &self,
        attempt: u32,
        usage: UsageObservation,
        precedence: ObservationPrecedence,
    ) -> Result<(), AccountingError> {
        let mut log = self.0.lock().map_err(|e| {
            tracing::error!(cause_type = "attempt-log-lock", %e);
            AccountingError("poisoned")
        })?;
        let slot = log
            .get_mut(
                attempt
                    .checked_sub(1)
                    .ok_or(AccountingError("attempt id"))? as usize,
            )
            .ok_or(AccountingError("attempt id"))?;
        match slot.observation {
            Some(existing) if existing != usage => {
                if precedence > slot.precedence {
                    slot.observation = Some(usage);
                    slot.precedence = precedence;
                } else if precedence == slot.precedence {
                    let (merged, conflicts) = existing.reconcile(usage);
                    tracing::warn!(
                        cause_type = "conflicting-usage-observation",
                        attempt,
                        usage.fields = %conflicting_fields(conflicts),
                        "attempt reported disagreeing usage; those fields are unknown"
                    );
                    slot.observation = Some(merged);
                }
            }
            // An interim report never displaces the terminal one, and an identical repeat is not a
            // second observation.
            Some(_) => {}
            None => {
                slot.observation = Some(usage);
                slot.precedence = precedence;
            }
        }
        Ok(())
    }
}

/// Decode top-level members incrementally: usage can arrive before malformed content, trailing
/// garbage, or a later reader error. Byte bounds apply to the reader, not a post-allocation check.
pub(crate) fn read_usage_json(
    reader: impl std::io::Read,
    limit: u64,
    recorder: &dyn AttemptRecorder,
    attempt: u32,
    chat: bool,
) -> Result<Value, ModelError> {
    use serde::de::{DeserializeSeed, MapAccess, Visitor};
    struct Seed<'a> {
        recorder: &'a dyn AttemptRecorder,
        attempt: u32,
        chat: bool,
        error: &'a std::cell::Cell<Option<AccountingError>>,
    }
    impl<'de> DeserializeSeed<'de> for Seed<'_> {
        type Value = Value;
        fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
            d.deserialize_map(self)
        }
    }
    impl<'de> Visitor<'de> for Seed<'_> {
        type Value = Value;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("model response object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
            let mut object = serde_json::Map::new();
            while let Some(key) = map.next_key::<String>()? {
                let value = if key == "response" {
                    map.next_value_seed(Seed {
                        recorder: self.recorder,
                        attempt: self.attempt,
                        chat: self.chat,
                        error: self.error,
                    })?
                } else {
                    map.next_value::<Value>()?
                };
                if key == "usage" && !value.is_null() {
                    // A whole response body is the provider's final word, so both halves of a
                    // duplicate `"usage"` key rank equally and reconcile rather than one winning.
                    self.recorder
                        .observe(self.attempt, UsageObservation::from_json(&value, self.chat))
                        .map_err(|e| {
                            self.error.set(Some(e));
                            serde::de::Error::custom(e)
                        })?;
                }
                object.insert(key, value);
            }
            Ok(Value::Object(object))
        }
    }
    let mut reader = reader.take(limit + 1);
    let error = std::cell::Cell::new(None);
    let mut de = serde_json::Deserializer::from_reader(&mut reader);
    let result = Seed {
        recorder,
        attempt,
        chat,
        error: &error,
    }
    .deserialize(&mut de)
    .and_then(|v| {
        de.end()?;
        Ok(v)
    });
    if let Some(error) = error.get() {
        return Err(error.into());
    }
    if reader.limit() == 0 {
        return Err(ModelError::Response("response exceeded byte bound".into()));
    }
    result.map_err(|e| ModelError::Response(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn usage_preceding_bad_content_or_a_later_read_error_is_not_lost() {
        struct Broken(std::io::Cursor<&'static [u8]>);
        impl std::io::Read for Broken {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                let n = std::io::Read::read(&mut self.0, b)?;
                if n == 0 {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "fixture read failed",
                    ))
                } else {
                    Ok(n)
                }
            }
        }
        let log = AttemptLog::default();
        let attempt = log.begin(AttemptKind::Adapter).unwrap();
        let response = br#"{"usage":{"prompt_tokens":17,"completion_tokens":4},"choices":"#;
        assert!(
            read_usage_json(
                Broken(std::io::Cursor::new(response)),
                4096,
                &log,
                attempt,
                true
            )
            .is_err()
        );
        assert_eq!(
            log.observations()[0]
                .observation
                .unwrap()
                .usage
                .input_tokens,
            Some(17)
        );
    }
    #[test]
    fn malformed_usage_fields_do_not_erase_valid_siblings_and_conflicts_go_unknown() {
        let log = AttemptLog::default();
        let attempt = log.begin(AttemptKind::Http).unwrap();
        let observation = UsageObservation::from_json(
            &serde_json::json!({"input_tokens":4,"output_tokens":"bad","total_tokens":6}),
            false,
        );
        assert_eq!(observation.usage.input_tokens, Some(4));
        assert!(observation.invalid[2]);
        log.observe(attempt, observation).unwrap();
        log.observe(attempt, observation).unwrap();
        assert_eq!(log.observations().len(), 1);
        assert_eq!(log.observations()[0].observation, Some(observation));
        // A disagreeing second report of the same rank blanks the fields it disagrees on and
        // keeps the ledger usable, rather than refusing and fencing every later transmission.
        log.observe(attempt, UsageObservation::default()).unwrap();
        let merged = log.observations()[0].observation.unwrap();
        assert_eq!(merged.usage.input_tokens, None);
        assert_eq!(merged.usage.total_tokens, None);
        assert_eq!(merged.invalid, [true, false, true, false, true]);
        log.begin(AttemptKind::Http).unwrap();
        assert!(log.begin(AttemptKind::Http).is_err());
    }
    #[test]
    fn a_terminal_report_supersedes_an_interim_one_and_an_interim_one_never_displaces_it() {
        let interim = UsageObservation::from_json(&serde_json::json!({"input_tokens":7}), false);
        let completed = UsageObservation::from_json(
            &serde_json::json!({"input_tokens":7,"output_tokens":9}),
            false,
        );
        let log = AttemptLog::default();
        let attempt = log.begin(AttemptKind::Http).unwrap();
        log.observe_ranked(attempt, interim, ObservationPrecedence::Interim)
            .unwrap();
        log.observe_ranked(attempt, completed, ObservationPrecedence::Final)
            .unwrap();
        assert_eq!(log.observations()[0].observation, Some(completed));
        log.observe_ranked(attempt, interim, ObservationPrecedence::Interim)
            .unwrap();
        assert_eq!(log.observations()[0].observation, Some(completed));
    }
    #[test]
    fn duplicate_usage_keys_in_one_body_decode_and_leave_the_conflict_unknown() {
        let log = AttemptLog::default();
        let attempt = log.begin(AttemptKind::Http).unwrap();
        // Duplicate object keys are legal JSON and a provider controls both of them.
        let body = br#"{"usage":{"prompt_tokens":11,"completion_tokens":2},"usage":{"prompt_tokens":900,"completion_tokens":2},"choices":[]}"#;
        let value = read_usage_json(std::io::Cursor::new(&body[..]), 4096, &log, attempt, true)
            .expect("a duplicated usage key is not a decode failure");
        assert!(value.is_object());
        let observed = log.observations()[0].observation.unwrap();
        assert_eq!(observed.usage.input_tokens, None);
        assert!(observed.invalid[0]);
        assert_eq!(observed.usage.output_tokens, Some(2));
    }
}
