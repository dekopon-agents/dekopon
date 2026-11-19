use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Backoff {
    base: Duration,
    cap: Duration,
    max_jitter: Duration,
    next: Duration,
}

impl Backoff {
    #[must_use]
    pub fn new(base: Duration, cap: Duration, max_jitter: Duration) -> Self {
        Self {
            base: base.min(cap),
            cap,
            max_jitter,
            next: base.min(cap),
        }
    }

    #[must_use]
    pub fn next_delay(&mut self, jitter: Duration) -> Duration {
        let delay = self.next.saturating_add(jitter.min(self.max_jitter));
        self.next = self.next.saturating_mul(2).min(self.cap);
        delay
    }

    pub fn reset(&mut self) {
        self.next = self.base;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_from_the_base() {
        let mut backoff = Backoff::new(
            Duration::from_millis(3),
            Duration::from_secs(1),
            Duration::ZERO,
        );
        let delays = std::array::from_fn::<_, 4, _>(|_| backoff.next_delay(Duration::ZERO));
        assert_eq!(delays.map(|delay| delay.as_millis()), [3, 6, 12, 24]);
    }

    #[test]
    fn growth_caps_and_saturates_without_overflow() {
        let mut backoff = Backoff::new(
            Duration::from_millis(3),
            Duration::from_millis(10),
            Duration::ZERO,
        );
        let delays = std::array::from_fn::<_, 4, _>(|_| backoff.next_delay(Duration::ZERO));
        assert_eq!(delays.map(|delay| delay.as_millis()), [3, 6, 10, 10]);
        let mut backoff = Backoff::new(Duration::MAX, Duration::MAX, Duration::MAX);
        assert_eq!(backoff.next_delay(Duration::MAX), Duration::MAX);
        assert_eq!(backoff.next_delay(Duration::ZERO), Duration::MAX);
    }

    #[test]
    fn a_success_reset_restores_the_base() {
        let mut backoff = Backoff::new(
            Duration::from_millis(3),
            Duration::from_millis(10),
            Duration::ZERO,
        );
        for _ in 0..4 {
            let _delay = backoff.next_delay(Duration::ZERO);
        }
        backoff.reset();
        assert_eq!(backoff.next_delay(Duration::ZERO), Duration::from_millis(3));
        assert_eq!(backoff.next_delay(Duration::ZERO), Duration::from_millis(6));
    }

    #[test]
    fn bounded_jitter_is_added_after_the_cap() {
        let mut backoff = Backoff::new(
            Duration::from_millis(10),
            Duration::from_millis(10),
            Duration::from_millis(5),
        );
        for (jitter, expected) in [(0, 10), (3, 13), (5, 15), (99, 15)] {
            assert_eq!(
                backoff.next_delay(Duration::from_millis(jitter)),
                Duration::from_millis(expected)
            );
        }
    }

    #[test]
    fn zero_jitter_is_deterministic() {
        let mut first = Backoff::new(
            Duration::from_millis(3),
            Duration::from_millis(10),
            Duration::ZERO,
        );
        let mut second = first.clone();
        for _ in 0..8 {
            assert_eq!(
                first.next_delay(Duration::from_secs(1)),
                second.next_delay(Duration::ZERO)
            );
        }
    }
}
