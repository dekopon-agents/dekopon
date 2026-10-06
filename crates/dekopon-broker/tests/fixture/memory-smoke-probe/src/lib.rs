use clap::Parser;
use dekopon_provider_sdk::provider::{
    Capability, Code, Failure, Jsonl, Proposal, Provider, Stdout, Storage, Usage,
};
use dekopon_provider_sdk::{EffectKind, RiskLevel};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use std::io::Write;

struct MemorySmokeProbe;
struct Record;
struct Recent;
struct Search;

#[derive(Parser)]
#[command(name = "smokerecent")]
struct Args {}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RecordInput {
    operation: String,
    id: String,
    commitment: String,
    user: String,
    assistant: String,
    #[serde(rename = "maxTurnBytes")]
    max_turn_bytes: u64,
    #[serde(rename = "maxLookbackTurns")]
    max_lookback_turns: u64,
    #[serde(rename = "compactionTargetBytes")]
    compaction_target_bytes: u64,
    #[serde(rename = "compactionThresholdBytes")]
    compaction_threshold_bytes: u64,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "maxLookbackTurns"
    )]
    max_lookback_turns: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "maxRecentTurns"
    )]
    max_recent_turns: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "maxResultBytes"
    )]
    max_result_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    query: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "maxSearchResults"
    )]
    max_search_results: Option<u64>,
}

#[derive(Debug)]
struct ProbeError;
impl std::fmt::Display for ProbeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("smoke probe storage failure")
    }
}
impl Failure for ProbeError {
    fn code(&self) -> Code {
        Code::new("storage-error")
    }
}

impl Provider for MemorySmokeProbe {
    const ID: &'static str = "memory";
    const COMMAND_WORDS: &'static [&'static str] = &["smokerecent"];
    const DESCRIPTION: &'static str = "Three-route synthetic memory fixture";
    type Args = Args;
    type Capabilities = (Record, Recent, Search);
    fn propose(_: Args, _: bool) -> Result<Proposal<Self>, Usage> {
        Ok(Proposal::to::<Recent>(ReadInput {
            operation: None,
            last: Some(1),
            max_lookback_turns: None,
            max_recent_turns: None,
            max_result_bytes: None,
            query: None,
            max_search_results: None,
        }))
    }
}
impl Capability for Record {
    type Provider = MemorySmokeProbe;
    const NAME: &'static str = "chat.record";
    const DESCRIPTION: &'static str = "Record a delivered turn";
    const EFFECT: EffectKind = EffectKind::LocalWrite;
    const RISK: RiskLevel = RiskLevel::Medium;
    type Input = RecordInput;
    type Needs = Storage<Jsonl>;
    type Error = ProbeError;
    fn run(input: RecordInput, storage: Storage<Jsonl>, _: &mut Stdout) -> Result<(), ProbeError> {
        let encoded = serde_json::to_vec(&input).map_err(|_| ProbeError)?;
        let size = storage.size("turns").unwrap_or(0);
        storage
            .append("turns", size, &encoded)
            .map_err(|_| ProbeError)?;
        Ok(())
    }
}
impl Capability for Recent {
    type Provider = MemorySmokeProbe;
    const NAME: &'static str = "chat.recent";
    const DESCRIPTION: &'static str = "Read the synthetic turn";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::High;
    type Input = ReadInput;
    type Needs = Storage<Jsonl>;
    type Error = ProbeError;
    fn run(_: ReadInput, storage: Storage<Jsonl>, out: &mut Stdout) -> Result<(), ProbeError> {
        let size = storage.size("turns").unwrap_or(0);
        let chunk = storage
            .read_chunk("turns", 0, size.min(65_536) as u32)
            .map_err(|_| ProbeError)?;
        out.write_all(&chunk.bytes).map_err(|_| ProbeError)
    }
}
impl Capability for Search {
    type Provider = MemorySmokeProbe;
    const NAME: &'static str = "chat.search";
    const DESCRIPTION: &'static str = "Read the synthetic turn by query";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::High;
    type Input = ReadInput;
    type Needs = Storage<Jsonl>;
    type Error = ProbeError;
    fn run(_: ReadInput, _: Storage<Jsonl>, _: &mut Stdout) -> Result<(), ProbeError> {
        Ok(())
    }
}

dekopon_provider_sdk::export!(MemorySmokeProbe);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixture_declares_exactly_three_memory_routes() {
        let manifest = dekopon_provider_sdk::provider::manifest::<MemorySmokeProbe>()
            .expect("three valid capabilities");
        let sample = serde_json::json!({"operation":"record","id":"id","commitment":"hash","user":"marker","assistant":"assistant","maxTurnBytes":100,"maxLookbackTurns":10,"compactionTargetBytes":1000,"compactionThresholdBytes":2000});
        assert!(serde_json::from_value::<RecordInput>(sample).is_ok());
        assert_eq!(manifest.capabilities.len(), 3);
    }
}
