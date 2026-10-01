use clap::Parser;
use dekopon_provider_sdk::provider::{Bounded, Truncated};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Input {
    text: Bounded<4>,
    excerpt: Truncated<4>,
}

#[derive(Debug, Parser)]
struct Args {
    #[arg(long)]
    text: Bounded<4>,
    #[arg(long)]
    excerpt: Truncated<4>,
}

#[test]
fn multibyte_over_byte_limit_is_rejected_in_json_and_clap() {
    assert!(serde_json::from_value::<Input>(json!({"text":"ééa", "excerpt":"ok"})).is_err());
    assert!(Args::try_parse_from(["test", "--text", "ééa", "--excerpt", "ok"]).is_err());
    assert!(
        serde_json::from_value::<Input>(json!({"text":"ok", "excerpt":"ok", "other":1})).is_err()
    );
}

#[test]
fn truncation_stays_on_unicode_boundary_and_records_cut() {
    let parsed: Input = serde_json::from_value(json!({"text":"éé", "excerpt":"ééa"})).unwrap();
    assert_eq!(parsed.text.as_str(), "éé");
    assert_eq!(parsed.excerpt.as_str(), "éé");
    assert!(parsed.excerpt.was_cut());
    let args = Args::try_parse_from(["test", "--text", "éé", "--excerpt", "ééa"]).unwrap();
    assert_eq!(args.excerpt.as_str(), "éé");
    assert!(args.excerpt.was_cut());
    assert_eq!(Truncated::<1>::new("éa").as_str(), "");
    assert!(Truncated::<1>::new("éa").was_cut());
}

#[test]
fn typed_roundtrip_and_schema_are_closed_and_character_bounded() {
    let input: Input = serde_json::from_value(json!({"text":"éé", "excerpt":"é"})).unwrap();
    let encoded = serde_json::to_value(&input).unwrap();
    let decoded: Input = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), encoded);
    let schema = schemars::schema_for!(Input);
    let schema = serde_json::to_value(schema).unwrap();
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["properties"]["text"]["maxLength"], 4);
    assert_eq!(schema["properties"]["text"]["type"], "string");
    assert_eq!(schema["properties"]["excerpt"]["type"], "string");
    assert!(schema["properties"]["excerpt"].get("maxLength").is_none());
    assert!(schema["properties"]["text"].get("description").is_none());
    assert!(schema["properties"]["text"].get("x-byte-limit").is_none());
}
