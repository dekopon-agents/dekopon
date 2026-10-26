#[expect(dead_code, reason = "summary engine is staged before the B4 switch")]
pub(crate) mod engine;
pub(crate) mod generation;
pub(crate) mod journal;

#[expect(
    dead_code,
    reason = "record types are introduced before the v3 writer and engine are wired"
)]
pub(crate) mod record;
