# Security model

What each boundary protects, and what Dekopon deliberately doesn't. Decisions are in
[authorization](authorization.md).

## Trust boundaries

Trusted: the owner's broker configuration, Cedar policy, constraint sets, private secret map and
peer-UID mapping; the provider bytes the lock pins; the telemetry store. Untrusted: everything the
model writes, everything it reads (chat messages, pull-request text, web pages, provider output),
and the catalog's instructions and skills. Those are operator text the model reads in full, so
they're no place for a secret, and they grant nothing.

Identity comes from the kernel (the peer UID) and, through an attestor grant, from the chat service;
never from text.

## Current local process boundary

The chart runs the broker and the gateway under different UIDs that share only an IPC group
(`charts/dekopon/values.yaml`). The broker's socket is group-writable inside a directory only the
broker can write, so the gateway can connect but not replace the socket. The broker maps the
kernel's peer UID; the gateway checks the socket's owner and the server's live UID against the UID
it's configured to expect. Group membership grants reachability, not identity.

Each UID is one trust domain. Anything running as the broker UID holds every provider credential.
Anything running as the gateway UID reads conversation history and journals, holds the chat and
model keys, and attests for every subject its attestor grant covers, so narrow that grant to the
workspaces it serves. Run both daemons under one UID and the attestor grant gives attribution and
deny-by-default scoping, not separation.

## What each process holds

| Process | Holds | Never holds |
|---|---|---|
| `dekopon-gatewayd` | chat tokens, model keys, conversation history and journals, the model proxy's TLS key | provider credentials, policy, constraint sets |
| `dekopon-brokerd` | policy, constraint sets, peer and principal maps, provider credentials and the secret map, provider storage | the gateway's model keys, a model loop, chat tokens unless the owner also binds one as a provider credential |
| A Wasm provider | its input, in a fresh store per invocation | a credential: the host injects it after the request passes its checks |
| The model | prompts and command output | a credential or any authority |
| A VM behind the model proxy | its own work | a model key |

## Endpoint trust

The endpoint you authorize receives the credential; that's the point of sending it. The native HTTP
engine injects a credential only for a destination inside that credential's binding, rejects a
provider-supplied `authorization` header instead of overwriting it, and refuses a response that
contains the raw credential (`crates/dekopon-http-host/src/lib.rs`). Plaintext HTTP reaches only
loopback and hosts the owner lists as plaintext (`crates/dekopon-brokerd/src/config.rs`).

Allowlist a host that reflects headers back in another encoding (base64, JSON escapes) and the token
walks out through the provider's output to the model. Basic authentication hands the endpoint the
password. An endpoint that logs requests keeps your token. Pick your hosts, and give each credential
the narrowest upstream scope.

## Prompt injection

Dekopon doesn't detect prompt injection or make a model trustworthy. It shrinks what a successful
injection can reach: the broker authorizes each proposal for the person and agent the turn runs as,
under constraint sets no text can touch. Inside that envelope an injection can do anything the grant
allows.

The sharp case is an agent that can read something private and write somewhere public: injected text
can move one to the other. Give those capabilities to different agents, or fence the write with a
request `forbid`. In a mapped channel a script may act under any current participant's handle, so
text from one participant can steer the model into acting under another's grant. An unattended watch
probe can only read.

## Conversation memory as a trust surface

Memory never grants authority ([memory](memory.md)): replayed and recalled text is prompt text, and
each turn is authorized afresh. What memory widens is time and audience.

- **Time.** On a one-shot route an injected instruction dies with its session. With history it stays
  in every later prompt until the window, the idle timeout or retention drops it. Anything an agent
  writes to agent-scoped storage and reads back lasts until deleted, across sessions and people: the
  longest-lived injection surface.
- **Audience.** Shared scope replays each participant's text and the agent's answers into prompts
  others start. The chat service decides how wide a conversation is: a Discord message outside a
  thread shares the whole channel.
- **Revocation.** A changed grant drops history before the next turn, but the check compares
  capability names only, so tightening a constraint set keeps what was read under the looser one.
  Dropping history removes it from future prompts, not from the chat transcript or the telemetry
  store.
- **Residence.** History lives under the gateway UID, in memory and, with a journal, in 0600 files.
  The UID boundary separates it from the broker, not from other processes running as the gateway.

## Signed downloads

GitHub serves Actions job logs through a 302 to a signed storage URL, itself a bearer credential. A
capability whose constraint set enables signed downloads may follow that one hop. The API request
gets the usual checks. The storage request goes only to GitHub's compiled storage host families,
carries no credential, inherited header or `traceparent`, and its URL stays inside the native host
and out of traces. Any other redirect is returned unfollowed, with its query redacted.

Redaction replaces the signature and every long query value
(`crates/dekopon-http-host/src/signed_download.rs`). A shorter parameter that an unfollowed redirect
echoes into its body passes through.

## The guest model proxy trusts the jail to name its VM

The proxy accepts only a client certificate that chains to the configured CA and carries the
configured jail identity as a URI SAN. It then believes the VM subject the jail sends in a header.
The certificate identifies the jail, not a VM, so a guest that escapes into the jail process can
name any configured subject and spend that agent's token budget on that agent's models. That's
accepted: an escape already owns the jail.

The escape doesn't get the model key: the gateway injects it per request and forwards from the guest
only the body and three named headers. It gets no broker authority and reaches no provider. The
guest's body otherwise passes through as sent, so a VM can use your key for anything the upstream
API offers on a granted model. The budget bounds the spend.

## Threat-model limits

- Dekopon isn't production-hardened and has no multi-tenant transport. Per-agent credentials don't
  give each organization its own OS identity.
- A compromised gateway speaks for every subject inside its attestor namespaces. A `*` kind spans
  every kind its other fields admit: `drn:github:*:*:42` covers repository and login `42` as well
  as ids `42`.
- A GitHub name mapping follows the name: a repository later created under a renamed or deleted
  name speaks for that principal, even beside a mapped `repo-id`. Map only the `repo-id` form for
  a stable identity.
- The provider lock pins bytes by digest. A digest proves identity, not publisher, and there's no
  provenance check or revocation.
- Wasmtime bounds memory, fuel and time per invocation; it isn't claimed as a production sandbox.
- Dependency or compiler compromise, side channels, and denial of service from model endpoints or
  component compilation are out of scope.
- Audit lives in the trace and dies with the exporter ([trace as audit](design.md#trace-as-audit)).
- Whoever reads the telemetry store reads every conversation.
- If something hostile already runs as root or as the broker's user, you've lost.
- Gateway history in memory can reach swap and core dumps; the daemon controls neither.
- Chat memory with `continuityPolicy: stable`, and shared, agent and space storage always, stay
  readable after the capability, constraints or credential that wrote them change.
- A GitHub issue template can apply labels for an outsider, so never permit on a label. Logins are
  chosen by their owners; trust the numeric actor ID.
- A GitHub webhook delivery carries no signed timestamp, so a captured delivery can be replayed.
- The gateway's own chat calls (replies, reactions, downloads) are not Cedar-governed; Cedar governs
  provider effects.
- A provider credential is one account at its destination: every sender granted the capability acts
  as that account, a GitHub token's user for example.
- A fine-grained GitHub token limited to selected repositories still reads every public repository.
- A grant decides who may run a command, not who sees the answer: in a shared channel, every member
  reads the reply.
- A ChatGPT subscription credential calls an undocumented backend; the OpenAI API's retention terms
  don't cover it.
