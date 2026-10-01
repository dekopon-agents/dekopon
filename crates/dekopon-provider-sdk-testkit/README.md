# dekopon-provider-sdk-testkit

The typed test kit runs a provider against the same SDK dispatch in two modes:
`Native<P>` with injected ports and `Harness::<P>::get(path)` against a checked
WebAssembly component loaded by the real broker host. `conformance::<P>(path)`
checks the component manifest, help and usage, decoded imports against the
provider's declared `Needs`, and closed input schemas. `Run` can script a
loopback HTTPS origin, inject a guest clock and narrow host limits.

Storage and external components have no native typed provider; their fixture
integration tests live in `dekopon-broker-host/tests/fixture_host.rs` and use
exact authorized grants against the real storage host. Neither harness grants
production authority or evaluates policy.
