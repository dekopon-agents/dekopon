use std::process::ExitCode;

use clap::{
    Arg, Command,
    error::{ContextKind, ContextValue, ErrorKind},
};

/// Every Clap error a daemon prints passes here: a value that may have come from the environment
/// is never echoed, because value parsers repeat their input.
#[must_use]
pub fn render_cli_error(error: clap::Error, mut command: Command) -> ExitCode {
    command.build();
    if matches!(
        error.kind(),
        ErrorKind::InvalidValue | ErrorKind::ValueValidation | ErrorKind::InvalidUtf8
    ) && let Some(ContextValue::String(argument)) = error.get(ContextKind::InvalidArg)
        && let Some(variable) = find_argument(&command, argument).and_then(Arg::get_env)
    {
        eprintln!(
            "{}: invalid value for '{argument}' (or {})",
            command.get_name(),
            variable.display()
        );
        return ExitCode::from(2);
    }
    let code = u8::try_from(error.exit_code()).unwrap_or(2);
    if let Err(print_error) = error.print() {
        eprintln!(
            "{}: could not print command-line error: {print_error}",
            command.get_name()
        );
    }
    ExitCode::from(code)
}

fn find_argument<'a>(command: &'a Command, rendered: &str) -> Option<&'a Arg> {
    command
        .get_arguments()
        .find(|argument| argument.to_string() == rendered)
        .or_else(|| {
            command
                .get_subcommands()
                .find_map(|subcommand| find_argument(subcommand, rendered))
        })
}
