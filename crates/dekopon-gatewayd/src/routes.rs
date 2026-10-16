use std::{collections::BTreeSet, sync::Arc, time::Duration};

use dekopon_agent::prompt::PromptLimits;
use dekopon_broker_protocol::{Conversation, ConversationKind, ConversationMatch};
use dekopon_config::{LocalCatalog, Skill};
use dekopon_core::{AgentId, ExternalSubject};
use thiserror::Error;

use crate::{
    cache_key,
    config::{
        GithubEvent, MemoryPolicy, ModelConfig, ResolvedConfig, ResolvedLiveness, ResolvedPolicy,
        Steering, render_problems,
    },
    progress::ProgressDetail,
    transport::InboundMessage,
    wake::Anchor,
};

#[expect(
    clippy::struct_excessive_bools,
    reason = "bound route carries independent validated route switches"
)]
#[derive(Clone, Debug)]
pub(crate) struct BoundRoute {
    pub route_index: usize,
    pub transport: String,
    pub conversation: ConversationMatch,
    pub subjects: Option<Vec<ExternalSubject>>,
    pub github_events: Option<Vec<GithubEvent>>,
    pub agent: AgentId,
    pub description: String,
    pub model_class: Option<String>,
    pub instructions: Option<String>,
    pub skills: Arc<[Skill]>,
    pub model: Arc<ModelConfig>,
    pub inspect_agent_config: bool,
    pub limits: PromptLimits,
    pub max_duration: Option<Duration>,
    pub script_timeout: Duration,
    pub job_timeout: Option<Duration>,
    pub progress_detail: ProgressDetail,
    pub steering: Steering,
    pub progress_notes: bool,
    pub memory: MemoryPolicy,
    pub memory_policy: Option<ResolvedPolicy>,
    pub indicators: std::collections::BTreeMap<ConversationKind, Arc<ResolvedLiveness>>,
    pub wakes: bool,
    pub follow: bool,
    /// This cache lane is safe to share since its prefix is byte-identical and sender-agnostic
    /// across the route's traffic, and grants nothing: every new session opens its own attested
    /// broker leg.
    pub cache_key: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RoutingTable {
    routes: Vec<BoundRoute>,
}

impl RoutingTable {
    pub(crate) fn has_persistent_memory(&self) -> bool {
        self.routes
            .iter()
            .any(|route| route.memory.window().is_some())
    }

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
        for (route_index, route) in config.routes.iter().enumerate() {
            let Some(agent) = catalog.agent(&route.agent) else {
                problems.push(RouteProblem::UnknownAgent { route: route_index });
                continue;
            };
            if !agent.spec.enabled {
                problems.push(RouteProblem::DisabledAgent {
                    route: route_index,
                    agent: route.agent.to_string(),
                });
                continue;
            }
            let selected = match &route.model {
                Some(name) => models
                    .iter()
                    .find(|model| model.name() == name)
                    .ok_or(RouteProblem::UnknownModel { route: route_index }),
                None => match agent.spec.model_class.as_deref() {
                    None => Err(RouteProblem::NoModelClass {
                        route: route_index,
                        agent: route.agent.to_string(),
                    }),
                    Some(class) => models
                        .iter()
                        .find(|model| model.classes().iter().any(|offered| offered == class))
                        .ok_or_else(|| RouteProblem::NoModelForClass {
                            route: route_index,
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
                route_index,
                transport: route.transport.clone(),
                conversation: route.conversation.clone(),
                subjects: route.subjects.clone(),
                github_events: route.github_events.clone(),
                agent: route.agent.clone(),
                description: agent.spec.description.clone(),
                model_class: agent.spec.model_class.clone(),
                instructions: agent.spec.instructions.clone(),
                skills: Arc::from(catalog.agent_skills(&route.agent).to_vec()),
                model: Arc::clone(model),
                inspect_agent_config: route.inspect_agent_config,
                limits: PromptLimits {
                    max_steps: route.limits.max_steps,
                    max_capability_calls: route.limits.max_capability_calls,
                },
                max_duration: route.limits.max_duration_ms.map(Duration::from_millis),
                script_timeout: route.limits.script_timeout(),
                job_timeout: route.limits.job_timeout_ms.map(Duration::from_millis),
                progress_detail: route.progress_detail,
                steering: route.steering,
                progress_notes: route.progress_notes,
                memory: route.memory,
                memory_policy: route.memory_policy.clone(),
                indicators: route.indicators.clone(),
                wakes: route.wakes,
                follow: route.follow,
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

    pub(crate) fn serving(&self, model: &str) -> Vec<String> {
        self.routes
            .iter()
            .filter(|route| route.model.name() == model)
            .map(|route| format!("{}:{}", route.transport, route.agent))
            .collect()
    }

    /// Declaration order is the only precedence rule, with no specificity ranking, so an operator
    /// reads top to bottom to see which route wins; a catch-all still needs dispatch's own
    /// addressing check.
    pub(crate) fn github_route(
        &self,
        transport: &str,
        conversation: &Conversation,
        subject: &ExternalSubject,
        event: GithubEvent,
    ) -> Option<&BoundRoute> {
        self.find(transport, conversation, subject)
            .map(|(_, route)| route)
            .filter(|route| {
                route
                    .github_events
                    .as_ref()
                    .is_some_and(|events| events.contains(&event))
            })
    }

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
    #[error("routes[{route}] names an agent which is not in the catalog")]
    UnknownAgent { route: usize },
    #[error("routes[{route}] names agent {agent:?}, which the catalog disables")]
    DisabledAgent { route: usize, agent: String },
    #[error("routes[{route}] names a model which is not configured")]
    UnknownModel { route: usize },
    #[error(
        "routes[{route}]: agent {agent:?} declares no modelClass and its route names no model, so no model can serve it"
    )]
    NoModelClass { route: usize, agent: String },
    #[error(
        "routes[{route}]: agent {agent:?} needs model class {class:?}, which no configured model offers"
    )]
    NoModelForClass {
        route: usize,
        agent: String,
        class: String,
    },
}
