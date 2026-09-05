//! A scripted loopback HTTP endpoint for transport tests.
//!
//! Both transports in this crate are ureq clients, and the behavior worth pinning is what they put
//! on the wire and what they do with what comes back — including a connection that dies mid-flight,
//! which no in-process fake can produce.

use std::{
    io::{BufRead as _, BufReader, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use serde_json::Value;

/// One scripted reply, consumed in order by [`MockServer`].
pub(crate) struct MockResponse {
    status: u16,
    content_type: &'static str,
    body: String,
    /// Closes the connection after reading the request instead of answering, which is what a
    /// dropped packet or a reset TLS session looks like to the client.
    hang_up: bool,
}

impl MockResponse {
    pub(crate) fn json(body: Value) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body: body.to_string(),
            hang_up: false,
        }
    }

    pub(crate) fn sse(body: &str) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            body: body.to_owned(),
            hang_up: false,
        }
    }

    /// A failure status carrying a body, so a test can assert the body reaches the error.
    pub(crate) fn failure(status: u16, body: Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.to_string(),
            hang_up: false,
        }
    }

    pub(crate) fn raw_failure(status: u16, body: String) -> Self {
        Self {
            status,
            content_type: "application/json",
            body,
            hang_up: false,
        }
    }

    pub(crate) fn hang_up() -> Self {
        Self {
            status: 0,
            content_type: "text/plain",
            body: String::new(),
            hang_up: true,
        }
    }
}

pub(crate) struct MockServer {
    address: String,
    pub(crate) requests: Arc<Mutex<Vec<String>>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockServer {
    pub(crate) fn start(responses: Vec<MockResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock endpoint");
        let address = listener
            .local_addr()
            .expect("mock endpoint address")
            .to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let thread_requests = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                thread_requests.lock().expect("request lock").push(request);
                if response.hang_up {
                    continue;
                }
                write!(
                    stream,
                    "HTTP/1.1 {} OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.status,
                    response.content_type,
                    response.body.len(),
                    response.body
                )
                .expect("write response");
            }
        });
        Self {
            address,
            requests,
            handle: Some(handle),
        }
    }

    pub(crate) fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Every request the endpoint received, in order.
    pub(crate) fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("request lock").clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        // A test that fails early leaves part of the script unconsumed and the endpoint parked in
        // `accept`. Joining then would hang the whole suite instead of reporting the failure, so
        // the remaining slots are retired with throwaway connections the reader treats as EOF.
        while !handle.is_finished() {
            drop(TcpStream::connect(&self.address));
            thread::sleep(Duration::from_millis(10));
        }
        handle.join().expect("mock server thread");
    }
}

/// Reads one whole request, or `None` when the peer closed without sending one.
fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream.try_clone().expect("clone request stream"));
    let mut request = String::new();
    let mut content_length = 0_usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        request.push_str(&line);
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().expect("content length");
        }
        if line == "\r\n" {
            break;
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).ok()?;
    request.push_str(&String::from_utf8(body).expect("UTF-8 request"));
    Some(request)
}
