//! Binding configured routes to catalog agents and configured models, then matching messages.
//!
//! Every binding failure here is a *startup* failure. A route naming a disabled agent or an agent
//! with no reachable model is a configuration mistake, and finding it when the daemon starts beats
//! finding it in a chat reply an hour later.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use dekopon_agent::prompt::PromptLimits;
use dekopon_broker_protocol::ConversationMatch;
use dekopon_config::{LocalCatalog, Skill};
use dekopon_core::{AgentId, ExternalSubject};
use thiserror::Error;

use crate::{
    cache_key,
    config::{MemoryPolicy, ModelConfig, ResolvedConfig, render_problems},
    progress::ProgressDetail,
    transport::InboundMessage,
};

/// One route after its agent and model were resolved.
#[derive(Clone, Debug)]
pub(crate) struct BoundRoute {
    pub transport: String,
    /// Which conversations on that transport this route claims.
    pub conversation: ConversationMatch,
    /// Canonical subjects this route answers; every subject when absent.
    ///
    /// Only a `kind: [directMessage]` route may carry one, which configuration enforces. It
    /// restricts and never widens: a subject listed here still reaches the broker as an ordinary
    /// attested claim, and an unmapped one is refused before any model call.
    pub subjects: Option<Vec<ExternalSubject>>,
    pub agent: AgentId,
    /// Operator-authored purpose safe to expose through credential-free introspection.
    pub description: String,
    /// Catalog model class; never the selected model endpoint or credential configuration.
    pub model_class: Option<String>,
    /// The agent's standing orders, which are untrusted model text and grant nothing.
    pub instructions: Option<String>,
    /// The agent's mounted skills, read whole at catalog load and shared by every session.
    ///
    /// Shared rather than cloned because a bound route is cloned per message, and a skill set
    /// can be a megabyte of text that never changes while the daemon runs.
    pub skills: Arc<[Skill]>,
    pub model: Arc<ModelConfig>,
    /// Attachments one reply on this route may carry; zero for a route that delivers none.
    pub provider_attachments: u8,
    /// Capabilities whose input may name a chat attachment, already checked against the catalog.
    ///
    /// Shared rather than cloned for the reason the skills are: a bound route is cloned per message
    /// and this list never changes while the daemon runs.
    pub chat_asset_inputs: Arc<[String]>,
    /// Whether this route's sessions may record improvement suggestions.
    pub improvement_suggestions: bool,
    /// Whether this route's sessions are offered `inspect_agent_config`.
    pub inspect_agent_config: bool,
    pub limits: PromptLimits,
    /// Wall-clock bound on one session, counted from the moment the agent starts working.
    ///
    /// Absent means no wall-clock bound: the step and capability-call budgets are what most
    /// deployments need, and a bound that cancels a working session is worth choosing rather than
    /// inheriting.
    pub max_duration: Option<Duration>,
    /// How much this route's progress surface says.
    pub progress_detail: ProgressDetail,
    /// What this route remembers between messages.
    pub memory: MemoryPolicy,
    /// The provider cache lane a message on this route uses when it has no conversation of its own.
    ///
    /// Minted once here, at bind time, and shared by every sender the route answers. That reads
    /// alarming and is not, because of what a `oneShot` route's shared prefix actually is: the
    /// agent's `instructions` and the tool definitions, and then this one message. Those are byte
    /// for byte identical for everyone the route serves and contain nothing about any of them, so
    /// pointing the route's traffic at one cache lane shares a prefix that was already common to
    /// all of it. Nothing sender-specific can hit: a different sender's message diverges from the
    /// first token that differs, and a cache key is a hint about a shared *prefix* rather than a
    /// handle on somebody's answer. It is not an authorization boundary and confers nothing — every
    /// message still opens its own attested broker leg.
    ///
    /// The alternative, a fresh key per message, names a lane holding exactly one request and gives
    /// up the only caching a stateless route can have.
    pub cache_key: String,
}

/// Every bound route, consulted in declaration order.
#[derive(Clone, Debug, Default)]
pub(crate) struct RoutingTable {
    routes: Vec<BoundRoute>,
}

impl RoutingTable {
    /// Resolves every configured route against the catalog and the configured models.
    ///
    /// Every route is examined before any of them is refused, for the reason `resolve` scans a
    /// whole configuration file: a deployment whose catalog disabled two of its agents is one
    /// refusal naming both, not two restarts.
    ///
    /// # Errors
    ///
    /// Returns one [`RouteError`] carrying every route that no configuration could satisfy.
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
            // An agent that is absent or disabled settles nothing about which model would serve
            // it, so the class lookup below would report a second problem about the same route.
            // The same skip `dekopon-config` makes when a referenced resource never parsed.
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
            // An explicit override wins; otherwise the agent's declared class picks the first
            // endpoint that offers it. Declaration order is the tie-break, so an operator controls
            // preference by ordering `models` rather than by a hidden score.
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
                provider_attachments: route.provider_attachments,
                chat_asset_inputs: route
                    .chat_asset_inputs
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                improvement_suggestions: route.improvement_suggestions,
                inspect_agent_config: route.inspect_agent_config,
                limits: PromptLimits {
                    max_steps: route.limits.max_steps,
                    max_capability_calls: route.limits.max_capability_calls,
                },
                max_duration: route.limits.max_duration_ms.map(Duration::from_millis),
                progress_detail: route.progress_detail,
                memory: route.memory,
                cache_key: cache_key::for_route(),
            });
        }
        if problems.is_empty() {
            Ok(Self { routes })
        } else {
            Err(RouteError { problems })
        }
    }

    /// The distinct models bound routes can actually reach, in declaration order.
    ///
    /// Startup resolves each one's credential before any transport accepts work. A configured
    /// model no route reaches is not a reason to refuse to start.
    pub fn bound_models(&self) -> Vec<&ModelConfig> {
        let mut seen = BTreeSet::new();
        self.routes
            .iter()
            .filter(|route| seen.insert(route.model.name().to_owned()))
            .map(|route| route.model.as_ref())
            .collect()
    }

    /// First route claiming this conversation, or `None` for ambient traffic.
    ///
    /// Declaration order decides, and that is the whole precedence rule: a route matching one named
    /// channel, written above a route matching any channel, keeps that channel for itself while the
    /// catch-all takes everything else. No specificity ranking sorts them, because a hidden score is
    /// how an operator ends up unable to explain which route answered — the file is read top to
    /// bottom exactly as it looks.
    ///
    /// A catch-all is not a wakeup on its own. `dispatch` still requires channel traffic to address
    /// the bot before any of this becomes a session.
    pub fn route(&self, message: &InboundMessage) -> Option<&BoundRoute> {
        self.routes.iter().find(|route| {
            route.transport == message.transport
                && route.conversation.matches(&message.conversation)
                && route
                    .subjects
                    .as_ref()
                    .is_none_or(|subjects| subjects.contains(&message.subject))
        })
    }

    /// How many routes are bound, for the startup lifecycle event.
    pub fn len(&self) -> usize {
        self.routes.len()
    }
}

/// Every route that no configuration could satisfy, reported as one refusal.
#[derive(Debug, Error)]
#[error("{}", render_problems(.problems))]
pub struct RouteError {
    /// Every unsatisfiable route, in declaration order.
    pub problems: Vec<RouteProblem>,
}

/// One route that no configuration could ever satisfy.
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
