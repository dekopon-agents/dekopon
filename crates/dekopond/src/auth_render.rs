//! Deterministic auth-only rendering.
use crate::{
    auth_result::{CommandResult, ModelAuthStatus},
    cli::OutputFormat,
};
use serde::Serialize;
use thiserror::Error;
/// Failure to serialize a typed command result.
#[derive(Debug, Error)]
pub enum RenderError {
    /// JSON serialization failed.
    #[error("could not render JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// YAML serialization failed.
    #[error("could not render YAML: {0}")]
    Yaml(#[from] serde_yaml::Error),
}

pub(crate) fn render(result: &CommandResult, format: OutputFormat) -> Result<String, RenderError> {
    match result {
        CommandResult::Auth(status) => render_auth(status, format),
        // Both export guards have passed; unwrap only for the intended stdout writer.
        CommandResult::CredentialExport(document) => Ok(document.expose().clone()),
    }
}
fn render_auth(status: &ModelAuthStatus, format: OutputFormat) -> Result<String, RenderError> {
    match format {
        OutputFormat::Json => to_json(status),
        OutputFormat::Yaml => to_yaml(status),
        OutputFormat::Name => Ok(format!("auth/{}", status.account)),
        OutputFormat::Table => {
            let state = if !status.signed_in {
                "not signed in"
            } else if status.expired {
                "signed in; refresh required"
            } else {
                "signed in"
            };
            Ok(table(
                &["ACCOUNT", "STATUS", "CREDENTIAL FILE"],
                &[vec![
                    cell_text(status.account),
                    state.to_owned(),
                    cell_text(&status.credential_file),
                ]],
            ))
        }
    }
}

fn to_json(value: &impl Serialize) -> Result<String, RenderError> {
    serde_json::to_string_pretty(value).map_err(RenderError::Json)
}

fn to_yaml(value: &impl Serialize) -> Result<String, RenderError> {
    serde_yaml::to_string(value).map_err(RenderError::Yaml)
}

fn cell_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths = headers
        .iter()
        .map(|header| header.chars().count())
        .collect::<Vec<_>>();
    for row in rows {
        for (index, cell) in row.iter().enumerate().take(widths.len()) {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }

    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(table_row(
        &headers.iter().map(ToString::to_string).collect::<Vec<_>>(),
        &widths,
    ));
    lines.extend(rows.iter().map(|row| table_row(row, &widths)));
    lines.join("\n")
}

fn table_row(cells: &[String], widths: &[usize]) -> String {
    let mut output = String::new();
    for (index, width) in widths.iter().copied().enumerate() {
        if index > 0 {
            output.push_str("   ");
        }
        let cell = cells.get(index).map_or("", String::as_str);
        output.push_str(cell);
        if index + 1 < widths.len() {
            output.push_str(&" ".repeat(width.saturating_sub(cell.chars().count())));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use crate::cli::OutputFormat;

    use super::{cell_text, table};

    #[test]
    fn table_has_no_trailing_whitespace() {
        let rendered = table(
            &["NAME", "STATUS"],
            &[vec!["reviewer".to_owned(), "Ready".to_owned()]],
        );
        assert!(rendered.lines().all(|line| !line.ends_with(' ')));
    }

    #[test]
    fn table_cells_remove_terminal_control_characters() {
        assert_eq!(cell_text("safe\n\u{1b}[31mtext"), "safe [31mtext");
    }

    #[test]
    fn output_format_values_remain_distinct() {
        assert_ne!(OutputFormat::Table, OutputFormat::Name);
        assert_ne!(OutputFormat::Json, OutputFormat::Yaml);
    }
}
