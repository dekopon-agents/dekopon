# Authorization

How `dekopon-brokerd` decides whether an action happens and how far it reaches
(`crates/dekopon-policy`, `crates/dekopon-broker`).

## Cedar decides who; constraint sets decide how far

Cedar answers one question: may this principal take this action on this resource, in this context?
The owner's constraint sets answer the rest: which hosts and methods, how many requests and bytes,
the timeout, the credential, the storage scope. Policy can't reach them. A Cedar decision is allow
or deny and nothing else, so a policy edit can broaden who may act but can't widen a timeout, add a
host or bind a credential.

A declarative policy language suits "who may do what" and is the wrong place for a byte ceiling.
Constraint sets are checked at startup against what Cedar never sees: the loaded provider manifests,
the host's own ceilings and the credential store.

Three broker rules sit outside both layers, and no policy overrides them. A capability with no
constraint set is denied before Cedar runs. A watch probe, and any job it starts, may only read,
because it runs unattended on every tick. A storage capability needs a chat scope; agent-scoped
storage runs only outside a mapped channel, and space-scoped storage only inside one.

Cedar validates types, not instances, so the broker refuses a policy naming a principal, group or
secret it didn't declare. Agents aren't declared (the catalog belongs to the gateway), so a
misspelled agent loads and matches nothing.

## Subjects, attestation and principals

The gateway relays who the chat service said someone is. The broker authenticates the gateway by its
Unix peer UID; a peer the owner gave an attestor grant may attach an attestation to a request: a
subject such as `drn:slack:user:t0123abc:u9xyz`, the agent answering and the chat scope. A GitHub
subject always arrives as its name and its numeric id. The broker checks that the grant covers
every form sent, then maps them to one principal through the owner's subject map
(`crates/dekopon-brokerd/src/config.rs`). An unmapped subject resolves to nothing; two forms
mapped to different principals are refused as `split-subject`.

Policy sees the attestor's principal as `context.via` and the agent as `context.agent`, and every
action requires both. A direct peer has no `via`, so its request fails Cedar's schema check: every
capability runs inside an attested agent session. Pinning `context.via` means a second gateway
inherits no grant written for the first.

A channel subject may map to a principal of kind `space`. Then each decision in that channel must
allow both the person and the space, and a wake created in the space runs as the space alone.

## `agent.prompt`

Driving an agent is its own statement: action `Dekopon::Action::"agent.prompt"` on resource
`Dekopon::Agent::"<id>"`. The broker checks it on every attested request, so a sender the owner
never allowed is refused before the gateway calls a model. Capabilities imply no session, and a
session no capabilities.

## Conversation attributes

Policy sees where a message arrived as `context.conversation`
([schema](../crates/dekopon-policy/src/lib.rs)): `kind`, `container`, `id` and `thread`. `container`
and `thread` are optional, so a policy that reads either guards with `has` or fails to load. Each
transport fills them:

| Transport | `container` | `id` | `thread` |
|---|---|---|---|
| [Slack](../crates/dekopon-gatewayd/src/transport/slack.rs) | team | channel | thread root timestamp (a top-level message is its own root); none in classic DMs |
| [Discord](../crates/dekopon-gatewayd/src/transport/discord.rs) | guild; none in a DM | channel or thread parent | the thread's channel id |
| [Telegram](../crates/dekopon-gatewayd/src/transport/telegram.rs) | none | chat | topic |
| [WhatsApp](../crates/dekopon-gatewayd/src/transport/whatsapp.rs) | `waba_id:phone_number_id` | sender | none |
| [GitHub](../crates/dekopon-gatewayd/src/transport/github.rs) | owner | `owner.repo` | issue, pull request or discussion number, else [`repository`](../crates/dekopon-core/src/github.rs) |
| [Fauxlack](../crates/dekopon-gatewayd/src/transport/fauxlack.rs) | server | channel | thread |
| [local](../crates/dekopon-gatewayd/src/transport/local.rs) | the caller's; default none | the caller's; default `dev` | none ([validated](../crates/dekopon-broker-protocol/src/conversation.rs)) |

This `forbid` limits pull-request comments to one Slack thread:

```cedar
@id("comments-stay-in-one-thread")
forbid(principal, action == Dekopon::Action::"gh.pull-request.comment", resource)
unless { context has transportKind && context.transportKind == "slack"
      && context has conversation && context.conversation has container
      && context.conversation has thread
      && context.conversation.container == "t0123abcd"
      && context.conversation.id == "c0123abcd"
      && context.conversation.thread == "1761900000.000100" };
```

A timestamp is unique only within a channel, so the fence pins the channel too. It
fences that capability only, not the agent's Slack replies.

## `secret.use` and public DRNs

A DRN (`drn:…`) publicly names a secret; knowing one grants nothing. The model passes it on a
provider command's argv; the command step returns it as a typed proposal beside the capability and
input, never inside the invoke input. A constraint set may also name a default DRN for proposals
that name none.

Using it takes three allows:

1. the ordinary capability decision;
2. a separate `secret.use` decision on `Dekopon::Secret::"<drn>"`, whose context names the exact
   capability, provider and sink;
3. an owner binding in the private secret map that fixes the capability, sink, hosts, methods, path
   rules, query and injection count, inside the capability's own HTTP authority.

Every refusal looks the same, so guessing names reveals nothing about the map, though a DRN's public
path can reveal what the secret is for.

After the decision record is emitted, the broker resolves the DRN once, for this invocation, from
the store its map entry names, such as a file, a Kubernetes projection, Vault, 1Password Connect or
a cloud secret manager (`crates/dekopon-brokerd/src/secrets.rs`). The bytes go only into an HTTP
`Authorization` header, Bearer or Basic, never a URL, body or environment; the invoked provider sees
neither name nor value. The store is read per invocation: a rotated secret applies at once, a
deleted one fails closed. You write the private map: an owner-only file the gateway can't read.

## Each outbound request is checked again

A capability decision says nothing about what the provider will send, so every HTTP request passes,
in order, the constraint set's method, host, count and byte limits; a second Cedar decision on the
same principal, action and context plus `context.request`; and then credential injection
(`crates/dekopon-http-host/src/lib.rs`). `context.request` holds the method, host, path, query and
the provider's headers, plus the parsed body when it's a small buffered JSON object
(`crates/dekopon-policy/src/request.rs`). Each request decision is its own audit record.

Only a `forbid` whose first condition is `when { context has request }` may read `context.request`;
any other policy that reads it refuses to load. The request layer can therefore only narrow what the
capability decision allowed. A body Cedar can't read, because it's streamed, not JSON, too large or
not representable, enters as an opaque string, so a forbid that tests a body field errors and
denies. Write body fences as allowlists with `unless`: a JSON `null` member is dropped and `==`
across types is false, so a `when … !=` fence lets both through.

The check sees what the provider sends, not what the API does with it. The token's own scope
upstream stays the final boundary.

## Example

Alice may drive `reviewer`, which reads pull requests and comments on them, and its comments land
only in `dekopon-agents` repositories:

```cedar
@id("alice-may-prompt-reviewer")
permit(principal == Dekopon::Principal::"alice",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"reviewer")
when { context.via == "dekopon-gatewayd" };

@id("reviewer-reads-and-comments")
permit(principal == Dekopon::Principal::"alice",
       action in [Dekopon::Action::"gh.pull-request.read",
                  Dekopon::Action::"gh.pull-request.comment"],
       resource == Dekopon::Provider::"gh")
when { context.via == "dekopon-gatewayd" && context.agent == "reviewer" };

@id("comments-stay-in-dekopon-agents")
forbid(principal, action == Dekopon::Action::"gh.pull-request.comment", resource)
when { context has request }
unless { context.request.url.path like "/repos/dekopon-agents/*" };
```

Nothing here can widen the hosts and methods the comment's constraint set allows; the third
statement only narrows each request. A direct peer, another gateway or another agent matches neither
permit.
