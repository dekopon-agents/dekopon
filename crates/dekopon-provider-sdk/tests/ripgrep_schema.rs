use std::convert::Infallible;

use clap::Parser;
use dekopon_provider_sdk::provider::{Capability, Proposal, Provider, Usage, manifest};
use dekopon_provider_sdk::{EffectKind, RiskLevel};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const RIPGREP_MANIFEST_COMPACT_BYTES: usize = 3_040;

struct Ripgrep;

#[derive(Parser)]
#[command(name = "rg")]
struct Args {
    pattern: String,
}

impl Provider for Ripgrep {
    const ID: &'static str = "ripgrep";
    const COMMAND_WORDS: &'static [&'static str] = &["rg"];
    const DESCRIPTION: &'static str = "Searches bounded caller-supplied UTF-8 virtual documents with Rust ripgrep matchers; never reads paths or performs I/O";
    type Args = Args;
    type Capabilities = (Search,);

    fn propose(args: Args, stdin: Option<&str>) -> Result<Proposal<Self>, Usage> {
        let text = stdin.ok_or_else(|| Usage::new("rg: pipe the text to search"))?;
        Ok(Proposal::to::<Search>(SearchInput {
            documents: vec![Document {
                path: "stdin".to_owned(),
                text: text.to_owned(),
            }],
            pattern: args.pattern,
            mode: SearchMode::default(),
            case: CaseMode::default(),
            word: false,
            line: false,
            multiline: false,
            invert: false,
            context: Context::default(),
            max_results: default_max_results(),
        }))
    }
}

#[derive(Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum SearchMode {
    #[default]
    Regex,
    Fixed,
}

#[derive(Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum CaseMode {
    #[default]
    Sensitive,
    Insensitive,
    Smart,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Document {
    /// Opaque relative /-separated label, enforced at 1–256 UTF-8 bytes; never dereferenced. Empty, dot, dot-dot, control, backslash, absolute, drive-prefixed, UNC-like, and empty components are rejected.
    #[schemars(length(min = 1, max = 256))]
    path: String,
    /// Decoded UTF-8 virtual content, at most 131072 bytes. LF alone terminates lines; CR and BOM bytes are preserved.
    #[schemars(length(max = 131_072))]
    text: String,
}

#[derive(Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Context {
    #[schemars(range(min = 0, max = 8))]
    before: usize,
    #[schemars(range(min = 0, max = 8))]
    after: usize,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors ripgrep's input, which carries four flags"
)]
#[serde(deny_unknown_fields)]
#[schemars(
    description = "Closed semantic input. Runtime limits include 64 submatches per selected result and a 1000000-byte provider success envelope. The host separately enforces the raw serialized invocation limit."
)]
struct SearchInput {
    /// Caller-fed virtual documents in search order. Paths must be exact-byte unique. Each text is at most 131072 decoded UTF-8 bytes and aggregate text is at most 786432 decoded UTF-8 bytes.
    #[schemars(length(min = 1, max = 16))]
    documents: Vec<Document>,
    /// One Rust-regex pattern or one fixed literal, enforced at 1–4,096 decoded UTF-8 bytes. PCRE2 look-around and backreferences are unsupported.
    #[schemars(length(min = 1, max = 4096))]
    pattern: String,
    /// Regex syntax, or one literal fixed string.
    #[serde(default)]
    mode: SearchMode,
    #[serde(default)]
    case: CaseMode,
    /// Require ripgrep word-boundary matching; cannot be combined with line.
    #[serde(default)]
    word: bool,
    /// Require a whole LF-delimited line; cannot be combined with word or multiline.
    #[serde(default)]
    line: bool,
    /// Permit explicit matches across LF. Dot still excludes LF unless the Rust pattern enables s; cannot be combined with line or invert.
    #[serde(default)]
    multiline: bool,
    /// Select non-matching lines; incompatible with multiline.
    #[serde(default)]
    invert: bool,
    /// Both counts are required when context is supplied; defaults to zero lines on both sides.
    #[serde(default)]
    context: Context,
    /// Maximum returned selected records, not occurrences or context records.
    #[serde(default = "default_max_results")]
    #[schemars(range(min = 1, max = 1000))]
    max_results: usize,
}

const fn default_max_results() -> usize {
    100
}

struct Search;

impl Capability for Search {
    type Provider = Ripgrep;
    const NAME: &'static str = "search";
    const DESCRIPTION: &'static str = "Search 1–16 virtual documents in caller order with bounded regex/fixed matching, context, byte offsets, and deterministic truncation";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = SearchInput;
    type Needs = ();
    type Output = usize;
    type Error = Infallible;

    fn run(input: SearchInput, (): ()) -> Result<usize, Infallible> {
        Ok(input.documents.len())
    }
}

fn keys(schema: &Value, found: &mut Vec<String>) {
    match schema {
        Value::Object(object) => {
            found.extend(object.keys().filter(|key| key.starts_with('$')).cloned());
            found.extend(
                object
                    .keys()
                    .filter(|key| ["oneOf", "anyOf", "allOf"].contains(&key.as_str()))
                    .cloned(),
            );
            object.values().for_each(|value| keys(value, found));
        }
        Value::Array(items) => items.iter().for_each(|value| keys(value, found)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

#[test]
fn the_ripgrep_input_derives_an_inline_schema_within_half_again_its_hand_written_size() {
    let manifest = manifest::<Ripgrep>().expect("the ripgrep identifiers are valid");
    let mut found = Vec::new();
    keys(&manifest.capabilities[0].input_schema, &mut found);
    assert!(found.is_empty(), "{found:?}");

    let bytes = serde_json::to_string(&manifest)
        .expect("the manifest serializes")
        .len();
    assert!(
        bytes * 2 <= RIPGREP_MANIFEST_COMPACT_BYTES * 3,
        "{bytes} bytes against {RIPGREP_MANIFEST_COMPACT_BYTES}"
    );
}
