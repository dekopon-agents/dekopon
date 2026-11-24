use std::time::Duration;

pub struct Backoff {
    base: Duration,
    cap: Duration,
    jitter_limit: Duration,
    delay: Duration,
    failures: u32,
}

impl Backoff {
    pub const fn new(base: Duration, cap: Duration, jitter_limit: Duration) -> Self {
        Self {
            base,
            cap,
            jitter_limit,
            delay: base,
            failures: 0,
        }
    }

    pub fn record_failure(&mut self) {
        if self.failures != 0 {
            self.delay = self.delay.saturating_mul(2).min(self.cap);
        }
        self.failures = self.failures.saturating_add(1);
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }

    pub fn delay(&self, jitter: Duration) -> Duration {
        self.delay
            .min(self.cap)
            .saturating_add(jitter.min(self.jitter_limit))
    }

    pub fn reset(&mut self) {
        self.delay = self.base;
        self.failures = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_jitter_doubles_to_the_cap_and_resets_after_success() {
        let mut backoff = Backoff::new(
            Duration::from_millis(10),
            Duration::from_millis(35),
            Duration::from_millis(5),
        );
        for (failure, millis) in [(1, 10), (2, 20), (3, 35), (4, 35)] {
            backoff.record_failure();
            assert_eq!(backoff.failures(), failure);
            assert_eq!(backoff.delay(Duration::ZERO), Duration::from_millis(millis));
        }

        backoff.reset();
        backoff.record_failure();
        assert_eq!(backoff.failures(), 1);
        assert_eq!(backoff.delay(Duration::ZERO), Duration::from_millis(10));
    }

    #[test]
    fn jitter_is_bounded_and_added_after_the_cap() {
        let mut backoff = Backoff::new(
            Duration::from_millis(10),
            Duration::from_millis(15),
            Duration::from_millis(5),
        );
        backoff.record_failure();
        assert_eq!(
            backoff.delay(Duration::from_millis(3)),
            Duration::from_millis(13)
        );
        backoff.record_failure();
        assert_eq!(backoff.delay(Duration::MAX), Duration::from_millis(20));
    }
}
