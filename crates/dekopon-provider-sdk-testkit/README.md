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

`Native<P>` supplies a fixed monotonic reading (zero by default) and deterministic entropy (0xa5 bytes by default); `.monotonic(nanos)` and `.entropy(bytes)` override them for native tests only. Scripted entropy is consumed once; reading past its end panics. The real-component harness always uses the broker's actual invocation clock and OS entropy.

`Run::child(ChildScript { script, status, stdout, stderr, runs_for })` scripts
one expected child per call, in order. Unexpected scripts are typed
`HarnessError::Fixture` refusals for components; native fixture mistakes panic.
`ComponentOutput.children` records each child script and the first MiB of its
stdin as `ChildInput::None`, `Inherit`, or `Reader`. `Native::child` scripts the
same native `Port::spawn` path and `Native::children` returns the captured
runs. A scripted delay advances the native monotonic clock and runs on the
real host for a component.

Storage and external components have no native typed provider; their fixture
integration tests live in `dekopon-broker-host/tests/fixture_host.rs` and use
exact authorized grants against the real storage host. Neither harness grants
production authority or evaluates policy.
