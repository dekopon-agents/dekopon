use dekopon_context::replay::{ReplayInput, run};
use std::io::{self, Read};
fn main() {
    let mut input = String::new();
    io::stdin()
        .take(262_145)
        .read_to_string(&mut input)
        .expect("synthetic input");
    if input.len() > 262_144 {
        eprintln!("input-too-large");
        std::process::exit(1);
    }
    let result = serde_json::from_str::<ReplayInput>(&input)
        .map_err(|error| format!("invalid-json: {error}"))
        .and_then(|input| run(&input).map_err(|error| format!("{error:?}")));
    match result {
        Ok(snapshots) => println!(
            "{}",
            serde_json::to_string(&snapshots).expect("snapshot serializes")
        ),
        Err(reason) => {
            eprintln!("{reason}");
            std::process::exit(1);
        }
    }
}
