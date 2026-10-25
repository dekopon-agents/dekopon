use crate::{AgentId, PrincipalId};
use serde::Serialize;

#[derive(Clone, Eq, PartialEq)]
pub struct SpaceBinding {
    pub key: SpaceKey,
    pub principal: PrincipalId,
}

macro_rules! coordinate {
    ($name:ident) => {
        #[derive(Clone, Eq, Hash, PartialEq, Serialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
    };
}

coordinate!(SpaceTransport);
coordinate!(SpaceWorkspace);
coordinate!(SpaceChannel);

#[derive(Clone, Eq, Hash, PartialEq, Serialize)]
pub struct SpaceKey {
    pub agent: AgentId,
    pub transport: SpaceTransport,
    pub workspace: Option<SpaceWorkspace>,
    pub channel: SpaceChannel,
}

impl SpaceKey {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        let absorb = |bytes: &mut Vec<u8>, value: &str| {
            bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
            bytes.extend_from_slice(value.as_bytes());
        };
        absorb(&mut bytes, self.agent.as_str());
        absorb(&mut bytes, &self.transport.0);
        match &self.workspace {
            Some(workspace) => {
                bytes.push(1);
                absorb(&mut bytes, &workspace.0);
            }
            None => bytes.push(0),
        }
        absorb(&mut bytes, &self.channel.0);
        bytes
    }
}
