use std::{collections::BTreeSet, sync::Arc, time::Duration};

use dekopon_agent::prompt::PromptLimits;
use dekopon_broker_protocol::{Conversation, ConversationMatch};
use dekopon_config::{LocalCatalog, Skill};
use dekopon_core::{AgentId, ExternalSubject};
use thiserror::Error;

use crate::{
    cache_key,
    config::{MemoryPolicy, ModelConfig, ResolvedConfig, render_problems},
    progress::ProgressDetail,
    transport::InboundMessage,
    wake::Anchor,
};

#[derive(Clone, Debug)]
pub(crate) struct BoundRoute {
    pub transport: String,
    pub conversation: ConversationMatch,
    pub subjects: Option<Vec<ExternalSubject>>,
    pub agent: AgentId,
    pub description: String,
    pub model_class: Option<String>,
    pub instructions: Option<String>,
    pub skills: Arc<[Skill]>,
    pub model: Arc<ModelConfig>,
    pub improvement_suggestions: bool,
    pub inspect_agent_config: bool,
    pub limits: PromptLimits,
    pub max_duration: Option<Duration>,
    pub script_timeout: Duration,
    pub progress_detail: ProgressDetail,
    pub memory: MemoryPolicy,
    pub wakes: bool,
    /// This cache lane is safe to share since its prefix is byte-identical and sender-agnostic
    /// across the route's traffic, and grants nothing: every message still opens its own attested
    /// broker leg.
    pub cache_key: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RoutingTable {
    routes: Vec<BoundRoute>,
}

impl RoutingTable {
    /// Every route is checked before any is refused, so a config with several broken routes gets
    /// one error naming all of them instead of one restart per fix.
    pub fn bind(config: &ResolvedConfig, catalog: &LocalCatalog) -> Result<Self, RouteError> {
        let models = config
            .models
            .iter()
            .cloned()
            .map(Arc::new)
            .collect::<Vec<_>>();
        let mut routes = Vec::with_capacity(config.routes.len());
        let mut problems = Vec::new();
        for route in &config.routes {
            let Some(agent) = catalog.agent(&route.agent) else {
                problems.push(RouteProblem::UnknownAgent {
                    agent: route.agent.to_string(),
                });
                continue;
            };
            if !agent.spec.enabled {
                problems.push(RouteProblem::DisabledAgent {
                    agent: route.agent.to_string(),
                });
                continue;
            }
            let selected = match &route.model {
                Some(name) => models
                    .iter()
                    .find(|model| model.name() == name)
                    .ok_or_else(|| RouteProblem::UnknownModel {
                        model: name.clone(),
                    }),
                None => match agent.spec.model_class.as_deref() {
                    None => Err(RouteProblem::NoModelClass {
                        agent: route.agent.to_string(),
                    }),
                    Some(class) => models
                        .iter()
                        .find(|model| model.classes().iter().any(|offered| offered == class))
                        .ok_or_else(|| RouteProblem::NoModelForClass {
                            agent: route.agent.to_string(),
                            class: class.to_owned(),
                        }),
                },
            };
            let model = match selected {
                Ok(model) => model,
                Err(problem) => {
                    problems.push(problem);
                    continue;
                }
            };
            routes.push(BoundRoute {
                transport: route.transport.clone(),
                conversation: route.conversation.clone(),
                subjects: route.subjects.clone(),
                agent: route.agent.clone(),
                description: agent.spec.description.clone(),
                model_class: agent.spec.model_class.clone(),
                instructions: agent.spec.instructions.clone(),
                skills: Arc::from(catalog.agent_skills(&route.agent).to_vec()),
                model: Arc::clone(model),
                improvement_suggestions: route.improvement_suggestions,
                inspect_agent_config: route.inspect_agent_config,
                limits: PromptLimits {
                    max_steps: route.limits.max_steps,
                    max_capability_calls: route.limits.max_capability_calls,
                },
                max_duration: route.limits.max_duration_ms.map(Duration::from_millis),
                script_timeout: route.limits.script_timeout(),
                progress_detail: route.progress_detail,
                memory: route.memory,
                wakes: route.wakes,
                cache_key: cache_key::for_route(),
            });
        }
        if problems.is_empty() {
            Ok(Self { routes })
        } else {
            Err(RouteError { problems })
        }
    }

    pub fn bound_models(&self) -> Vec<&ModelConfig> {
        let mut seen = BTreeSet::new();
        self.routes
            .iter()
            .filter(|route| seen.insert(route.model.name().to_owned()))
            .map(|route| route.model.as_ref())
            .collect()
    }

    /// Declaration order is the only precedence rule, with no specificity ranking, so an operator
    /// reads top to bottom to see which route wins; a catch-all still needs dispatch's own
    /// addressing check.
    pub fn route(&self, message: &InboundMessage) -> Option<&BoundRoute> {
        self.route_index(message).map(|(_, route)| route)
    }

    pub(crate) fn route_index(&self, message: &InboundMessage) -> Option<(usize, &BoundRoute)> {
        self.find(&message.transport, &message.conversation, &message.subject)
    }

    pub(crate) fn route_for_anchor(&self, anchor: &Anchor) -> Option<&BoundRoute> {
        self.find(
            &anchor.transport().to_string(),
            anchor.conversation(),
            anchor.subject(),
        )
        .map(|(_, route)| route)
        .filter(|route| route.wakes && &route.agent == anchor.agent())
    }

    fn find(
        &self,
        transport: &str,
        conversation: &Conversation,
        subject: &ExternalSubject,
    ) -> Option<(usize, &BoundRoute)> {
        self.routes.iter().enumerate().find(|(_, route)| {
            route.transport == transport
                && route.conversation.matches(conversation)
                && route
                    .subjects
                    .as_ref()
                    .is_none_or(|subjects| subjects.contains(subject))
        })
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }
}

#[derive(Debug, Error)]
#[error("{}", render_problems(.problems))]
pub struct RouteError {
    pub problems: Vec<RouteProblem>,
}

#[derive(Debug, Error)]
pub enum RouteProblem {
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
