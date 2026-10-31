use std::{error::Error, fmt};

use dekopon_agent::BrokerLegError;
use dekopon_broker_protocol::ClientError;

#[must_use]
pub fn chain(error: &(dyn Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

#[must_use]
pub fn broker_refusal(error: &(dyn Error + 'static)) -> Option<(String, String)> {
    let mut current = Some(error);
    while let Some(link) = current {
        let client = link.downcast_ref::<ClientError>().or_else(|| {
            link.downcast_ref::<BrokerLegError>()
                .and_then(|leg| match leg {
                    BrokerLegError::Client(client) => Some(client),
                    BrokerLegError::DuplicateCapabilities { .. } => None,
                })
        });
        if let Some(ClientError::Remote { code, message }) = client {
            return Some((code.clone(), message.clone()));
        }
        current = link.source();
    }
    None
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ErrorReport {
    pub operation: String,
    pub context: Vec<(String, String)>,
    pub error: String,
    pub refusal: Option<(String, String)>,
    pub note: Option<String>,
}

impl ErrorReport {
    #[must_use]
    pub fn from_error(operation: impl Into<String>, error: &(dyn Error + 'static)) -> Self {
        Self {
            operation: operation.into(),
            error: chain(error),
            refusal: broker_refusal(error),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn from_message(operation: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            error: message.into(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with(mut self, label: impl Into<String>, value: impl Into<String>) -> Self {
        self.context.push((label.into(), value.into()));
        self
    }

    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    #[must_use]
    pub fn summary(&self) -> String {
        match &self.refusal {
            Some((code, message)) => format!(
                "[e: full error] {} refused by broker: {code}: {message}",
                self.operation
            ),
            None => format!("[e: full error] {} failed: {}", self.operation, self.error),
        }
    }

    #[must_use]
    pub fn fact(&self, label: &str) -> Option<&str> {
        self.context
            .iter()
            .find(|(candidate, _)| candidate == label)
            .map(|(_, value)| value.as_str())
    }
}

impl fmt::Display for ErrorReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "dekopon-console: {} failed", self.operation)?;
        for (label, value) in &self.context {
            writeln!(formatter, "{label}: {value}")?;
        }
        write!(formatter, "error: {}", self.error)?;
        if let Some((code, message)) = &self.refusal {
            write!(
                formatter,
                "\nbroker refusal code: {code}\nbroker refusal message: {message}"
            )?;
        }
        if let Some(note) = &self.note {
            write!(formatter, "\nnote: {note}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
