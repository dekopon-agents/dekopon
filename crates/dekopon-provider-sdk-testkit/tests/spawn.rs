#![allow(
    clippy::unwrap_used,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]
use std::{fmt, io::Write as _, sync::mpsc, time::Duration};

use dekopon_provider_sdk::{
    EffectKind, RiskLevel,
    clap::Parser,
    provider::{
        Capability, ChildStdin, Code, Exit, Failure, Monotonic, Proposal, Provider, Spawn,
        SpawnError, Stdout, Usage,
    },
};
use dekopon_provider_sdk_testkit::{
    BrokerHostLimits, ChildInput, ChildRun, ChildScript, ComponentOutput, ConformanceError,
    Harness, HarnessError, Native, conformance,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

const SCRIPT: &str = "gh pr list | rg x";

#[derive(Parser)]
#[command(name = "kit")]
struct KitArgs {}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Empty {}

enum KitError {
    Busy,
    Output,
    Child(u8),
}

impl fmt::Display for KitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => formatter.write_str("busy"),
            Self::Output => formatter.write_str("stdout is closed"),
            Self::Child(status) => write!(formatter, "child exited {status}"),
        }
    }
}

impl Failure for KitError {
    fn code(&self) -> Code {
        match self {
            Self::Busy => Code::new("busy"),
            Self::Output => Code::new("output"),
            Self::Child(status) => Code::new("child-failed").exiting(*status),
        }
    }
}

fn started(
    spawn: &Spawn,
    stdin: ChildStdin,
) -> Result<dekopon_provider_sdk::provider::Child, KitError> {
    spawn
        .run(SCRIPT, stdin)
        .map_err(|SpawnError::Busy| KitError::Busy)
}

fn finished(exit: Exit) -> Result<(), KitError> {
    match exit.status {
        0 => Ok(()),
        status => Err(KitError::Child(status)),
    }
}

struct Kit;
struct Relay;
struct Waits;
struct Elapsed;

impl Provider for Kit {
    const ID: &'static str = "spawn-kit";
    const COMMAND_WORDS: &'static [&'static str] = &["kit"];
    const DESCRIPTION: &'static str = "Runs child scripts";
    type Args = KitArgs;
    type Capabilities = (Relay, Waits, Elapsed);
    fn propose(_: KitArgs, _: bool) -> Result<Proposal<Self>, Usage> {
        Ok(Proposal::to::<Relay>(Empty {}))
    }
}

impl Capability for Relay {
    type Provider = Kit;
    const NAME: &'static str = "relay";
    const DESCRIPTION: &'static str = "Relays a child that inherits stdin";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = Empty;
    type Needs = Spawn;
    type Error = KitError;
    fn run(_: Empty, spawn: Spawn, out: &mut Stdout) -> Result<(), KitError> {
        let mut child = started(&spawn, ChildStdin::Inherit)?;
        std::io::copy(&mut child.stdout, out).map_err(|_closed| KitError::Output)?;
        finished(child.wait())
    }
}

impl Capability for Waits {
    type Provider = Kit;
    const NAME: &'static str = "wait";
    const DESCRIPTION: &'static str = "Waits on a child without reading it";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = Empty;
    type Needs = Spawn;
    type Error = KitError;
    fn run(_: Empty, spawn: Spawn, _: &mut Stdout) -> Result<(), KitError> {
        finished(started(&spawn, ChildStdin::None)?.wait())
    }
}

impl Capability for Elapsed {
    type Provider = Kit;
    const NAME: &'static str = "elapsed";
    const DESCRIPTION: &'static str = "Measures a child's wait on the monotonic clock";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = Empty;
    type Needs = (Spawn, Monotonic);
    type Error = KitError;
    fn run(_: Empty, (spawn, clock): (Spawn, Monotonic), out: &mut Stdout) -> Result<(), KitError> {
        let start = clock.now_nanos();
        let exit = started(&spawn, ChildStdin::None)?.wait();
        out.write_all(&(clock.now_nanos() - start).to_le_bytes())
            .map_err(|_closed| KitError::Output)?;
        finished(exit)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("\\{byte:02x}")).collect()
}

fn documents() -> (String, String, String) {
    let capability = |name: &str| {
        json!({
            "id": format!("spawn-kit.{name}"),
            "description": name,
            "effect": "read-only",
            "risk": "Low",
            "inputSchema": {"type": "object", "additionalProperties": false}
        })
    };
    let manifest = json!({
        "apiVersion": "dekopon.dev/provider/v1alpha1",
        "id": "spawn-kit",
        "description": "Runs child scripts",
        "commandWords": ["kit"],
        "capabilities": [capability("relay"), capability("wait"), capability("elapsed")]
    })
    .to_string();
    let help = json!({"outcome": "rendered", "stdout": "Usage: kit\n", "stderr": "", "status": 0})
        .to_string();
    let usage = json!({"outcome": "rendered", "stdout": "", "stderr": "error: unexpected argument\n", "status": 2})
        .to_string();
    (manifest, help, usage)
}

fn component() -> tempfile::NamedTempFile {
    let (manifest, help, usage) = documents();
    let descriptor = hex(&[
        6144_u32.to_le_bytes(),
        (manifest.len() as u32).to_le_bytes(),
    ]
    .concat());
    let wat = format!(
        r#"(component
    (import "dekopon:stdio/streams@0.1.0" (instance $streams
        (export "reader" (type $reader (sub resource)))
        (export "writer" (type $writer (sub resource)))
        (type $write-error-enum (enum "closed"))
        (export "write-error" (type $write-error (eq $write-error-enum)))
        (export "[method]reader.read" (func (param "self" (borrow $reader)) (param "max" u32) (result (list u8))))
        (export "[method]writer.write" (func (param "self" (borrow $writer)) (param "bytes" (list u8)) (result (result (error $write-error)))))
        (export "stdout" (func (result (own $writer))))
    ))
    (alias export $streams "reader" (type $reader))
    (import "dekopon:clock/monotonic@1.1.0" (instance $monotonic
        (export "now-nanos" (func (result u64)))
    ))
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
    (core module $mem
        (memory (export "memory") 16)
        (global $bump (mut i32) (i32.const 65536))
        (func (export "realloc") (param i32 i32 i32 i32) (result i32) (local $at i32)
            global.get $bump i32.const 7 i32.add i32.const -8 i32.and local.tee $at
            local.get 3 i32.add global.set $bump
            local.get $at))
    (core instance $mem (instantiate $mem))
    (alias core export $mem "memory" (core memory $memory))
    (alias core export $mem "realloc" (core func $realloc))
    (core func $run (canon lower (func $spawn "run") (memory $memory) (realloc $realloc)))
    (core func $wait (canon lower (func $spawn "[static]status.wait") (memory $memory) (realloc $realloc)))
    (core func $read (canon lower (func $streams "[method]reader.read") (memory $memory) (realloc $realloc)))
    (core func $write (canon lower (func $streams "[method]writer.write") (memory $memory)))
    (core func $stdout (canon lower (func $streams "stdout")))
    (core func $now (canon lower (func $monotonic "now-nanos")))
    (core module $guest
        (import "memory" "memory" (memory 16))
        (import "host" "run" (func $run (param i32 i32 i32 i32 i32)))
        (import "host" "wait" (func $wait (param i32 i32)))
        (import "host" "read" (func $read (param i32 i32 i32)))
        (import "host" "write" (func $write (param i32 i32 i32 i32)))
        (import "host" "stdout" (func $stdout (result i32)))
        (import "host" "now" (func $now (result i64)))
        (data (i32.const 0) "{descriptor}")
        (data (i32.const 1024) "{script}")
        (data (i32.const 4096) "{help}")
        (data (i32.const 5120) "{usage}")
        (data (i32.const 6144) "{manifest}")
        (func (export "describe") (result i32) i32.const 0)
        (func (export "run-command") (param i32 i32 i32) (result i32)
            local.get 0 i32.load offset=4 i32.const 6 i32.eq
            if
                i32.const 128 i32.const 4096 i32.store
                i32.const 132 i32.const {help_len} i32.store
            else
                i32.const 128 i32.const 5120 i32.store
                i32.const 132 i32.const {usage_len} i32.store
            end
            i32.const 128)
        (func $start (param $stdin i32)
            i32.const 1024 i32.const {script_len} local.get $stdin i32.const 0 i32.const 2048 call $run
            i32.const 2048 i32.load8_u if unreachable end)
        (func $finish (result i32)
            i32.const 2056 i32.load i32.const 2064 call $wait
            i32.const 16 i32.const 2064 i32.load8_u i32.const 0 i32.ne i32.store8
            i32.const 17 i32.const 2064 i32.load8_u i32.store8
            i32.const 16)
        (func (export "invoke") (param i32 i32 i32 i32) (result i32) (local $out i32) (local $count i32)
            local.get 1 i32.const 15 i32.eq
            if
                call $stdout local.set $out
                i32.const 1 call $start
                block $done
                    loop $more
                        i32.const 2052 i32.load i32.const 4096 i32.const 2080 call $read
                        i32.const 2084 i32.load local.tee $count
                        i32.eqz br_if $done
                        local.get $out i32.const 2080 i32.load local.get $count i32.const 2096 call $write
                        br $more
                    end
                end
                call $finish return
            end
            local.get 1 i32.const 14 i32.eq
            if
                i32.const 0 call $start
                call $finish return
            end
            call $stdout local.set $out
            i32.const 2112 call $now i64.store
            i32.const 0 call $start
            call $finish drop
            i32.const 2120 call $now i32.const 2112 i64.load i64.sub i64.store
            local.get $out i32.const 2120 i32.const 8 i32.const 2096 call $write
            i32.const 16))
    (core instance $guest (instantiate $guest
        (with "memory" (instance $mem))
        (with "host" (instance
            (export "run" (func $run)) (export "wait" (func $wait)) (export "read" (func $read))
            (export "write" (func $write)) (export "stdout" (func $stdout)) (export "now" (func $now))))))
    (func (export "describe") (result string)
        (canon lift (core func $guest "describe") (memory $memory)))
    (func (export "run-command") (param "argv" (list string)) (param "stdin-piped" bool) (result string)
        (canon lift (core func $guest "run-command") (memory $memory) (realloc $realloc)))
    (func (export "invoke") (param "capability" string) (param "input-json" string) (result (result (error u8)))
        (canon lift (core func $guest "invoke") (memory $memory) (realloc $realloc)))
)"#,
        descriptor = descriptor,
        script = hex(SCRIPT.as_bytes()),
        script_len = SCRIPT.len(),
        help = hex(help.as_bytes()),
        help_len = help.len(),
        usage = hex(usage.as_bytes()),
        usage_len = usage.len(),
        manifest = hex(manifest.as_bytes()),
    );
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&wat::parse_str(wat).unwrap()).unwrap();
    file
}

fn bounded(
    call: impl FnOnce() -> Result<ComponentOutput, HarnessError> + Send + 'static,
) -> Result<ComponentOutput, HarnessError> {
    let (done, result) = mpsc::channel();
    std::thread::spawn(move || drop(done.send(call())));
    result
        .recv_timeout(Duration::from_secs(30))
        .expect("the component call finishes")
}

fn child(stdout: &[u8], status: u8) -> ChildScript {
    ChildScript {
        script: SCRIPT.to_owned(),
        status,
        stdout: stdout.to_vec(),
        stderr: "kept".to_owned(),
        ..ChildScript::default()
    }
}

#[test]
fn native_guest_spawn_parity() {
    let file = component();
    let path = file.path().to_path_buf();
    let native = Native::<Kit>::new()
        .stdin(b"parent input".to_vec())
        .child(child(b"from child\n", 3));
    let native_output = native.call("spawn-kit.relay", "{}");
    let component = bounded(move || {
        Harness::<Kit>::get(path)
            .stdin(b"parent input".to_vec())
            .child(child(b"from child\n", 3))
            .call("spawn-kit.relay", json!({}))
    })
    .unwrap();
    assert_eq!(native_output.status, 3, "{}", native_output.stderr);
    assert_eq!(component.status, 3, "{}", component.stderr);
    assert_eq!(native_output.stdout, b"from child\n");
    assert_eq!(component.stdout, native_output.stdout);
    let expected = vec![ChildRun {
        script: SCRIPT.to_owned(),
        stdin: ChildInput::Inherit(b"parent input".to_vec()),
    }];
    assert_eq!(native.children(), expected);
    assert_eq!(component.children, expected);
}

#[test]
fn unexpected_child_script_typed_refusal() {
    let file = component();
    for scripted in [
        vec![],
        vec![ChildScript {
            script: "rg y".to_owned(),
            ..ChildScript::default()
        }],
    ] {
        let path = file.path().to_path_buf();
        let error = bounded(move || {
            scripted
                .into_iter()
                .fold(Harness::<Kit>::get(path), |run, script| run.child(script))
                .call("spawn-kit.wait", json!({}))
        })
        .unwrap_err();
        assert!(
            matches!(error, HarnessError::Fixture("unexpected child script")),
            "{error}"
        );
    }
    let native = std::panic::catch_unwind(|| {
        Native::<Kit>::new()
            .child(ChildScript {
                script: "rg y".to_owned(),
                ..ChildScript::default()
            })
            .call("spawn-kit.wait", "{}")
    })
    .unwrap_err();
    assert_eq!(
        native.downcast_ref::<String>().map(String::as_str),
        Some("invalid fixture: unexpected child script")
    );
}

#[test]
fn wait_consumes_reader() {
    let file = component();
    let path = file.path().to_path_buf();
    let output = vec![b'x'; 300 * 1024];
    let scripted = child(&output, 0);
    let component = bounded(move || {
        Harness::<Kit>::get(path)
            .child(scripted)
            .call("spawn-kit.wait", json!({}))
    })
    .unwrap();
    assert_eq!(component.status, 0, "{}", component.stderr);
    assert!(component.stdout.is_empty());
    let native = Native::<Kit>::new()
        .child(child(&output, 0))
        .call("spawn-kit.wait", "{}");
    assert_eq!(
        (native.status, native.stdout.len()),
        (0, 0),
        "{}",
        native.stderr
    );
}

#[test]
fn child_stdin_capture_is_bounded_without_blocking_the_feeder() {
    let file = component();
    let path = file.path().to_path_buf();
    let input = vec![b'x'; 1024 * 1024 + 8192];
    let component = bounded({
        let input = input.clone();
        move || {
            Harness::<Kit>::get(path)
                .stdin(input)
                .child(child(b"", 0))
                .call("spawn-kit.relay", json!({}))
        }
    })
    .unwrap();
    assert_eq!(component.status, 0, "{}", component.stderr);
    assert_eq!(
        component.children[0].stdin,
        ChildInput::Inherit(input[..1024 * 1024].to_vec())
    );
    let native = Native::<Kit>::new().stdin(input).child(child(b"", 0));
    assert_eq!(native.call("spawn-kit.relay", "{}").status, 0);
    assert_eq!(native.children(), component.children);
}

#[test]
fn unused_child_script_is_a_typed_fixture_refusal() {
    let file = component();
    let path = file.path().to_path_buf();
    let result = bounded(move || {
        Harness::<Kit>::get(path)
            .child(child(b"", 0))
            .call("spawn-kit.unknown", json!({}))
    });
    assert!(
        matches!(result, Err(HarnessError::Fixture("expected child not run"))),
        "{result:?}"
    );
}

#[test]
fn monotonic_includes_child_wait() {
    let file = component();
    let path = file.path().to_path_buf();
    let runs_for = Duration::from_millis(600);
    let scripted = ChildScript {
        runs_for,
        ..child(b"", 0)
    };
    let limits = BrokerHostLimits {
        max_timeout: Duration::from_millis(300),
        ..BrokerHostLimits::default()
    };
    let guest = scripted.clone();
    let component = bounded(move || {
        Harness::<Kit>::get(path)
            .host_limits(limits)
            .child(guest)
            .call("spawn-kit.elapsed", json!({}))
    })
    .unwrap();
    assert_eq!(component.status, 0, "{}", component.stderr);
    let elapsed = u64::from_le_bytes(component.stdout.try_into().unwrap());
    let nanos = u64::try_from(runs_for.as_nanos()).unwrap();
    assert!(elapsed >= nanos, "{elapsed} < {nanos}");
    let native = Native::<Kit>::new()
        .monotonic(7)
        .child(scripted)
        .call("spawn-kit.elapsed", "{}");
    assert_eq!(native.status, 0, "{}", native.stderr);
    assert_eq!(u64::from_le_bytes(native.stdout.try_into().unwrap()), nanos);
}

struct Undeclared;
struct Plain;

impl Provider for Undeclared {
    const ID: &'static str = "spawn-kit";
    const COMMAND_WORDS: &'static [&'static str] = &["kit"];
    const DESCRIPTION: &'static str = "Declares no spawn";
    type Args = KitArgs;
    type Capabilities = (Plain,);
    fn propose(_: KitArgs, _: bool) -> Result<Proposal<Self>, Usage> {
        Ok(Proposal::to::<Plain>(Empty {}))
    }
}

impl Capability for Plain {
    type Provider = Undeclared;
    const NAME: &'static str = "elapsed";
    const DESCRIPTION: &'static str = "Reads the monotonic clock";
    const EFFECT: EffectKind = EffectKind::ReadOnly;
    const RISK: RiskLevel = RiskLevel::Low;
    type Input = Empty;
    type Needs = Monotonic;
    type Error = KitError;
    fn run(_: Empty, _: Monotonic, _: &mut Stdout) -> Result<(), KitError> {
        Ok(())
    }
}

#[test]
fn declared_spawn_import() {
    let file = component();
    conformance::<Kit>(file.path()).unwrap();
    let error = conformance::<Undeclared>(file.path()).unwrap_err();
    assert!(
        matches!(&error, ConformanceError::Imports { component, .. } if component.contains("dekopon:spawn/run@0.1.0")),
        "{error}"
    );
}
