use serde::Serialize;

use super::{
    DecodedMemoryEstimate, EncodedBytes, LogicalPayloadBytes, RequestedCapacityEstimateBytes,
};

/// Nested measurements must keep encoded, logical, and requested-capacity
/// quantities distinct so policy work cannot mistake one for another.
#[test]
fn nested_shapes_separate_logical_bytes_from_requested_capacity() {
    #[derive(Serialize)]
    /// Nested valid protocol-like workload shape.
    struct Shape {
        /// Nested text sequences.
        rows: Vec<Vec<String>>,
        #[serde(with = "serde_bytes")]
        /// Byte-string leaf.
        bytes: Vec<u8>,
    }

    let estimate = DecodedMemoryEstimate::from_serializable(
        &Shape {
            rows: vec![vec!["expanded text".to_owned(), "x".repeat(2_048)]],
            bytes: vec![7; 512],
        },
        tau_proto::ProtocolMessageBytes::new(17).expect("nonzero encoded size"),
    )
    .expect("serializable diagnostic shape");

    assert_eq!(estimate.encoded_bytes.get(), 17);
    assert!(estimate.logical_payload_bytes.get() >= 2_048 + 512);
    assert!(estimate.requested_capacity_estimate.get() >= estimate.logical_payload_bytes.get());
    assert!(estimate.container_count >= 6);
    assert!(estimate.expansion_milli() > 1_000);
}

/// Zero-capable dimensions must preserve exact saturation, aggregation, and
/// expansion arithmetic while rejecting cross-dimension substitution at compile
/// time through the public `EncodedBytes` documentation example.
#[test]
fn opaque_dimensions_preserve_exact_scalar_behavior() {
    let estimate = DecodedMemoryEstimate {
        encoded_bytes: EncodedBytes::new(10),
        logical_payload_bytes: LogicalPayloadBytes::new(20),
        requested_capacity_estimate: RequestedCapacityEstimateBytes::new(30),
        container_count: 1,
    };
    let saturated = estimate.saturating_add(DecodedMemoryEstimate {
        encoded_bytes: EncodedBytes::new(u64::MAX),
        logical_payload_bytes: LogicalPayloadBytes::new(u64::MAX),
        requested_capacity_estimate: RequestedCapacityEstimateBytes::new(u64::MAX),
        container_count: u64::MAX,
    });

    assert_eq!(DecodedMemoryEstimate::default().encoded_bytes.get(), 0);
    assert_eq!(
        DecodedMemoryEstimate::default().logical_payload_bytes.get(),
        0
    );
    assert_eq!(
        DecodedMemoryEstimate::default()
            .requested_capacity_estimate
            .get(),
        0
    );
    assert_eq!(estimate.expansion_milli(), 2_000);
    assert_eq!(saturated.encoded_bytes.get(), u64::MAX);
    assert_eq!(saturated.logical_payload_bytes.get(), u64::MAX);
    assert_eq!(saturated.requested_capacity_estimate.get(), u64::MAX);
    assert_eq!(saturated.container_count, u64::MAX);
}
