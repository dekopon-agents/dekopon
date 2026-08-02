//! Hand-built sandbox bounds for the tree-walking evaluator.
//!
//! This interpreter is native Rust, not Wasm: there is no fuel meter, no linear-memory ceiling, and
//! no engine-level deadline to fall back on. Every bound a script can exhaust is owned here and
//! enforced from the evaluator.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// Default statement/loop/call budget for one script.
pub const DEFAULT_MAX_STEPS: u64 = 100_000;
/// Default shell-function call-stack depth.
pub const DEFAULT_MAX_RECURSION_DEPTH: u32 = 64;
/// Default accumulated output ceiling in bytes.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// Default accumulated output ceiling in lines.
pub const DEFAULT_MAX_OUTPUT_LINES: usize = 2_000;
/// Default wall-clock deadline for one script.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default number of capability invocations one script may drive.
pub const DEFAULT_MAX_CAPABILITY_CALLS: u32 = 32;

/// How often the deadline is re-read, in steps.
///
/// The deadline is checked cooperatively during evaluation rather than from a watchdog thread:
/// Phase 1 has no async story, and a native tree walk has no safe point to interrupt from outside.
const DEADLINE_CHECK_INTERVAL: u64 = 128;

/// Configurable execution bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    /// Statements, loop iterations, and function calls one script may execute.
    pub max_steps: u64,
    /// Maximum nested shell-function calls.
    pub max_recursion_depth: u32,
    /// Maximum accumulated output bytes.
    pub max_output_bytes: usize,
    /// Maximum accumulated output lines.
    pub max_output_lines: usize,
    /// Wall-clock deadline for the whole script.
    pub timeout: Duration,
    /// Maximum capability invocations one script may drive.
    pub max_capability_calls: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_steps: DEFAULT_MAX_STEPS,
            max_recursion_depth: DEFAULT_MAX_RECURSION_DEPTH,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_output_lines: DEFAULT_MAX_OUTPUT_LINES,
            timeout: DEFAULT_TIMEOUT,
            max_capability_calls: DEFAULT_MAX_CAPABILITY_CALLS,
        }
    }
}

/// A limit a running script exhausted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitExceeded {
    /// The statement/loop/call budget ran out.
    Steps {
        /// Configured budget.
        maximum: u64,
    },
    /// Shell functions nested too deeply.
    RecursionDepth {
        /// Configured depth cap.
        maximum: u32,
    },
    /// The script exceeded its wall-clock deadline.
    Deadline {
        /// Configured deadline in milliseconds.
        timeout_ms: u128,
    },
    /// The script drove too many capability invocations.
    CapabilityCalls {
        /// Configured call cap.
        maximum: u32,
    },
}

/// Mutable per-execution counters for one script run.
#[derive(Debug)]
pub struct Budget {
    limits: Limits,
    started: Instant,
    steps: u64,
    depth: u32,
    capability_calls: u32,
}

impl Budget {
    /// Starts a fresh budget and its wall clock.
    #[must_use]
    pub fn start(limits: Limits) -> Self {
        Self {
            limits,
            started: Instant::now(),
            steps: 0,
            depth: 0,
            capability_calls: 0,
        }
    }

    /// Charges one evaluation step and periodically re-checks the deadline.
    ///
    /// This is the only backstop against `while true; do :; done`.
    pub fn charge_step(&mut self) -> Result<(), LimitExceeded> {
        self.steps = self.steps.saturating_add(1);
        if self.steps > self.limits.max_steps {
            return Err(LimitExceeded::Steps {
                maximum: self.limits.max_steps,
            });
        }
        if self.steps % DEADLINE_CHECK_INTERVAL == 0 {
            self.check_deadline()?;
        }
        Ok(())
    }

    /// Re-reads the wall clock immediately.
    pub fn check_deadline(&self) -> Result<(), LimitExceeded> {
        if self.started.elapsed() > self.limits.timeout {
            return Err(LimitExceeded::Deadline {
                timeout_ms: self.limits.timeout.as_millis(),
            });
        }
        Ok(())
    }

    /// Returns the time left before the deadline trips.
    #[must_use]
    pub fn remaining(&self) -> Duration {
        self.limits.timeout.saturating_sub(self.started.elapsed())
    }

    /// Enters one shell-function frame.
    pub fn enter_call(&mut self) -> Result<(), LimitExceeded> {
        if self.depth >= self.limits.max_recursion_depth {
            return Err(LimitExceeded::RecursionDepth {
                maximum: self.limits.max_recursion_depth,
            });
        }
        self.depth = self.depth.saturating_add(1);
        Ok(())
    }

    /// Leaves one shell-function frame.
    pub fn leave_call(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Charges one capability invocation.
    ///
    /// This counter is deliberately independent of the step budget: a single script can loop and
    /// drive many capability calls where one model tool call drives exactly one today, so the
    /// amplification vector needs its own ceiling.
    pub fn charge_capability_call(&mut self) -> Result<(), LimitExceeded> {
        if self.capability_calls >= self.limits.max_capability_calls {
            return Err(LimitExceeded::CapabilityCalls {
                maximum: self.limits.max_capability_calls,
            });
        }
        self.capability_calls = self.capability_calls.saturating_add(1);
        Ok(())
    }

    /// Returns the number of capability invocations charged so far.
    #[must_use]
    pub fn capability_calls(&self) -> u32 {
        self.capability_calls
    }

    /// Returns the number of steps charged so far.
    #[must_use]
    pub fn steps(&self) -> u64 {
        self.steps
    }
}

/// Bounded combined stdout/stderr accumulator.
///
/// The byte and line ceilings are independent so a single oversized line cannot slip past a
/// line-count-only limit. When either trips, the head and the tail are both retained with a marker
/// in between; head-only truncation would hide a script's final result, which is usually the part
/// worth reading.
#[derive(Debug)]
pub struct OutputBuffer {
    max_bytes: usize,
    max_lines: usize,
    head: Vec<String>,
    head_bytes: usize,
    tail: VecDeque<String>,
    tail_bytes: usize,
    total_lines: usize,
    truncated: bool,
    pending: String,
}

impl OutputBuffer {
    /// Creates an empty buffer under the configured ceilings.
    #[must_use]
    pub fn new(limits: &Limits) -> Self {
        Self {
            max_bytes: limits.max_output_bytes.max(1),
            max_lines: limits.max_output_lines.max(1),
            head: Vec::new(),
            head_bytes: 0,
            tail: VecDeque::new(),
            tail_bytes: 0,
            total_lines: 0,
            truncated: false,
            pending: String::new(),
        }
    }

    fn tail_line_budget(&self) -> usize {
        (self.max_lines / 2).max(1)
    }

    fn tail_byte_budget(&self) -> usize {
        (self.max_bytes / 2).max(1)
    }

    /// Appends one already-rendered line.
    pub fn push_line(&mut self, line: &str) {
        let line = clamp_line(line, self.max_bytes);
        let cost = line.len().saturating_add(1);
        self.total_lines = self.total_lines.saturating_add(1);

        if !self.truncated {
            let fits = self.head.len() < self.max_lines
                && self.head_bytes.saturating_add(cost) <= self.max_bytes;
            if fits {
                self.head_bytes = self.head_bytes.saturating_add(cost);
                self.head.push(line);
                return;
            }
            self.begin_truncation();
        }

        self.tail_bytes = self.tail_bytes.saturating_add(cost);
        self.tail.push_back(line);
        self.evict_tail();
    }

    /// Appends text without terminating the current line.
    ///
    /// This is what `echo -n` and `printf` produce; the fragment joins whatever the next write
    /// appends, exactly as it would on a real terminal.
    pub fn push_fragment(&mut self, fragment: &str) {
        self.pending.push_str(fragment);
        self.drain_complete_lines();
    }

    /// Appends a possibly multi-line block and terminates the line.
    pub fn push_block(&mut self, block: &str) {
        self.pending.push_str(block);
        self.pending.push('\n');
        self.drain_complete_lines();
    }

    fn drain_complete_lines(&mut self) {
        while let Some(offset) = self.pending.find('\n') {
            let line = self.pending[..offset].to_owned();
            self.pending.drain(..=offset);
            self.push_line(&line);
        }
    }

    /// Flushes any unterminated trailing fragment. Call once before rendering.
    pub fn finish(&mut self) {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.push_line(&line);
        }
    }

    fn begin_truncation(&mut self) {
        self.truncated = true;
        let head_lines = self.max_lines.saturating_sub(self.tail_line_budget());
        let head_bytes = self.max_bytes.saturating_sub(self.tail_byte_budget());
        while self.head.len() > head_lines || self.head_bytes > head_bytes {
            let Some(dropped) = self.head.pop() else {
                break;
            };
            self.head_bytes = self
                .head_bytes
                .saturating_sub(dropped.len().saturating_add(1));
        }
    }

    fn evict_tail(&mut self) {
        while self.tail.len() > self.tail_line_budget() || self.tail_bytes > self.tail_byte_budget()
        {
            let Some(dropped) = self.tail.pop_front() else {
                break;
            };
            self.tail_bytes = self
                .tail_bytes
                .saturating_sub(dropped.len().saturating_add(1));
            if self.tail.is_empty() {
                break;
            }
        }
    }

    /// Reports whether any line was dropped or clamped.
    #[must_use]
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Renders the retained output, including the truncation marker when one applies.
    #[must_use]
    pub fn render(&self) -> String {
        let mut lines = Vec::with_capacity(self.head.len() + self.tail.len() + 1);
        lines.extend(self.head.iter().cloned());
        if self.truncated {
            lines.push(format!(
                "... Output truncated ({} total lines) ...",
                self.total_lines
            ));
            lines.extend(self.tail.iter().cloned());
        }
        lines.join("\n")
    }
}

/// Clamps one line so a single enormous line cannot defeat the byte ceiling.
fn clamp_line(line: &str, maximum: usize) -> String {
    if line.len() <= maximum {
        return line.to_owned();
    }
    let mut end = maximum;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &line[..end])
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Budget, LimitExceeded, Limits, OutputBuffer};

    fn tight(limits: Limits) -> Limits {
        limits
    }

    #[test]
    fn step_budget_trips_at_the_configured_ceiling() {
        let mut budget = Budget::start(tight(Limits {
            max_steps: 3,
            ..Limits::default()
        }));
        assert!(budget.charge_step().is_ok());
        assert!(budget.charge_step().is_ok());
        assert!(budget.charge_step().is_ok());
        assert_eq!(
            budget.charge_step(),
            Err(LimitExceeded::Steps { maximum: 3 })
        );
    }

    #[test]
    fn recursion_depth_is_capped_and_released() {
        let mut budget = Budget::start(tight(Limits {
            max_recursion_depth: 2,
            ..Limits::default()
        }));
        assert!(budget.enter_call().is_ok());
        assert!(budget.enter_call().is_ok());
        assert_eq!(
            budget.enter_call(),
            Err(LimitExceeded::RecursionDepth { maximum: 2 })
        );
        budget.leave_call();
        assert!(budget.enter_call().is_ok());
    }

    #[test]
    fn capability_calls_are_counted_separately_from_steps() {
        let mut budget = Budget::start(tight(Limits {
            max_capability_calls: 1,
            ..Limits::default()
        }));
        assert!(budget.charge_capability_call().is_ok());
        assert_eq!(
            budget.charge_capability_call(),
            Err(LimitExceeded::CapabilityCalls { maximum: 1 })
        );
        assert_eq!(budget.steps(), 0);
        assert_eq!(budget.capability_calls(), 1);
    }

    #[test]
    fn an_expired_deadline_is_reported_immediately() {
        let budget = Budget::start(tight(Limits {
            timeout: Duration::ZERO,
            ..Limits::default()
        }));
        std::thread::sleep(Duration::from_millis(2));
        assert!(matches!(
            budget.check_deadline(),
            Err(LimitExceeded::Deadline { .. })
        ));
        assert_eq!(budget.remaining(), Duration::ZERO);
    }

    #[test]
    fn output_under_both_ceilings_is_preserved_exactly() {
        let mut buffer = OutputBuffer::new(&Limits::default());
        buffer.push_line("first");
        buffer.push_block("second\nthird");
        assert!(!buffer.is_truncated());
        assert_eq!(buffer.render(), "first\nsecond\nthird");
    }

    #[test]
    fn the_line_ceiling_keeps_head_and_tail() {
        let mut buffer = OutputBuffer::new(&Limits {
            max_output_lines: 4,
            ..Limits::default()
        });
        for index in 0..20 {
            buffer.push_line(&format!("line-{index}"));
        }
        let rendered = buffer.render();
        assert!(buffer.is_truncated());
        assert!(rendered.starts_with("line-0\nline-1\n"), "{rendered}");
        assert!(rendered.ends_with("line-18\nline-19"), "{rendered}");
        assert!(
            rendered.contains("... Output truncated (20 total lines) ..."),
            "{rendered}"
        );
    }

    #[test]
    fn one_oversized_line_cannot_bypass_the_byte_ceiling() {
        let mut buffer = OutputBuffer::new(&Limits {
            max_output_bytes: 32,
            max_output_lines: 10_000,
            ..Limits::default()
        });
        buffer.push_line(&"x".repeat(4096));
        buffer.push_line("tail");
        assert!(buffer.is_truncated());
        assert!(buffer.render().len() < 200, "{}", buffer.render());
        assert!(buffer.render().ends_with("tail"));
    }
}
