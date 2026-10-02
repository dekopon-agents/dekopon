#![allow(
    clippy::unwrap_used,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]

use std::io::{Read as _, Write as _};
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::time::{Duration, Instant};

use dekopon_broker_host::asset::AssetInputs;
use dekopon_broker_host::{
    BrokerHostError, BrokerHostLimits, BrokerInvocationFailure, BrokerInvocationOutput,
    BrokerProviderRegistry, MAX_READ_BYTES, MAX_STDERR_BYTES, STDERR_TRUNCATION_MARKER, StdioTrap,
    Streams, ZERO_FAILURE_STATUS_NOTE,
};
use dekopon_capability::{AuthorizedInvocation, ExecutionConstraints, broker::AuthorizationGate};
use dekopon_core::{Actor, AgentId, InvocationId, PrincipalId, TraceId};
use serde_json::json;

const BUFFER: u32 = 4096;
const STDERR_CHUNK: u32 = 40 * 1024;

fn hex(data: &[u8]) -> String {
    data.iter().map(|byte| format!("\\{byte:02x}")).collect()
}

fn manifest() -> String {
    serde_json::to_string(&json!({
        "apiVersion": dekopon_provider_sdk::ProviderApiVersion::V1Alpha1,
        "id": "stdio-probe",
        "description": "Inline standard-stream probe",
        "commandWords": ["stdioprobe"],
        "capabilities": [{
            "id": "stdio-probe.run",
            "description": "Runs the inline body",
            "effect": dekopon_capability::EffectKind::ReadOnly,
            "risk": dekopon_core::RiskLevel::Low,
            "inputSchema": {"type": "object"}
        }]
    }))
    .unwrap()
}

fn descriptor(offset: u32, length: usize) -> String {
    hex(&[
        offset.to_le_bytes(),
        u32::try_from(length).unwrap().to_le_bytes(),
    ]
    .concat())
}

fn component(body: &str) -> tempfile::NamedTempFile {
    let manifest = manifest();
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
    (core module $libc
        (memory (export "memory") 8)
        (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 131072))
    (core instance $libc (instantiate $libc))
    (alias core export $libc "memory" (core memory $mem))
    (alias core export $libc "realloc" (core func $realloc))
    (core func $read (canon lower (func $streams "[method]reader.read") (memory $mem) (realloc $realloc)))
    (core func $write (canon lower (func $streams "[method]writer.write") (memory $mem)))
    (core func $stdin (canon lower (func $streams "stdin") (memory $mem)))
    (core func $stdout (canon lower (func $streams "stdout")))
    (core func $stderr (canon lower (func $streams "write-stderr") (memory $mem)))
    (alias export $streams "writer" (type $writer))
    (core func $drop-writer (canon resource.drop $writer))
    (core module $m
        (import "libc" "memory" (memory 8))
        (import "host" "read" (func $read (param i32 i32 i32)))
        (import "host" "write" (func $write (param i32 i32 i32 i32)))
        (import "host" "stdin" (func $stdin (param i32)))
        (import "host" "stdout" (func $stdout (result i32)))
        (import "host" "stderr" (func $stderr (param i32 i32)))
        (import "host" "drop-writer" (func $drop-writer (param i32)))
        (data (i32.const 0) "{manifest_descriptor}")
        (data (i32.const 64) "{manifest}")
        (data (i32.const 3072) "boom\0a")
        (func $ok (result i32) i32.const 16 i32.const 0 i32.store16 i32.const 16)
        (func $err (param i32) (result i32)
            i32.const 16 i32.const 1 i32.store8
            i32.const 17 local.get 0 i32.store8
            i32.const 16)
        (func (export "describe") (result i32) i32.const 0)
        (func (export "command") (param i32 i32 i32) (result i32) i32.const 0)
        (func (export "invoke") (param i32 i32 i32 i32) (result i32)
            (local $h i32) (local $n i32) (local $max i32) (local $total i32) (local $w i32)
            {body}))
    (core instance $i (instantiate $m
        (with "libc" (instance $libc))
        (with "host" (instance
            (export "read" (func $read))
            (export "write" (func $write))
            (export "stdin" (func $stdin))
            (export "stdout" (func $stdout))
            (export "stderr" (func $stderr))
            (export "drop-writer" (func $drop-writer))))))
    (func (export "describe") (result string)
        (canon lift (core func $i "describe") (memory $mem)))
    (func (export "invoke") (param "capability" string) (param "input-json" string) (result (result (error u8)))
        (canon lift (core func $i "invoke") (memory $mem) (realloc $realloc)))
    (func (export "run-command") (param "argv" (list string)) (param "stdin-piped" bool) (result string)
        (canon lift (core func $i "command") (memory $mem) (realloc $realloc)))
)"#,
        manifest_descriptor = descriptor(64, manifest.len()),
        manifest = hex(manifest.as_bytes()),
    );
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(wat.as_bytes()).unwrap();
    file
}

// The 0.3.0 shape: `invoke` lifted a JSON string and `run-command` took the piped text.
fn component_0_3_0() -> tempfile::NamedTempFile {
    let manifest = manifest();
    let response = r#"{"outcome":"succeeded","output":{}}"#;
    let wat = format!(
        r#"(component
    (core module $m
        (memory (export "memory") 1)
        (data (i32.const 0) "{manifest_descriptor}")
        (data (i32.const 8) "{response_descriptor}")
        (data (i32.const 64) "{manifest}")
        (data (i32.const 2048) "{response}")
        (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 4096)
        (func (export "describe") (result i32) i32.const 0)
        (func (export "invoke") (param i32 i32 i32 i32) (result i32) i32.const 8)
        (func (export "command") (param i32 i32 i32 i32 i32) (result i32) i32.const 8))
    (core instance $i (instantiate $m))
    (func (export "describe") (result string)
        (canon lift (core func $i "describe") (memory (core memory $i "memory"))))
    (func (export "invoke") (param "capability" string) (param "input" string) (result string)
        (canon lift (core func $i "invoke") (memory (core memory $i "memory"))
            (realloc (core func $i "realloc"))))
    (func (export "run-command") (param "argv" (list string)) (param "stdin" (option string)) (result string)
        (canon lift (core func $i "command") (memory (core memory $i "memory"))
            (realloc (core func $i "realloc"))))
)"#,
        manifest_descriptor = descriptor(64, manifest.len()),
        response_descriptor = descriptor(2048, response.len()),
        manifest = hex(manifest.as_bytes()),
        response = hex(response.as_bytes()),
    );
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(wat.as_bytes()).unwrap();
    file
}

fn authorized(timeout_ms: u64) -> AuthorizedInvocation {
    let proposal = dekopon_capability::ProposedInvocation::new(
        "invoke-stdio".parse::<InvocationId>().unwrap(),
        "stdio-probe.run".parse().unwrap(),
        Actor::Agent {
            agent: "stdio-test".parse::<AgentId>().unwrap(),
        },
        "0000000000000000000000000000f1c7"
            .parse::<TraceId>()
            .unwrap(),
        json!({}),
    );
    AuthorizationGate::new()
        .authorize(
            proposal,
            "stdio-probe".parse().unwrap(),
            "decision-test".to_owned(),
            "broker-test".parse::<PrincipalId>().unwrap(),
            "policy-test".to_owned(),
            ExecutionConstraints {
                timeout_ms,
                max_output_bytes: 1024,
                ..ExecutionConstraints::default()
            },
        )
        .unwrap()
}

fn streams(stdin: Option<UnixStream>, stdout: UnixStream) -> AssetInputs {
    AssetInputs {
        streams: Some(Streams {
            stdin: stdin.map(Into::into),
            stdout: stdout.into(),
        }),
        ..AssetInputs::default()
    }
}

async fn run(
    body: &str,
    timeout_ms: u64,
    assets: AssetInputs,
) -> Result<BrokerInvocationOutput, BrokerInvocationFailure> {
    let component = component(body);
    let registry = BrokerProviderRegistry::load([component.path()], BrokerHostLimits::default())
        .await
        .unwrap();
    registry.invoke(authorized(timeout_ms), None, assets).await
}

fn drain(mut peer: UnixStream) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        peer.read_to_end(&mut bytes).unwrap();
        bytes
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_0_3_0_component_is_refused_at_load_on_the_invoke_type() {
    let component = component_0_3_0();
    let error = BrokerProviderRegistry::load([component.path()], BrokerHostLimits::default())
        .await
        .expect_err("a provider@0.3.0 invoke export no longer type-checks");
    let BrokerHostError::Instantiate { source, .. } = &error else {
        panic!("expected a typed instantiation refusal, got {error:?}");
    };
    assert!(format!("{source:?}").contains("invoke"), "{source:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn guest_results_are_exit_statuses_and_err_0_is_status_1_with_a_note() {
    let (host, peer) = UnixStream::pair().unwrap();
    let ok = run("call $ok", 5_000, streams(None, host))
        .await
        .expect("ok is status 0");
    drop(peer);
    assert!(ok.stderr.is_empty(), "{:?}", ok.stderr);

    for (code, status, expected) in [
        (0, 1, format!("boom\n{ZERO_FAILURE_STATUS_NOTE}")),
        (7, 7, "boom\n".to_owned()),
        (255, 255, "boom\n".to_owned()),
    ] {
        let (host, _peer) = UnixStream::pair().unwrap();
        let body = format!("i32.const 3072 i32.const 5 call $stderr i32.const {code} call $err");
        let failure = run(&body, 5_000, streams(None, host))
            .await
            .expect_err("err(n) is a guest exit");
        let BrokerHostError::ProviderFailure {
            status: actual,
            stderr,
            ..
        } = *failure.error
        else {
            panic!(
                "err({code}) must be a provider exit, got {:?}",
                failure.error
            );
        };
        assert_eq!(actual, status, "err({code})");
        assert_eq!(stderr, expected, "err({code})");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_asking_for_u32_max_is_lowered_to_the_host_bound() {
    let (stdin_host, mut stdin_peer) = UnixStream::pair().unwrap();
    let (stdout_host, stdout_peer) = UnixStream::pair().unwrap();
    for (socket, size) in [(&stdin_peer, 4 << 20), (&stdin_host, 4 << 20)] {
        let _best_effort = rustix::net::sockopt::set_socket_send_buffer_size(socket, size);
        let _best_effort = rustix::net::sockopt::set_socket_recv_buffer_size(socket, size);
    }
    let total: u32 = 3 * MAX_READ_BYTES + 17;
    let feeder = std::thread::spawn(move || {
        stdin_peer.write_all(&vec![b'a'; total as usize]).unwrap();
    });
    let deadline = Instant::now() + Duration::from_millis(500);
    while !feeder.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let buffered_beyond_the_bound = feeder.is_finished();
    let capture = drain(stdout_peer);
    let body = r"
        i32.const 32 call $stdin
        i32.const 32 i32.load8_u i32.eqz
        if i32.const 9 call $err return end
        i32.const 36 i32.load local.set $h
        block $done
            loop $more
                local.get $h i32.const -1 i32.const 48 call $read
                i32.const 52 i32.load local.tee $n
                i32.eqz br_if $done
                local.get $total local.get $n i32.add local.set $total
                local.get $n local.get $max i32.gt_u
                if local.get $n local.set $max end
                br $more
            end
        end
        i32.const 40 local.get $max i32.store
        i32.const 44 local.get $total i32.store
        call $stdout i32.const 40 i32.const 8 i32.const 56 call $write
        call $ok";
    run(body, 30_000, streams(Some(stdin_host), stdout_host))
        .await
        .expect("the guest reads to end of input");
    feeder.join().unwrap();
    let report = capture.join().unwrap();
    let largest = u32::from_le_bytes(report[0..4].try_into().unwrap());
    let read = u32::from_le_bytes(report[4..8].try_into().unwrap());
    assert_eq!(read, total, "every byte arrives");
    assert!(largest <= MAX_READ_BYTES, "{largest} > {MAX_READ_BYTES}");
    if buffered_beyond_the_bound {
        assert_eq!(
            largest, MAX_READ_BYTES,
            "a full buffer fills one capped read"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stderr_keeps_64_kib_and_ends_in_the_marker() {
    let (host, _peer) = UnixStream::pair().unwrap();
    let body = format!(
        "i32.const {BUFFER} i32.const 120 i32.const {STDERR_CHUNK} memory.fill
        i32.const {BUFFER} i32.const {STDERR_CHUNK} call $stderr
        i32.const {BUFFER} i32.const {STDERR_CHUNK} call $stderr
        i32.const {BUFFER} i32.const {STDERR_CHUNK} call $stderr
        call $ok"
    );
    let output = run(&body, 5_000, streams(None, host))
        .await
        .expect("a long stderr is not a failure");
    assert!(
        output.stderr.len() <= MAX_STDERR_BYTES,
        "{}",
        output.stderr.len()
    );
    assert!(output.stderr.ends_with(STDERR_TRUNCATION_MARKER));
    assert_eq!(
        output.stderr.len(),
        MAX_STDERR_BYTES,
        "the prefix fills the bound"
    );
    assert!(output.stderr.starts_with(&"x".repeat(1024)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_after_the_reader_closed_returns_closed_and_the_invocation_succeeds() {
    let (host, peer) = UnixStream::pair().unwrap();
    drop(peer);
    let body = r"
        call $stdout local.set $w
        local.get $w i32.const 3072 i32.const 5 i32.const 56 call $write
        i32.const 56 i32.load16_u i32.const 1 i32.ne
        if i32.const 4 call $err return end
        local.get $w i32.const 3072 i32.const 5 i32.const 56 call $write
        i32.const 56 i32.load16_u i32.const 1 i32.ne
        if i32.const 5 call $err return end
        call $ok";
    let output = run(body, 5_000, streams(None, host))
        .await
        .expect("closed is the guest's to handle, not a host violation");
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_streams_stdin_is_absent_and_stdout_is_closed() {
    let body = r"
        i32.const 32 call $stdin
        i32.const 32 i32.load8_u
        if i32.const 2 call $err return end
        call $stdout i32.const 3072 i32.const 5 i32.const 56 call $write
        i32.const 56 i32.load16_u i32.const 1 i32.ne
        if i32.const 3 call $err return end
        call $ok";
    run(body, 5_000, AssetInputs::default())
        .await
        .expect("no streams means no stdin and a closed stdout");
}

#[tokio::test(flavor = "multi_thread")]
async fn time_parked_on_stdin_is_not_charged_to_the_timeout() {
    let body = r"
        i32.const 32 call $stdin
        i32.const 36 i32.load i32.const 64 i32.const 48 call $read
        i32.const 52 i32.load i32.eqz
        if i32.const 3 call $err return end
        call $ok";
    for (write, expected) in [(true, None), (false, Some(3))] {
        let (stdin_host, mut stdin_peer) = UnixStream::pair().unwrap();
        let (stdout_host, _stdout_peer) = UnixStream::pair().unwrap();
        let peer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            if write {
                stdin_peer.write_all(b"x").unwrap();
            }
        });
        let started = Instant::now();
        let result = run(body, 100, streams(Some(stdin_host), stdout_host)).await;
        assert!(started.elapsed() >= Duration::from_millis(600));
        peer.join().unwrap();
        match (result, expected) {
            (Ok(_), None) => {}
            (Err(failure), Some(status)) => assert!(
                matches!(*failure.error, BrokerHostError::ProviderFailure { status: actual, .. } if actual == status),
                "peer close ends the parked read as end of input: {:?}",
                failure.error
            ),
            (other, _) => panic!("parked time must not time out: {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_ends_that_are_not_stream_sockets_are_refused_at_admission() {
    let file = tempfile::tempfile().unwrap();
    let (datagram, _peer) = UnixDatagram::pair().unwrap();
    for stdout in [std::os::fd::OwnedFd::from(file), datagram.into()] {
        let assets = AssetInputs {
            streams: Some(Streams {
                stdin: None,
                stdout,
            }),
            ..AssetInputs::default()
        };
        let failure = run("call $ok", 5_000, assets)
            .await
            .expect_err("a non-stream descriptor is not admitted");
        assert!(
            matches!(
                &*failure.error,
                BrokerHostError::StdioAdmission {
                    source: dekopon_broker_host::StdioAdmissionError::NotStreamSocket
                }
            ),
            "{:?}",
            failure.error
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn live_stdio_handles_are_bounded_and_dropped_ones_are_returned() {
    let mint_and_drop = r"
        block $done
            loop $more
                call $stdout call $drop-writer
                local.get $n i32.const 1 i32.add local.tee $n
                i32.const 1000 i32.lt_u br_if $more
            end
        end
        call $ok";
    let (host, _peer) = UnixStream::pair().unwrap();
    run(mint_and_drop, 5_000, streams(None, host))
        .await
        .expect("dropped handles do not count");

    let hoard = r"
        block $done
            loop $more
                call $stdout drop
                local.get $n i32.const 1 i32.add local.tee $n
                i32.const 1000 i32.lt_u br_if $more
            end
        end
        call $ok";
    let (host, _peer) = UnixStream::pair().unwrap();
    let failure = run(hoard, 5_000, streams(None, host))
        .await
        .expect_err("a guest cannot hoard host handles");
    let BrokerHostError::Invoke { source, .. } = &*failure.error else {
        panic!("expected a trap, got {:?}", failure.error);
    };
    assert!(
        matches!(
            source.downcast_ref::<StdioTrap>(),
            Some(StdioTrap::TooManyHandles)
        ),
        "{source:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_err_0_note_is_its_own_line_even_after_a_full_stderr() {
    let unterminated = "i32.const 3072 i32.const 4 call $stderr i32.const 0 call $err";
    let flood = format!(
        "i32.const {BUFFER} i32.const 120 i32.const {STDERR_CHUNK} memory.fill
        i32.const {BUFFER} i32.const {STDERR_CHUNK} call $stderr
        i32.const {BUFFER} i32.const {STDERR_CHUNK} call $stderr
        i32.const 0 call $err"
    );
    for body in [unterminated.to_owned(), flood] {
        let (host, _peer) = UnixStream::pair().unwrap();
        let failure = run(&body, 5_000, streams(None, host))
            .await
            .expect_err("err(0) is a guest exit");
        let BrokerHostError::ProviderFailure { status, stderr, .. } = *failure.error else {
            panic!("expected a provider exit, got {:?}", failure.error);
        };
        assert_eq!(status, 1);
        assert!(stderr.len() <= MAX_STDERR_BYTES, "{}", stderr.len());
        let prefix = stderr
            .strip_suffix(ZERO_FAILURE_STATUS_NOTE)
            .expect("the note ends stderr");
        assert!(
            prefix.ends_with('\n'),
            "{:?}",
            &prefix[prefix.len().saturating_sub(20)..]
        );
        assert!(prefix.starts_with("boom") || prefix.starts_with('x'));
        assert!(prefix.matches(STDERR_TRUNCATION_MARKER).count() <= 1);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_zero_byte_read_traps_rather_than_reading_as_end_of_input() {
    let (stdin_host, _stdin_peer) = UnixStream::pair().unwrap();
    let (stdout_host, _stdout_peer) = UnixStream::pair().unwrap();
    let body = r"
        i32.const 32 call $stdin
        i32.const 36 i32.load i32.const 0 i32.const 48 call $read
        call $ok";
    let failure = run(body, 5_000, streams(Some(stdin_host), stdout_host))
        .await
        .expect_err("read(0) is a guest bug");
    let BrokerHostError::Invoke { source, .. } = &*failure.error else {
        panic!("expected a trap, got {:?}", failure.error);
    };
    assert!(
        matches!(
            source.downcast_ref::<StdioTrap>(),
            Some(StdioTrap::ZeroRead)
        ),
        "{source:?}"
    );
}
