// These streaks stand in for Alertmanager or OpenObserve silences.

use std::collections::{BTreeMap, btree_map::Entry};

use serde::{Deserialize, Deserializer, Serialize, de};

const MAX_SUBJECTS: usize = 64;
const MAX_CAUSES: usize = 8;

#[derive(Debug, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Streaks<K> {
    #[serde(
        deserialize_with = "deserialize_active",
        bound(deserialize = "K: Ord + Deserialize<'de>")
    )]
    active: BTreeMap<K, Recovery>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Notice;

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Recovery {
    pub failures: u64,
    pub causes: Vec<String>,
}

impl<K> Default for Streaks<K> {
    fn default() -> Self {
        Self {
            active: BTreeMap::new(),
        }
    }
}

impl<K: Ord> Streaks<K> {
    #[must_use]
    pub fn failure(&mut self, subject: K, cause: &str) -> Option<Notice> {
        let subjects_full = self.active.len() >= MAX_SUBJECTS;
        match self.active.entry(subject) {
            Entry::Occupied(mut entry) => {
                let streak = entry.get_mut();
                streak.failures = streak.failures.saturating_add(1);
                if streak.causes.len() < MAX_CAUSES
                    && !streak.causes.iter().any(|seen| seen == cause)
                {
                    streak.causes.push(cause.to_owned());
                }
                None
            }
            Entry::Vacant(_) if subjects_full => None,
            Entry::Vacant(entry) => {
                entry.insert(Recovery {
                    failures: 1,
                    causes: vec![cause.to_owned()],
                });
                Some(Notice)
            }
        }
    }

    #[must_use]
    pub fn success(&mut self, subject: &K) -> Option<Recovery> {
        self.active.remove(subject)
    }
}

fn deserialize_active<'de, D, K>(deserializer: D) -> Result<BTreeMap<K, Recovery>, D::Error>
where
    D: Deserializer<'de>,
    K: Ord + Deserialize<'de>,
{
    let active = BTreeMap::<K, Recovery>::deserialize(deserializer)?;
    if active.len() > MAX_SUBJECTS {
        return Err(de::Error::custom("too many active streaks"));
    }
    if active
        .values()
        .any(|streak| streak.causes.len() > MAX_CAUSES)
    {
        return Err(de::Error::custom("too many causes in an active streak"));
    }
    Ok(active)
}

#[cfg(test)]
mod tests {
    use super::{MAX_CAUSES, MAX_SUBJECTS, Notice, Recovery, Streaks};

    #[test]
    fn opening_a_streak_returns_a_notice() {
        let mut streaks = Streaks::default();
        assert_eq!(streaks.failure("worker", "timeout"), Some(Notice));
    }

    #[test]
    fn a_repeat_counts_without_a_notice_or_duplicate_cause() {
        let mut streaks = Streaks::default();
        streaks
            .failure("worker", "timeout")
            .expect("opening notice");
        assert_eq!(streaks.failure("worker", "timeout"), None);
        assert_eq!(
            streaks.success(&"worker"),
            Some(Recovery {
                failures: 2,
                causes: vec!["timeout".into()],
            })
        );
    }

    #[test]
    fn closing_a_streak_returns_its_failure_count_and_causes_in_order() {
        let mut streaks = Streaks::default();
        streaks
            .failure("worker", "timeout")
            .expect("opening notice");
        streaks
            .failure("gateway", "restart")
            .expect("opening notice");
        assert_eq!(streaks.failure("worker", "connection refused"), None);
        assert_eq!(
            streaks.success(&"worker"),
            Some(Recovery {
                failures: 2,
                causes: vec!["timeout".into(), "connection refused".into()],
            })
        );
        assert_eq!(
            streaks.success(&"gateway"),
            Some(Recovery {
                failures: 1,
                causes: vec!["restart".into()],
            })
        );
    }

    #[test]
    fn closing_clears_the_streak_and_reopening_returns_a_notice() {
        let mut streaks = Streaks::default();
        streaks
            .failure("worker", "timeout")
            .expect("opening notice");
        streaks.success(&"worker").expect("recovery");
        assert_eq!(streaks.success(&"worker"), None);
        assert_eq!(streaks.failure("worker", "restart"), Some(Notice));
        assert_eq!(
            streaks.success(&"worker"),
            Some(Recovery {
                failures: 1,
                causes: vec!["restart".into()],
            })
        );
    }

    fn streaks_at_capacity() -> Streaks<usize> {
        let mut streaks = Streaks::default();
        for subject in 0..MAX_SUBJECTS {
            streaks.failure(subject, "0").expect("opening notice");
        }
        for cause in 1..MAX_CAUSES {
            assert_eq!(streaks.failure(0, &cause.to_string()), None);
        }
        streaks
    }

    #[test]
    fn capacity_preserves_active_subjects_and_first_causes() {
        let mut streaks = streaks_at_capacity();
        let new_subject = MAX_SUBJECTS;
        assert_eq!(streaks.failure(new_subject, "restart"), None);
        assert_eq!(streaks.success(&new_subject), None);
        assert_eq!(streaks.failure(0, "another cause"), None);
        let recovery = streaks.success(&0).expect("active streak was retained");
        assert_eq!(recovery.failures, MAX_CAUSES as u64 + 1);
        assert_eq!(
            recovery.causes,
            (0..MAX_CAUSES)
                .map(|cause| cause.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(streaks.failure(new_subject, "restart"), Some(Notice));
    }

    #[test]
    fn serialization_at_capacity_preserves_the_continuation_of_active_streaks() {
        let mut original = streaks_at_capacity();
        let encoded = serde_json::to_string(&original).expect("serialize active streaks");
        let mut restored: Streaks<usize> =
            serde_json::from_str(&encoded).expect("restore active streaks");

        for subject in 0..=MAX_SUBJECTS {
            assert_eq!(
                restored.failure(subject, "restart"),
                original.failure(subject, "restart")
            );
        }
        for subject in 0..=MAX_SUBJECTS {
            assert_eq!(restored.success(&subject), original.success(&subject));
        }
    }

    #[test]
    fn deserialization_refuses_state_over_either_cap() {
        let mut streaks = streaks_at_capacity();
        streaks
            .active
            .get_mut(&0)
            .expect("active streak")
            .causes
            .push("another cause".into());
        let encoded = serde_json::to_string(&streaks).expect("serialize oversized cause list");
        assert!(serde_json::from_str::<Streaks<usize>>(&encoded).is_err());

        let mut streaks = streaks_at_capacity();
        let mut extra = Streaks::default();
        extra
            .failure(MAX_SUBJECTS, "restart")
            .expect("opening notice");
        streaks.active.append(&mut extra.active);
        let encoded = serde_json::to_string(&streaks).expect("serialize oversized subject map");
        assert!(serde_json::from_str::<Streaks<usize>>(&encoded).is_err());
    }
}
