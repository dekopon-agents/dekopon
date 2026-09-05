// Source-kernel experiment, not the gateway. history_source.rs is copied byte-for-byte
// from the fenced repository source. Uncalled model serialization types are stubs.
#![allow(dead_code)]
extern crate self as dekopon_model;
pub mod model {
    #[derive(Clone)]
    pub struct ModelMessage;
    impl ModelMessage {
        pub fn user(_: &str) -> Self {
            Self
        }
    }
    pub struct AssistantTurn {
        pub content: Option<String>,
        pub tool_calls: Vec<()>,
        pub usage: Option<()>,
        pub replay_items: Vec<()>,
    }
    pub fn assistant_message(_: &AssistantTurn) -> ModelMessage {
        ModelMessage
    }
}
#[path = "history_source.rs"]
mod history;
use history::{ConversationTurn, History, HistoryLimits};
use std::io::{Read, Write};
use std::time::{Duration, Instant};
fn phase(index: u8) {
    // The collector gives this fixture two private inherited pipes. Open via procfd
    // rather than introducing unsafe fd ownership or a wall-clock alignment guess.
    let mut markers = std::fs::OpenOptions::new()
        .write(true)
        .open("/proc/self/fd/100")
        .expect("phase pipe");
    let mut acks = std::fs::File::open("/proc/self/fd/101").expect("phase ack pipe");
    markers.write_all(&[index]).expect("phase marker");
    let mut ack = [255];
    acks.read_exact(&mut ack).expect("phase acknowledgement");
    assert_eq!(ack, [index]);
}
fn emit(start: Instant, phase: &str, metric: &str, value: u128) {
    println!("{{\"phase\":\"{}\",\"clock_origin\":\"child-relative\",\"elapsed_ns\":{},\"metrics\":{{\"{}\":{}}}}}", phase, start.elapsed().as_nanos(), metric, value);
}
fn hold() {
    std::thread::sleep(Duration::from_millis(150));
}
fn main() {
    let args: Vec<usize> = std::env::args()
        .skip(1)
        .map(|s| s.parse().expect("integer factor"))
        .collect();
    assert_eq!(args.len(), 6);
    let (keys, turns, window_bytes, user_bytes, answer_bytes, clones) =
        (args[0], args[1], args[2], args[3], args[4], args[5]);
    assert!(keys > 0 && keys <= 128 && turns > 0 && turns <= 12 && window_bytes <= 65536);
    assert!(
        user_bytes > 0
            && user_bytes <= 2048
            && answer_bytes > 0
            && answer_bytes <= 4096
            && clones <= 4
            && clones <= keys
    );
    let retained = turns.min(window_bytes / (user_bytes + answer_bytes));
    let start = Instant::now();
    hold();
    phase(0);
    let mut windows = Vec::with_capacity(keys);
    let mut operation_ns = 0;
    for _ in 0..keys {
        let mut h = History::new(HistoryLimits {
            max_turns: turns,
            max_bytes: window_bytes,
        });
        for _ in 0..turns {
            let turn =
                ConversationTurn::completed("u".repeat(user_bytes), "a".repeat(answer_bytes));
            let before = Instant::now();
            h.record(turn);
            let elapsed = before.elapsed().as_nanos();
            operation_ns += elapsed;
            emit(start, "load", "operation_latency_ns", elapsed);
        }
        assert_eq!(h.len(), retained);
        assert_eq!(h.bytes(), retained * (user_bytes + answer_bytes));
        windows.push(h);
    }
    phase(1);
    emit(
        start,
        "retained",
        "retained_text_bytes",
        windows.iter().map(|h| h.bytes() as u128).sum(),
    );
    emit(
        start,
        "retained",
        "history_turn_count",
        (keys * retained) as u128,
    );
    hold();
    phase(2);
    let mut held = Vec::with_capacity(clones);
    for h in &windows[0..clones] {
        let before = Instant::now();
        let cloned = h.clone();
        let ns = before.elapsed().as_nanos();
        held.push(cloned);
        emit(start, "clones", "clone_latency_ns", ns);
    }
    emit(
        start,
        "clones",
        "held_seed_text_bytes",
        held.iter().map(|h| h.bytes() as u128).sum(),
    );
    hold();
    phase(131);
    drop(held);
    phase(3);
    emit(start, "clones-dropped", "held_seed_text_bytes", 0);
    hold();
    phase(132);
    drop(windows);
    phase(4);
    emit(start, "dropped", "retained_text_bytes", 0);
    hold();
    phase(5);
    emit(
        start,
        "throughput",
        "operations_per_second",
        (keys * turns) as u128 * 1_000_000_000 / operation_ns.max(1),
    );
    println!(
        "{{\"kind\":\"workload-complete\",\"operations\":{},\"clock_origin\":\"child-relative\"}}",
        keys * turns
    );
}
