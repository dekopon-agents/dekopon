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

/// Missing/malformed fields stay unknown independently of other reported fields.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageObservation {
    pub usage: ModelUsage,
    pub invalid: [bool; 5],
}
impl UsageObservation {
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

/// Implemented by the harness for each logical call. Reserve before inference HTTP; auth refresh
/// is not inference. Observe usage before decoding content, even on unsuccessful responses.
pub trait AttemptRecorder {
    fn begin(&self, kind: AttemptKind) -> Result<u32, AccountingError>;
    fn observe(&self, attempt: u32, usage: UsageObservation) -> Result<(), AccountingError>;
}

/// Bounded test capture; production callers must supply their own job recorder.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct AttemptLog(std::sync::Mutex<Vec<(AttemptKind, Option<UsageObservation>)>>);
#[cfg(test)]
impl AttemptLog {
    pub fn observations(&self) -> Vec<(AttemptKind, Option<UsageObservation>)> {
        self.0.lock().expect("attempt log lock").clone()
    }
}
#[cfg(test)]
impl AttemptRecorder for AttemptLog {
    fn begin(&self, kind: AttemptKind) -> Result<u32, AccountingError> {
        let mut log = self.0.lock().map_err(|e| {
            tracing::error!(cause_type = "attempt-log-lock", %e);
            AccountingError("poisoned")
        })?;
        if log.len() >= 2 {
            return Err(AccountingError("attempt limit"));
        }
        log.push((kind, None));
        Ok(log.len() as u32)
    }
    fn observe(&self, attempt: u32, usage: UsageObservation) -> Result<(), AccountingError> {
        let mut log = self.0.lock().map_err(|e| {
            tracing::error!(cause_type = "attempt-log-lock", %e);
            AccountingError("poisoned")
        })?;
        let slot = &mut log
            .get_mut(
                attempt
                    .checked_sub(1)
                    .ok_or(AccountingError("attempt id"))? as usize,
            )
            .ok_or(AccountingError("attempt id"))?
            .1;
        if slot.is_some_and(|old| old != usage) {
            return Err(AccountingError("conflicting observation"));
        }
        *slot = Some(usage);
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
            log.observations()[0].1.unwrap().usage.input_tokens,
            Some(17)
        );
    }
    #[test]
    fn malformed_usage_fields_do_not_erase_valid_siblings_and_conflicts_refuse() {
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
        assert!(log.observe(attempt, UsageObservation::default()).is_err());
        log.begin(AttemptKind::Http).unwrap();
        assert!(log.begin(AttemptKind::Http).is_err());
    }
}
