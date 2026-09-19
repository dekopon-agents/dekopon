# dekopon-provider-http

Rust guest bindings for the buffered and streamed `dekopon:http@1.1.0` WebAssembly Component Model interface.

The crate is statically compiled into a provider component. It supplies request/response types and calls the component import; it does not contain a network client, TLS implementation, credentials, destination policy, or ambient I/O. Only `dekopon-brokerd` may implement the import and grant a request under an authorized invocation.

```rust,ignore
use dekopon_provider_http::{Header, Request, method};

let request = Request::new(method::POST, "https://api.example.test/items")?
    .with_header(Header::text("content-type", "application/json")?)
    .with_body(br#"{"name":"example"}"#.to_vec());
let response = dekopon_provider_http::send(request)?;
```

Methods are represented as validated HTTP tokens rather than a closed enum, so standard and extension methods are supported. Headers preserve order and duplicate names, and their values are byte sequences. Buffered request and response bodies are complete byte buffers. The broker independently enforces method, destination, header, host-call, byte, and time limits; constructing a guest request never grants authority.

`StreamedRequest` instead takes ordered `Part::literal(bytes)` and `Part::asset(&handle, encoding)`
segments. Handles and encodings come from `dekopon_provider_sdk::asset`; the same resource type is
used by both crates. `stream(request)` returns a `StreamedResponse` with a response body `Handle`.
The broker composes the request at exact length, converts asset encodings on the fly, and spools
and echo-scans the response before exposing it. Only literal segments count against the HTTP
request byte limit; asset and invocation limits still apply. Streaming requires a configured
broker asset directory. Neither request construction nor opening a handle grants HTTP authority.
