use cedar_policy::{Context, RestrictedExpression};
use serde::Deserialize;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde_json::{Value, error::Category, json};
use std::{collections::BTreeMap, fmt};

const MAX_BODY_BYTES: usize = 16 * 1024;
const OPAQUE: &str = "opaque";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpRequestFacts {
    pub method: String,
    pub host: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: RequestBody,
    pub content_type: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequestBody {
    Buffered(Vec<u8>),
    Streamed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpaqueBody {
    Streamed,
    MediaType,
    TooLarge,
    Malformed,
    Unrepresentable,
}

impl OpaqueBody {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Streamed => "streamed",
            Self::MediaType => "media-type",
            Self::TooLarge => "too-large",
            Self::Malformed => "malformed",
            Self::Unrepresentable => "unrepresentable",
        }
    }
}

impl HttpRequestFacts {
    pub(super) fn context(self) -> Result<(Context, Option<OpaqueBody>), super::RequestError> {
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
        let body = match self.body {
            RequestBody::Buffered(body) if body.is_empty() => None,
            RequestBody::Buffered(body) => Some(json_record(&body, self.content_type.as_deref())),
            RequestBody::Streamed => Some(Err(OpaqueBody::Streamed)),
        };
        let opaque = match body {
            None => None,
            Some(Ok(body)) => {
                request.push(("body".to_owned(), body));
                None
            }
            // A body policy cannot read is present as a non-record, so any attribute test on it
            // errors and denies instead of reading as absent.
            Some(Err(kind)) => {
                request.push((
                    "body".to_owned(),
                    RestrictedExpression::new_string(OPAQUE.to_owned()),
                ));
                Some(kind)
            }
        };
        let context =
            Context::from_pairs([("request".to_owned(), record(request)?)]).map_err(|source| {
                super::RequestError::Context {
                    message: source.to_string(),
                }
            })?;
        Ok((context, opaque))
    }
}

fn json_record(body: &[u8], media: Option<&str>) -> Result<RestrictedExpression, OpaqueBody> {
    if !media
        .and_then(|media| media.split(';').next())
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"))
    {
        return Err(OpaqueBody::MediaType);
    }
    if body.len() > MAX_BODY_BYTES {
        return Err(OpaqueBody::TooLarge);
    }
    let value = match serde_json::from_slice::<Unique>(body) {
        Ok(Unique(value @ Value::Object(_))) => value,
        Ok(Unique(_)) => return Err(OpaqueBody::Unrepresentable),
        Err(error) => {
            return Err(match error.classify() {
                Category::Data => OpaqueBody::Unrepresentable,
                Category::Io | Category::Syntax | Category::Eof => OpaqueBody::Malformed,
            });
        }
    };
    Context::from_json_value(json!({ "body": value }), None)
        .ok()
        .and_then(|context| context.into_iter().next())
        .map(|(_, body)| body)
        .ok_or(OpaqueBody::Unrepresentable)
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
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Unique, E> {
                let text = v.to_string();
                let text = if text.contains('.') {
                    text
                } else {
                    format!("{text}.0")
                };
                match text.split_once('.') {
                    Some((_, fraction)) if fraction.len() <= 4 => {
                        Ok(Unique(json!({"__extn": {"fn": "decimal", "arg": text}})))
                    }
                    Some(_) | None => Err(E::custom("fraction beyond decimal precision")),
                }
            }
            fn visit_unit<E: de::Error>(self) -> Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Unique, A::Error> {
                let mut values = Vec::new();
                while let Some(Unique(value)) = seq.next_element()? {
                    if value.is_null() {
                        return Err(de::Error::custom("null set element"));
                    }
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
                fields.retain(|_, value| !value.is_null());
                Ok(Unique(Value::Object(fields)))
            }
        }
        deserializer.deserialize_any(Walk)
    }
}
