#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        reason = "tests spawn and abort freely; production sites carry their own expectation"
    )
)]

mod budget;
mod meter;
mod metering;
mod usage;

#[cfg(test)]
mod tests;

pub use budget::{Budget, GuestRefusal, Refusal, Reservation, Retry};
pub use meter::{Meter, MeterKind, MeterSpec, MeterStatus, Tokens, UnixMillis, Verdict};
pub use metering::{
    Admission, Call, Clock, DEFAULT_OUTPUT_RESERVE, Estimate, HistoryRow, IMAGE_TOKENS, InputHint,
    Metering, Outcome, REASONING_OUTPUT_RESERVE, Sizes, Via,
};
pub use usage::ModelUsage;
