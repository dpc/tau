//! Opaque identities and content-free references for runtime observations.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Opaque, non-ordering 128-bit identity allocated at an observation point.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ObservationId(
    /// Random bytes carry equality only; they encode no time or ordering.
    [u8; 16],
);

impl ObservationId {
    /// Constructs an identity from its typed 16-byte representation.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Returns the typed 16-byte representation.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Allocates a random identity from operating-system entropy.
    pub fn random() -> Self {
        use rand::RngCore as _;

        let mut bytes = [0_u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }
}

impl std::fmt::Display for ObservationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for ObservationId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ObservationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.len() != 32 {
            return Err(serde::de::Error::custom(
                "observation id must contain 32 lowercase hexadecimal digits",
            ));
        }
        let mut bytes = [0_u8; 16];
        for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let high = decode_hex(pair[0]).ok_or_else(|| {
                serde::de::Error::custom(
                    "observation id must contain 32 lowercase hexadecimal digits",
                )
            })?;
            let low = decode_hex(pair[1]).ok_or_else(|| {
                serde::de::Error::custom(
                    "observation id must contain 32 lowercase hexadecimal digits",
                )
            })?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

fn decode_hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

/// Exact identity of one provider-declared tool call.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ToolCallRef {
    /// Observation containing the provider response declaration.
    pub declaration: ObservationId,
    /// Zero-based index in the declaration's output items.
    pub item_index: u32,
}

#[cfg(test)]
mod tests;
