use super::Evaluator;
use dekopon_process::LimitExceeded;

pub(super) struct Nesting(u32);

impl Nesting {
    pub(super) fn root() -> Self {
        Self(0)
    }
    pub(super) fn fork(&self) -> Self {
        Self(self.0)
    }
}

impl Evaluator<'_> {
    pub(super) fn nested<T, E: From<LimitExceeded>>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, E>,
    ) -> Result<T, E> {
        if self.nesting.0 >= self.limits.max_recursion_depth {
            return Err(LimitExceeded::RecursionDepth {
                maximum: self.limits.max_recursion_depth,
            }
            .into());
        }
        let saved = self.nesting.0;
        self.nesting.0 += 1;
        let result = f(self);
        self.nesting.0 = saved;
        result
    }
}
