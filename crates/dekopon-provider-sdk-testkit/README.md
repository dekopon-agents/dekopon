# dekopon-provider-sdk-testkit

The typed test kit runs a provider against the same SDK dispatch in two modes:
`Native<P>` with injected ports and `Harness::<P>::get(path)` against a checked
WebAssembly component loaded by the real broker host. `conformance::<P>(path)`
checks the component manifest, help and usage, decoded imports against the
provider's declared `Needs`, and closed input schemas. `Run` can script a
loopback HTTPS origin, inject a guest wall clock and narrow host limits. It can also
close the real stdout reader after a bounded prefix (`close_stdout_after(0)`
closes it before invocation) to assert early producer exit without buffering
the rest of the stream.

`Native<P>` supplies a fixed monotonic reading (zero by default) and deterministic entropy (0xa5 bytes by default); `.monotonic(nanos)` and `.entropy(bytes)` override them for native tests only. Scripted entropy is consumed once and exhaustion returns `RandomError::SourceUnavailable`. The real-component harness always uses the broker's actual invocation clock and OS entropy.

Storage and external components have no native typed provider; their fixture
integration tests live in `dekopon-broker-host/tests/fixture_host.rs` and use
exact authorized grants against the real storage host. Neither harness grants
production authority or evaluates policy.
