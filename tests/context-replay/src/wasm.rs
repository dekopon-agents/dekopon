use crate::{ReplayInput, run};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn run_replay(input: &str) -> Result<String, JsValue> {
    if input.len() > 262_144 {
        return Err(JsValue::from_str("input-too-large"));
    }
    let input: ReplayInput =
        serde_json::from_str(input).map_err(|error| JsValue::from_str(&error.to_string()))?;
    let snapshots = run(&input).map_err(|error| JsValue::from_str(&format!("{error:?}")))?;
    serde_json::to_string(&snapshots).map_err(|error| JsValue::from_str(&error.to_string()))
}
