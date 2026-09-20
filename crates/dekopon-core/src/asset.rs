//! Content and host-call limits shared by the gateway, native host and guest SDK.
//!
//! Disk retention and spool budgets count stored bytes instead; these decoded content limits do not.

/// Maximum decoded bytes in one asset.
pub const MAX_DECODED_ASSET_BYTES: usize = 8 * 1024 * 1024;
/// Maximum cumulative decoded input or output bytes in one invocation.
pub const MAX_DECODED_INVOCATION_BYTES: usize = 40 * 1024 * 1024;
/// Maximum bytes copied by one asset host call.
pub const MAX_ASSET_CHUNK_BYTES: usize = 65_536;
