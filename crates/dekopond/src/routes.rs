//! Binding configured routes to catalog agents and configured models, then matching messages.
//!
//! Every binding failure here is a *startup* failure. A route naming a disabled agent or an agent
//! with no reachable model is a configuration mistake, and finding it when the daemon starts beats
//! finding it in a chat reply an hour later.

use std::sync::Arc;

use dekopon_agent::prompt::PromptLimits;
use dekopon_config::LocalCatalog;
use dekopon_core::AgentId;
use dekopon_protocol::Agent;
use thiserror::Error;

use crate::{
    config::{ConversationPolicy, ModelConfig, ResolvedConfig, RouteMatch},
    transport::ConversationKind,
};

/// One route after its agent and model were resolved.
#[derive(Clone, Debug)]
pub(crate) struct BoundRoute {
    pub transport: String,
    pub r#match: RouteMatch,
    pub agent: AgentId,
    /// The agent's standing orders, which are untrusted model text and grant nothing.
    pub instructions: Option<String>,
    pub model: Arc<ModelConfig>,
    pub limits: PromptLimits,
    /// What this route remembers between messages.
    pub conversation: ConversationPolicy,
}

/// Every bound route, consulted in declaration order.
#[derive(Clone, Debug, Default)]
pub(crate) struct RoutingTable {
    routes: Vec<BoundRoute>,
}

impl RoutingTable {
    /// Resolves every configured route against the catalog and the configured models.
    pub fn bind(config: &ResolvedConfig, catalog: &LocalCatalog) -> Result<Self, RouteError> {
        let models = config
            .models
            .iter()
            .cloned()
            .map(Arc::new)
            .collect::<Vec<_>>();
        let mut routes = Vec::with_capacity(config.routes.len());
        for route in &config.routes {
            let agent: &Agent =
                catalog
                    .agent(&route.agent)
                    .ok_or_else(|| RouteError::UnknownAgent {
                        agent: route.agent.to_string(),
                    })?;
            if !agent.spec.enabled {
                return Err(RouteError::DisabledAgent {
                    agent: route.agent.to_string(),
                });
            }
            // An explicit override wins; otherwise the agent's declared class picks the first
            // endpoint that offers it. Declaration order is the tie-break, so an operator controls
            // preference by ordering `models` rather than by a hidden score.
            let model = match &route.model {
                Some(name) => models
                    .iter()
                    .find(|model| model.name() == name)
                    .ok_or_else(|| RouteError::UnknownModel {
                        model: name.clone(),
                    })?,
                None => {
                    let class = agent.spec.model_class.as_deref().ok_or_else(|| {
                        RouteError::NoModelClass {
                            agent: route.agent.to_string(),
                        }
                    })?;
                    models
                        .iter()
                        .find(|model| model.classes().iter().any(|offered| offered == class))
                        .ok_or_else(|| RouteError::NoModelForClass {
                            agent: route.agent.to_string(),
                            class: class.to_owned(),
                        })?
                }
            };
            routes.push(BoundRoute {
                transport: route.transport.clone(),
                r#match: route.r#match.clone(),
                agent: route.agent.clone(),
                instructions: agent.spec.instructions.clone(),
                model: Arc::clone(model),
                limits: PromptLimits {
                    max_steps: route.limits.max_steps,
                    max_capability_calls: route.limits.max_capability_calls,
                },
                conversation: route.conversation,
            });
        }
        Ok(Self { routes })
    }

    /// First route claiming this conversation, or `None` for ambient traffic.
    pub fn route(&self, transport: &str, conversation: &ConversationKind) -> Option<&BoundRoute> {
        self.routes.iter().find(|route| {
            route.transport == transport
                && match (&route.r#match, conversation) {
                    (RouteMatch::DirectMessage, ConversationKind::DirectMessage) => true,
                    (RouteMatch::Channel { channel }, ConversationKind::Channel(actual)) => {
                        channel == actual
                    }
                    _ => false,
                }
        })
    }

    /// How many routes are bound, for the startup lifecycle event.
    pub fn len(&self) -> usize {
        self.routes.len()
    }
}

/// A route that no configuration could ever satisfy.
#[derive(Debug, Error)]
pub enum RouteError {
    #[error("route names agent {agent:?}, which is not in the catalog")]
    UnknownAgent { agent: String },
    #[error("route names agent {agent:?}, which the catalog disables")]
    DisabledAgent { agent: String },
    #[error("route names model {model:?}, which is not configured")]
    UnknownModel { model: String },
    #[error(
        "agent {agent:?} declares no modelClass and its route names no model, so no model can serve it"
    )]
    NoModelClass { agent: String },
    #[error("agent {agent:?} needs model class {class:?}, which no configured model offers")]
    NoModelForClass { agent: String, class: String },
}
