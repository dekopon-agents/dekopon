use std::io::Write as _;

use serde_json::json;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("\\{byte:02x}")).collect()
}

pub fn component() -> tempfile::NamedTempFile {
    let manifest = json!({
        "apiVersion": "dekopon.dev/provider/v1alpha1",
        "id": "cli-probe",
        "description": "Parked stdio broker fixture",
        "commandWords": ["probe"],
        "capabilities": [{
            "id": "cli-probe.upper",
            "description": "Parks on stdin",
            "effect": "read-only",
            "risk": "Low",
            "inputSchema": {"type": "object"}
        }]
    })
    .to_string();
    let descriptor = hex(&[64_u32.to_le_bytes(), (manifest.len() as u32).to_le_bytes()].concat());
    let wat = format!(
        r#"(component
    (import "dekopon:stdio/streams@0.1.0" (instance $streams
        (type $error (enum "closed"))
        (export "write-error" (type $write-error (eq $error)))
        (export "reader" (type $reader (sub resource)))
        (export "writer" (type $writer (sub resource)))
        (export "[method]reader.read" (func (param "self" (borrow $reader)) (param "max" u32) (result (list u8))))
        (export "[method]writer.write" (func (param "self" (borrow $writer)) (param "bytes" (list u8)) (result (result (error $write-error)))))
        (export "stdin" (func (result (option (own $reader)))))
        (export "stdout" (func (result (own $writer))))
        (export "write-stderr" (func (param "text" string)))
    ))
    (core module $mem (memory (export "memory") 2)
        (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 65536))
    (core instance $mem (instantiate $mem))
    (alias core export $mem "memory" (core memory $memory))
    (alias core export $mem "realloc" (core func $realloc))
    (core func $read (canon lower (func $streams "[method]reader.read") (memory $memory) (realloc $realloc)))
    (core func $stdin (canon lower (func $streams "stdin") (memory $memory)))
    (core module $guest
        (import "memory" "memory" (memory 2))
        (import "host" "read" (func $read (param i32 i32 i32)))
        (import "host" "stdin" (func $stdin (param i32)))
        (data (i32.const 0) "{descriptor}")
        (data (i32.const 64) "{manifest}")
        (func (export "describe") (result i32) i32.const 0)
        (func (export "run-command") (param i32 i32 i32) (result i32)
            i32.const 128 i32.const 2 i32.store
            i32.const 132 i32.const 2 i32.store
            i32.const 128)
        (func (export "invoke") (param i32 i32 i32 i32) (result i32)
            i32.const 32 call $stdin
            i32.const 36 i32.load i32.const 16 i32.const 48 call $read
            i32.const 16 i32.const 0 i32.store16
            i32.const 16))
    (core instance $guest (instantiate $guest
        (with "memory" (instance $mem))
        (with "host" (instance (export "read" (func $read)) (export "stdin" (func $stdin))))))
    (func (export "describe") (result string)
        (canon lift (core func $guest "describe") (memory $memory)))
    (func (export "run-command") (param "argv" (list string)) (param "stdin-piped" bool) (result string)
        (canon lift (core func $guest "run-command") (memory $memory) (realloc $realloc)))
    (func (export "invoke") (param "capability" string) (param "input-json" string) (result (result (error u8)))
        (canon lift (core func $guest "invoke") (memory $memory) (realloc $realloc)))
)"#,
        descriptor = descriptor,
        manifest = hex(manifest.as_bytes())
    );
    let mut file = tempfile::NamedTempFile::new().expect("temporary component");
    file.write_all(&wat::parse_str(wat).expect("valid inline component"))
        .expect("write inline component");
    file
}
