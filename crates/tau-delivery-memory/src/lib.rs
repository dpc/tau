//! Content-free estimates for opt-in decoded-delivery memory measurement.

use serde::Serialize;

/// Content-free recursive measurements of one decoded protocol shape.
///
/// `requested_capacity_estimate` measures the capacities requested by a
/// diagnostic CBOR value projection. It is useful for comparing shapes, but is
/// not allocator usable size, resident memory, or the exact layout of the
/// directionally typed protocol value.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DecodedMemoryEstimate {
    /// Exact encoded bytes observed at the owning transport boundary.
    pub encoded_bytes: u64,
    /// Logical bytes in recursively visited text and byte-string leaves.
    pub logical_payload_bytes: u64,
    /// Requested capacity of recursively visited projection containers.
    pub requested_capacity_estimate: u64,
    /// Number of recursively visited allocated containers.
    pub container_count: u64,
}

impl DecodedMemoryEstimate {
    /// Builds an estimate whose encoded-byte count comes from the exact
    /// diagnostic CBOR encoding produced by this call.
    pub fn from_serializable_encoding(value: &impl Serialize) -> Option<Self> {
        let mut encoded = Vec::new();
        ciborium::into_writer(value, &mut encoded).ok()?;
        let encoded_bytes = tau_proto::ProtocolMessageBytes::new(encoded.len() as u64)?;
        Self::from_cbor_bytes(&encoded, encoded_bytes)
    }

    /// Builds a content-free recursive estimate only when the caller's explicit
    /// measurement guard is enabled.
    pub fn from_serializable(
        value: &impl Serialize,
        encoded_bytes: tau_proto::ProtocolMessageBytes,
    ) -> Option<Self> {
        let mut encoded = Vec::new();
        ciborium::into_writer(value, &mut encoded).ok()?;
        Self::from_cbor_bytes(&encoded, encoded_bytes)
    }

    /// Measures one already encoded diagnostic projection.
    fn from_cbor_bytes(
        encoded: &[u8],
        encoded_bytes: tau_proto::ProtocolMessageBytes,
    ) -> Option<Self> {
        let projection: ciborium::Value = ciborium::from_reader(encoded).ok()?;
        let mut estimate = Self {
            encoded_bytes: encoded_bytes.get(),
            ..Self::default()
        };
        estimate.visit(&projection);
        Some(estimate)
    }

    /// Adds another independently owned estimate with saturating diagnostics.
    #[must_use]
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            encoded_bytes: self.encoded_bytes.saturating_add(other.encoded_bytes),
            logical_payload_bytes: self
                .logical_payload_bytes
                .saturating_add(other.logical_payload_bytes),
            requested_capacity_estimate: self
                .requested_capacity_estimate
                .saturating_add(other.requested_capacity_estimate),
            container_count: self.container_count.saturating_add(other.container_count),
        }
    }

    /// Returns the logical-to-encoded expansion ratio in fixed-point
    /// thousandths.
    #[must_use]
    pub fn expansion_milli(self) -> u64 {
        self.logical_payload_bytes
            .saturating_mul(1_000)
            .checked_div(self.encoded_bytes)
            .unwrap_or_default()
    }

    /// Recursively visits the diagnostic projection without retaining content.
    fn visit(&mut self, value: &ciborium::Value) {
        use ciborium::Value;

        match value {
            Value::Bytes(bytes) => {
                self.logical_payload_bytes = self
                    .logical_payload_bytes
                    .saturating_add(bytes.len() as u64);
                self.requested_capacity_estimate = self
                    .requested_capacity_estimate
                    .saturating_add(bytes.capacity() as u64);
                self.container_count = self.container_count.saturating_add(1);
            }
            Value::Text(text) => {
                self.logical_payload_bytes =
                    self.logical_payload_bytes.saturating_add(text.len() as u64);
                self.requested_capacity_estimate = self
                    .requested_capacity_estimate
                    .saturating_add(text.capacity() as u64);
                self.container_count = self.container_count.saturating_add(1);
            }
            Value::Array(values) => {
                self.requested_capacity_estimate = self.requested_capacity_estimate.saturating_add(
                    (values.capacity() * std::mem::size_of::<ciborium::Value>()) as u64,
                );
                self.container_count = self.container_count.saturating_add(1);
                for value in values {
                    self.visit(value);
                }
            }
            Value::Map(entries) => {
                self.requested_capacity_estimate = self.requested_capacity_estimate.saturating_add(
                    (entries.capacity() * std::mem::size_of::<(ciborium::Value, ciborium::Value)>())
                        as u64,
                );
                self.container_count = self.container_count.saturating_add(1);
                for (key, value) in entries {
                    self.visit(key);
                    self.visit(value);
                }
            }
            Value::Tag(_, value) => self.visit(value),
            Value::Integer(_) | Value::Float(_) | Value::Bool(_) | Value::Null => {}
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;
