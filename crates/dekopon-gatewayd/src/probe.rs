use std::{ffi::OsString, num::NonZeroU32, ops::ControlFlow, path::PathBuf, time::Duration};

use dekopon_model::{
    control::TurnControl,
    error::InferenceError,
    inference::{GenerateRequest, InferenceModel as _},
    model::{CompletionOptions, ModelMessage},
    openrouter::{
        OpenRouterClient,
        settings::{Generation, Settings},
    },
};
use thiserror::Error;

use crate::{
    CheckReport, CheckWarning, GatewaydError,
    config::{self, ModelConfig, ResolvedConfig},
    routes::RoutingTable,
    session::{ModelCredentialError, model_credential},
};

const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// The OpenAI Responses API refuses a cap below 16, so a smaller one would fail every
/// OpenAI-backed route for the cap alone.
const PROBE_OUTPUT_TOKENS: NonZeroU32 = NonZeroU32::MIN.saturating_add(15);

#[derive(Clone, Debug)]
pub enum ProbeScope {
    Every,
    ChangedFrom(PathBuf),
}

pub(crate) struct Vendor {
    pub(crate) openrouter_endpoint: Option<String>,
    pub(crate) environment: fn(&str) -> Option<OsString>,
}

impl Vendor {
    pub(crate) const LIVE: Self = Self {
        openrouter_endpoint: None,
        environment: process_environment,
    };
}

fn process_environment(variable: &str) -> Option<OsString> {
    std::env::var_os(variable)
}

#[derive(Clone, Copy, Debug)]
pub enum NotProbed {
    SubscriptionAuth,
    ProxyOnly,
    OpenaiCompatible,
}

impl std::fmt::Display for NotProbed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::SubscriptionAuth => "subscription auth",
            Self::ProxyOnly => "proxy-only model",
            Self::OpenaiCompatible => "openaiCompatible endpoint",
        })
    }
}

#[derive(Debug, Error)]
#[error(
    "model {model} ({vendor_model}) in {}, {}, failed its vendor probe",
    .file.display(),
    render_routes(.routes)
)]
pub struct ProbeProblem {
    pub model: String,
    pub vendor_model: String,
    pub file: PathBuf,
    pub routes: Vec<String>,
    #[source]
    pub source: ProbeError,
}

#[derive(Debug, Error)]
pub enum ProbeError {
    #[error(transparent)]
    Credential(#[from] ModelCredentialError),
    // The vendor's text is already in this display; chaining its sources repeats it.
    #[error("{0}")]
    Vendor(InferenceError),
}

impl From<InferenceError> for ProbeError {
    fn from(error: InferenceError) -> Self {
        Self::Vendor(error)
    }
}

fn render_routes(routes: &[String]) -> String {
    if routes.is_empty() {
        "bound by no route".to_owned()
    } else {
        format!("routes {}", routes.join(", "))
    }
}

pub(crate) async fn probe(
    config: &ResolvedConfig,
    routes: Option<&RoutingTable>,
    scope: &ProbeScope,
    vendor: &Vendor,
    report: &mut CheckReport,
) {
    let baseline = match scope {
        ProbeScope::Every => None,
        ProbeScope::ChangedFrom(path) => {
            match config::load_in(path, crate::current_uid(), config::LoadMode::Check).await {
                Ok((against, _)) => Some(against.models),
                Err(source) => {
                    report.warnings.push(CheckWarning::Baseline {
                        path: path.clone(),
                        source,
                    });
                    None
                }
            }
        }
    };
    for model in &config.models {
        if baseline
            .as_ref()
            .is_some_and(|models| models.iter().any(|old| old == model))
        {
            continue;
        }
        match probe_model(model, vendor).await {
            Ok(None) => {}
            Ok(Some(reason)) => report.warnings.push(CheckWarning::NotProbed {
                model: model.name().to_owned(),
                reason,
            }),
            Err(source) => {
                let name = model.name();
                report
                    .problems
                    .push(GatewaydError::Probe(Box::new(ProbeProblem {
                        model: name.to_owned(),
                        vendor_model: model.vendor_model().to_owned(),
                        file: config
                            .model_files
                            .get(name)
                            .cloned()
                            .unwrap_or_else(|| config.source.clone()),
                        routes: routes
                            .map(|routes| routes.serving(name))
                            .unwrap_or_default(),
                        source,
                    })));
            }
        }
    }
}

async fn probe_model(
    model: &ModelConfig,
    vendor: &Vendor,
) -> Result<Option<NotProbed>, ProbeError> {
    match model {
        ModelConfig::Openrouter {
            name,
            model,
            api_key_env,
            timeout_ms,
            generation,
            reasoning,
            routing,
            cache,
            ..
        } => {
            let settings = Settings {
                generation: Some(Generation {
                    max_output_tokens: Some(PROBE_OUTPUT_TOKENS),
                    ..generation.clone().unwrap_or_default()
                }),
                reasoning: reasoning.clone(),
                routing: routing.clone(),
                cache: cache.clone(),
            };
            let timeout = Duration::from_millis(*timeout_ms).min(PROBE_TIMEOUT);
            probe_openrouter(name, model, api_key_env, timeout, settings, vendor)
                .await
                .map(|()| None)
        }
        ModelConfig::ChatgptSubscription { .. } => Ok(Some(NotProbed::SubscriptionAuth)),
        ModelConfig::Anthropic { .. } => Ok(Some(NotProbed::ProxyOnly)),
        ModelConfig::OpenaiCompatible { .. } => Ok(Some(NotProbed::OpenaiCompatible)),
    }
}

async fn probe_openrouter(
    name: &str,
    vendor_model: &str,
    api_key_env: &str,
    timeout: Duration,
    settings: Settings,
    vendor: &Vendor,
) -> Result<(), ProbeError> {
    let token = model_credential(name, api_key_env, (vendor.environment)(api_key_env))?;
    let mut client = OpenRouterClient::new(vendor_model, token, timeout, settings)?.with_name(name);
    if let Some(endpoint) = &vendor.openrouter_endpoint {
        client = client.with_loopback_endpoint(endpoint)?;
    }
    let control = TurnControl::new(tokio::sync::watch::channel(false).1, timeout)?;
    let messages = [
        ModelMessage::system("Reply with one word."),
        ModelMessage::user("ping"),
    ];
    client
        .generate(
            GenerateRequest {
                messages: &messages,
                tools: &[],
                options: &CompletionOptions::default(),
            },
            &mut |_| ControlFlow::Continue(()),
            &control,
        )
        .await?;
    Ok(())
}
