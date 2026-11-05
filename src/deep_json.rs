//! JSON at file/tree boundaries. Serde's growing-stack adapter handles
//! parsing/printing; owned values are drained iteratively on every exit path.

use std::fmt;
use std::io::Read;
use std::ops::Deref;

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) struct OwnedValue(pub(crate) Value);

impl Deref for OwnedValue {
    type Target = Value;

    fn deref(&self) -> &Value {
        &self.0
    }
}

impl Drop for OwnedValue {
    fn drop(&mut self) {
        let mut pending = vec![std::mem::take(&mut self.0)];
        while let Some(value) = pending.pop() {
            match value {
                Value::Array(items) => pending.extend(items),
                Value::Object(map) => pending.extend(map.into_values()),
                _ => {}
            }
        }
    }
}

impl OwnedValue {
    /// Transfer to another guarded owner or to the response boundary. Callers
    /// must keep iterative cleanup in place after taking an unbounded value.
    pub(crate) fn into_inner(mut self) -> Value {
        std::mem::take(&mut self.0)
    }

    pub(crate) fn array() -> Self {
        Self(Value::Array(Vec::new()))
    }

    pub(crate) fn object() -> Self {
        Self(Value::Object(serde_json::Map::new()))
    }

    pub(crate) fn push(&mut self, value: Self) {
        self.0
            .as_array_mut()
            .expect("JSON array")
            .push(value.into_inner());
    }

    /// Insertion may replace an arbitrarily deep duplicate-key value.
    pub(crate) fn insert(&mut self, key: String, value: Self) {
        if let Some(old) = self
            .0
            .as_object_mut()
            .expect("JSON object")
            .insert(key, value.into_inner())
        {
            drop(Self(old));
        }
    }
}

/// Guard collections while serde is still constructing them. In particular,
/// an error in a later element/key must not recursively destroy earlier values.
impl<'de> Deserialize<'de> for OwnedValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct JsonVisitor;

        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = OwnedValue;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON value")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(OwnedValue(Value::Bool(value)))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(OwnedValue(Value::Number(value.into())))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(OwnedValue(Value::Number(value.into())))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
                Ok(OwnedValue(Value::from(value)))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(OwnedValue(Value::String(value.to_string())))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(OwnedValue(Value::String(value)))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(OwnedValue(Value::Null))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = OwnedValue::array();
                while let Some(value) = seq.next_element::<OwnedValue>()? {
                    values.push(value);
                }
                Ok(values)
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = OwnedValue::object();
                while let Some(key) = map.next_key::<String>()? {
                    values.insert(key, map.next_value::<OwnedValue>()?);
                }
                Ok(values)
            }
        }

        deserializer.deserialize_any(JsonVisitor)
    }
}

pub(crate) fn from_str(text: &str) -> serde_json::Result<OwnedValue> {
    deserialize(serde_json::Deserializer::from_str(text))
}

pub(crate) fn from_reader(reader: impl Read) -> serde_json::Result<OwnedValue> {
    deserialize(serde_json::Deserializer::from_reader(reader))
}

fn deserialize<'de, R: serde_json::de::Read<'de>>(
    mut deserializer: serde_json::Deserializer<R>,
) -> serde_json::Result<OwnedValue> {
    deserializer.disable_recursion_limit();
    let value = OwnedValue::deserialize(serde_stacker::Deserializer::new(&mut deserializer))?;
    deserializer.end()?;
    Ok(value)
}

pub(crate) fn to_string(value: &(impl Serialize + ?Sized)) -> serde_json::Result<String> {
    let mut bytes = Vec::new();
    let mut serializer = serde_json::Serializer::new(&mut bytes);
    value.serialize(serde_stacker::Serializer::new(&mut serializer))?;
    Ok(String::from_utf8(bytes).expect("JSON is UTF-8"))
}

pub(crate) fn to_string_pretty(value: &Value) -> serde_json::Result<String> {
    let mut bytes = Vec::new();
    let mut serializer = serde_json::Serializer::pretty(&mut bytes);
    value.serialize(serde_stacker::Serializer::new(&mut serializer))?;
    Ok(String::from_utf8(bytes).expect("JSON is UTF-8"))
}

#[cfg(test)]
mod tests;
