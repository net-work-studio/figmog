//! JSON at file/tree boundaries. Serde's growing-stack adapter handles
//! parsing/printing; owned values are drained iteratively on every exit path.

use std::io::Read;
use std::ops::Deref;

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

pub(crate) fn from_str(text: &str) -> serde_json::Result<Value> {
    deserialize(serde_json::Deserializer::from_str(text))
}

pub(crate) fn from_reader(reader: impl Read) -> serde_json::Result<Value> {
    deserialize(serde_json::Deserializer::from_reader(reader))
}

fn deserialize<'de, R: serde_json::de::Read<'de>>(
    mut deserializer: serde_json::Deserializer<R>,
) -> serde_json::Result<Value> {
    deserializer.disable_recursion_limit();
    let mut value = OwnedValue(Value::deserialize(serde_stacker::Deserializer::new(
        &mut deserializer,
    ))?);
    deserializer.end()?;
    Ok(std::mem::take(&mut value.0))
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
