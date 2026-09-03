//! Content-free estimates for opt-in decoded-delivery memory measurement.

use serde::Serialize;

/// Exact encoded bytes in one decoded-delivery diagnostic estimate.
///
/// ```compile_fail
/// let mut estimate =
///     tau_delivery_memory::DecodedMemoryEstimate::from_serializable_encoding(&"message")
///         .expect("serializable diagnostic value");
/// estimate.encoded_bytes = estimate.logical_payload_bytes;
/// ```
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct EncodedBytes {
    /// Raw scalar retained only inside the semantic wrapper.
    bytes: u64,
}

impl EncodedBytes {
    /// Creates an encoded-byte estimate within this crate's measurement owner.
    #[must_use]
    pub(crate) const fn new(bytes: u64) -> Self {
        Self { bytes }
    }

    /// Returns the scalar for diagnostic trace emission.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.bytes
    }

    /// Adds two encoded-byte estimates without overflow.
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self::new(self.bytes.saturating_add(other.bytes))
    }

    /// Returns the larger encoded-byte estimate.
    #[must_use]
    pub const fn max(self, other: Self) -> Self {
        if self.bytes >= other.bytes {
            self
        } else {
            other
        }
    }
}

/// Logical text and byte-string payload bytes in one decoded-delivery estimate.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct LogicalPayloadBytes {
    /// Raw scalar retained only inside the semantic wrapper.
    bytes: u64,
}

impl LogicalPayloadBytes {
    /// Creates a logical-payload-byte estimate within this crate's measurement
    /// owner.
    #[must_use]
    pub(crate) const fn new(bytes: u64) -> Self {
        Self { bytes }
    }

    /// Returns the scalar for diagnostic trace emission.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.bytes
    }

    /// Adds two logical-payload-byte estimates without overflow.
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self::new(self.bytes.saturating_add(other.bytes))
    }

    /// Returns the larger logical-payload-byte estimate.
    #[must_use]
    pub const fn max(self, other: Self) -> Self {
        if self.bytes >= other.bytes {
            self
        } else {
            other
        }
    }
}

/// Requested projection-container capacity bytes in one decoded-delivery
/// estimate.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct RequestedCapacityEstimateBytes {
    /// Raw scalar retained only inside the semantic wrapper.
    bytes: u64,
}

impl RequestedCapacityEstimateBytes {
    /// Creates a requested-capacity estimate within this crate's measurement
    /// owner.
    #[must_use]
    pub(crate) const fn new(bytes: u64) -> Self {
        Self { bytes }
    }

    /// Returns the scalar for diagnostic trace emission.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.bytes
    }

    /// Adds two requested-capacity estimates without overflow.
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self::new(self.bytes.saturating_add(other.bytes))
    }

    /// Returns the larger requested-capacity estimate.
    #[must_use]
    pub const fn max(self, other: Self) -> Self {
        if self.bytes >= other.bytes {
            self
        } else {
            other
        }
    }
}

/// Content-free recursive measurements of one decoded protocol shape.
///
/// `requested_capacity_estimate` measures the capacities requested by a
/// diagnostic CBOR value projection. It is useful for comparing shapes, but is
/// not allocator usable size, resident memory, or the exact layout of the
/// directionally typed protocol value.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DecodedMemoryEstimate {
    /// Exact encoded bytes observed at the owning transport boundary.
    pub encoded_bytes: EncodedBytes,
    /// Logical bytes in recursively visited text and byte-string leaves.
    pub logical_payload_bytes: LogicalPayloadBytes,
    /// Requested capacity of recursively visited projection containers.
    pub requested_capacity_estimate: RequestedCapacityEstimateBytes,
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
            encoded_bytes: EncodedBytes::new(encoded_bytes.get()),
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
            .bytes
            .saturating_mul(1_000)
            .checked_div(self.encoded_bytes.bytes)
            .unwrap_or_default()
    }

    /// Recursively visits the diagnostic projection without retaining content.
    fn visit(&mut self, value: &ciborium::Value) {
        use ciborium::Value;

        match value {
            Value::Bytes(bytes) => {
                self.logical_payload_bytes = self
                    .logical_payload_bytes
                    .saturating_add(LogicalPayloadBytes::new(bytes.len() as u64));
                self.requested_capacity_estimate = self
                    .requested_capacity_estimate
                    .saturating_add(RequestedCapacityEstimateBytes::new(bytes.capacity() as u64));
                self.container_count = self.container_count.saturating_add(1);
            }
            Value::Text(text) => {
                self.logical_payload_bytes = self
                    .logical_payload_bytes
                    .saturating_add(LogicalPayloadBytes::new(text.len() as u64));
                self.requested_capacity_estimate = self
                    .requested_capacity_estimate
                    .saturating_add(RequestedCapacityEstimateBytes::new(text.capacity() as u64));
                self.container_count = self.container_count.saturating_add(1);
            }
            Value::Array(values) => {
                self.requested_capacity_estimate = self.requested_capacity_estimate.saturating_add(
                    RequestedCapacityEstimateBytes::new(
                        (values.capacity() * std::mem::size_of::<ciborium::Value>()) as u64,
                    ),
                );
                self.container_count = self.container_count.saturating_add(1);
                for value in values {
                    self.visit(value);
                }
            }
            Value::Map(entries) => {
                self.requested_capacity_estimate = self.requested_capacity_estimate.saturating_add(
                    RequestedCapacityEstimateBytes::new(
                        (entries.capacity()
                            * std::mem::size_of::<(ciborium::Value, ciborium::Value)>())
                            as u64,
                    ),
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
