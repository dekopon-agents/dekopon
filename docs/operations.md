# Running Dekopon

**Status: current.** This is the operator's index, not a second copy of the manuals. Dekopon keeps
each implementation contract beside its code, so the authoritative text for running the privileged
broker is [`crates/dekopon-brokerd/README.md`](../crates/dekopon-brokerd/README.md) and for the
gateway it is [`dekopond.md`](dekopond.md). This page exists so an operator can find them by the
question they arrived with, rather than by guessing that a crate README is the operations manual.

One instruction is restated here because it is the most consequential in the project and must not be
reachable only by guessing.

## The audit chain and its checkpoint

> **A non-empty audit log with no checkpoint fails closed and requires explicit operator recovery
> from trusted copies. So does any checkpoint that is not an exact verified prefix of the audit
> file. Do not delete one file to make the broker start.**

`dekopon-brokerd` keeps a hash-linked JSONL audit chain and, in a separately locked file, a
checkpoint holding the retained record count and the SHA-256 chain head. At startup the checkpoint
must identify an exact prefix of the fully verified chain; that is what detects replacement,
truncation, and valid-prefix rollback. An audit exactly one record ahead of a valid checkpoint is the
recoverable crash window and is advanced before the broker listens. A larger gap is not.

Deleting the checkpoint does not repair the state — it destroys the evidence that would have told you
what happened. Recovery means restoring both files from copies you trust.

`dekopon-brokerd audit verify --audit-path <PATH>` runs that same chain check offline, against a
live log or a retained copy, without starting the broker.

Full mechanics, filesystem requirements, and the limits of local integrity evidence:
[`crates/dekopon-brokerd/README.md`](../crates/dekopon-brokerd/README.md#audit-checkpoint-and-recovery).

## By the question you arrived with

### Starting, stopping, and upgrading

| Question | Read |
|---|---|
| What files and directories must exist, and with what ownership and modes? | [`dekopon-brokerd` § Configuration](../crates/dekopon-brokerd/README.md#configuration) |
| How do I resolve, materialize, list, or verify a managed provider set? | [`dekopon-brokerd` § Managed provider sets](../crates/dekopon-brokerd/README.md#managed-provider-sets) — normal startup, `list`, and `verify` are offline; successful lock changes apply after restart |
| Is a retained audit log still intact? | [`dekopon-brokerd` § Verifying a chain offline](../crates/dekopon-brokerd/README.md#verifying-a-chain-offline) — `audit verify` reports the record count and head, or names the record that broke the chain |
| Why did a managed provider refuse to load? | The same section distinguishes desired references, the generated manifest/component lock, installed blob hygiene, and complete host validation. A digest proves bytes, not publisher provenance. |
| Why did the broker refuse to start? | [`dekopon-brokerd` § Configuration](../crates/dekopon-brokerd/README.md#configuration) for path and permission refusals; [`broker-http.md` § Startup validation](broker-http.md#startup-validation) for policy refusals |
| Why did the gateway refuse to start? | [`dekopond.md` § Startup fails closed](dekopond.md#startup-fails-closed) |
| The broker refuses to start naming `dev.*` subjects. | [`dekopon-brokerd` § Development identities](../crates/dekopon-brokerd/README.md#development-identities) — `allowDevelopmentSubjects` is off by default, and the refusal lists every offending entry at once |
| The console refuses to start naming a credential file. | [`cli.md` § The model credential](cli.md#the-model-credential) — it will not share the file the gateway rotates |
| The console says no broker was found. | It names the exact path and the discovery tier that produced it; candidates are never probed, so a stopped broker reports against the path it would have used |
| What does shutdown actually do, and how long may it take? | [`dekopon-brokerd` § Configuration](../crates/dekopon-brokerd/README.md#configuration) — signals, draining, and the grace that must cover one host deadline plus two frame deadlines |
| In what order do I restart the two daemons? | [`upgrading.md`](upgrading.md#restart-the-broker-first-and-stop-it-last) |
| This release changed configuration — what do I edit? | [`upgrading.md`](upgrading.md) |
| Can I run a newer broker against an older gateway? | No. [`broker-http.md` § Version and compatibility](broker-http.md#version-and-compatibility) |

### Authority, policy, and credentials

| Question | Read |
|---|---|
| Who may drive which agent, and where is that written? | [`dekopon-brokerd` § Policy](../crates/dekopon-brokerd/README.md#policy) |
| How narrowly does an authorized invocation actually run? | [`broker-http.md` § Broker HTTP enforcement](broker-http.md#broker-http-enforcement) |
| Where do legacy provider credentials live, and how are they bound to a destination? | [`broker-http.md` § Broker HTTP enforcement](broker-http.md#broker-http-enforcement) and [`dekopon-brokerd` § One capability, one token per agent](../crates/dekopon-brokerd/README.md#one-capability-one-token-per-agent) |
| How may an agent name a secret without seeing it, and which stores can back it? | [`secrets.md`](secrets.md) — DRNs, dual policy, private bindings, source adapters, path scope, bootstrap, rotation and reflection limits |
| Why did a DRN return `secret-denied`? | The same document: unknown, unbound, wrong-sink/username and policy-denied names intentionally share one result; inspect broker-side policy/map validation rather than probing names. |
| A grant looks right and every session is denied. | Check the agent name. [`broker-http.md` § Startup validation](broker-http.md#startup-validation) — agent literals are the one class that is not proved at startup |
| What does an agent's catalog entry actually decide? | [`catalog.md`](catalog.md) |
| How do I get a ChatGPT credential onto a host or into a pod? | [`chatgpt-credential.md`](chatgpt-credential.md), and [`1password-eso.md`](1password-eso.md) for the secret store |

### Seeing what is happening

| Question | Read |
|---|---|
| What do the traces, spans, and audit-safe logs contain? | [`observability.md`](observability.md) |
| What is the dashboard, and what does exposing it disclose? | [`dekopon-brokerd` § Read-only web UI](../crates/dekopon-brokerd/README.md#read-only-web-ui) |
| A client got a failure code — is it safe to resubmit? | [`broker-http.md` § Failure codes](broker-http.md#failure-codes) |
| An invocation may have taken effect and was not recorded. | `outcome-unaudited`, in the same table. The durable audit is the only record; do not resubmit under any identifier |

### Deploying

| Question | Read |
|---|---|
| Kubernetes | [`charts/dekopon/README.md`](../charts/dekopon/README.md) |
| The container image — what is in it and what does it assume? | [`container-image.md`](container-image.md) |
| Getting an ordinary daemon file or a projection-backed DRN source into a pod | [`1password-eso.md`](1password-eso.md) and [`secrets.md` § `kubernetesProjection`](secrets.md#kubernetesprojection) |
| Optional provider storage and durable chat memory | [`dekopon-brokerd` § Optional provider storage and chat memory](../crates/dekopon-brokerd/README.md#optional-provider-storage-and-chat-memory) |

## The boundaries an operator must not paper over

These are invariants, not defaults, and no operational convenience overrides them. The complete list
is [`dekopon-brokerd` § Boundaries](../crates/dekopon-brokerd/README.md#boundaries); the two that
most often come up while operating are:

- **The owner-only socket is one UID trust domain.** Every process running under that UID can act as
  its configured principal. An attestor grant buys attribution and deny-by-default scoping in that
  shape, not process separation. A dedicated gateway UID is committed direction, not current
  behavior.
- **The checkpoint is local integrity evidence, not tamper-proof storage.** It detects truncation and
  rollback relative to a retained checkpoint. Coordinated deletion of both files by whoever owns the
  host is not detectable from local state; retain or export checkpoint generations elsewhere if that
  is in your threat model.

[`security-model.md`](security-model.md) is the full statement of what is trusted, what is not, and
what is presently out of scope.
