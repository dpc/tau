use super::*;

/// Maximum decimal allocation remains inside the canonical connection-ID
/// grammar without truncation or alternate formatting.
#[test]
fn maximum_connection_counter_formats_as_valid_identifier() {
    let mut bus = EventBus {
        next_connection_id: u64::MAX - 1,
        connections: HashMap::new(),
    };

    let id = bus.allocate_connection_id();

    assert_eq!(id.as_str(), format!("conn-{}", u64::MAX));
    assert!(id.as_str().len() <= 128);
}
