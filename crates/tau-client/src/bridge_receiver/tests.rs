use super::*;

/// Manual defaults may restore state but cannot activate selection/creation.
#[test]
fn opt_in_and_admitted_input_are_independent_creation_gates() {
    let id = AgentId::parse("receiver").expect("id");
    let mut config = BridgeReceiverConfig::default();
    assert_eq!(
        config.resolution_mode(Some(id.clone()), true),
        BridgeReceiverMode::Restore {
            agent_id: Some(id.clone())
        }
    );
    config.register_on_start = true;
    assert!(matches!(
        config.resolution_mode(None, true),
        BridgeReceiverMode::Select { .. }
    ));
    config.role = Some("coordinator".to_owned());
    assert!(matches!(
        config.resolution_mode(None, false),
        BridgeReceiverMode::Select { .. }
    ));
    assert!(matches!(
        config.resolution_mode(None, true),
        BridgeReceiverMode::Ensure { .. }
    ));
    assert!(BridgeReceiverSnapshot::new(id).is_supported());
}
