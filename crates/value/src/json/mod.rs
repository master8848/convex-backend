//! Bijection between a subset of JSON values and our [`Value`]s.
//!
//! Notable features:
//!
//! 1) JSON numbers (64-bit floating point) are mapped to `Number`s.
//! 2) Int64 integers are encoded as their little endian representation in
//!    base64: {"$integer": "..."}.
//! 3) Blobs are encoded as base64: {"$binary": "..."}.
//! 4) Objects are not allowed to have keys starting with "$".

pub mod bytes;
pub mod float;
pub mod integer;
pub(crate) mod json_packed_value;

use std::{
    collections::BTreeMap,
    num::FpCategory,
};

use anyhow::{
    anyhow,
    bail,
    Error,
    Result,
};
use serde::{
    de::{
        DeserializeSeed,
        Error as DeError,
        MapAccess,
        SeqAccess,
        Visitor,
    },
    Deserializer,
};
use serde_json::Value as JsonValue;

use crate::{
    json::{
        bytes::JsonBytes,
        float::JsonFloat,
        integer::JsonInteger,
    },
    numeric::is_negative_zero,
    object::ConvexObject,
    walk::ConvexValueType,
    ConvexArray,
    ConvexString,
    ConvexValue,
    FieldName,
};

pub mod value {
    use std::{
        cell::Cell,
        num::FpCategory,
    };

    use serde::{
        ser::{
            Error,
            SerializeMap,
            SerializeSeq,
        },
        Serialize,
        Serializer,
    };

    use crate::{
        numeric::is_negative_zero,
        walk::{
            ConvexArrayWalker,
            ConvexBytesWalker,
            ConvexObjectWalker,
            ConvexStringWalker,
            ConvexValueType,
            ConvexValueWalker,
        },
        JsonBytes,
        JsonFloat,
        JsonInteger,
    };

    /// Wrapper for `ConvexValueWalker` that implements `Serialize`.
    ///
    /// Note that `ConvexValueWalker` can only be walked once (consuming the
    /// walker) whereas the `serde::Serialize` trait takes `&self`; to bridge
    /// them, we use a `Cell<Option>` and return an error if the same
    /// `SerializeValue` is serialized more than once.
    pub(crate) struct SerializeValue<V>(Cell<Option<V>>);
    impl<V> SerializeValue<V> {
        pub(crate) fn new(value: V) -> Self {
            Self(Cell::new(Some(value)))
        }
    }
    impl<V: ConvexValueWalker> Serialize for SerializeValue<V> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serialize(
                self.0
                    .take()
                    .ok_or_else(|| Error::custom("cannot serialize value more than once"))?,
                serializer,
            )
        }
    }

    pub fn serialize<V: ConvexValueWalker, S: Serializer>(
        value: V,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value.walk().map_err(Error::custom)? {
            ConvexValueType::Null => serializer.serialize_unit(),
            ConvexValueType::Int64(n) => {
                let mut obj = serializer.serialize_map(Some(1))?;
                obj.serialize_entry("$integer", &JsonInteger::encode(n))?;
                obj.end()
            },
            ConvexValueType::Float64(n) => {
                let mut is_special = is_negative_zero(n);
                is_special |= match n.classify() {
                    FpCategory::Zero | FpCategory::Normal | FpCategory::Subnormal => false,
                    FpCategory::Infinite | FpCategory::Nan => true,
                };
                if is_special {
                    let mut obj = serializer.serialize_map(Some(1))?;
                    obj.serialize_entry("$float", &JsonFloat::encode(n))?;
                    obj.end()
                } else {
                    serializer.serialize_f64(n)
                }
            },
            ConvexValueType::Boolean(b) => serializer.serialize_bool(b),
            ConvexValueType::String(s) => serializer.serialize_str(s.as_str()),
            ConvexValueType::Bytes(b) => {
                let mut obj = serializer.serialize_map(Some(1))?;
                obj.serialize_entry("$bytes", &JsonBytes::encode(b.as_bytes()))?;
                obj.end()
            },
            ConvexValueType::Array(a) => {
                let iter = a.walk();
                let mut seq = serializer.serialize_seq(size_hint(&iter))?;
                for value in iter {
                    let value = value.map_err(Error::custom)?;
                    seq.serialize_element(&SerializeValue::new(value))?;
                }
                seq.end()
            },
            ConvexValueType::Object(o) => {
                let iter = o.walk();
                let mut map = serializer.serialize_map(size_hint(&iter))?;
                for pair in iter {
                    let (key, value) = pair.map_err(Error::custom)?;
                    map.serialize_entry(key.as_str(), &SerializeValue::new(value))?;
                }
                map.end()
            },
        }
    }

    fn size_hint(iter: &impl Iterator) -> Option<usize> {
        let (lo, hi) = iter.size_hint();
        if hi == Some(lo) {
            hi
        } else {
            None
        }
    }
}

pub mod object {
    use serde::{
        Deserialize,
        Deserializer,
        Serializer,
    };
    use serde_json::Value as JsonValue;

    use crate::{
        walk::ConvexValueType,
        ConvexObject,
        ConvexValue,
    };

    pub fn serialize<S: Serializer>(
        object: &ConvexObject,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        super::value::serialize(ConvexValueType::<&ConvexValue>::Object(object), serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<ConvexObject, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = JsonValue::deserialize(deserializer)?;
        ConvexObject::try_from(value).map_err(serde::de::Error::custom)
    }
}

impl TryFrom<JsonValue> for ConvexValue {
    type Error = Error;

    #[allow(clippy::float_cmp)]
    fn try_from(value: JsonValue) -> Result<Self> {
        let r = match value {
            JsonValue::Null => Self::Null,
            JsonValue::Bool(b) => Self::from(b),
            JsonValue::Number(n) => {
                // TODO/WTF: JSON supports arbitrary precision numbers?
                let n = n
                    .as_f64()
                    .ok_or_else(|| anyhow!("Arbitrary precision JSON integers unsupported"))?;
                ConvexValue::from(n)
            },
            JsonValue::String(s) => Self::try_from(s)?,
            JsonValue::Array(arr) => {
                let mut out = Vec::with_capacity(arr.len());
                for a in arr {
                    out.push(ConvexValue::try_from(a)?);
                }
                ConvexValue::Array(out.try_into()?)
            },
            JsonValue::Object(map) => {
                if map.len() == 1 {
                    let (key, value) = map.into_iter().next().unwrap();
                    match &key[..] {
                        "$bytes" => {
                            let i: String = serde_json::from_value(value)?;
                            Self::Bytes(JsonBytes::decode(i)?)
                        },
                        "$integer" => {
                            let i: String = serde_json::from_value(value)?;
                            Self::from(JsonInteger::decode(i)?)
                        },
                        "$float" => {
                            let i: String = serde_json::from_value(value)?;
                            let n = JsonFloat::decode(i)?;
                            // Float64s encoded as a $float object must not fit into a regular
                            // `number`.
                            if !is_negative_zero(n)
                                && let FpCategory::Normal | FpCategory::Subnormal = n.classify()
                            {
                                bail!("Float64 {} should be encoded as a number", n);
                            }
                            Self::from(n)
                        },
                        _ => Self::Object(ConvexObject::for_value(
                            key.parse()?,
                            Self::try_from(value)?,
                        )?),
                    }
                } else {
                    let mut fields = BTreeMap::new();
                    for (key, value) in map {
                        fields.insert(key.parse()?, Self::try_from(value)?);
                    }
                    Self::Object(fields.try_into()?)
                }
            },
        };
        Ok(r)
    }
}

impl TryFrom<JsonValue> for ConvexArray {
    type Error = Error;

    fn try_from(object: JsonValue) -> Result<Self> {
        Self::try_from(ConvexValue::try_from(object)?)
    }
}

impl TryFrom<JsonValue> for ConvexObject {
    type Error = anyhow::Error;

    fn try_from(object: JsonValue) -> anyhow::Result<Self> {
        ConvexValue::try_from(object)?.try_into()
    }
}

impl From<ConvexValue> for JsonValue {
    fn from(value: ConvexValue) -> Self {
        value.to_internal_json()
    }
}

impl ConvexValue {
    pub fn to_internal_json(&self) -> JsonValue {
        value::serialize(self, serde_json::value::Serializer)
            .expect("Failed to serialize to JsonValue")
    }

    pub fn json_serialize(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string(&value::SerializeValue::new(self))?)
    }
}

impl From<ConvexObject> for JsonValue {
    fn from(value: ConvexObject) -> Self {
        value.to_internal_json()
    }
}

impl ConvexObject {
    pub fn to_internal_json(&self) -> JsonValue {
        value::serialize(
            ConvexValueType::<&ConvexValue>::Object(self),
            serde_json::value::Serializer,
        )
        .expect("Failed to serialize to JsonValue")
    }

    pub fn json_serialize(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string(&value::SerializeValue::new(
            ConvexValueType::<&ConvexValue>::Object(self),
        ))?)
    }
}

impl ConvexArray {
    pub fn to_internal_json(&self) -> JsonValue {
        value::serialize(
            ConvexValueType::<&ConvexValue>::Array(self),
            serde_json::value::Serializer,
        )
        .expect("Failed to serialize to JsonValue")
    }

    pub fn json_serialize(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string(&value::SerializeValue::new(
            ConvexValueType::<&ConvexValue>::Array(self),
        ))?)
    }
}

/// Deserialize a [`ConvexValue`] from its internal JSON encoding, parsing
/// straight from the JSON text into the value tree.
///
/// This mirrors the semantics of `ConvexValue::try_from(JsonValue)` (including
/// the `$integer`/`$bytes`/`$float` wrappers) but avoids materializing the
/// intermediate `serde_json::Value` tree, halving the number of allocations on
/// per-document decode paths (SQLite/Postgres/MySQL storage backends, sync
/// protocol messages, ...).
pub fn json_deserialize_bytes(s: &[u8]) -> anyhow::Result<ConvexValue> {
    let mut deserializer = serde_json::Deserializer::from_slice(s);
    let value = InternalJsonValueSeed.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

/// Deserialize a [`ConvexValue`] from its internal JSON encoding, parsing
/// straight from the JSON text into the value tree. See
/// [`json_deserialize_bytes`].
pub fn json_deserialize(s: &str) -> anyhow::Result<ConvexValue> {
    let mut deserializer = serde_json::Deserializer::from_str(s);
    let value = InternalJsonValueSeed.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

/// Deserialize a `ConvexValue` from a serde deserializer (typically
/// `serde_json`), handling the internal JSON wrappers.
struct InternalJsonValueSeed;

impl<'de> DeserializeSeed<'de> for InternalJsonValueSeed {
    type Value = ConvexValue;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(InternalJsonValueVisitor)
    }
}

struct InternalJsonValueVisitor;

impl<'de> Visitor<'de> for InternalJsonValueVisitor {
    type Value = ConvexValue;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a ConvexValue in internal JSON format")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(ConvexValue::Null)
    }

    fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(ConvexValue::Boolean(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        // JSON numbers are all mapped to Float64, matching
        // `serde_json::Number::as_f64` on the number path.
        Ok(ConvexValue::Float64(v as f64))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(ConvexValue::Float64(v as f64))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(ConvexValue::Float64(v))
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(ConvexValue::String(
            ConvexString::try_from(v).map_err(E::custom)?,
        ))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(InternalJsonValueSeed)? {
            values.push(value);
        }
        Ok(ConvexValue::Array(
            values.try_into().map_err(A::Error::custom)?,
        ))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        // Collect the raw entries first (last duplicate key wins, matching
        // `serde_json::Map`), then apply the same single-key `$`-wrapper logic
        // as `ConvexValue::try_from(JsonValue)`.
        let mut fields: BTreeMap<String, ConvexValue> = BTreeMap::new();
        while let Some(key) = map.next_key::<String>()? {
            let value = map.next_value_seed(InternalJsonValueSeed)?;
            fields.insert(key, value);
        }
        if fields.len() == 1 {
            let (key, value) = fields.into_iter().next().expect("checked len == 1");
            match key.as_str() {
                "$bytes" => {
                    let s = string_from_value(value, "$bytes").map_err(A::Error::custom)?;
                    Ok(ConvexValue::Bytes(
                        JsonBytes::decode(s).map_err(A::Error::custom)?,
                    ))
                },
                "$integer" => {
                    let s = string_from_value(value, "$integer").map_err(A::Error::custom)?;
                    Ok(ConvexValue::Int64(
                        JsonInteger::decode(s).map_err(A::Error::custom)?,
                    ))
                },
                "$float" => {
                    let s = string_from_value(value, "$float").map_err(A::Error::custom)?;
                    let n = JsonFloat::decode(s).map_err(A::Error::custom)?;
                    // Float64s encoded as a $float object must not fit into a regular
                    // `number`.
                    if !is_negative_zero(n)
                        && let FpCategory::Normal | FpCategory::Subnormal = n.classify()
                    {
                        return Err(A::Error::custom(format!(
                            "Float64 {n} should be encoded as a number"
                        )));
                    }
                    Ok(ConvexValue::Float64(n))
                },
                _ => {
                    let key: FieldName = key.parse().map_err(A::Error::custom)?;
                    Ok(ConvexValue::Object(
                        ConvexObject::for_value(key, value).map_err(A::Error::custom)?,
                    ))
                },
            }
        } else {
            let mut object = BTreeMap::new();
            for (key, value) in fields {
                object.insert(key.parse().map_err(A::Error::custom)?, value);
            }
            Ok(ConvexValue::Object(
                object.try_into().map_err(A::Error::custom)?,
            ))
        }
    }
}

fn string_from_value(value: ConvexValue, wrapper: &str) -> anyhow::Result<String> {
    match value {
        ConvexValue::String(s) => Ok(String::from(s)),
        _ => anyhow::bail!("expected a string for {wrapper} value"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        json_deserialize,
        json_deserialize_bytes,
    };
    use crate::{
        ConvexArray,
        ConvexObject,
        ConvexValue,
    };

    /// Round-trip every value through internal JSON, both via the JSON string
    /// and via the bytes path.
    fn round_trip(value: &ConvexValue) {
        let json = value.json_serialize().unwrap();
        let parsed = json_deserialize(&json).unwrap();
        assert_eq!(&parsed, value, "str round-trip failed for {json}");
        let parsed_bytes = json_deserialize_bytes(json.as_bytes()).unwrap();
        assert_eq!(&parsed_bytes, value, "bytes round-trip failed for {json}");
    }

    #[test]
    fn test_json_round_trip() {
        let values = [
            ConvexValue::Null,
            ConvexValue::Boolean(true),
            ConvexValue::Boolean(false),
            ConvexValue::Int64(0),
            ConvexValue::Int64(-1),
            ConvexValue::Int64(i64::MAX),
            ConvexValue::Int64(i64::MIN),
            ConvexValue::Float64(0.0),
            ConvexValue::Float64(-0.0),
            ConvexValue::Float64(f64::NAN),
            ConvexValue::Float64(f64::INFINITY),
            ConvexValue::Float64(f64::NEG_INFINITY),
            ConvexValue::Float64(3.14159),
            ConvexValue::String("hello".try_into().unwrap()),
            ConvexValue::Bytes(vec![0, 1, 2, 255].try_into().unwrap()),
            ConvexValue::Array(vec![].try_into().unwrap()),
            ConvexValue::Array(
                vec![
                    ConvexValue::Int64(1),
                    ConvexValue::String("two".try_into().unwrap()),
                    ConvexValue::Boolean(true),
                    ConvexValue::Null,
                ]
                .try_into()
                .unwrap(),
            ),
            ConvexValue::Object(ConvexObject::empty()),
            ConvexValue::Object(
                ConvexObject::for_value("key".parse().unwrap(), ConvexValue::Float64(1.5)).unwrap(),
            ),
        ];
        for value in values {
            round_trip(&value);
        }
    }

    #[test]
    fn test_json_deserialize_matches_try_from() {
        // Cases that exercise the single-key `$` wrappers and the plain paths.
        let cases = [
            json!(null),
            json!(true),
            json!(1),
            json!(-1),
            json!(1.5),
            json!("a string"),
            json!([1, 2, 3]),
            json!({"a": 1, "b": [true, null]}),
            json!({"$integer": crate::json::integer::JsonInteger::encode(42)}),
            json!({"$bytes": crate::json::bytes::JsonBytes::encode(&[1, 2, 3]).to_string()}),
            json!({"$float": crate::json::float::JsonFloat::encode(f64::NAN)}),
            json!({"$float": crate::json::float::JsonFloat::encode(-0.0)}),
            json!({"single": "field"}),
        ];
        for case in cases {
            let expected = ConvexValue::try_from(case.clone()).unwrap();
            let actual = json_deserialize(&serde_json::to_string(&case).unwrap()).unwrap();
            assert_eq!(actual, expected, "case: {case}");
        }

        // A `$`-prefixed key alongside other keys is not a valid field name and
        // must be rejected by both the old JsonValue path and the direct path.
        let mixed = json!({"$integer": crate::json::integer::JsonInteger::encode(7), "x": 1});
        let mixed_json = serde_json::to_string(&mixed).unwrap();
        assert!(ConvexValue::try_from(mixed).is_err());
        assert!(json_deserialize(&mixed_json).is_err());
    }

    #[test]
    fn test_json_deserialize_errors() {
        // $float values that fit in a plain number must be rejected.
        let bad_float = json!({"$float": crate::json::float::JsonFloat::encode(1.0)});
        let err = json_deserialize(&serde_json::to_string(&bad_float).unwrap()).unwrap_err();
        assert!(
            err.to_string().contains("should be encoded as a number"),
            "unexpected error: {err}"
        );

        // Non-string payloads for $ wrappers must be rejected.
        assert!(json_deserialize(r#"{"$bytes": 5}"#).is_err());
        assert!(json_deserialize(r#"{"$integer": null}"#).is_err());

        // Reserved `$` field names must be rejected.
        assert!(json_deserialize(r#"{"$foo": 1}"#).is_err());

        // Invalid base64 must be rejected.
        assert!(json_deserialize(r#"{"$integer": "!!!"}"#).is_err());
        assert!(json_deserialize(r#"{"$bytes": "!!!"}"#).is_err());
    }

    #[test]
    fn test_json_deserialize_array_type() {
        // `ConvexArray::try_from(JsonValue)` is a public API that must keep
        // working through the same internal JSON encoding.
        let arr = json!([1, "two", null]);
        let expected = ConvexArray::try_from(arr).unwrap();
        let parsed = json_deserialize("[1, \"two\", null]").unwrap();
        assert_eq!(ConvexArray::try_from(parsed).unwrap(), expected);
    }

    #[test]
    fn test_json_deserialize_trailing_garbage() {
        assert!(json_deserialize("1 2").is_err());
        assert!(json_deserialize_bytes(b"null x").is_err());
        assert!(json_deserialize("").is_err());
    }
}
