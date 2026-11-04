use std::collections::BTreeSet;

use cedar_policy::{Effect, PartialResponse, Policy, PolicySet};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequestOutcome {
    Denied,
    Open {
        permit_satisfied: bool,
        residuals: BTreeSet<String>,
    },
}

impl RequestOutcome {
    pub(crate) fn from_partial(response: &PartialResponse) -> Self {
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
        Self::Open {
            permit_satisfied,
            residuals: response
                .nontrivial_residuals()
                .map(|residual| canonical_residual(&residual))
                .collect(),
        }
    }
}

fn canonical_residual(residual: &Policy) -> String {
    match residual.to_json() {
        Ok(mut json) => {
            if let Some(fields) = json.as_object_mut() {
                fields.remove("annotations");
            }
            json.to_string()
        }
        Err(_) => residual.to_string(),
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
