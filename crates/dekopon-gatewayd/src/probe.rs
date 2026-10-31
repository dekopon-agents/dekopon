use std::{
    borrow::Cow, ffi::OsString, num::NonZeroU32, ops::ControlFlow, path::PathBuf, time::Duration,
};

use dekopon_model::{
    control::TurnControl,
    error::InferenceError,
    inference::{GenerateRequest, InferenceModel as _},
    model::{CompletionOptions, ContentPart, ModelMessage, ModelTool},
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
    #[error("vendor canary returned an invalid tool call (HTTP 200)")]
    InvalidToolCall,
    #[error("vendor canary returned a malformed response (HTTP 200): {0}")]
    MalformedResponse(dekopon_model::error::ProtocolFailure),
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
    active: Option<(&ResolvedConfig, &RoutingTable)>,
    vendor: &Vendor,
    report: &mut CheckReport,
) {
    let loaded_baseline = if active.is_some() {
        None
    } else {
        match scope {
            ProbeScope::Every => None,
            ProbeScope::ChangedFrom(path) => {
                match config::load_in(path, crate::current_uid(), config::LoadMode::Check).await {
                    Ok((mut against, _)) => match against.load_model_ca_bundles() {
                        Ok(()) => Some(against),
                        Err(source) => {
                            report.warnings.push(CheckWarning::Baseline {
                                path: path.clone(),
                                source,
                            });
                            None
                        }
                    },
                    Err(source) => {
                        report.warnings.push(CheckWarning::Baseline {
                            path: path.clone(),
                            source,
                        });
                        None
                    }
                }
            }
        }
    };
    let baseline = active
        .map(|(config, _)| config)
        .or(loaded_baseline.as_ref());
    let baseline_routes = if let Some((_, routes)) = active {
        Some(Cow::Borrowed(routes))
    } else {
        baseline.as_ref().and_then(|old| {
            match dekopon_config::LocalCatalog::load(&old.catalog_path)
                .map_err(GatewaydError::Catalog)
                .and_then(|catalog| RoutingTable::bind(old, &catalog).map_err(GatewaydError::from))
            {
                Ok(routes) => Some(Cow::Owned(routes)),
                Err(source) => {
                    report.warnings.push(CheckWarning::BaselineRoutes {
                        source: Box::new(source),
                    });
                    None
                }
            }
        })
    };
    for model in &config.models {
        if baseline
            .as_ref()
            .is_some_and(|old| old.models.iter().any(|old| old == model))
            && baseline_routes.as_ref().is_some_and(|old| {
                routes.is_none_or(|routes| !routes.new_binding(model.name(), old))
            })
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
            probe_openrouter(model, api_key_env, timeout, settings, vendor)
                .await
                .map(|()| None)
        }
        ModelConfig::ChatgptSubscription { .. } => Ok(Some(NotProbed::SubscriptionAuth)),
        ModelConfig::Anthropic { .. } => Ok(Some(NotProbed::ProxyOnly)),
        ModelConfig::OpenaiCompatible { .. } => Ok(Some(NotProbed::OpenaiCompatible)),
    }
}

async fn probe_openrouter(
    model: &ModelConfig,
    api_key_env: &str,
    timeout: Duration,
    settings: Settings,
    vendor: &Vendor,
) -> Result<(), ProbeError> {
    let name = model.name();
    let token = model_credential(name, api_key_env, (vendor.environment)(api_key_env))?;
    let mut client = OpenRouterClient::new(
        model.vendor_model(),
        token,
        timeout,
        settings,
        model.ca_bundles(),
    )?
    .with_name(name);
    if let Some(endpoint) = &vendor.openrouter_endpoint {
        client = client.with_loopback_endpoint(endpoint)?;
    }
    let control = TurnControl::new(tokio::sync::watch::channel(false).1, timeout)?;
    let user = if model.accepts_images() {
        ModelMessage::user_with_parts(vec![ContentPart::Image {
            mime: "image/png".to_owned(),
            data: dekopon_model::asset::DiskBlob::from_bytes(&[
                137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0,
                1, 8, 4, 0, 0, 0, 181, 28, 12, 2, 0, 0, 0, 11, 73, 68, 65, 84, 120, 218, 99, 100,
                248, 15, 0, 1, 5, 1, 1, 39, 24, 227, 102, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96,
                130,
            ])
            .map_err(InferenceError::from)?
            .into(),
        }])
    } else {
        ModelMessage::user("Confirm this text turn by calling canary.")
    };
    let messages = [
        ModelMessage::system("Call the canary tool exactly once with no arguments."),
        user,
    ];
    let tools = [ModelTool {
        name: "canary".to_owned(),
        description: "Confirm the canary turn can be received.".to_owned(),
        parameters: serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
    }];
    let turn = client
        .generate(
            GenerateRequest {
                messages: &messages,
                tools: &tools,
                options: &CompletionOptions::default(),
            },
            &mut |_| ControlFlow::Continue(()),
            &control,
        )
        .await
        .map_err(|error| match error {
            InferenceError::Protocol(source) => ProbeError::MalformedResponse(source),
            other @ (InferenceError::InvalidRequest(_)
            | InferenceError::Provider(_)
            | InferenceError::Transport(_)
            | InferenceError::Attachment(_)
            | InferenceError::Authentication(_)
            | InferenceError::RateLimited(_)
            | InferenceError::DeadlineExceeded
            | InferenceError::Cancelled
            | InferenceError::OverBudget(_)) => ProbeError::Vendor(other),
        })?;
    let [call] = turn.tool_calls.as_slice() else {
        return Err(ProbeError::InvalidToolCall);
    };
    if call.id.as_str().is_empty()
        || call.kind != "function"
        || call.function.name != tools[0].name
        || serde_json::from_str::<serde_json::Value>(&call.function.arguments).ok()
            != Some(serde_json::json!({}))
    {
        return Err(ProbeError::InvalidToolCall);
    }
    Ok(())
}
