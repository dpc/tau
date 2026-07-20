//! Bounded publisher-defined data carried by message reports and canonical
//! facts.

#[cfg(test)]
#[path = "message_extension_data/tests.rs"]
mod tests;

use std::fmt;

use serde::de::{self, DeserializeSeed, EnumAccess, MapAccess, SeqAccess, VariantAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::CborValue;

/// Maximum standalone encoded bytes retained in one opaque message value.
pub const MESSAGE_EXTENSION_DATA_MAX_BYTES: usize = 65_536;
/// Maximum root-inclusive container or tag depth in one opaque message value.
pub const MESSAGE_EXTENSION_DATA_MAX_DEPTH: usize = 16;
/// Maximum aggregate scalar, container, map-key, map-value, and tag nodes.
pub const MESSAGE_EXTENSION_DATA_MAX_NODES: usize = 4_096;

/// Bounded publisher-defined data attached opaquely to a message report or
/// canonical fact.
///
/// The data is not confidential: Tau persists it and exposes it to authorized
/// event subscribers. Tau does not interpret it for routing or transcript
/// semantics.
#[derive(Clone, Debug, PartialEq)]
pub struct MessageExtensionData(
    /// Validated publisher-defined CBOR value.
    CborValue,
);

impl MessageExtensionData {
    /// Construct opaque data from an already materialized CBOR value.
    ///
    /// Validation counts the root and every scalar, container, map key, map
    /// value, and tag as a node. Container children and tagged values add one
    /// level of root-inclusive depth. The encoded-byte bound applies to this
    /// standalone value, not the enclosing protocol frame.
    ///
    /// # Errors
    ///
    /// Returns [`MessageExtensionDataError`] when the value exceeds the encoded
    /// byte, structural depth, or aggregate node bound.
    pub fn new(value: CborValue) -> Result<Self, MessageExtensionDataError> {
        validate_materialized_extension_data(&value)?;
        Ok(Self(value))
    }

    /// Borrow the opaque CBOR value.
    #[must_use]
    pub fn value(&self) -> &CborValue {
        &self.0
    }

    /// Consume the wrapper and return its opaque CBOR value.
    #[must_use]
    pub fn into_value(self) -> CborValue {
        self.0
    }
}

impl Default for MessageExtensionData {
    fn default() -> Self {
        Self(CborValue::Null)
    }
}

impl Serialize for MessageExtensionData {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for MessageExtensionData {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut budget = ExtensionDataBudget::default();
        let value = BoundedValueSeed {
            budget: &mut budget,
            depth: 1,
        }
        .deserialize(deserializer)?;
        validate_encoded_extension_data(&value).map_err(de::Error::custom)?;
        Ok(Self(value))
    }
}

/// Validation failure for publisher-defined message data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageExtensionDataError {
    /// The encoded standalone value exceeds its byte budget.
    EncodedBytes,
    /// A container child or tagged value exceeds the maximum nesting depth.
    Depth,
    /// The aggregate value-node budget is exhausted.
    Nodes,
}

impl fmt::Display for MessageExtensionDataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EncodedBytes => "message extension data exceeds encoded byte limit",
            Self::Depth => "message extension data exceeds nesting depth limit",
            Self::Nodes => "message extension data exceeds node limit",
        })
    }
}

impl std::error::Error for MessageExtensionDataError {}

/// Mutable aggregate-node accounting shared by recursive decode seeds.
#[derive(Default)]
struct ExtensionDataBudget {
    /// Number of value nodes decoded so far.
    nodes: usize,
}

/// Recursive value decoder carrying the structural budget and current depth.
struct BoundedValueSeed<'a> {
    /// Shared aggregate-node budget.
    budget: &'a mut ExtensionDataBudget,
    /// Root-inclusive depth of the value being decoded.
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for BoundedValueSeed<'_> {
    type Value = CborValue;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        if self.depth > MESSAGE_EXTENSION_DATA_MAX_DEPTH {
            return Err(de::Error::custom(MessageExtensionDataError::Depth));
        }
        self.budget.nodes = self.budget.nodes.saturating_add(1);
        if self.budget.nodes > MESSAGE_EXTENSION_DATA_MAX_NODES {
            return Err(de::Error::custom(MessageExtensionDataError::Nodes));
        }
        deserializer.deserialize_any(BoundedValueVisitor {
            budget: self.budget,
            depth: self.depth,
        })
    }
}

/// Serde adapter that preserves CBOR values while recursively enforcing bounds.
struct BoundedValueVisitor<'a> {
    /// Shared aggregate-node budget.
    budget: &'a mut ExtensionDataBudget,
    /// Root-inclusive depth of the value currently being visited.
    depth: usize,
}

/// Implement scalar visitor methods that preserve each value without recursion.
macro_rules! visit_scalar {
    ($($method:ident($value:ty)),+ $(,)?) => {
        $(
            fn $method<E: de::Error>(self, value: $value) -> Result<Self::Value, E> {
                Ok(value.into())
            }
        )+
    };
}

impl<'de> Visitor<'de> for BoundedValueVisitor<'_> {
    type Value = CborValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded CBOR message extension data")
    }

    visit_scalar! {
        visit_bool(bool),
        visit_f32(f32),
        visit_f64(f64),
        visit_i8(i8),
        visit_i16(i16),
        visit_i32(i32),
        visit_i64(i64),
        visit_i128(i128),
        visit_u8(u8),
        visit_u16(u16),
        visit_u32(u32),
        visit_u64(u64),
        visit_u128(u128),
        visit_char(char),
        visit_str(&str),
        visit_borrowed_str(&'de str),
        visit_string(String),
        visit_bytes(&[u8]),
        visit_borrowed_bytes(&'de [u8]),
        visit_byte_buf(Vec<u8>),
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(CborValue::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(CborValue::Null)
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(BoundedValueSeed {
            budget: &mut *self.budget,
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(CborValue::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut values = Vec::new();
        while let Some(key) = map.next_key_seed(BoundedValueSeed {
            budget: &mut *self.budget,
            depth: self.depth + 1,
        })? {
            let value = map.next_value_seed(BoundedValueSeed {
                budget: &mut *self.budget,
                depth: self.depth + 1,
            })?;
            values.push((key, value));
        }
        Ok(CborValue::Map(values))
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Self::Value, A::Error> {
        /// Serde adapter for ciborium's private tagged-value tuple convention.
        struct TaggedValueVisitor<'a> {
            /// Shared aggregate-node budget.
            budget: &'a mut ExtensionDataBudget,
            /// Depth of the tag node whose value is being decoded.
            depth: usize,
        }

        impl<'de> Visitor<'de> for TaggedValueVisitor<'_> {
            type Value = CborValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a CBOR tag and bounded tagged value")
            }

            fn visit_seq<S: SeqAccess<'de>>(
                self,
                mut sequence: S,
            ) -> Result<Self::Value, S::Error> {
                let tag = sequence
                    .next_element::<u64>()?
                    .ok_or_else(|| de::Error::custom("missing CBOR tag"))?;
                let value = sequence
                    .next_element_seed(BoundedValueSeed {
                        budget: &mut *self.budget,
                        depth: self.depth + 1,
                    })?
                    .ok_or_else(|| de::Error::custom("missing CBOR tagged value"))?;
                Ok(CborValue::Tag(tag, Box::new(value)))
            }
        }

        let (name, variant) = data.variant::<String>()?;
        if name != "@@TAGGED@@" {
            return Err(de::Error::custom("unsupported enum in CBOR value"));
        }
        variant.tuple_variant(
            2,
            TaggedValueVisitor {
                budget: &mut *self.budget,
                depth: self.depth,
            },
        )
    }
}

/// Validate a materialized value against structural and encoded-byte bounds.
fn validate_materialized_extension_data(
    value: &CborValue,
) -> Result<(), MessageExtensionDataError> {
    let mut nodes = 0;
    validate_value_structure(value, 1, &mut nodes)?;
    validate_encoded_extension_data(value)
}

/// Recursively validate depth and node count for a materialized value.
fn validate_value_structure(
    value: &CborValue,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), MessageExtensionDataError> {
    if depth > MESSAGE_EXTENSION_DATA_MAX_DEPTH {
        return Err(MessageExtensionDataError::Depth);
    }
    *nodes = nodes.saturating_add(1);
    if *nodes > MESSAGE_EXTENSION_DATA_MAX_NODES {
        return Err(MessageExtensionDataError::Nodes);
    }
    match value {
        CborValue::Array(values) => {
            for value in values {
                validate_value_structure(value, depth + 1, nodes)?;
            }
        }
        CborValue::Map(values) => {
            for (key, value) in values {
                validate_value_structure(key, depth + 1, nodes)?;
                validate_value_structure(value, depth + 1, nodes)?;
            }
        }
        CborValue::Tag(_, value) => validate_value_structure(value, depth + 1, nodes)?,
        _ => {}
    }
    Ok(())
}

/// Validate the standalone encoded size of a materialized value.
fn validate_encoded_extension_data(value: &CborValue) -> Result<(), MessageExtensionDataError> {
    let mut encoded = Vec::new();
    ciborium::into_writer(value, &mut encoded)
        .map_err(|_| MessageExtensionDataError::EncodedBytes)?;
    if encoded.len() > MESSAGE_EXTENSION_DATA_MAX_BYTES {
        return Err(MessageExtensionDataError::EncodedBytes);
    }
    Ok(())
}
