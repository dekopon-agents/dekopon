//! A runtime-owning embedder. Scripts are never executed; the runtime returns a fixed outcome.
//! Compile this example for validation; real inference requires explicit operator authorization.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use dekopon_agent::prompt::{
    History, PromptLimits, ScriptRuntime, SessionInputs, run_prompt_session,
};
use dekopon_model::{
    blocking::BlockingModel,
    codex::CodexClient,
    inference::ModelClient,
    openrouter::{OpenRouterClient, settings::Settings},
};
use dekopon_shell::{ExitCode, ScriptOutcome};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use thiserror::Error;

#[derive(Clone, Copy)]
enum Kind {
    Codex,
    OpenRouter,
}

struct Arguments {
    kind: Kind,
    model: String,
    label: String,
    prompt: String,
    loopback: Option<SocketAddr>,
    auth_file: Option<PathBuf>,
}

#[derive(Debug, Error)]
enum ExampleError {
    #[error(
        "usage: --kind codex|openrouter --model <id> [--label <experiment>] [--prompt <text>] [--loopback <ip:port>] [--auth-file <path>]"
    )]
    Arguments,
    #[error("--kind codex --loopback requires --auth-file with a fresh synthetic credential")]
    OfflineCredential,
    #[error("OPENROUTER_API_KEY must contain a nonblank Unicode credential")]
    Credential,
    #[error(transparent)]
    Inference(#[from] dekopon_model::error::InferenceError),
    #[error(transparent)]
    Prompt(#[from] dekopon_agent::prompt::PromptError),
    #[error("runtime could not start")]
    Runtime(#[from] std::io::Error),
    #[error("blocking session did not finish")]
    Join(#[from] tokio::task::JoinError),
}

impl Arguments {
    fn parse() -> Result<Self, ExampleError> {
        Self::parse_from(std::env::args().skip(1))
    }

    fn parse_from(mut arguments: impl Iterator<Item = String>) -> Result<Self, ExampleError> {
        let (mut kind, mut model, mut label, mut prompt, mut loopback, mut auth_file) =
            (None, None, None, None, None, None);
        while let Some(flag) = arguments.next() {
            let value = arguments.next().ok_or(ExampleError::Arguments)?;
            match flag.as_str() {
                "--kind" if kind.is_none() => {
                    kind = Some(match value.as_str() {
                        "codex" => Kind::Codex,
                        "openrouter" => Kind::OpenRouter,
                        _ => return Err(ExampleError::Arguments),
                    })
                }
                "--model" if model.is_none() => model = Some(value),
                "--label" if label.is_none() => label = Some(value),
                "--prompt" if prompt.is_none() => prompt = Some(value),
                "--auth-file" if auth_file.is_none() => auth_file = Some(PathBuf::from(value)),
                "--loopback" if loopback.is_none() => {
                    loopback = Some(
                        value
                            .parse()
                            .map_err(|_invalid_address| ExampleError::Arguments)?,
                    )
                }
                _ => return Err(ExampleError::Arguments),
            }
        }
        let kind = kind.ok_or(ExampleError::Arguments)?;
        if matches!(kind, Kind::Codex) && loopback.is_some() && auth_file.is_none() {
            return Err(ExampleError::OfflineCredential);
        }
        Ok(Self {
            kind,
            model: model.ok_or(ExampleError::Arguments)?,
            label: label.unwrap_or_else(|| "compare-models".into()),
            prompt: prompt.unwrap_or_else(|| {
                "Call bash once with a greeting, then summarize its synthetic result.".into()
            }),
            loopback,
            auth_file,
        })
    }
}

struct SyntheticRuntime;

impl ScriptRuntime for SyntheticRuntime {
    fn run_script(&self, _script: &str, _max_capability_calls: u32) -> ScriptOutcome {
        ScriptOutcome {
            output: "synthetic: no script was executed".into(),
            exit_code: ExitCode::SUCCESS,
            truncated: false,
            capability_calls: 0,
            steps: 0,
        }
    }
}

fn main() -> Result<(), ExampleError> {
    let arguments = Arguments::parse()?;
    let timeout = Duration::from_secs(120);
    let endpoint = arguments
        .loopback
        .map(|address| format!("http://{address}/"));
    // Credential-file IO happens before entering the async runtime.
    let client = match arguments.kind {
        Kind::Codex => {
            let client =
                CodexClient::new(&arguments.model, arguments.auth_file.as_deref(), timeout)?
                    .with_name(&arguments.label);
            ModelClient::Codex(match endpoint.as_deref() {
                Some(endpoint) => client.with_loopback_endpoint(endpoint)?,
                None => client,
            })
        }
        Kind::OpenRouter => {
            let token = match endpoint {
                Some(_) => "synthetic-offline-key".to_owned(),
                None => std::env::var("OPENROUTER_API_KEY")
                    .map_err(|_credential_error| ExampleError::Credential)?,
            };
            let client =
                OpenRouterClient::new(&arguments.model, token, timeout, Settings::default())?
                    .with_name(&arguments.label);
            ModelClient::OpenRouter(match endpoint.as_deref() {
                Some(endpoint) => client.with_loopback_endpoint(endpoint)?,
                None => client,
            })
        }
    };
    let runtime = tokio::runtime::Runtime::new()?;
    let (_cancel, receiver) = tokio::sync::watch::channel(false);
    let model = BlockingModel::new(
        Arc::new(client),
        runtime.handle().clone(),
        receiver,
        timeout,
    );
    let outcome = runtime.block_on(runtime.spawn_blocking(move || {
        run_prompt_session(
            &model,
            &SyntheticRuntime,
            SessionInputs::new(
                &arguments.prompt,
                PromptLimits {
                    max_steps: 4,
                    max_capability_calls: 4,
                },
            )
            .with_system(Some(
                "Tools return fixed synthetic data; no script or external effect is executed.",
            )),
            &mut History::default(),
        )
    }))??;
    println!("{}", outcome.answer);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Result<Arguments, ExampleError> {
        Arguments::parse_from(arguments.iter().map(|value| (*value).to_owned()))
    }

    #[test]
    fn codex_loopback_requires_an_explicit_auth_file_before_any_credential_io() {
        assert!(matches!(
            parse(&[
                "--kind",
                "codex",
                "--model",
                "test",
                "--loopback",
                "127.0.0.1:8000"
            ]),
            Err(ExampleError::OfflineCredential)
        ));
        let arguments = parse(&[
            "--kind",
            "codex",
            "--model",
            "test",
            "--loopback",
            "127.0.0.1:8000",
            "--auth-file",
            "synthetic-auth.json",
        ])
        .unwrap();
        assert_eq!(
            arguments.auth_file.as_deref(),
            Some(std::path::Path::new("synthetic-auth.json"))
        );
    }

    #[test]
    fn non_loopback_codex_and_openrouter_keep_their_credential_defaults() {
        for arguments in [
            vec!["--kind", "codex", "--model", "test"],
            vec![
                "--kind",
                "openrouter",
                "--model",
                "test",
                "--loopback",
                "127.0.0.1:8000",
            ],
        ] {
            assert!(parse(&arguments).unwrap().auth_file.is_none());
        }
    }

    #[test]
    fn auth_file_requires_one_value_and_cannot_be_repeated() {
        for arguments in [
            vec!["--kind", "codex", "--model", "test", "--auth-file"],
            vec![
                "--kind",
                "codex",
                "--model",
                "test",
                "--auth-file",
                "one",
                "--auth-file",
                "two",
            ],
        ] {
            assert!(matches!(parse(&arguments), Err(ExampleError::Arguments)));
        }
    }
}
