use crate::{AgentId, PrincipalId};
use serde::Serialize;

#[derive(Clone, Eq, PartialEq)]
pub struct SpaceBinding {
    pub key: SpaceKey,
    pub principal: PrincipalId,
}

#[derive(Clone, Eq, Hash, PartialEq, Serialize)]
pub struct SpaceKey {
    pub agent: AgentId,
    pub transport: String,
    pub workspace: Option<String>,
    pub channel: String,
}

impl SpaceKey {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        let absorb = |bytes: &mut Vec<u8>, value: &str| {
            bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
            bytes.extend_from_slice(value.as_bytes());
        };
        absorb(&mut bytes, self.agent.as_str());
        absorb(&mut bytes, &self.transport);
        match &self.workspace {
            Some(workspace) => {
                bytes.push(1);
                absorb(&mut bytes, workspace);
            }
            None => bytes.push(0),
        }
        absorb(&mut bytes, &self.channel);
        bytes
    }
}
