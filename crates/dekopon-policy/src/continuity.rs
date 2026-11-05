use std::{collections::BTreeSet, str::FromStr as _};

use cedar_policy::{
    Effect, Entities, EntityId, EntityTypeName, EntityUid, PartialResponse, PolicySet,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequestOutcome {
    Denied,
    Open {
        permit_satisfied: bool,
        residuals: BTreeSet<String>,
        entities: BTreeSet<String>,
    },
}

impl RequestOutcome {
    pub(crate) fn from_partial(response: &PartialResponse, store: &Entities) -> Self {
        // Dekopon denies on any evaluation error, so an errored policy must never read as false.
        if response.definitely_errored().next().is_some() {
            return Self::Denied;
        }
        let mut permit_satisfied = false;
        for policy in response.definitely_satisfied() {
            match policy.effect() {
                Effect::Forbid => return Self::Denied,
                Effect::Permit => permit_satisfied = true,
            }
        }
        let mut residuals = BTreeSet::new();
        let mut referenced = Some(BTreeSet::new());
        for residual in response.nontrivial_residuals() {
            match residual.to_json() {
                Ok(mut json) => {
                    if let (Some(found), Some(fields)) = (referenced.as_mut(), json.as_object_mut())
                    {
                        fields.remove("annotations");
                        entity_literals(&serde_json::Value::Object(fields.clone()), found);
                    }
                    residuals.insert(json.to_string());
                }
                Err(_) => {
                    referenced = None;
                    residuals.insert(residual.to_string());
                }
            }
        }
        // A residual still tests entity facts the request decides between (both branches of a
        // request-dependent selector), so their attributes and ancestry are part of its meaning.
        let entities = match referenced {
            Some(uids) => uids.iter().map(|uid| entity_facts(store, uid)).collect(),
            None => store
                .iter()
                .map(|entity| entity_facts(store, &entity.uid()))
                .collect(),
        };
        Self::Open {
            permit_satisfied,
            residuals,
            entities,
        }
    }
}

fn entity_literals(value: &serde_json::Value, found: &mut BTreeSet<EntityUid>) {
    match value {
        serde_json::Value::Object(fields) => {
            if let Some(entity) = fields.get("__entity")
                && let (Some(kind), Some(id)) = (
                    entity.get("type").and_then(serde_json::Value::as_str),
                    entity.get("id").and_then(serde_json::Value::as_str),
                )
                && let Ok(kind) = EntityTypeName::from_str(kind)
            {
                found.insert(EntityUid::from_type_name_and_id(kind, EntityId::new(id)));
            }
            for nested in fields.values() {
                entity_literals(nested, found);
            }
        }
        serde_json::Value::Array(values) => {
            for nested in values {
                entity_literals(nested, found);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}

fn entity_facts(store: &Entities, uid: &EntityUid) -> String {
    let attributes = store
        .get(uid)
        .and_then(|entity| entity.to_json_value().ok())
        .map(|json| sorted_json(&json["attrs"]).to_string())
        .unwrap_or_default();
    let mut ancestors = store
        .ancestors(uid)
        .into_iter()
        .flatten()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    ancestors.sort();
    format!("{uid}\0{attributes}\0{}", ancestors.join("\0"))
}

fn sorted_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(fields) => {
            let mut keys = fields.keys().collect::<Vec<_>>();
            keys.sort();
            serde_json::Value::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), sorted_json(&fields[key])))
                    .collect(),
            )
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(sorted_json).collect())
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => value.clone(),
    }
}

pub(crate) fn routing_dependent_policies(policies: &PolicySet) -> Vec<String> {
    let mut dependent = policies
        .policies()
        .filter(|policy| {
            let Ok(json) = policy.to_json() else {
                return true;
            };
            let mut uses = ContextUses::default();
            walk(&json, &mut uses);
            uses.reads_request() && uses.reads_routing()
        })
        .map(|policy| policy.id().to_string())
        .collect::<Vec<_>>();
    dependent.sort();
    dependent
}

#[derive(Default)]
struct ContextUses {
    whole: bool,
    paths: Vec<Vec<String>>,
}

impl ContextUses {
    fn reads_request(&self) -> bool {
        self.whole
            || self
                .paths
                .iter()
                .any(|path| path.first().is_some_and(|field| field == "request"))
    }

    fn reads_routing(&self) -> bool {
        self.whole
            || self.paths.iter().any(|path| match path.as_slice() {
                [first, ..]
                    if ["via", "trigger", "startedBy", "github"].contains(&first.as_str()) =>
                {
                    true
                }
                [first] => first == "conversation",
                [first, second, ..] => {
                    first == "conversation" && (second == "kind" || second == "container")
                }
                [] => false,
            })
    }
}

// Any use of `context` other than a chain of literal attribute steps counts as the whole value, so a
// record wrapper, comparison, extension argument or unfamiliar node can never hide a read.
fn walk(value: &serde_json::Value, uses: &mut ContextUses) {
    match value {
        serde_json::Value::Object(fields) => {
            if fields.get("Var").and_then(serde_json::Value::as_str) == Some("context") {
                uses.whole = true;
                return;
            }
            for op in [".", "has"] {
                if let Some(access) = fields.get(op)
                    && let Some(path) = context_path(op, access)
                {
                    uses.paths.push(path);
                    return;
                }
            }
            for nested in fields.values() {
                walk(nested, uses);
            }
        }
        serde_json::Value::Array(values) => {
            for nested in values {
                walk(nested, uses);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}

fn context_path(op: &str, access: &serde_json::Value) -> Option<Vec<String>> {
    let left = access.get("left")?;
    let mut path = if left.get("Var").and_then(serde_json::Value::as_str) == Some("context") {
        Vec::new()
    } else {
        let inner = left.get(".")?;
        context_path(".", inner)?
    };
    match access.get("attr")? {
        serde_json::Value::String(attr) => path.push(attr.clone()),
        serde_json::Value::Array(attrs) if op == "has" => {
            for attr in attrs {
                path.push(attr.as_str()?.to_owned());
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::Array(_)
        | serde_json::Value::Object(_) => return None,
    }
    Some(path)
}
