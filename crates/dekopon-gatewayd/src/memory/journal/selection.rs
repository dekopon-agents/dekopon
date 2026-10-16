use super::{Backward, Journal, JournalError, Line, body, duration_millis, millis};
use crate::config::{
    MemoryWindow,
    memory::{ResolvedPolicy, RoutineKind, SelectionMode},
};
use dekopon_agent::prompt::{ConversationTurn, History, Representation};
use dekopon_agent::{ShellRuntime, prompt::ScriptRuntime};
use dekopon_shell::{
    CapabilityCallResult, CapabilityInvoker, CommandProposal, CommandRun, ExitCode, Streams,
    TreeContext,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
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

fn text(line: &Line, id: &str, policy: &ResolvedPolicy) -> Option<String> {
    let routine = policy.routines.iter().find(|routine| routine.id == id)?;
    body(line, id, &crate::memory::generation::version(routine))
}

fn eligible_ids(line: &Line, policy: &ResolvedPolicy) -> Vec<String> {
    policy
        .retain
        .iter()
        .filter(|id| text(line, id, policy).is_some())
        .cloned()
        .collect()
}

fn evidence_for(line: &Line, now: u64) -> Evidence {
    let exchange = line.exchanges.first();
    Evidence {
        tool: exchange.map_or_else(|| "none".to_owned(), |exchange| exchange.tool.clone()),
        outcome: exchange.map_or_else(|| "none".to_owned(), |exchange| exchange.outcome.clone()),
        exit: exchange.and_then(|exchange| exchange.exit_status),
        age_ms: now.saturating_sub(line.at_ms),
        bytes: line.original_bytes,
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

struct Chosen {
    requested: String,
    actual: Option<String>,
    reason: String,
    original_bytes: u64,
    replacement_bytes: u64,
    pin: Option<String>,
    pin_bytes: usize,
}

struct Fitted {
    history: History,
    records: VecDeque<Option<u64>>,
    chosen: BTreeMap<u64, Chosen>,
}

impl Fitted {
    fn push(&mut self, turn: ConversationTurn, record: Option<u64>) {
        let before = self.history.len();
        if self.history.record_projected(turn) {
            for _ in 0..before + 1 - self.history.len() {
                if let Some(Some(old)) = self.records.pop_front()
                    && let Some(chosen) = self.chosen.get_mut(&old)
                {
                    chosen.actual = None;
                    chosen.reason = "window-fit".to_owned();
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

struct Choice<'a> {
    routine: &'a str,
    authority: &'static str,
    fallback_reason: Option<&'static str>,
}

fn envelope(
    line: &Line,
    id: &str,
    text: &str,
    original_id: Option<&str>,
    eligible: &[String],
) -> ConversationTurn {
    let label = format!(
        "[memory: {}]\n",
        json!({
            "recordId": line.record_id,
            "representationId": id,
            "eligibleRepresentationIds": eligible,
            "omittedBytes": line
                .representations
                .iter()
                .find(|rep| rep.routine_id == id && rep.text.is_some())
                .map_or(0, Representation::omitted_bytes),
        })
    );
    if Some(id) == original_id
        && line.exchanges.is_empty()
        && let Some(user) = &line.user
    {
        let user = format!("{label}{user}");
        match &line.answer {
            Some(answer) => ConversationTurn::completed(user, answer.clone()),
            None => ConversationTurn::unanswered(user),
        }
    } else {
        ConversationTurn::unanswered(format!("{label}{text}"))
    }
}

fn choose(
    line: Line,
    choice: &Choice<'_>,
    policy: &ResolvedPolicy,
    max_bytes: usize,
) -> (Chosen, Option<ConversationTurn>) {
    let Choice {
        routine: choice,
        authority,
        fallback_reason,
    } = *choice;
    let original_id = original_id(policy);
    let pin = line
        .pins
        .iter()
        .find(|pin| pin.record_id == line.record_id)
        .map(|pin| pin.representation_id.clone());
    let requested = if authority == "decider" {
        choice
    } else {
        pin.as_deref().unwrap_or(choice)
    };
    let eligible = eligible_ids(&line, policy);
    let wrap = |id: &str, text: String| envelope(&line, id, &text, original_id, &eligible);
    let original = original_id.and_then(|id| text(&line, id, policy).map(|text| wrap(id, text)));
    let mut target = requested;
    let mut replacement = text(&line, requested, policy).map(|text| wrap(requested, text));
    if replacement.is_none()
        && authority == "decider"
        && let Some(fallback) = policy.selection.fallback.as_deref()
    {
        target = fallback;
        replacement = text(&line, fallback, policy).map(|text| wrap(fallback, text));
    }
    let original_bytes = original
        .as_ref()
        .map_or(line.original_bytes, |turn| turn.bytes() as u64);
    let replacement_bytes = replacement.as_ref().map_or(0, |turn| turn.bytes() as u64);
    let savings = i128::from(original_bytes) - i128::from(replacement_bytes);
    let automatic = authority == "fixed" || authority == "fallback";
    let (actual, reason) = if replacement.is_none() {
        (original_id.unwrap_or("original"), "unavailable")
    } else if Some(target) != original_id && automatic && pin.is_none() && original.is_none() {
        (target, "kept-replacement: original-unavailable")
    } else if Some(target) != original_id && automatic && pin.is_none() && savings <= 0 {
        (
            original_id.unwrap_or("original"),
            "kept-original: no-saving",
        )
    } else if target != requested {
        (target, "fallback: choice-unavailable")
    } else {
        (target, fallback_reason.unwrap_or("selected"))
    };
    let pin_bytes = pin
        .as_deref()
        .and_then(|id| text(&line, id, policy).map(|text| wrap(id, text).bytes()))
        .unwrap_or(0);
    let turn = if Some(actual) == original_id {
        original
    } else {
        replacement
    };
    let included = turn.as_ref().is_some_and(|turn| turn.bytes() <= max_bytes);
    let reason = if turn.is_none() {
        "representation-unavailable"
    } else if !included {
        "record-exceeds-budget"
    } else if pin.as_deref().is_some_and(|pin| pin != actual) && authority == "decider" {
        "decider-overrode-pin"
    } else {
        reason
    };
    (
        Chosen {
            requested: requested.to_owned(),
            actual: included.then(|| actual.to_owned()),
            reason: reason.to_owned(),
            original_bytes,
            replacement_bytes,
            pin,
            pin_bytes,
        },
        turn.filter(|_| included),
    )
}

impl Journal {
    fn horizon(&self, input: &Projection<'_>) -> u64 {
        let now = millis(SystemTime::now());
        now.saturating_sub(duration_millis(self.age_for(input.stem)))
            .max(now.saturating_sub(duration_millis(input.window.forget_after)))
    }

    fn tail(
        &self,
        input: &Projection<'_>,
        mut visit: impl FnMut(Line, bool) -> bool,
    ) -> Result<(), JournalError> {
        let horizon = self.horizon(input);
        let _lock = self.lock.lock();
        let mut lines = match Backward::open(&self.path(input.stem)) {
            Ok(lines) => lines,
            Err(JournalError::Io {
                kind: io::ErrorKind::NotFound,
            }) => return Ok(()),
            Err(error) => return Err(error),
        };
        while let Some(line) = lines.next_line()? {
            let eligible = line.grant == input.grant
                && line.at_ms >= horizon
                && input.sealed_at.is_none_or(|at| line.at_ms > millis(at));
            if !visit(line, eligible) || !eligible {
                break;
            }
        }
        Ok(())
    }

    pub(crate) fn project(
        &self,
        input: Projection<'_>,
        resident: &History,
        mut decide: impl FnMut(&[String], &[Evidence]) -> Result<String, &'static str>,
    ) -> Result<History, JournalError> {
        let Projection {
            window,
            policy,
            sealed_at,
            ..
        } = input;
        let horizon = self.horizon(&input);
        let limit = window.limits.max_turns.min(window.limits.max_bytes.max(1));
        let now = millis(SystemTime::now());
        let mut persisted = HashSet::new();
        let mut rows = Vec::new();
        self.tail(&input, |line, eligible| {
            persisted.insert(line.turn_id);
            if eligible {
                rows.push((
                    line.record_id,
                    evidence_for(&line, now),
                    eligible_ids(&line, policy),
                ));
            }
            rows.len() < limit
        })?;
        let mut unpersisted = unpersisted(resident, &persisted, horizon, sealed_at);
        let mut fitted = Fitted {
            history: History::new(window.limits),
            records: VecDeque::new(),
            chosen: BTreeMap::new(),
        };
        let Some(first) = rows.last().map(|row| row.0) else {
            unpersisted
                .into_iter()
                .for_each(|turn| fitted.push(turn, None));
            return Ok(fitted.history);
        };
        rows.reverse();
        let available: Vec<String> = policy
            .retain
            .iter()
            .filter(|id| rows.iter().any(|(_, _, ids)| ids.contains(id)))
            .cloned()
            .collect();
        let last = rows.last().map_or(first, |row| row.0);
        let evidence: Vec<_> = rows.into_iter().map(|(_, evidence, _)| evidence).collect();
        let default = default_choice(policy);
        let (routine, authority, fallback_reason) = match policy.selection.mode {
            SelectionMode::Fixed => (default.to_owned(), "fixed", None),
            SelectionMode::AgentPreference => (default.to_owned(), "agentPreference", None),
            SelectionMode::Decider => match decide(&available, &evidence) {
                Ok(choice) if available.contains(&choice) => (choice, "decider", None),
                Ok(_) => (default.to_owned(), "fallback", Some("invalid-choice")),
                Err(reason) => (default.to_owned(), "fallback", Some(reason)),
            },
        };
        let choice = Choice {
            routine: &routine,
            authority,
            fallback_reason,
        };
        let mut projected = Vec::new();
        self.tail(&input, |line, _eligible| {
            if line.record_id < first {
                return false;
            }
            if line.record_id <= last {
                let (record_id, turn_id) = (line.record_id, line.turn_id);
                let (chosen, turn) = choose(line, &choice, policy, window.limits.max_bytes);
                projected.push((record_id, turn_id, chosen, turn));
            }
            true
        })?;
        for (record_id, turn_id, chosen, turn) in projected.into_iter().rev() {
            while let Some(resident) =
                unpersisted.pop_front_if(|turn| turn.memory_id().unwrap_or(u64::MAX) < turn_id)
            {
                fitted.push(resident, None);
            }
            fitted.chosen.insert(record_id, chosen);
            if let Some(turn) = turn {
                fitted.push(turn, Some(record_id));
            }
        }
        unpersisted
            .into_iter()
            .for_each(|turn| fitted.push(turn, None));
        let mode = match policy.selection.mode {
            SelectionMode::Fixed => "fixed",
            SelectionMode::AgentPreference => "agentPreference",
            SelectionMode::Decider => "decider",
        };
        for (record_id, chosen) in &fitted.chosen {
            tracing::info!(
                event = "gateway_memory_selection",
                record_id,
                requested = chosen.requested,
                actual = chosen.actual.as_deref().unwrap_or("omitted"),
                original_bytes = chosen.original_bytes,
                replacement_bytes = chosen.replacement_bytes,
                savings_bytes = i64::try_from(
                    i128::from(chosen.original_bytes) - i128::from(chosen.replacement_bytes)
                )
                .unwrap_or(i64::MIN),
                reason = chosen.reason,
                pin = chosen.pin.as_deref().unwrap_or("none"),
                pin_bytes = chosen.pin_bytes,
                pinned = chosen.pin.is_some(),
                authority,
                fallback_reason = fallback_reason.unwrap_or("none"),
                policy = mode,
            );
        }
        Ok(fitted.history)
    }
}
