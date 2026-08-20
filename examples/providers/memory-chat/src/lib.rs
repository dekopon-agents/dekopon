use dekopon_provider_sdk::{
    CapabilityId, CommandInvocation, EffectKind, Idempotency, Provider, ProviderApiVersion,
    ProviderCapability, ProviderError, ProviderManifest, RiskLevel,
};
use dekopon_provider_storage::jsonl::{self, StorageError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const TURNS: &str = "turns.jsonl";
const DEDUP: &str = "dedup.jsonl";
const CHUNK: u32 = 256 * 1024;
const USAGE: &str = "usage: memory recent --last N | memory search --query TEXT";

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "provider",
        generate_all,
        pub_export_macro: true,
    });
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Turn {
    format: String,
    version: u8,
    id: String,
    commitment: String,
    user: String,
    assistant: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Dedup {
    format: String,
    version: u8,
    id: String,
    commitment: String,
}

#[derive(Deserialize)]
#[serde(
    tag = "operation",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum Input {
    Record {
        id: String,
        commitment: String,
        user: String,
        assistant: String,
        max_turn_bytes: u64,
        max_lookback_turns: u32,
        max_dedup_records: u64,
        max_dedup_bytes: u64,
        compaction_target_bytes: u64,
        compaction_threshold_bytes: u64,
    },
    Recent {
        last: u64,
        max_lookback_turns: u32,
        max_recent_turns: u32,
        max_result_bytes: u64,
    },
    Search {
        query: String,
        max_lookback_turns: u32,
        max_search_results: u32,
        max_result_bytes: u64,
    },
}

struct MemoryChat;

impl Provider for MemoryChat {
    fn manifest() -> ProviderManifest {
        ProviderManifest {
            api_version: ProviderApiVersion::V1Alpha1,
            id: "memory-chat".parse().expect("static provider ID"),
            description: "Durable, on-demand, namespace-isolated chat memory".to_owned(),
            command_words: vec!["memory".to_owned()],
            capabilities: vec![
                capability(
                    "memory.chat.record",
                    "Records one gateway-attested transport-accepted turn",
                    EffectKind::LocalWrite,
                    RiskLevel::Medium,
                    Idempotency::Conditional,
                    json!({"type":"object","additionalProperties":false}),
                ),
                capability(
                    "memory.chat.recent",
                    "Returns recent durable turns in chronological order",
                    EffectKind::ReadOnly,
                    RiskLevel::High,
                    Idempotency::Idempotent,
                    json!({
                        "type":"object",
                        "properties":{"last":{"type":"integer","minimum":1}},
                        "required":["last"],"additionalProperties":false
                    }),
                ),
                capability(
                    "memory.chat.search",
                    "Searches recent durable turns with literal case-insensitive matching",
                    EffectKind::ReadOnly,
                    RiskLevel::High,
                    Idempotency::Idempotent,
                    json!({
                        "type":"object",
                        "properties":{"query":{"type":"string","minLength":1}},
                        "required":["query"],"additionalProperties":false
                    }),
                ),
            ],
        }
    }

    fn invoke(capability: &CapabilityId, input: Value) -> Result<Value, ProviderError> {
        let input: Input = serde_json::from_value(input).map_err(|_| invalid())?;
        match (capability.as_str(), input) {
            (
                "memory.chat.record",
                Input::Record {
                    id,
                    commitment,
                    user,
                    assistant,
                    max_turn_bytes,
                    max_lookback_turns,
                    max_dedup_records,
                    max_dedup_bytes,
                    compaction_target_bytes,
                    compaction_threshold_bytes,
                },
            ) => record(RecordLimits {
                id,
                commitment,
                user,
                assistant,
                max_turn_bytes,
                max_lookback_turns,
                max_dedup_records,
                max_dedup_bytes,
                compaction_target_bytes,
                compaction_threshold_bytes,
            }),
            (
                "memory.chat.recent",
                Input::Recent {
                    last,
                    max_lookback_turns,
                    max_recent_turns,
                    max_result_bytes,
                },
            ) => {
                if last == 0 || last > u64::from(max_recent_turns) {
                    return Err(invalid());
                }
                let turns = read_turns_tail(max_lookback_turns as usize)?;
                bounded_result(&turns, last as usize, max_result_bytes)
            }
            (
                "memory.chat.search",
                Input::Search {
                    query,
                    max_lookback_turns,
                    max_search_results,
                    max_result_bytes,
                },
            ) => {
                if query.is_empty() {
                    return Err(invalid());
                }
                let query = query.to_lowercase();
                let turns = read_turns_tail(max_lookback_turns as usize)?;
                let matched = turns
                    .iter()
                    .filter(|turn| {
                        turn.user.to_lowercase().contains(&query)
                            || turn.assistant.to_lowercase().contains(&query)
                    })
                    .collect::<Vec<_>>();
                bounded_refs(&matched, max_search_results as usize, max_result_bytes)
            }
            _ => Err(ProviderError::new(
                "unknown-capability",
                "unsupported memory operation",
            )),
        }
    }

    fn resolve_command(argv: &[String]) -> Result<CommandInvocation, ProviderError> {
        match argv {
            [operation, flag, last] if operation == "recent" && flag == "--last" => {
                let last = last
                    .parse::<u64>()
                    .ok()
                    .filter(|value| *value > 0)
                    .ok_or_else(usage)?;
                Ok(CommandInvocation {
                    capability: "memory.chat.recent".parse().expect("static capability"),
                    input: json!({"last": last}),
                })
            }
            [operation, flag, query]
                if operation == "search" && flag == "--query" && !query.is_empty() =>
            {
                Ok(CommandInvocation {
                    capability: "memory.chat.search".parse().expect("static capability"),
                    input: json!({"query": query}),
                })
            }
            _ => Err(usage()),
        }
    }
}

fn capability(
    id: &str,
    description: &str,
    effect: EffectKind,
    risk: RiskLevel,
    idempotency: Idempotency,
    input_schema: Value,
) -> ProviderCapability {
    ProviderCapability {
        id: id.parse().expect("static capability"),
        description: description.to_owned(),
        effect,
        risk,
        idempotency,
        input_schema,
    }
}

struct RecordLimits {
    id: String,
    commitment: String,
    user: String,
    assistant: String,
    max_turn_bytes: u64,
    max_lookback_turns: u32,
    max_dedup_records: u64,
    max_dedup_bytes: u64,
    compaction_target_bytes: u64,
    compaction_threshold_bytes: u64,
}

fn record(input: RecordLimits) -> Result<Value, ProviderError> {
    let (dedup_size, dedup_bytes) = read_file(DEDUP)?;
    let entries = parse_lines::<Dedup>(&dedup_bytes, "dekopon.chat-memory.dedup")?;
    if let Some(existing) = entries.iter().find(|entry| entry.id == input.id) {
        if existing.commitment == input.commitment {
            return Ok(json!({"recorded":false,"duplicate":true}));
        }
        return Err(ProviderError::new(
            "dedup-conflict",
            "record identity conflicts",
        ));
    }
    if entries.len() as u64 >= input.max_dedup_records {
        return Err(ProviderError::new(
            "dedup-capacity",
            "deduplication capacity reached",
        ));
    }

    let turn = Turn {
        format: "dekopon.chat-memory.turn".to_owned(),
        version: 1,
        id: input.id.clone(),
        commitment: input.commitment.clone(),
        user: input.user,
        assistant: input.assistant,
    };
    let turn_line = serde_json::to_vec(&turn).map_err(|_| corrupt())?;
    let canonical_turn_bytes = (turn_line.len() as u64)
        .checked_add(1)
        .ok_or_else(corrupt)?;
    if canonical_turn_bytes > input.max_turn_bytes {
        return Err(ProviderError::new(
            "result-too-large",
            "turn exceeds configured canonical line bound",
        ));
    }
    let dedup = Dedup {
        format: "dekopon.chat-memory.dedup".to_owned(),
        version: 1,
        id: input.id,
        commitment: input.commitment,
    };
    let dedup_line = serde_json::to_vec(&dedup).map_err(|_| corrupt())?;
    if dedup_size
        .checked_add(dedup_line.len() as u64)
        .and_then(|value| value.checked_add(1))
        .is_none_or(|value| value > input.max_dedup_bytes)
    {
        return Err(ProviderError::new(
            "dedup-capacity",
            "deduplication byte capacity reached",
        ));
    }

    let (turns_size, turns_bytes) = read_file(TURNS)?;
    let mut turns = parse_lines::<Turn>(&turns_bytes, "dekopon.chat-memory.turn")?;
    let appended_size = jsonl::append(TURNS, turns_size, &turn_line).map_err(storage)?;
    jsonl::append(DEDUP, dedup_size, &dedup_line).map_err(storage)?;
    turns.push(turn);

    if appended_size >= input.compaction_threshold_bytes {
        let compacted = compact(
            &turns,
            input.max_lookback_turns as usize,
            input.compaction_target_bytes,
        )?;
        jsonl::replace(TURNS, appended_size, &compacted).map_err(storage)?;
    }
    Ok(json!({"recorded":true,"duplicate":false}))
}

fn compact(turns: &[Turn], lookback: usize, target: u64) -> Result<Vec<u8>, ProviderError> {
    let mut selected = Vec::new();
    let mut bytes = 0_u64;
    for turn in turns.iter().rev().take(lookback) {
        let line = serde_json::to_vec(turn).map_err(|_| corrupt())?;
        let next = bytes
            .checked_add(line.len() as u64 + 1)
            .ok_or_else(corrupt)?;
        if next > target {
            if selected.is_empty() {
                return Err(ProviderError::new(
                    "result-too-large",
                    "newest turn cannot fit compaction target",
                ));
            }
            break;
        }
        selected.push(line);
        bytes = next;
    }
    selected.reverse();
    Ok(join_lines(&selected))
}

fn bounded_result(turns: &[Turn], maximum: usize, max_bytes: u64) -> Result<Value, ProviderError> {
    let refs = turns.iter().collect::<Vec<_>>();
    bounded_refs(&refs, maximum, max_bytes)
}

fn bounded_refs(turns: &[&Turn], maximum: usize, max_bytes: u64) -> Result<Value, ProviderError> {
    let candidates = turns
        .iter()
        .rev()
        .take(maximum)
        .copied()
        .collect::<Vec<_>>();
    let mut kept = Vec::new();
    for turn in candidates {
        kept.push(turn);
        let mut chronological = kept.clone();
        chronological.reverse();
        let value = json!({"turns": chronological, "truncated": false});
        let size = serde_json::to_vec(&value).map_err(|_| corrupt())?.len() as u64;
        if size > max_bytes {
            kept.pop();
            if kept.is_empty() {
                return Err(ProviderError::new(
                    "result-too-large",
                    "newest matching turn cannot fit result bound",
                ));
            }
            break;
        }
    }
    kept.reverse();
    let truncated = kept.len() < turns.len().min(maximum) || turns.len() > maximum;
    let result = json!({"turns": kept, "truncated": truncated});
    if serde_json::to_vec(&result).map_err(|_| corrupt())?.len() as u64 > max_bytes {
        return Err(ProviderError::new(
            "result-too-large",
            "memory result envelope cannot fit configured bound",
        ));
    }
    Ok(result)
}

fn read_turns_tail(maximum: usize) -> Result<Vec<Turn>, ProviderError> {
    let size = match jsonl::size(TURNS) {
        Ok(size) => size,
        Err(StorageError::NotFound) => return Ok(Vec::new()),
        Err(error) => return Err(storage(error)),
    };
    let mut end = size;
    let mut bytes = Vec::new();
    while end > 0 && bytes.iter().filter(|byte| **byte == b'\n').count() <= maximum {
        let start = end.saturating_sub(u64::from(CHUNK));
        let length = u32::try_from(end - start).map_err(|_| corrupt())?;
        let chunk = jsonl::read_chunk(TURNS, start, length).map_err(storage)?;
        if chunk.next_offset != end || chunk.bytes.len() != length as usize {
            return Err(corrupt());
        }
        let mut combined = Vec::with_capacity(chunk.bytes.len().saturating_add(bytes.len()));
        combined.extend_from_slice(&chunk.bytes);
        combined.extend_from_slice(&bytes);
        bytes = combined;
        end = start;
    }
    if end > 0 {
        let boundary = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(corrupt)?;
        bytes.drain(..=boundary);
    }
    let mut turns = parse_lines(&bytes, "dekopon.chat-memory.turn")?;
    if turns.len() > maximum {
        turns.drain(..turns.len() - maximum);
    }
    Ok(turns)
}

fn read_file(name: &str) -> Result<(u64, Vec<u8>), ProviderError> {
    let size = match jsonl::size(name) {
        Ok(size) => size,
        Err(StorageError::NotFound) => return Ok((0, Vec::new())),
        Err(error) => return Err(storage(error)),
    };
    let mut bytes = Vec::with_capacity(size.min(16 * 1024 * 1024) as usize);
    let mut offset = 0;
    while offset < size {
        let chunk = jsonl::read_chunk(name, offset, CHUNK).map_err(storage)?;
        if chunk.next_offset <= offset || chunk.next_offset > size {
            return Err(corrupt());
        }
        bytes.extend_from_slice(&chunk.bytes);
        offset = chunk.next_offset;
        if chunk.eof {
            break;
        }
    }
    if offset != size || bytes.len() as u64 != size {
        return Err(corrupt());
    }
    Ok((size, bytes))
}

fn parse_lines<T: for<'de> Deserialize<'de>>(
    bytes: &[u8],
    expected: &str,
) -> Result<Vec<T>, ProviderError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if !bytes.ends_with(b"\n") {
        return Err(corrupt());
    }
    bytes[..bytes.len() - 1]
        .split(|byte| *byte == b'\n')
        .map(|line| {
            let value: Value = serde_json::from_slice(line).map_err(|_| corrupt())?;
            if value.get("format").and_then(Value::as_str) != Some(expected)
                || value.get("version").and_then(Value::as_u64) != Some(1)
            {
                return Err(corrupt());
            }
            serde_json::from_value(value).map_err(|_| corrupt())
        })
        .collect()
}

fn join_lines(lines: &[Vec<u8>]) -> Vec<u8> {
    let mut output = Vec::new();
    for line in lines {
        output.extend_from_slice(line);
        output.push(b'\n');
    }
    output
}

fn storage(error: StorageError) -> ProviderError {
    match error {
        StorageError::Corrupt => corrupt(),
        _ => ProviderError::new("storage-failed", "memory storage operation failed"),
    }
}
fn corrupt() -> ProviderError {
    ProviderError::new("memory-corrupt", "chat memory is corrupt")
}
fn invalid() -> ProviderError {
    ProviderError::new("invalid-input", "memory input is invalid")
}
fn usage() -> ProviderError {
    ProviderError::new("usage", USAGE)
}

dekopon_provider_sdk::export_provider_with_commands!(MemoryChat, bindings);

#[cfg(test)]
mod tests {
    use super::{
        MemoryChat, Provider, Turn, USAGE, bounded_refs, compact, join_lines, parse_lines,
    };

    fn turn(id: &str, text: &str) -> Turn {
        Turn {
            format: "dekopon.chat-memory.turn".to_owned(),
            version: 1,
            id: id.to_owned(),
            commitment: format!("commitment-{id}"),
            user: text.to_owned(),
            assistant: format!("answer-{text}"),
        }
    }

    #[test]
    fn manifest_is_exact_and_record_is_not_a_command() {
        let manifest = MemoryChat::manifest();
        assert_eq!(manifest.id.as_str(), "memory-chat");
        assert_eq!(manifest.command_words, ["memory"]);
        assert_eq!(manifest.capabilities.len(), 3);
        assert!(
            MemoryChat::resolve_command(&["record".into()])
                .expect_err("record never resolves")
                .message()
                .contains(USAGE)
        );
    }

    #[test]
    fn compaction_accepts_its_exact_target_and_rejects_one_byte_less() {
        let newest = turn("newest", "payload");
        let encoded = serde_json::to_vec(&newest).expect("turn JSON");
        let exact = encoded.len() as u64 + 1;
        assert_eq!(
            compact(std::slice::from_ref(&newest), 1, exact).expect("exact target"),
            join_lines(&[encoded])
        );
        assert_eq!(
            compact(std::slice::from_ref(&newest), 1, exact - 1)
                .expect_err("one byte below newest line")
                .code(),
            "result-too-large"
        );
    }

    #[test]
    fn result_envelopes_enforce_empty_exact_and_one_byte_bounds() {
        let empty = serde_json::json!({"turns": Vec::<Turn>::new(), "truncated": false});
        let empty_bytes = serde_json::to_vec(&empty).expect("empty envelope").len() as u64;
        assert_eq!(
            bounded_refs(&[], 1, empty_bytes).expect("exact empty envelope"),
            empty
        );
        assert_eq!(
            bounded_refs(&[], 1, empty_bytes - 1)
                .expect_err("empty envelope still has a bound")
                .code(),
            "result-too-large"
        );

        let older = turn("older", "first");
        let newer = turn("newer", "second");
        let result = bounded_refs(&[&older, &newer], 2, 64 * 1024).expect("two turns");
        assert_eq!(result["turns"][0]["id"], "older");
        assert_eq!(result["turns"][1]["id"], "newer");

        let one = bounded_refs(&[&newer], 1, 64 * 1024).expect("one turn");
        let exact = serde_json::to_vec(&one).expect("one-turn envelope").len() as u64;
        assert_eq!(
            bounded_refs(&[&newer], 1, exact).expect("exact one-turn result"),
            one
        );
        assert_eq!(
            bounded_refs(&[&newer], 1, exact - 1)
                .expect_err("one byte below the complete newest turn")
                .code(),
            "result-too-large"
        );
    }

    #[test]
    fn only_complete_versioned_lines_survive_parsing() {
        let valid = serde_json::to_vec(&turn("one", "text")).expect("turn JSON");
        assert_eq!(
            parse_lines::<Turn>(&join_lines(&[valid]), "dekopon.chat-memory.turn")
                .expect("valid line")
                .len(),
            1
        );
        for corrupt in [
            b"{}\n".as_slice(),
            b"{\"format\":\"dekopon.chat-memory.turn\",\"version\":1}".as_slice(),
            b"not-json\n".as_slice(),
        ] {
            assert_eq!(
                parse_lines::<Turn>(corrupt, "dekopon.chat-memory.turn")
                    .expect_err("corrupt line")
                    .code(),
                "memory-corrupt"
            );
        }
    }
}
