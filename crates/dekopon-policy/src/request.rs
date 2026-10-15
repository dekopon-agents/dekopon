use cedar_policy::Context;
use serde::Deserialize;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fmt};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpRequestFacts {
    pub method: String,
    pub host: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Option<Vec<u8>>,
    pub content_type: Option<String>,
}

impl HttpRequestFacts {
    pub(super) fn context(self) -> Result<Context, super::RequestError> {
        let mut url = json!({"host": self.host, "path": self.path});
        if !self.query.is_empty() {
            url["query"] = json!(self.query);
        }
        let mut facts = json!({"request": {"method": self.method, "url": url}});
        let mut headers = BTreeMap::new();
        let mut valid = true;
        for (name, value) in self.headers {
            let name = name.to_ascii_lowercase();
            if name == "__entity" || name == "__extn" {
                valid = false;
            } else if let Ok(value) = String::from_utf8(value) {
                valid &= headers.insert(name, value).is_none();
            } else {
                valid = false;
            }
        }
        if valid {
            facts["request"]["headers"] = json!(headers);
        }
        if let (Some(body), Some(media)) = (self.body, self.content_type)
            && body.len() <= 16 * 1024
            && media
                .split(';')
                .next()
                .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"))
            && let Ok(Unique(value @ Value::Object(_))) = serde_json::from_slice::<Unique>(&body)
            // Cedar must validate the entire body before any of its attributes are exposed.
            && Context::from_json_value(json!({"body": value}), None).is_ok()
        {
            facts["request"]["body"] = value;
        }
        Context::from_json_value(facts, None).map_err(|source| super::RequestError::Context {
            message: source.to_string(),
        })
    }
}

struct Unique(Value);

impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Walk;
        impl<'de> Visitor<'de> for Walk {
            type Value = Unique;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("Cedar-representable JSON")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Unique, E> {
                i64::try_from(v)
                    .map(|n| Unique(json!(n)))
                    .map_err(E::custom)
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Unique, E> {
                Err(E::custom("floating point"))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Unique, E> {
                Err(E::custom("null"))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Unique, A::Error> {
                let mut values = Vec::new();
                while let Some(Unique(value)) = seq.next_element()? {
                    values.push(value);
                }
                Ok(Unique(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Unique, A::Error> {
                let mut fields = serde_json::Map::new();
                while let Some((key, Unique(value))) = map.next_entry::<String, Unique>()? {
                    if key == "__entity" || key == "__extn" || fields.insert(key, value).is_some() {
                        return Err(de::Error::custom("duplicate or reserved key"));
                    }
                }
                Ok(Unique(Value::Object(fields)))
            }
        }
        deserializer.deserialize_any(Walk)
    }
}
