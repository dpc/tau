use serde::Serialize;

use super::DecodedMemoryEstimate;

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

    assert_eq!(estimate.encoded_bytes, 17);
    assert!(estimate.logical_payload_bytes >= 2_048 + 512);
    assert!(estimate.requested_capacity_estimate >= estimate.logical_payload_bytes);
    assert!(estimate.container_count >= 6);
    assert!(estimate.expansion_milli() > 1_000);
}
