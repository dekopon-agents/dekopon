mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "provider",
        generate_all,
        pub_export_macro: true,
    });
}

struct RawClock;
impl bindings::Guest for RawClock {
    fn describe() -> String {
        serde_json::json!({
            "apiVersion": "dekopon.dev/provider/v1alpha1",
            "id": "clock-probe",
            "description": "Clock provider fixture: the date word and the host wall clock",
            "commandWords": ["date"],
            "capabilities": [{
                "id": "clock.now",
                "description": "Reads the broker host's wall clock in UTC",
                "effect": "read-only",
                "risk": "Low",
                "inputSchema": {"type":"object","additionalProperties":false}
            }]
        })
        .to_string()
    }

    fn invoke(_: String, _: String) -> String {
        let now = bindings::dekopon::clock::wall::now_unix_millis();
        serde_json::json!({"outcome":"succeeded","output":{"unixMillis":now}}).to_string()
    }

    fn run_command(argv: Vec<String>, _: Option<String>) -> String {
        if argv.iter().any(|arg| arg == "--clock-in-run-command") {
            let _ = bindings::dekopon::clock::wall::now_unix_millis();
        }
        serde_json::json!({"outcome":"proposed","capability":"clock.now","input":{}}).to_string()
    }
}

bindings::export!(RawClock with_types_in bindings);
