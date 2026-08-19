use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU64, Ordering},
};

use dekopon_broker_protocol::{AgentInventory, ModelUsageReport};

/// Live informational state reported by unprivileged gateway processes.
///
/// This store has no authority. It is not consulted by Cedar, constraint selection, credential
/// resolution, provider routing, evidence, or durable audit. Everything resets on broker restart.
#[derive(Clone, Debug, Default)]
pub struct ServiceStatus {
    inner: Arc<StatusInner>,
}

#[derive(Debug, Default)]
struct StatusInner {
    agents: RwLock<AgentInventory>,
    inventory_reports: AtomicU64,
    tokens: RwLock<TokenTotals>,
}

/// Point-in-time provider-reported token totals received by the broker.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TokenTotals {
    /// Informational usage reports retained since broker startup.
    pub reports: u64,
    /// Model calls represented by those reports.
    pub model_calls: u64,
    /// Provider-reported input tokens.
    pub input_tokens: u64,
    /// Calls for which input usage was absent.
    pub input_unreported_calls: u64,
    /// Provider-reported cached input tokens.
    pub cached_input_tokens: u64,
    /// Calls for which cached-input usage was absent.
    pub cached_input_unreported_calls: u64,
    /// Provider-reported output tokens.
    pub output_tokens: u64,
    /// Calls for which output usage was absent.
    pub output_unreported_calls: u64,
    /// Provider-reported reasoning output tokens.
    pub reasoning_output_tokens: u64,
    /// Calls for which reasoning usage was absent.
    pub reasoning_unreported_calls: u64,
    /// Provider-reported total tokens.
    pub total_tokens: u64,
    /// Calls for which total usage was absent.
    pub total_unreported_calls: u64,
}

impl ServiceStatus {
    /// Replaces the informational agent inventory with one complete gateway snapshot.
    ///
    /// Callers must validate the protocol bounds before retaining it.
    pub fn replace_agents(&self, mut inventory: AgentInventory) {
        inventory
            .agents
            .sort_by(|left, right| left.id.cmp(&right.id));
        for agent in &mut inventory.agents {
            agent.providers.sort();
            agent.providers.dedup();
            agent
                .capabilities
                .sort_by(|left, right| left.id.cmp(&right.id));
        }
        *self
            .inner
            .agents
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = inventory;
        increment(&self.inner.inventory_reports);
    }

    /// Returns the latest complete gateway inventory and how many replacements were received.
    #[must_use]
    pub fn agents(&self) -> (AgentInventory, u64) {
        let inventory = self
            .inner
            .agents
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        (
            inventory,
            self.inner.inventory_reports.load(Ordering::Relaxed),
        )
    }

    /// Adds one validated informational model-usage delta with saturating arithmetic.
    pub fn record_usage(&self, report: ModelUsageReport) {
        let mut totals = self
            .inner
            .tokens
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        totals.reports = totals.reports.saturating_add(1);
        totals.model_calls = totals.model_calls.saturating_add(report.model_calls);
        totals.input_tokens = totals.input_tokens.saturating_add(report.input_tokens);
        totals.input_unreported_calls = totals
            .input_unreported_calls
            .saturating_add(report.input_unreported_calls);
        totals.cached_input_tokens = totals
            .cached_input_tokens
            .saturating_add(report.cached_input_tokens);
        totals.cached_input_unreported_calls = totals
            .cached_input_unreported_calls
            .saturating_add(report.cached_input_unreported_calls);
        totals.output_tokens = totals.output_tokens.saturating_add(report.output_tokens);
        totals.output_unreported_calls = totals
            .output_unreported_calls
            .saturating_add(report.output_unreported_calls);
        totals.reasoning_output_tokens = totals
            .reasoning_output_tokens
            .saturating_add(report.reasoning_output_tokens);
        totals.reasoning_unreported_calls = totals
            .reasoning_unreported_calls
            .saturating_add(report.reasoning_unreported_calls);
        totals.total_tokens = totals.total_tokens.saturating_add(report.total_tokens);
        totals.total_unreported_calls = totals
            .total_unreported_calls
            .saturating_add(report.total_unreported_calls);
    }

    /// Captures one coherent view of the live token counters.
    #[must_use]
    pub fn tokens(&self) -> TokenTotals {
        *self
            .inner
            .tokens
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(1))
    });
}

#[cfg(test)]
mod tests {
    use dekopon_broker_protocol::ModelUsageReport;

    use super::ServiceStatus;

    #[test]
    fn token_totals_saturate_instead_of_wrapping() {
        let status = ServiceStatus::default();
        status.record_usage(ModelUsageReport {
            model_calls: 1,
            input_tokens: u64::MAX,
            output_tokens: 7,
            ..ModelUsageReport::default()
        });
        status.record_usage(ModelUsageReport {
            model_calls: 1,
            input_tokens: 1,
            output_tokens: 5,
            ..ModelUsageReport::default()
        });

        let totals = status.tokens();
        assert_eq!(totals.reports, 2);
        assert_eq!(totals.model_calls, 2);
        assert_eq!(totals.input_tokens, u64::MAX);
        assert_eq!(totals.output_tokens, 12);
    }
}
