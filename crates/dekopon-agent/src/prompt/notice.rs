#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OmissionReason {
    WindowFit,
    RecordExceedsBudget,
    RepresentationUnavailable,
}

impl OmissionReason {
    pub const fn label(self) -> &'static str {
        match self {
            Self::WindowFit => "window-fit",
            Self::RecordExceedsBudget => "record-exceeds-budget",
            Self::RepresentationUnavailable => "representation-unavailable",
        }
    }
}

#[derive(Debug)]
pub enum MemoryFact {
    WindowOmission {
        first_shown: u64,
        limits: super::HistoryLimits,
    },
    Omitted {
        record: u64,
        reason: OmissionReason,
    },
    Replaced {
        record: u64,
        omitted_bytes: u64,
    },
    GrantBoundary,
    ThreadWithoutMemory,
    RecallFailed {
        reason: &'static str,
    },
    Fidelity,
}

impl MemoryFact {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::WindowOmission { .. } => "window-omission",
            Self::Omitted { .. } => "omitted",
            Self::Replaced { .. } => "replaced",
            Self::GrantBoundary => "grant-boundary",
            Self::ThreadWithoutMemory => "thread-without-memory",
            Self::RecallFailed { .. } => "recall-failed",
            Self::Fidelity => "fidelity",
        }
    }
}

pub struct MemoryNotice {
    text: String,
    kinds: Vec<&'static str>,
    first_shown: Option<u64>,
    omitted: u64,
}

impl MemoryNotice {
    pub fn new(facts: &[MemoryFact], first_shown: Option<u64>) -> Option<Self> {
        if facts.is_empty() {
            return None;
        }
        let mut notice = Self {
            text: "[gateway: memory]\n".into(),
            kinds: Vec::new(),
            first_shown,
            omitted: 0,
        };
        let mut omitted = Vec::new();
        let mut replaced = Vec::new();
        for fact in facts {
            if !notice.kinds.contains(&fact.label()) {
                notice.kinds.push(fact.label());
            }
            let line = match fact {
                MemoryFact::WindowOmission { first_shown, limits } => {
                    notice.first_shown = Some(*first_shown);
                    format!("Turns before record {first_shown} are not shown: maxTurns={}, maxBytes={}. read_memory_record reads eligible records by id.\n", limits.max_turns, limits.max_bytes)
                }
                MemoryFact::Omitted { record, reason } => {
                    omitted.push((*record, *reason));
                    continue;
                }
                MemoryFact::Replaced { record, omitted_bytes } => {
                    replaced.push((*record, *omitted_bytes));
                    continue;
                }
                MemoryFact::GrantBoundary => "Earlier turns were recorded under a different capability grant and are not shown.\n".into(),
                MemoryFact::ThreadWithoutMemory => "This message replies in a thread with earlier messages; none are in your memory.\n".into(),
                MemoryFact::RecallFailed { reason } => format!("Memory recall or projection failed this turn ({reason}).\n"),
                MemoryFact::Fidelity => "Replayed turns keep the request, your final reply and bash script exchanges; reasoning and other tool calls are not kept.\n".into(),
            };
            notice.text.push_str(&line);
        }
        notice.omitted = omitted.len() as u64;
        for reason in [
            OmissionReason::WindowFit,
            OmissionReason::RecordExceedsBudget,
            OmissionReason::RepresentationUnavailable,
        ] {
            let ids: Vec<_> = omitted
                .iter()
                .filter(|(_, why)| *why == reason)
                .map(|(id, _)| id.to_string())
                .collect();
            if !ids.is_empty() {
                notice.text.push_str(&format!(
                    "Omitted {}: {}.\n",
                    reason.label(),
                    Self::ids(&ids)
                ));
            }
        }
        if !replaced.is_empty() {
            let ids: Vec<_> = replaced
                .iter()
                .map(|(id, bytes)| format!("{id} ({bytes} bytes omitted)"))
                .collect();
            let bytes = replaced
                .iter()
                .fold(0_u64, |total, (_, bytes)| total.saturating_add(*bytes));
            notice.text.push_str(&format!(
                "Replaced with non-original representations: {}; total {bytes} bytes omitted.\n",
                Self::ids(&ids)
            ));
        }
        if notice.text.len() > Self::MAX_BYTES {
            tracing::warn!(
                event = "gateway_memory_notice_failed",
                reason = "size",
                "gateway.memory.notice.failed"
            );
            return None;
        }
        Some(notice)
    }

    const MAX_BYTES: usize = 1024;

    fn ids(ids: &[String]) -> String {
        if ids.len() > 2 {
            format!("{} records", ids.len())
        } else {
            ids.join(", ")
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn kinds(&self) -> &[&'static str] {
        &self.kinds
    }
    pub const fn first_shown(&self) -> Option<u64> {
        self.first_shown
    }
    pub const fn omitted(&self) -> u64 {
        self.omitted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_record_lists_collapse_without_losing_any_fact_kind() {
        let mut facts = vec![
            MemoryFact::WindowOmission {
                first_shown: u64::MAX,
                limits: super::super::HistoryLimits {
                    max_turns: usize::MAX,
                    max_bytes: usize::MAX,
                },
            },
            MemoryFact::GrantBoundary,
            MemoryFact::ThreadWithoutMemory,
            MemoryFact::RecallFailed { reason: "corrupt" },
            MemoryFact::Fidelity,
        ];
        for record in 1..1000 {
            for reason in [
                OmissionReason::WindowFit,
                OmissionReason::RecordExceedsBudget,
                OmissionReason::RepresentationUnavailable,
            ] {
                facts.push(MemoryFact::Omitted { record, reason });
            }
            facts.push(MemoryFact::Replaced {
                record,
                omitted_bytes: u64::MAX,
            });
        }
        let notice = MemoryNotice::new(&facts, None).expect("all facts fit");
        assert!(notice.text().len() <= MemoryNotice::MAX_BYTES);
        assert_eq!(notice.kinds().len(), 7);
        assert_eq!(notice.omitted(), 2997);
        assert!(notice.text().contains("999 records"));
        for reason in [
            OmissionReason::WindowFit,
            OmissionReason::RecordExceedsBudget,
            OmissionReason::RepresentationUnavailable,
        ] {
            assert!(notice.text().contains(reason.label()));
        }
    }
}
