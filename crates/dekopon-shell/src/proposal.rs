use serde_json::Value;

#[must_use]
pub struct CommandProposal {
    pub capability: String,
    pub input: Value,
    pub secret_use: Option<dekopon_core::SecretUseProposal>,
    pub report: Option<CommandReport>,
}
impl CommandProposal {
    pub fn new(
        capability: impl Into<String>,
        input: Value,
        secret_use: Option<dekopon_core::SecretUseProposal>,
    ) -> Self {
        Self {
            capability: capability.into(),
            input,
            secret_use,
            report: None,
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub enum CommandReportOutcome {
    Succeeded,
    Denied,
    Failed,
    Cancelled,
}
#[must_use]
pub struct CommandReport {
    finish: Option<Box<dyn FnOnce(CommandReportOutcome) + Send + Sync>>,
}
impl CommandReport {
    pub fn new(finish: impl FnOnce(CommandReportOutcome) + Send + Sync + 'static) -> Self {
        Self {
            finish: Some(Box::new(finish)),
        }
    }
    pub fn complete(mut self, outcome: CommandReportOutcome) {
        if let Some(finish) = self.finish.take() {
            finish(outcome);
        }
    }
}
impl std::fmt::Debug for CommandReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CommandReport")
    }
}
impl Drop for CommandReport {
    fn drop(&mut self) {
        if let Some(finish) = self.finish.take() {
            finish(CommandReportOutcome::Failed);
        }
    }
}
