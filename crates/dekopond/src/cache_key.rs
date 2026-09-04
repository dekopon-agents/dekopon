//! Minting the opaque identifier that points one set of model requests at one provider cache lane.
//!
//! A prompt cache key is a **routing hint and never an access-control boundary.** It tells a
//! provider which requests are likely to share a leading prefix so they can be served by one cache;
//! it grants nothing, isolates nothing, and a backend that ignores it returns the identical answer
//! at full price. Nothing in this daemon may ever read a shared key as a shared permission — every
//! message still opens its own attested broker leg and asks the broker again.
//!
//! # Why the key is minted rather than derived
//!
//! The obvious key for a conversation is the thing that already identifies its audience: a
//! canonical subject or service-native conversation identifier, or a hash of either. All are
//! refused here. A canonical subject can be a phone number (`tel.16035550100`), and a channel or
//! thread identifier can identify a small group, so sending either as a cache key would hand a model
//! provider extra identity for no benefit — the request routes the same either way. Hashing does
//! not fix it: a hash of a stable identifier is a stable pseudonym, which is exactly the linkability
//! this project declines to put in its own telemetry when `telemetryPayloads` is off, and it would
//! be worse to hand one to a third party. A configured salt is worse still, because it is a new
//! secret to manage whose only purchase is a pseudonym that survives restarts.
//!
//! So the identifier is minted from entropy and carries nothing at all. It rotates whenever the
//! thing it names is recreated — a conversation evicted for idleness, capacity, or a changed grant
//! comes back with a new one, and a restarted process shares nothing with the one before it — so it
//! never accumulates into a durable pseudonym. Its lifetime is exactly the window over which a
//! prefix is genuinely shared, which is also the only window in which it is useful.
//!
//! Entropy comes from [`IdSequence`], which is the workspace's existing answer to needing an
//! unguessable identifier without adding a dependency: an OS-seeded `RandomState` hasher mixed with
//! the process ID and a nanosecond wall-clock reading. The broker already trusts that construction
//! for invocation identifiers, where a collision is a replay-rejection failure; here a collision
//! costs a wasted cache lookup.

use dekopon_agent::IdSequence;

/// Prefix for a key naming one remembered conversation on one `persistent` route.
const CONVERSATION_PREFIX: &str = "dekopond-conversation";

/// Prefix for a key naming one bound route, shared by every sender that route answers.
const ROUTE_PREFIX: &str = "dekopond-route";

/// Mints the key for one conversation, created with the conversation and rotated with it.
pub(crate) fn for_conversation() -> String {
    mint(CONVERSATION_PREFIX)
}

/// Mints the key for one bound route, minted once at startup and shared by every sender on it.
///
/// Sharing one key across senders sounds alarming and is not, because of what the shared prefix on
/// a `oneShot` route actually is: the agent's instructions and the tool definitions, and then this
/// one message. Those instructions and tools are identical for every sender the route answers and
/// contain nothing about any of them, so routing the route's traffic to one cache lane shares a
/// prefix that was already common property. The per-message half of the request is never a cache
/// hit for anybody — a different sender's message diverges from the first token that differs, which
/// is why a key is a hint about a *prefix* rather than a handle on a response.
///
/// The alternative, a fresh key per message, would name a lane of exactly one request and defeat
/// the only caching a stateless route can get.
pub(crate) fn for_route() -> String {
    mint(ROUTE_PREFIX)
}

/// Derives one opaque identifier under `prefix`.
///
/// [`IdSequence::new`] rejects only a malformed prefix, and both prefixes here are crate constants
/// covered by a test, so the error branch is unreachable. It still degrades rather than panics: an
/// empty key is dropped by `CompletionOptions::with_prompt_cache_key`, which leaves the request
/// exactly as it would have been with no key at all. A message is the wrong place to abort over a
/// routing hint.
fn mint(prefix: &str) -> String {
    IdSequence::new(prefix).map_or_else(
        |_| String::new(),
        |identifiers| identifiers.trace().as_str().to_owned(),
    )
}
