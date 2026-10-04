use std::io::Write as _;

use serde_json::json;

pub const SCRIPT: &str = "printf child";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("\\{byte:02x}")).collect()
}

pub fn component() -> tempfile::NamedTempFile {
    component_for(SCRIPT)
}

pub fn component_for(script: &str) -> tempfile::NamedTempFile {
    let proposal = json!({
        "outcome": "proposed",
        "capability": "cli-probe.write",
        "input": {}
    })
    .to_string();
    let manifest = json!({
        "apiVersion": "dekopon.dev/provider/v1alpha1",
        "id": "cli-probe",
        "description": "Child script broker fixture",
        "commandWords": ["probe"],
        "capabilities": [{
            "id": "cli-probe.upper",
            "description": "Runs a child script",
            "effect": "read-only",
            "risk": "Low",
            "inputSchema": {"type": "object"}
        }, {
            "id": "cli-probe.write",
            "description": "Tests denial of a nested write",
            "effect": "external-write",
            "risk": "High",
            "inputSchema": {"type": "object"}
        }]
    })
    .to_string();
    let descriptor = hex(&[64_u32.to_le_bytes(), (manifest.len() as u32).to_le_bytes()].concat());
    let wat = format!(
        r#"(component
    (import "dekopon:stdio/streams@0.1.0" (instance $streams
        (export "reader" (type $reader (sub resource)))
        (export "[method]reader.read" (func (param "self" (borrow $reader)) (param "max" u32) (result (list u8))))
    ))
    (alias export $streams "reader" (type $reader))
    (import "dekopon:spawn/run@0.1.0" (instance $spawn
        (export "status" (type $status (sub resource)))
        (type $exit-record (record (field "status" u8) (field "stderr" string)))
        (export "exit" (type $exit (eq $exit-record)))
        (alias outer 1 $reader (type $outer-reader))
        (export "reader" (type $child-reader (eq $outer-reader)))
        (type $stdin-variant (variant (case "none") (case "inherit") (case "reader" (own $child-reader))))
        (export "stdin" (type $stdin (eq $stdin-variant)))
        (type $child-record (record (field "stdout" (own $child-reader)) (field "status" (own $status))))
        (export "child" (type $child (eq $child-record)))
        (type $error-enum (enum "busy"))
        (export "spawn-error" (type $spawn-error (eq $error-enum)))
        (export "[static]status.wait" (func (param "this" (own $status)) (result $exit)))
        (export "run" (func (param "script" string) (param "stdin" $stdin) (result (result $child (error $spawn-error)))))
    ))
    (core module $mem (memory (export "memory") 2)
        (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 65536))
    (core instance $mem (instantiate $mem))
    (alias core export $mem "memory" (core memory $memory))
    (alias core export $mem "realloc" (core func $realloc))
    (core func $run (canon lower (func $spawn "run") (memory $memory) (realloc $realloc)))
    (core func $read (canon lower (func $streams "[method]reader.read") (memory $memory) (realloc $realloc)))
    (core func $wait (canon lower (func $spawn "[static]status.wait") (memory $memory) (realloc $realloc)))
    (core module $guest
        (import "memory" "memory" (memory 2))
        (import "host" "run" (func $run (param i32 i32 i32 i32 i32)))
        (import "host" "wait" (func $wait (param i32 i32)))
        (import "host" "read" (func $read (param i32 i32 i32)))
        (data (i32.const 0) "{descriptor}")
        (data (i32.const 64) "{manifest}")
        (data (i32.const 1024) "{script_bytes}")
        (data (i32.const 3072) "{proposal_bytes}")
        (func (export "describe") (result i32) i32.const 0)
        (func (export "run-command") (param i32 i32 i32) (result i32)
            i32.const 128 i32.const 3072 i32.store
            i32.const 132 i32.const {proposal_len} i32.store
            i32.const 128)
        (func (export "invoke") (param i32 i32 i32 i32) (result i32)
            i32.const 1024 i32.const {script_len} i32.const 0 i32.const 0 i32.const 2048 call $run
            i32.const 2048 i32.load8_u
            if
                i32.const 16 i32.const 1 i32.store8
                i32.const 17 i32.const 99 i32.store8
                i32.const 16 return
            end
            local.get 3 i32.const 2 i32.ne
            if
                i32.const 2052 i32.load i32.const 1 i32.const 2080 call $read
                unreachable
            end
            i32.const 2056 i32.load i32.const 2064 call $wait
            i32.const 16 i32.const 2064 i32.load8_u i32.const 0 i32.ne i32.store8
            i32.const 17 i32.const 2064 i32.load8_u i32.store8
            i32.const 16))
    (core instance $guest (instantiate $guest
        (with "memory" (instance $mem))
        (with "host" (instance (export "run" (func $run)) (export "wait" (func $wait)) (export "read" (func $read))))))
    (func (export "describe") (result string)
        (canon lift (core func $guest "describe") (memory $memory)))
    (func (export "run-command") (param "argv" (list string)) (param "stdin-piped" bool) (result string)
        (canon lift (core func $guest "run-command") (memory $memory) (realloc $realloc)))
    (func (export "invoke") (param "capability" string) (param "input-json" string) (result (result (error u8)))
        (canon lift (core func $guest "invoke") (memory $memory) (realloc $realloc)))
)"#,
        descriptor = descriptor,
        manifest = hex(manifest.as_bytes()),
        script_bytes = hex(script.as_bytes()),
        script_len = script.len(),
        proposal_bytes = hex(proposal.as_bytes()),
        proposal_len = proposal.len(),
    );
    let mut file = tempfile::NamedTempFile::new().expect("temporary component");
    file.write_all(&wat::parse_str(wat).expect("valid inline component"))
        .expect("write inline component");
    file
}
