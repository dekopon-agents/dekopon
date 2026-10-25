//! Error text complete enough to paste to someone else and have them debug it.
//!
//! A console failure is only useful if it names what was attempted, as whom, and every cause the
//! error carries. One line on a status bar does none of that, so every failure the console shows
//! becomes an [`ErrorReport`]: the operation, the identity context, the full `source()` chain, and
//! the broker's own refusal code and message verbatim when there is one.

use std::{error::Error, fmt};

use dekopon_agent::BrokerLegError;
use dekopon_broker_protocol::ClientError;

/// `Display` of `error` followed by each `source()`, joined with `": "` as anyhow's `{:#}` does.
///
/// A link whose text the previous message already ends with is skipped: several variants
/// interpolate their source into their own message and expose it as `source()` too.
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

/// The broker's own `(code, message)` when the chain holds a wire refusal.
///
/// `BrokerLegError::Client` is `#[error(transparent)]`, so its `ClientError` never appears as a
/// link of its own and has to be looked for inside it.
#[must_use]
pub fn broker_refusal(error: &(dyn Error + 'static)) -> Option<(String, String)> {
    let mut current = Some(error);
    while let Some(link) = current {
        let client = link.downcast_ref::<ClientError>().or_else(|| {
            link.downcast_ref::<BrokerLegError>()
                .and_then(|leg| match leg {
                    BrokerLegError::Client(client) => Some(client),
                    _ => None,
                })
        });
        if let Some(ClientError::Remote { code, message }) = client {
            return Some((code.clone(), message.clone()));
        }
        current = link.source();
    }
    None
}

/// One failure, with everything needed to reproduce or explain it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ErrorReport {
    /// What the console was doing, e.g. "open broker session".
    pub operation: String,
    /// Labelled facts: subject, agent, scope, profile, socket, trace.
    pub context: Vec<(String, String)>,
    /// The error and its whole source chain.
    pub error: String,
    /// The broker's refusal `(code, message)`, verbatim, when it sent one.
    pub refusal: Option<(String, String)>,
    /// Where to look next, when the console knows something the error text does not say.
    pub note: Option<String>,
}

impl ErrorReport {
    /// A report of a typed error, keeping its full chain and any broker refusal.
    #[must_use]
    pub fn from_error(operation: impl Into<String>, error: &(dyn Error + 'static)) -> Self {
        Self {
            operation: operation.into(),
            error: chain(error),
            refusal: broker_refusal(error),
            ..Self::default()
        }
    }

    /// A report of a refusal the console itself decided, with no error value behind it.
    #[must_use]
    pub fn from_message(operation: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            error: message.into(),
            ..Self::default()
        }
    }

    /// Adds one labelled fact.
    #[must_use]
    pub fn with(mut self, label: impl Into<String>, value: impl Into<String>) -> Self {
        self.context.push((label.into(), value.into()));
        self
    }

    /// Adds a pointer to where the rest of the explanation lives.
    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// One line for the status bar, most specific part first; the full text is one keystroke away.
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

    /// Looks a context fact up by label.
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
