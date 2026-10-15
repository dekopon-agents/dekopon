use cedar_policy::{Context, RestrictedExpression};
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
        let mut url = vec![
            (
                "host".to_owned(),
                RestrictedExpression::new_string(self.host),
            ),
            (
                "path".to_owned(),
                RestrictedExpression::new_string(self.path),
            ),
        ];
        if !self.query.is_empty() {
            url.push((
                "query".to_owned(),
                RestrictedExpression::new_string(self.query),
            ));
        }
        let mut request = vec![
            (
                "method".to_owned(),
                RestrictedExpression::new_string(self.method),
            ),
            ("url".to_owned(), record(url)?),
        ];
        let mut headers = BTreeMap::new();
        let mut valid = true;
        for (name, value) in self.headers {
            match String::from_utf8(value) {
                Ok(value) => {
                    let value = RestrictedExpression::new_string(value);
                    valid &= headers.insert(name.to_ascii_lowercase(), value).is_none();
                }
                Err(_) => valid = false,
            }
        }
        if valid {
            request.push(("headers".to_owned(), record(headers)?));
        }
        if let (Some(body), Some(media)) = (self.body, self.content_type)
            && body.len() <= 16 * 1024
            && media
                .split(';')
                .next()
                .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"))
            && let Ok(Unique(value @ Value::Object(_))) = serde_json::from_slice::<Unique>(&body)
            && let Ok(body) = Context::from_json_value(json!({"body": value}), None)
        {
            request.extend(body);
        }
        Context::from_pairs([("request".to_owned(), record(request)?)]).map_err(|source| {
            super::RequestError::Context {
                message: source.to_string(),
            }
        })
    }
}

fn record(
    fields: impl IntoIterator<Item = (String, RestrictedExpression)>,
) -> Result<RestrictedExpression, super::RequestError> {
    RestrictedExpression::new_record(fields).map_err(|source| super::RequestError::Context {
        message: source.to_string(),
    })
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
