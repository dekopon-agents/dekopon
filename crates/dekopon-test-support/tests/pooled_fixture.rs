//! `LoopbackServer::pooled` serves both outcomes of a pooling client's connection race.
//!
//! The second one used to cost a full `FIXTURE_TIMEOUT`: the fixture sat in a blocking read on
//! the connection the client had abandoned, and only the read deadline freed it to accept the
//! connection the client was actually waiting on. A caller with a deadline of its own then had a
//! coin flip on its hands, which is what these two elapsed-time bounds are here to keep fixed.

use std::{
    io::{Read as _, Write as _},
    net::TcpStream,
    time::{Duration, Instant},
};

use dekopon_test_support::LoopbackServer;

const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";

/// Well under the five-second wedge, and far above the two milliseconds either path really takes.
const PROMPTLY: Duration = Duration::from_secs(2);

fn exchange(stream: &mut TcpStream, path: &str) -> String {
    write!(stream, "GET {path} HTTP/1.1\r\nHost: fixture\r\n\r\n").expect("write fixture request");
    stream.flush().expect("flush fixture request");
    let mut buffer = [0_u8; 512];
    let read = stream.read(&mut buffer).expect("read fixture response");
    String::from_utf8_lossy(&buffer[..read]).into_owned()
}

#[test]
fn answers_a_reused_connection() {
    let server = LoopbackServer::pooled(RESPONSE, 2);
    let started = Instant::now();
    let mut connection = TcpStream::connect(server.authority()).expect("connect");
    assert!(exchange(&mut connection, "/one").starts_with("HTTP/1.1 200 OK"));
    assert!(exchange(&mut connection, "/two").starts_with("HTTP/1.1 200 OK"));
    let elapsed = started.elapsed();
    server.join();
    assert!(elapsed < PROMPTLY, "a reused connection waited {elapsed:?}");
}

#[test]
fn answers_a_second_connection_without_waiting_out_the_first() {
    let server = LoopbackServer::pooled(RESPONSE, 2);
    let started = Instant::now();
    let mut first = TcpStream::connect(server.authority()).expect("connect first");
    assert!(exchange(&mut first, "/one").starts_with("HTTP/1.1 200 OK"));
    // The lost race: the client's second request goes out on a new connection while the first is
    // still open and idle, so nothing more will ever arrive on the one the fixture is holding.
    let mut second = TcpStream::connect(server.authority()).expect("connect second");
    assert!(exchange(&mut second, "/two").starts_with("HTTP/1.1 200 OK"));
    let elapsed = started.elapsed();
    server.join();
    assert!(elapsed < PROMPTLY, "a second connection waited {elapsed:?}");
}
