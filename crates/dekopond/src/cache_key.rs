//! A prompt cache key is a routing hint only, never an access-control boundary; every message still
//! opens its own attested broker leg regardless of a shared key.

const CONVERSATION_PREFIX: &str = "dekopond-conversation";

const ROUTE_PREFIX: &str = "dekopond-route";

pub(crate) fn for_conversation() -> String {
    mint(CONVERSATION_PREFIX)
}

/// Sharing one cache key across every sender on a route is safe because the shared prefix is only
/// the common agent instructions and tools, never anything sender-specific.
pub(crate) fn for_route() -> String {
    mint(ROUTE_PREFIX)
}

/// If the OS will not supply entropy, this yields an empty key rather than a predictable one; an
/// empty key is dropped downstream, leaving the request as if none were set.
fn mint(prefix: &str) -> String {
    let mut bytes = [0_u8; 16];
    if let Err(error) = getrandom::fill(&mut bytes) {
        tracing::warn!(event = "gateway_cache_key_entropy_unavailable", error = %error);
        return String::new();
    }
    format!("{prefix}-{:032x}", u128::from_be_bytes(bytes))
}
