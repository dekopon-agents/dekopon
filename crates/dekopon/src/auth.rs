//! Model-account authentication command execution.

use std::io;

use dekopon_model::chatgpt::{self, ChatGptError};

use crate::{
    cli::{AuthCommand, ChatGptAuthCommand},
    command::{CommandResult, ModelAuthStatus},
};

pub(crate) fn execute(account: &AuthCommand) -> Result<CommandResult, ChatGptError> {
    match account {
        AuthCommand::ChatGpt { command } => execute_chatgpt(command),
    }
}

fn execute_chatgpt(command: &ChatGptAuthCommand) -> Result<CommandResult, ChatGptError> {
    let status = match command {
        ChatGptAuthCommand::Login { auth_file } => {
            let stderr = io::stderr();
            let mut output = stderr.lock();
            chatgpt::login_with_output(auth_file.as_deref(), &mut output)?;
            chatgpt::status(auth_file.as_deref())?
        }
        ChatGptAuthCommand::Status { auth_file } => chatgpt::status(auth_file.as_deref())?,
        ChatGptAuthCommand::Logout { auth_file } => {
            chatgpt::logout(auth_file.as_deref())?;
            chatgpt::status(auth_file.as_deref())?
        }
    };

    Ok(CommandResult::Auth(ModelAuthStatus::chatgpt(status)))
}
