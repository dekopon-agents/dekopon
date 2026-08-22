# dekopon-provider-http

Rust guest bindings for the buffered `dekopon:http@1.0.0` WebAssembly Component Model interface.

The crate is statically compiled into a provider component. It supplies request/response types and calls the component import; it does not contain a network client, TLS implementation, credentials, destination policy, or ambient I/O. Only `dekopon-brokerd` may implement the import and grant a request under an authorized invocation.

```rust,ignore
use dekopon_provider_http::{Header, Request, method};

let request = Request::new(method::POST, "https://api.example.test/items")?
    .with_header(Header::text("content-type", "application/json")?)
    .with_body(br#"{"name":"example"}"#.to_vec());
let response = dekopon_provider_http::send(request)?;
```

Methods are represented as validated HTTP tokens rather than a closed enum, so standard and extension methods are supported. Headers preserve order and duplicate names, and their values are byte sequences. Request and response bodies are complete byte buffers. The broker independently enforces method, destination, header, host-call, byte, and time limits; constructing a guest request never grants authority.
