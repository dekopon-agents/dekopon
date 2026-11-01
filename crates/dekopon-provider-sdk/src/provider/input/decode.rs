use super::Kind;
use serde::de::{
    self, DeserializeOwned, DeserializeSeed, Deserializer, EnumAccess, MapAccess, SeqAccess,
    VariantAccess, Visitor,
};
use serde_json::Value;
use std::{borrow::Cow, cell::RefCell, fmt};

#[derive(Clone, Copy)]
pub(super) enum Shape {
    Null,
    Bool,
    Number,
    String,
    Array,
    Object,
}

impl Shape {
    pub(super) fn matches(self, value: &Value) -> bool {
        match self {
            Self::Null => value.is_null(),
            Self::Bool => value.is_boolean(),
            Self::Number => value.is_number(),
            Self::String => value.is_string(),
            Self::Array => value.is_array(),
            Self::Object => value.is_object(),
        }
    }
    fn unexpected(value: de::Unexpected<'_>) -> Option<Self> {
        match value {
            de::Unexpected::Unit | de::Unexpected::Option => Some(Self::Null),
            de::Unexpected::Bool(_) => Some(Self::Bool),
            de::Unexpected::Unsigned(_) | de::Unexpected::Signed(_) | de::Unexpected::Float(_) => {
                Some(Self::Number)
            }
            de::Unexpected::Str(_) | de::Unexpected::Char(_) => Some(Self::String),
            de::Unexpected::Seq | de::Unexpected::Bytes(_) => Some(Self::Array),
            de::Unexpected::Map => Some(Self::Object),
            de::Unexpected::NewtypeStruct
            | de::Unexpected::Enum
            | de::Unexpected::UnitVariant
            | de::Unexpected::NewtypeVariant
            | de::Unexpected::TupleVariant
            | de::Unexpected::StructVariant
            | de::Unexpected::Other(_) => None,
        }
    }
}

pub(super) struct Error {
    pub(super) kind: Kind,
    pub(super) field: Option<Cow<'static, str>>,
    pub(super) unexpected: Option<Shape>,
}

impl Error {
    fn new(kind: Kind) -> Self {
        Self {
            kind,
            field: None,
            unexpected: None,
        }
    }
}
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InputError")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("input was refused")
    }
}
impl std::error::Error for Error {}
impl de::Error for Error {
    fn custom<T: fmt::Display>(_: T) -> Self {
        Self::new(Kind::InvalidValue)
    }
    fn invalid_type(value: de::Unexpected<'_>, _: &dyn de::Expected) -> Self {
        Self {
            unexpected: Shape::unexpected(value),
            ..Self::new(Kind::WrongType)
        }
    }
    fn invalid_value(value: de::Unexpected<'_>, _: &dyn de::Expected) -> Self {
        Self {
            unexpected: Shape::unexpected(value),
            ..Self::new(Kind::InvalidValue)
        }
    }
    fn invalid_length(_: usize, _: &dyn de::Expected) -> Self {
        Self::new(Kind::InvalidLength)
    }
    fn unknown_variant(_: &str, _: &'static [&'static str]) -> Self {
        Self::new(Kind::InvalidValue)
    }
    fn unknown_field(name: &str, _: &'static [&'static str]) -> Self {
        Self {
            field: Some(Cow::Owned(name.to_owned())),
            ..Self::new(Kind::UnknownField)
        }
    }
    fn missing_field(name: &'static str) -> Self {
        Self {
            field: Some(Cow::Borrowed(name)),
            ..Self::new(Kind::MissingField)
        }
    }
    fn duplicate_field(name: &'static str) -> Self {
        Self {
            field: Some(Cow::Borrowed(name)),
            ..Self::new(Kind::DuplicateField)
        }
    }
}

#[derive(Default)]
struct Capture(RefCell<Option<Error>>);
impl Capture {
    fn bridge<E: de::Error>(&self, error: Error) -> E {
        self.0.replace(Some(error));
        E::custom("input was refused")
    }
    fn restore<E>(&self, _: E) -> Error {
        self.0
            .borrow_mut()
            .take()
            .unwrap_or_else(|| Error::new(Kind::WrongType))
    }
}
struct Wrapped<'a, T> {
    inner: T,
    capture: &'a Capture,
}

macro_rules! deserialize {
    ($($method:ident($($arg:ident: $ty:ty),*);)*) => {$(
        fn $method<V: Visitor<'de>>(self, $($arg: $ty,)* visitor: V) -> Result<V::Value, Self::Error> {
            self.inner.$method($($arg,)* Wrapped { inner: visitor, capture: self.capture })
                .map_err(|error| self.capture.restore(error))
        }
    )*};
}
impl<'de, D: Deserializer<'de>> Deserializer<'de> for Wrapped<'_, D> {
    type Error = Error;
    deserialize! {
        deserialize_any(); deserialize_bool(); deserialize_i8(); deserialize_i16(); deserialize_i32();
        deserialize_i64(); deserialize_i128(); deserialize_u8(); deserialize_u16(); deserialize_u32();
        deserialize_u64(); deserialize_u128(); deserialize_f32(); deserialize_f64(); deserialize_char();
        deserialize_str(); deserialize_string(); deserialize_bytes(); deserialize_byte_buf();
        deserialize_option(); deserialize_unit(); deserialize_unit_struct(name: &'static str);
        deserialize_newtype_struct(name: &'static str); deserialize_seq(); deserialize_tuple(len: usize);
        deserialize_tuple_struct(name: &'static str, len: usize);
        deserialize_map(); deserialize_struct(name: &'static str, fields: &'static [&'static str]);
        deserialize_enum(name: &'static str, variants: &'static [&'static str]);
        deserialize_identifier(); deserialize_ignored_any();
    }
    fn is_human_readable(&self) -> bool {
        self.inner.is_human_readable()
    }
}

macro_rules! visit {
    ($($method:ident($($arg:ident: $ty:ty),*);)*) => {$(
        fn $method<E: de::Error>(self, $($arg: $ty),*) -> Result<Self::Value, E> {
            self.inner.$method::<Error>($($arg),*).map_err(|error| self.capture.bridge(error))
        }
    )*};
}
impl<'de, V: Visitor<'de>> Visitor<'de> for Wrapped<'_, V> {
    type Value = V::Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.expecting(f)
    }
    visit! {
        visit_bool(value: bool); visit_i8(value: i8); visit_i16(value: i16); visit_i32(value: i32);
        visit_i64(value: i64); visit_i128(value: i128); visit_u8(value: u8); visit_u16(value: u16);
        visit_u32(value: u32); visit_u64(value: u64); visit_u128(value: u128); visit_f32(value: f32);
        visit_f64(value: f64); visit_char(value: char); visit_str(value: &str);
        visit_borrowed_str(value: &'de str); visit_string(value: String); visit_bytes(value: &[u8]);
        visit_borrowed_bytes(value: &'de [u8]); visit_byte_buf(value: Vec<u8>); visit_unit(); visit_none();
    }
    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        self.inner
            .visit_some(Wrapped {
                inner: deserializer,
                capture: self.capture,
            })
            .map_err(|error| self.capture.bridge(error))
    }
    fn visit_newtype_struct<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        self.inner
            .visit_newtype_struct(Wrapped {
                inner: deserializer,
                capture: self.capture,
            })
            .map_err(|error| self.capture.bridge(error))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, access: A) -> Result<Self::Value, A::Error> {
        self.inner
            .visit_seq(Wrapped {
                inner: access,
                capture: self.capture,
            })
            .map_err(|error| self.capture.bridge(error))
    }
    fn visit_map<A: MapAccess<'de>>(self, access: A) -> Result<Self::Value, A::Error> {
        self.inner
            .visit_map(Wrapped {
                inner: access,
                capture: self.capture,
            })
            .map_err(|error| self.capture.bridge(error))
    }
    fn visit_enum<A: EnumAccess<'de>>(self, access: A) -> Result<Self::Value, A::Error> {
        self.inner
            .visit_enum(Wrapped {
                inner: access,
                capture: self.capture,
            })
            .map_err(|error| self.capture.bridge(error))
    }
}
impl<'de, S: DeserializeSeed<'de>> DeserializeSeed<'de> for Wrapped<'_, S> {
    type Value = S::Value;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        self.inner
            .deserialize(Wrapped {
                inner: deserializer,
                capture: self.capture,
            })
            .map_err(|error| self.capture.bridge(error))
    }
}
impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for Wrapped<'_, A> {
    type Error = Error;
    fn next_element_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, Error> {
        self.inner
            .next_element_seed(Wrapped {
                inner: seed,
                capture: self.capture,
            })
            .map_err(|error| self.capture.restore(error))
    }
    fn size_hint(&self) -> Option<usize> {
        self.inner.size_hint()
    }
}
impl<'de, A: MapAccess<'de>> MapAccess<'de> for Wrapped<'_, A> {
    type Error = Error;
    fn next_key_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, Error> {
        self.inner
            .next_key_seed(Wrapped {
                inner: seed,
                capture: self.capture,
            })
            .map_err(|error| self.capture.restore(error))
    }
    fn next_value_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<S::Value, Error> {
        self.inner
            .next_value_seed(Wrapped {
                inner: seed,
                capture: self.capture,
            })
            .map_err(|error| self.capture.restore(error))
    }
    fn next_entry_seed<K: DeserializeSeed<'de>, V: DeserializeSeed<'de>>(
        &mut self,
        key: K,
        value: V,
    ) -> Result<Option<(K::Value, V::Value)>, Error> {
        self.inner
            .next_entry_seed(
                Wrapped {
                    inner: key,
                    capture: self.capture,
                },
                Wrapped {
                    inner: value,
                    capture: self.capture,
                },
            )
            .map_err(|error| self.capture.restore(error))
    }
    fn size_hint(&self) -> Option<usize> {
        self.inner.size_hint()
    }
}
impl<'de, 'a, A: EnumAccess<'de>> EnumAccess<'de> for Wrapped<'a, A> {
    type Error = Error;
    type Variant = Wrapped<'a, A::Variant>;
    fn variant_seed<S: DeserializeSeed<'de>>(
        self,
        seed: S,
    ) -> Result<(S::Value, Self::Variant), Error> {
        self.inner
            .variant_seed(Wrapped {
                inner: seed,
                capture: self.capture,
            })
            .map(|(value, variant)| {
                (
                    value,
                    Wrapped {
                        inner: variant,
                        capture: self.capture,
                    },
                )
            })
            .map_err(|error| self.capture.restore(error))
    }
}
impl<'de, A: VariantAccess<'de>> VariantAccess<'de> for Wrapped<'_, A> {
    type Error = Error;
    fn unit_variant(self) -> Result<(), Error> {
        self.inner
            .unit_variant()
            .map_err(|error| self.capture.restore(error))
    }
    fn newtype_variant_seed<S: DeserializeSeed<'de>>(self, seed: S) -> Result<S::Value, Error> {
        self.inner
            .newtype_variant_seed(Wrapped {
                inner: seed,
                capture: self.capture,
            })
            .map_err(|error| self.capture.restore(error))
    }
    fn tuple_variant<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, Error> {
        self.inner
            .tuple_variant(
                len,
                Wrapped {
                    inner: visitor,
                    capture: self.capture,
                },
            )
            .map_err(|error| self.capture.restore(error))
    }
    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.inner
            .struct_variant(
                fields,
                Wrapped {
                    inner: visitor,
                    capture: self.capture,
                },
            )
            .map_err(|error| self.capture.restore(error))
    }
}

pub(super) fn failure<I: DeserializeOwned>(
    input: &str,
) -> Option<serde_path_to_error::Error<Error>> {
    let capture = Capture::default();
    let mut json = serde_json::Deserializer::from_str(input);
    serde_path_to_error::deserialize::<_, I>(Wrapped {
        inner: &mut json,
        capture: &capture,
    })
    .err()
}
