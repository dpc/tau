//! Closed private-capture attribution, never routing or prompt authority.

use serde::de::Error as _;
use serde::{Deserialize, Serialize};

/// Capture-local random 128-bit operation identity, encoded as lowercase hex.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct CacheOperationId([u8; 16]);

impl CacheOperationId {
    /// Wrap random bytes supplied by the producer; this does not create
    /// authority.
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Validate the exact canonical filename/wire representation.
    pub fn parse(text: &str) -> Option<Self> {
        if text.len() != 32
            || !text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        let mut bytes = [0; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
        }
        Some(Self(bytes))
    }

    /// Return the private canonical basename token, not a public identifier.
    pub fn to_hex(self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

impl std::fmt::Debug for CacheOperationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CacheOperationId(<private>)")
    }
}

impl Serialize for CacheOperationId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for CacheOperationId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| D::Error::custom("invalid cache operation identity"))
    }
}

/// Attribution copied from a prompt, or explicitly unrelated to any prompt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCaptureAttribution {
    /// Existing prompt attribution for ordinary exact and scalar captures.
    Prompt(crate::AgentPromptId),
    /// Private operation identity, permitted only for cache-diagnostic
    /// captures.
    CacheOperation(CacheOperationId),
}

impl ProviderCaptureAttribution {
    /// Prevent operation attribution from authorizing another raw capture
    /// class.
    pub fn permits(&self, class: crate::ProviderDebugCaptureClass) -> bool {
        matches!(self, Self::Prompt(_))
            || class == crate::ProviderDebugCaptureClass::CacheDiagnostic
    }
}

#[cfg(test)]
mod tests;
