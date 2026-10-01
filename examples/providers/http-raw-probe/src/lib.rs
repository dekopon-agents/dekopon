mod bindings {
    wit_bindgen::generate!({path: "wit", world: "provider", generate_all, pub_export_macro: true});
}
struct RawHttp;
impl bindings::Guest for RawHttp {
    fn describe() -> String {
        serde_json::json!({
            "apiVersion":"dekopon.dev/provider/v1alpha1","id":"http-probe",
            "description":"Raw asset write host boundary fixture","commandWords":["httpprobe"],
            "capabilities":[{"id":"http-probe.fetch","description":"Exercise a raw writer write",
                "effect":"read-only","risk":"Low","inputSchema":{"type":"object","additionalProperties":false}}]
        }).to_string()
    }
    fn invoke(_: String, input: String) -> String {
        use bindings::dekopon::asset::asset as raw;
        let input: serde_json::Value = serde_json::from_str(&input).unwrap();
        let bytes = input["bytes"].as_u64().unwrap() as usize;
        let writer = raw::allocate("application/octet-stream", raw::Encoding::Identity).unwrap();
        let result = writer.write(&vec![b'x'; bytes]);
        if result.is_ok() {
            raw::attach(writer).unwrap();
        }
        if result.is_err() {
            match input["afterWriteError"].as_str() {
                Some("spin") => loop {
                    std::hint::spin_loop();
                },
                Some("http-denied") => {
                    use bindings::dekopon::http::client as http;
                    let _ = http::send(&http::Request {
                        method: "GET".into(),
                        uri: "http://127.0.0.1:1".into(),
                        headers: Vec::new(),
                        body: Vec::new(),
                    });
                }
                _ => {}
            }
        }
        serde_json::json!({"outcome":"succeeded","output":{"caught":result.is_err()}}).to_string()
    }
    fn run_command(_: Vec<String>, _: Option<String>) -> String {
        serde_json::json!({"outcome":"proposed","capability":"http-probe.fetch","input":{}})
            .to_string()
    }
}
bindings::export!(RawHttp with_types_in bindings);
