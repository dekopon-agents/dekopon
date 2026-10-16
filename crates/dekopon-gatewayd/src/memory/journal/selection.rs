use super::{
    Journal, JournalError, JournalMemoryTools, Line, duration_millis, millis, validated_lines,
};
use crate::config::{
    MemoryWindow,
    memory::{ResolvedPolicy, RoutineKind, SelectionMode},
};
use dekopon_agent::prompt::{ConversationTurn, History, Selection};
use dekopon_agent::{ShellRuntime, prompt::ScriptRuntime};
use dekopon_shell::{
    CapabilityCallResult, CapabilityInvoker, CommandProposal, CommandRun, ExitCode, Streams,
    TreeContext,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    time::SystemTime,
};

pub(crate) struct Evidence {
    tool: String,
    outcome: String,
    exit: Option<i32>,
    age_ms: u64,
    bytes: u64,
    pinned: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct Projection<'a> {
    pub stem: &'a str,
    pub grant: &'a str,
    pub window: MemoryWindow,
    pub sealed_at: Option<SystemTime>,
    pub policy: &'a ResolvedPolicy,
}

struct Decider<'a, I> {
    inner: &'a I,
    capability: &'a str,
    input: Value,
}

impl<I: CapabilityInvoker> CapabilityInvoker for Decider<'_, I> {
    fn granted(&self) -> Vec<String> {
        self.inner.granted()
    }
    fn cancelled(&self) -> bool {
        self.inner.cancelled()
    }
    fn script_started(&self, timeout: std::time::Duration) {
        self.inner.script_started(timeout);
    }
    fn script_finished(&self) {
        self.inner.script_finished();
    }
    fn has_command_word(&self, word: &str) -> bool {
        word == "memory_decide"
    }
    fn run_command(&self, word: &str, _argv: &[String], _stdin_piped: bool) -> Option<CommandRun> {
        (word == "memory_decide").then(|| CommandRun::Proposed {
            capability: self.capability.to_owned(),
            input: self.input.clone(),
            secret_use: None,
            report: None,
        })
    }
    fn invoke(
        &self,
        proposal: CommandProposal,
        streams: Streams,
        tree: &TreeContext,
    ) -> CapabilityCallResult {
        self.inner.invoke(proposal, streams, tree)
    }
}

pub(crate) fn decide<I: CapabilityInvoker>(
    runtime: &ShellRuntime<I>,
    capability: &str,
    available: &[String],
    evidence: &[Evidence],
) -> Result<String, &'static str> {
    if !runtime.invoker.is_granted(capability) {
        return Err("not-granted");
    }
    let evidence: Vec<_> = evidence
        .iter()
        .map(|item| {
            json!({
                "tool": item.tool, "outcome": item.outcome, "exitStatus": item.exit,
                "ageMs": item.age_ms, "bytes": item.bytes, "pinned": item.pinned
            })
        })
        .collect();
    let decider = ShellRuntime {
        invoker: Decider {
            inner: &runtime.invoker,
            capability,
            input: json!({
                "available": available, "evidence": evidence
            }),
        },
        limits: runtime.limits,
        calls: runtime.calls.clone(),
    };
    let result = decider.run_script("memory_decide");
    if result.exit_code != ExitCode::SUCCESS {
        return Err("invocation-failed");
    }
    let response: Value =
        serde_json::from_str(result.output.trim()).map_err(|_malformed| "invalid-response")?;
    if response.get("refused") == Some(&Value::Bool(true)) {
        return Err("refused");
    }
    response
        .get("choice")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or("invalid-response")
}

pub(super) fn original_envelope_bytes(turn: &ConversationTurn) -> usize {
    dekopon_context::original_envelope_bytes(turn.user(), turn.answer(), turn.exchanges())
}

fn envelope(line: &Line, id: &str, version: &str, is_original: bool) -> Option<String> {
    dekopon_context::render_envelope(
        dekopon_context::EnvelopeSource {
            user: line.user.as_deref(),
            answer: line.answer.as_deref(),
            representations: &line.representations,
            exchanges: &line.exchanges,
        },
        id,
        version,
        is_original,
    )
}

fn representation<'a>(line: &'a Line, id: &str, policy: &'a ResolvedPolicy) -> Option<String> {
    let routine = policy.routines.iter().find(|routine| routine.id == id)?;
    let version = crate::memory::generation::version(routine);
    line.availability
        .iter()
        .any(|available| available == id || available.starts_with(&format!("{id}:part:")))
        .then(|| envelope(line, id, &version, routine.kind == RoutineKind::Original))
        .flatten()
}

fn evidence_for(line: &Line, now: u64) -> Evidence {
    let exchange = line.exchanges.first();
    Evidence {
        tool: exchange.map_or_else(|| "none".to_owned(), |exchange| exchange.tool.clone()),
        outcome: exchange.map_or_else(|| "none".to_owned(), |exchange| exchange.outcome.clone()),
        exit: exchange.and_then(|exchange| exchange.exit_status),
        age_ms: now.saturating_sub(line.at_ms),
        bytes: line
            .representations
            .iter()
            .map(|rep| rep.captured_bytes)
            .sum::<u64>()
            .saturating_add(
                line.exchanges
                    .iter()
                    .flat_map(|exchange| exchange.request.iter().chain(&exchange.result))
                    .map(|rep| rep.captured_bytes)
                    .sum::<u64>(),
            ),
        pinned: line.pins.iter().any(|pin| pin.record_id == line.record_id),
    }
}

fn original_id(policy: &ResolvedPolicy) -> Option<&str> {
    policy
        .routines
        .iter()
        .find(|routine| {
            routine.kind == RoutineKind::Original && policy.retain.contains(&routine.id)
        })
        .map(|routine| routine.id.as_str())
}

struct Fitted {
    history: History,
    records: VecDeque<Option<u64>>,
    selections: HashMap<u64, Selection>,
}

impl Fitted {
    fn push(&mut self, turn: ConversationTurn, record: Option<u64>) {
        let before = self.history.len();
        if self.history.record_projected(turn) {
            for _ in 0..before + 1 - self.history.len() {
                if let Some(Some(old)) = self.records.pop_front()
                    && let Some(selection) = self.selections.get_mut(&old)
                {
                    selection.actual = None;
                    selection.reason = Some("window-fit".into());
                }
            }
            self.records.push_back(record);
        }
    }
}

fn default_choice(policy: &ResolvedPolicy) -> &str {
    match policy.selection.mode {
        SelectionMode::Fixed => policy.selection.routine.as_deref(),
        SelectionMode::AgentPreference => policy
            .preferred_routine
            .as_deref()
            .or(policy.selection.routine.as_deref()),
        SelectionMode::Decider => policy.selection.fallback.as_deref(),
    }
    .unwrap_or("original")
}

fn unpersisted(
    resident: &History,
    persisted: &HashSet<u64>,
    horizon: u64,
    sealed_at: Option<SystemTime>,
) -> VecDeque<ConversationTurn> {
    let floor = persisted.iter().min().copied();
    resident
        .turns()
        .iter()
        .filter(|turn| {
            turn.memory_id().is_none_or(|id| {
                let minted_ms = id / 1_000_000;
                !persisted.contains(&id)
                    && floor.is_none_or(|floor| id > floor)
                    && minted_ms >= horizon
                    && sealed_at.is_none_or(|at| minted_ms > millis(at))
            })
        })
        .cloned()
        .collect()
}

impl Journal {
    fn horizon(&self, input: &Projection<'_>) -> u64 {
        let now = millis(SystemTime::now());
        now.saturating_sub(duration_millis(self.age_for(input.stem)))
            .max(now.saturating_sub(duration_millis(input.window.forget_after)))
    }

    fn eligible_lines(
        &self,
        input: &Projection<'_>,
        persisted: &mut HashSet<u64>,
    ) -> Result<VecDeque<(u64, Evidence, Vec<String>)>, JournalError> {
        let _lock = self.lock.lock();
        let mut lines = VecDeque::new();
        let now = millis(SystemTime::now());
        let horizon = self.horizon(input);
        match validated_lines(&self.path(input.stem), |line| {
            persisted.insert(line.turn_id);
            if line.grant != input.grant
                || line.at_ms < horizon
                || input.sealed_at.is_some_and(|at| line.at_ms <= millis(at))
            {
                lines.clear();
            } else {
                let available = input
                    .policy
                    .retain
                    .iter()
                    .filter(|id| representation(&line, id, input.policy).is_some())
                    .cloned()
                    .collect();
                lines.push_back((line.record_id, evidence_for(&line, now), available));
                if lines.len()
                    > input
                        .window
                        .limits
                        .max_turns
                        .min(input.window.limits.max_bytes.max(1))
                {
                    lines.pop_front();
                }
            }
            Ok(())
        }) {
            Err(JournalError::Io {
                kind: io::ErrorKind::NotFound,
            })
            | Ok(()) => Ok(lines),
            Err(error) => Err(error),
        }
    }

    fn persist_selections(
        &self,
        input: &Projection<'_>,
        selections: &HashMap<u64, Selection>,
        fallback: Option<&str>,
    ) {
        let Projection {
            stem,
            grant,
            policy,
            ..
        } = *input;
        let tools = JournalMemoryTools {
            journal: self,
            stem: stem.to_owned(),
            grant: grant.to_owned(),
            versions: HashMap::new(),
        };
        let _lock = self.lock.lock();
        if let Err(error) = tools.mutate(|line| {
            if line.grant != grant {
                return false;
            }
            let Some(selection) = selections.get(&line.record_id) else {
                return false;
            };
            let requested = selection.requested.as_deref().unwrap_or("none");
            let original_bytes = line.original_bytes;
            let replacement_bytes = if Some(requested) == original_id(policy) && line.exchanges.is_empty() {
                original_bytes
            } else { representation(line, requested, policy).map_or(0, |text| text.len()) };
            let pin = line.pins.iter().find(|pin| pin.record_id == line.record_id)
                .map(|pin| pin.representation_id.as_str());
            let pin_bytes = pin.and_then(|id| representation(line, id, policy)).map_or(0, |text| text.len());
            tracing::info!(event = "gateway_memory_selection", record_id = line.record_id,
                requested, actual = selection.actual.as_deref().unwrap_or("omitted"),
                original_bytes, replacement_bytes,
                savings_bytes = i64::try_from(original_bytes).unwrap_or(i64::MAX)
                    - i64::try_from(replacement_bytes).unwrap_or(i64::MAX),
                reason = selection.reason.as_deref().unwrap_or("none"),
                pin = pin.unwrap_or("none"), pin_bytes, pinned = pin.is_some(),
                authority = selection.authority.as_str(), fallback_reason = fallback.unwrap_or("none"),
                policy = ?policy.selection.mode);
            line.selections = vec![selection.clone()];
            true
        }) {
            tracing::warn!(
                event = "gateway_memory_retention_failed",
                reason = "selection-persistence",
                category = error.label()
            );
        }
    }

    pub(crate) fn project(
        &self,
        input: Projection<'_>,
        resident: &History,
        mut decide: impl FnMut(&[String], &[Evidence]) -> Result<String, &'static str>,
    ) -> Result<History, JournalError> {
        let mut persisted = HashSet::new();
        let lines = self.eligible_lines(&input, &mut persisted)?;
        let Projection {
            stem,
            grant,
            window,
            policy,
            sealed_at,
        } = input;
        let horizon = self.horizon(&input);
        let mut unpersisted = unpersisted(resident, &persisted, horizon, sealed_at);
        let mut fitted = Fitted {
            history: History::new(window.limits),
            records: VecDeque::new(),
            selections: HashMap::new(),
        };
        if lines.is_empty() {
            unpersisted
                .into_iter()
                .for_each(|turn| fitted.push(turn, None));
            return Ok(fitted.history);
        }
        let selection = &policy.selection;
        let original_id = original_id(policy);
        let default = default_choice(policy);
        let available: Vec<String> = policy
            .retain
            .iter()
            .filter(|id| lines.iter().any(|(_, _, ids)| ids.contains(id)))
            .cloned()
            .collect();
        let first = lines.front().map(|row| row.0).unwrap_or(0);
        let last = lines.back().map(|row| row.0).unwrap_or(0);
        let evidence: Vec<_> = lines.into_iter().map(|(_, evidence, _)| evidence).collect();
        let (choice, authority, fallback_reason) = match selection.mode {
            SelectionMode::Fixed => (default.to_owned(), "fixed", None),
            SelectionMode::AgentPreference => (default.to_owned(), "agentPreference", None),
            SelectionMode::Decider => match decide(&available, &evidence) {
                Ok(choice) if available.contains(&choice) => (choice, "decider", None),
                Ok(_) => (default.to_owned(), "fallback", Some("invalid-choice")),
                Err(reason) => (default.to_owned(), "fallback", Some(reason)),
            },
        };
        let _lock = self.lock.lock();
        validated_lines(&self.path(stem), |line| {
            if line.record_id < first
                || line.record_id > last
                || line.grant != grant
                || line.at_ms < horizon
                || sealed_at.is_some_and(|at| line.at_ms <= millis(at))
            {
                return Ok(());
            }
            while let Some(turn) =
                unpersisted.pop_front_if(|turn| turn.memory_id().unwrap_or(u64::MAX) < line.turn_id)
            {
                fitted.push(turn, None);
            }
            let pin = line
                .pins
                .iter()
                .find(|pin| pin.record_id == line.record_id)
                .map(|pin| pin.representation_id.as_str());
            let requested = if authority == "decider" {
                choice.as_str()
            } else {
                pin.unwrap_or(&choice)
            };
            let original = original_id.and_then(|id| representation(&line, id, policy));
            let replacement = representation(&line, requested, policy);
            let original_bytes = if original.is_some() && line.exchanges.is_empty() {
                line.user.as_ref().map_or(0, String::len)
                    + line.answer.as_ref().map_or(0, String::len)
            } else {
                original.as_ref().map_or(line.original_bytes, String::len)
            };
            let replacement_bytes = replacement.as_ref().map_or(0, String::len);
            let savings = i64::try_from(original_bytes).unwrap_or(i64::MAX)
                - i64::try_from(replacement_bytes).unwrap_or(i64::MAX);
            let automatic = authority == "fixed" || authority == "fallback";
            let (actual, reason) = if replacement.is_none() {
                (original_id.unwrap_or("original"), "unavailable")
            } else if Some(requested) != original_id
                && automatic
                && pin.is_none()
                && original.is_none()
            {
                (requested, "kept-replacement: original-unavailable")
            } else if Some(requested) != original_id && automatic && pin.is_none() && savings <= 0 {
                (
                    original_id.unwrap_or("original"),
                    "kept-original: no-saving",
                )
            } else {
                (requested, fallback_reason.unwrap_or("selected"))
            };
            let sent = if Some(actual) == original_id {
                original
            } else {
                replacement
            };
            let sent_bytes = if Some(actual) == original_id && line.exchanges.is_empty() {
                original_bytes
            } else {
                sent.as_ref().map_or(0, String::len)
            };
            let included = sent
                .as_ref()
                .is_some_and(|_| sent_bytes <= window.limits.max_bytes);
            let reason = if sent.is_none() {
                "representation-unavailable"
            } else if !included {
                "record-exceeds-budget"
            } else if pin.is_some_and(|pin| pin != actual) && authority == "decider" {
                "decider-overrode-pin"
            } else {
                reason
            };
            fitted.selections.insert(
                line.record_id,
                Selection {
                    requested: Some(requested.to_owned()),
                    available: available.clone(),
                    actual: included.then(|| actual.to_owned()),
                    authority: authority.to_owned(),
                    reason: Some(reason.to_owned()),
                },
            );
            if let Some(text) = sent.filter(|_| included) {
                let turn = if Some(actual) == original_id
                    && line.exchanges.is_empty()
                    && let Some(user) = line.user
                {
                    match line.answer {
                        Some(answer) => ConversationTurn::completed(user, answer),
                        None => ConversationTurn::unanswered(user),
                    }
                } else {
                    ConversationTurn::unanswered(text)
                };
                fitted.push(turn, Some(line.record_id));
            }
            Ok(())
        })?;
        drop(_lock);
        unpersisted
            .into_iter()
            .for_each(|turn| fitted.push(turn, None));
        self.persist_selections(&input, &fitted.selections, fallback_reason);
        Ok(fitted.history)
    }
}
