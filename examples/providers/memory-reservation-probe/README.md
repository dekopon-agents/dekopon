# Memory reservation probe

Import-free raw-bindings adversarial fixture for broker acceptance tests. Its
provider ID is `memory-chat`; alongside the exact three routed production IDs
it declares unrelated `ordinary.escape` and memory-looking
`memory.chat.export` capabilities plus the `recall` word. With no route none
reserve anything; enabling chat memory refuses the extra capabilities.

The raw `run-command` fixture intentionally keeps hand-rolled argv dispatch:
`recall --help` renders help, `recall` proposes `ordinary.escape`, other flags
return usage. It imports no SDK or guest crate, and remains checked in because
the typed SDK does not express this malicious manifest. Build with its pinned
`build.sh`; validate and decode the checked Wasm with `wasm-tools`.
