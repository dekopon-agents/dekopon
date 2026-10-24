# Memory lab (not for merge)

Runs the real `dekopon-brokerd` and `dekopon-gatewayd` from any ref, drives scripted
conversations through the gateway's local transport and collects plain files for hand analysis.
It lives on the `lab/memory-unify` branch only and is outside every workspace and CI gate.

## Run

```console
lab/memory/run.sh <ref> <scenario> [--profile fake|real|codex]
```

Needs Linux, Docker (a running daemon), the repository's Rust toolchain, Python 3, `setpriv`
and `curl`. Run as root to get separate daemon UIDs; otherwise both daemons share the caller's UID
and `summary.md` says so.

One run:

1. Resolves `<ref>` to a SHA (fetching it from `origin` if needed), checks it out as a detached
   worktree under `lab/memory/.work/src/<sha>` and builds both daemons into
   `lab/memory/.work/target` (debug by default; `LAB_BUILD=release` for release).
2. Copies the binaries and the ref's `cli-probe` component into a private state directory
   (`/var/tmp/dekopon-lab/<run>`, removed afterwards) and records their SHA-256s.
3. Starts an OpenTelemetry collector in Docker (`otel/opentelemetry-collector-contrib:0.115.1`,
   pinned by digest, host network, `LAB_OTLP_PORT` default 4318) with a file exporter.
4. Starts `dekopon-brokerd` as `lab-broker` and `dekopon-gatewayd` as `lab-gateway`, sharing the
   `lab-ipc` group for the broker socket (users are created on first run).
5. Runs the scenario, stops the gateway, copies its journal, stops everything and writes
   `summary.md`.

`python3 lab/memory/report.py <run dir>` rebuilds `summary.md` from the other files.

## Configuration under test

One agent, one local transport, two models on the fake (`chat`, and `summarizer` for the summary
routine, 20 s timeout) and journal memory on, with three routes selected by conversation ID:

| Conversation | Memory |
|---|---|
| `summary` | `recall: journal`, policy retaining `original` and a `progressiveOneShot` summary (`core` 256 B, `detail` 1 KiB), fixed selection of the summary |
| `plain` | `recall: journal`, default policy (original only, no model call) |
| `none` | `recall: none` |

## Scenarios

| Scenario | What it drives |
|---|---|
| `baseline` | 30 turns on each route, round-robin; every fifth turn runs `probe upper` through the broker |
| `flood` | 20 turns on `summary`, each a bash tool call printing 1,800 lines, about 290 KB (the shell keeps at most 256 KiB and 2,000 lines, and clamps any one line to half that) |
| `summarizer-hang`, `summarizer-fail`, `summarizer-refuse` | 10 turns on `summary` whose summaries hang until the client gives up, return 503, or refuse; each followed by a `none` turn |
| `restart-mid-summary` | 3 turns, a fourth whose summary hangs, `kill -9` of the gateway while it runs (journal copied to `journal-at-kill/`), restart, 3 more turns; reports whether the killed turn's summary was retried |
| `unloadable-journal` | 4 turns on `summary` and `plain`, stop, overwrite the smallest journal file with garbage (copy in `journal-before-corrupt/`), boot, 3 more turns each; expects one `gateway_journal_reset` and replies |
| `summaries-off-vs-on` | 20 turns on `summary` with the summary routine removed, then the gateway restarted with it on, a fresh journal and the same 20 turns |
| `config-check` | writes the configs and runs `dekopon-gatewayd check` on them; starts no daemon |

Every scenario runs `dekopon-gatewayd check` first (`gatewayd.check.log`) and stops if it fails.

A check whose behaviour the ref does not have reports `not applicable at <ref8>` instead of a pass:
slot resume applies only when the gateway logs an event or writes a file whose name mentions a slot.

## Output

`lab/memory/runs/<utc>-<ref8>-<scenario>/` (git-ignored):

| File | Content |
|---|---|
| `summary.md` | SHA, profile, counts, reply latency with and without a summary in flight, peak RSS, journal bytes, verdicts |
| `meta.json` | ref, SHA, build, UIDs, status, scenario findings, real-profile spend |
| `latency.csv` | per turn: sent and reply time, wall ms, outcome, `summary_in_flight` (a summary of the same conversation ran while the turn was outstanding) and `any_summary_overlap` |
| `replies.jsonl` | every reply line and up to eight non-reply lines per turn |
| `model-calls.jsonl` | every model request the fake served: model, tag, bytes, status, start/end |
| `traces.jsonl`, `logs.jsonl` | the collector's OTLP JSON, one export batch per line, both daemons |
| `gatewayd.stdout.log`, `gatewayd.stderr.log`, `brokerd.*` | daemon output, appended across restarts |
| `rss.csv` | per second per daemon: VmRSS, VmHWM, threads, CPU ticks from `/proc` |
| `events.jsonl` | the lab's own timeline: daemon start, stop, kill, journal snapshot, corruption |
| `journal/`, `journal-*/` | journal copies at the end and at the scenario's cut points |
| `config/` | the generated broker, gateway, catalog and policy files (paths only, no credential) |
| `artifacts.sha256`, `collector.log` | the binaries tested and the collector's own log |

## Models

`fake` (default) is `fakemodel.py`, a stdlib OpenAI-compatible server in the lab process. The
driver puts `[lab:key=value;...]` at the start of each user message and the fake reads it per
request: `act` (`answer`, `tool-big`, `tool-probe`, `fail` (503), `hang`, `refuse`) and `lat`
(ms) for the chat model; `sum` (`ok`, `hang`, `fail`, `refuse`) and `slat` (ms, default 1500)
for the summarizer, which finds the tag inside the source it is asked to summarize.

`real` forwards the non-scripted calls to OpenRouter (`LAB_REAL_MODEL`, default
`openai/gpt-4o-mini`) with `OPENROUTER_API_KEY` read from the lab's environment only; the
gateway gets a placeholder key and talks to the local proxy. Each request is capped at 1024 output
tokens and refused with 402 once the token-priced spend, or the next request's worst case, would
pass $2; the run then ends as `aborted: cost cap reached`. Scripted failures stay on the fake.

`codex` makes the chat model dekopon's own `chatgptSubscription` client (`LAB_CODEX_MODEL`,
default `gpt-5-codex`). The lab copies the file named by `LAB_CODEX_AUTH_JSON` into the private
state directory (0600, gateway UID) and points `authFile` at the copy; it never prints or commits
it, and the state directory is removed after the run. The refresh token rotates, so if the
gateway refreshed the copy, the lab writes it back over the original (temporary file, 0600, the
original's owner, rename) when the daemons stop; a run killed before that leaves the original
holding a retired token. Use a separate eval login. The cap is a turn budget,
`LAB_CODEX_MAX_TURNS` (default 120): the turn past it ends the run as `aborted: turn budget of
N reached`. The summarizer stays on the fake, so its scripted behaviours still apply, unless
`LAB_CODEX_SUMMARIZER=1` puts it on Codex too; chat-side scripted failures
(`act=fail|hang|refuse`) do not exist on this profile. The gateway is given the caller's proxy
variables.

## Lab overlay: extra CA

Dekopon's model clients trust only the webpki roots and ignore proxy variables. In a container
whose direct HTTPS is intercepted by an egress gateway, every real model call then fails with
`model transport failed: HTTP: error sending request`. Setting `LAB_EXTRA_CA=<pem bundle>` builds
the ref with `overlay/extra-ca.patch` applied to a separate worktree and target directory
(`.work/src/<sha>-extra-ca-<patch12>-<ca12>`, `.work/target-extra-ca`), with the bundle compiled
in as `crates/dekopon-model/lab-extra-ca.pem`:

- the reqwest client in `InferenceHttp::new` adds every certificate in the bundle beside its
  webpki roots;
- the ureq agent in `agent_from` uses exactly the bundle's certificates, because ureq cannot add
  roots to its compiled-in webpki set, so pass a full bundle (public roots plus the egress CA,
  like `/root/.ccr/ca-bundle.crt`), not the egress CA alone.

A run built this way says `lab overlay: extra CA` with the patch and bundle SHA-256s in
`summary.md` and `meta.json`; a stock run says `none (stock code)`. The patch is never applied to
a dekopon branch. `LAB_FAKE_TLS_CERT` and `LAB_FAKE_TLS_KEY` serve the fake model over HTTPS,
which is how the overlay is checked without a real endpoint.

`traces.jsonl` is the collector's unmodified OTLP JSON: each span keeps `traceId`, `spanId`,
`parentSpanId`, `name`, `kind`, start and end times, attributes, events and status.
`summary.md` reports roots and spans whose parent was never exported (expected only around a
`kill -9`).

## What the lab cannot see

- ARM64 and the Pi's memory-optimized Wasmtime build: this is stock Wasmtime on x86-64.
- Streaming model responses: both models use `stream: false`.
- Chat platform transports (Slack, Discord, WhatsApp) and `platform` recall: only the local
  transport is driven.
- Debug-build latency and RSS are not release numbers; use `LAB_BUILD=release` for those.

Remove the build worktrees with `git worktree remove lab/memory/.work/src/<sha>`.
