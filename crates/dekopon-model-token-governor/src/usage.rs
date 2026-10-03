/// Every field is None when the provider reported nothing rather than zero, since defaulting to
/// zero would misreport an unknown cost as a free one. A client normalizes before usage leaves it:
/// `input_tokens` includes cache reads and cache writes, and `output_tokens` includes reasoning.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModelUsage {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

impl ModelUsage {
    #[must_use]
    pub fn merged(self, later: Self) -> Self {
        Self {
            input_tokens: later.input_tokens.or(self.input_tokens),
            cached_input_tokens: later.cached_input_tokens.or(self.cached_input_tokens),
            cache_write_tokens: later.cache_write_tokens.or(self.cache_write_tokens),
            output_tokens: later.output_tokens.or(self.output_tokens),
            reasoning_output_tokens: later
                .reasoning_output_tokens
                .or(self.reasoning_output_tokens),
            total_tokens: later.total_tokens.or(self.total_tokens),
        }
    }
}
