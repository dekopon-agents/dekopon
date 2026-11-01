use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

pub const VM_SESSION_HEADER: &str = "x-dekopon-vm-session";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VmSession([u8; 36]);

impl VmSession {
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let bytes: [u8; 36] = text.trim().as_bytes().try_into().ok()?;
        bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| match index {
                8 | 13 | 18 | 23 => *byte == b'-',
                14 => *byte == b'7',
                19 => matches!(byte, b'8' | b'9' | b'a' | b'b'),
                _ => matches!(byte, b'0'..=b'9' | b'a'..=b'f'),
            })
            .then_some(Self(bytes))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or_default()
    }
}

impl Serialize for VmSession {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for VmSession {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?)
            .ok_or_else(|| D::Error::custom("VM session must be a lowercase canonical UUIDv7"))
    }
}
