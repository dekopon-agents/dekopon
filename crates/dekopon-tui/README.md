# dekopon-tui

The session and rendering library for [`dekopon-console`](../dekopon-console/README.md). It runs a bounded `dekopon-agent` prompt loop over a `BrokerLeg` connected to an authenticated Unix peer, without broker policy, provider credentials, or a direct Wasm host. The broker decides every capability proposal; this library does not authorize subjects or tool effects. It is a private Dekopon workspace member using the workspace version and core path dependencies.

An agent is orchestration configuration, not a human identity. `OperatorProfile` binds an agent to a typed `ExternalSubject`; the old authored `scope` key is strictly refused. Only `--smoke-conversation` proposes a broker-derived synthetic scope. The broker must map/attest the console peer UID and authorize `agent.prompt` and its effective surface; authenticated console peers cannot attest real conversations. Neither a picker nor a profile authorizes an identity or effect.

The catalog provides safe instructions, already bounded mounted skills, and safe meta-tool metadata; the broker provides actual capabilities/command words. The shared loop's `read_skill` and `inspect_agent_config` are available, while improvement suggestions remain opt-out. `RecordingInvoker` forwards typed `SecretUseProposal`, provider command words, and script completion; it sees model-authored script arguments and provider output for local redacted panes, without becoming a second authorizer. `RecordingProgress` relays bounded tool/model metadata, not model text. `StopFlag` signals both the prompt probe and a per-turn `CancelSignal` in the broker leg. A broker-accepted effect is not rollbackable.

The console has no production history, delivery sink, or model-facing improvement suggestion sink. Its selected-agent-entry asset source accepts local decoded attachments and returned descriptors for later provider input. Absent IDs refuse before submission; no chat send/delivery occurs. Descriptors are released by the core leg. See the console README for credential isolation and source-backed broker integration fixture setup.

Every drawn field passes through terminal-control sanitization. The manual shell transcript is sanitised at render time; model/session machinery remains in the library but model turns and the call-tree pane are not exposed by this console UI. The terminal guard restores raw mode and the alternate screen on exit/error/panic. Entering an agent opens the shell ready to type. Commands and bounded interpreter results appear above the editable trailing prompt in a single wrapped viewport. Every completed command has an exit marker (including zero); output truncation is also marked. Each command is a separate script; variables do not carry over. This is Dekopon's bounded interpreter, not a container OS shell.

| Key | Action |
|---|---|
| `tab` / `shift-tab` | next / previous pane |
| `j` `k` / arrows | move agent selection |
| `enter` | enter selected agent; in shell, run command |
| `i` | resume typing in shell after leaving input |
| `page up` / `page down` | page wrapped shell rows, including while typing |
| `home` / `end` | jump to beginning / follow bottom of shell transcript |
| `esc` | request cooperative stop when busy; otherwise leave input |

| `?` / `q` | help / quit when not typing |

On compact Mac keyboards, Fn-Up/Down and Fn-Left/Right usually deliver PageUp/PageDown and Home/End. Terminal emulators can intercept these keys; Command-key shortcuts are not guaranteed to reach the app. Tab/Shift-Tab leave input and switch among Agents, Shell, Capabilities.

Licensed under either [Apache-2.0](../../LICENSE-APACHE) or [MIT](../../LICENSE-MIT).
