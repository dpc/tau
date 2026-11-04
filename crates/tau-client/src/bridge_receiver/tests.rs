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

/// Main targeting supplies its own lazy role selection but cannot make startup
/// or manual restoration create an agent, and cannot downgrade on older
/// harnesses.
#[test]
fn canonical_main_receiver_creation_and_version_gates() {
    let mut config = BridgeReceiverConfig {
        target: BridgeReceiverTarget::CanonicalMain,
        ..Default::default()
    };
    assert!(matches!(
        config.resolution_mode(None, true),
        BridgeReceiverMode::Restore { .. }
    ));
    config.register_on_start = true;
    assert!(matches!(
        config.resolution_mode(None, false),
        BridgeReceiverMode::Select { .. }
    ));
    assert!(matches!(
        config.resolution_mode(None, true),
        BridgeReceiverMode::Ensure { .. }
    ));
    for version in [
        None,
        Some(ProtocolVersion::new(16, 4)),
        Some(ProtocolVersion::new(15, 99)),
    ] {
        assert!(validate_receiver_target_version(config.target, version).is_err());
        assert!(validate_receiver_target_version(BridgeReceiverTarget::Ordinary, version).is_ok());
    }
    assert!(
        validate_receiver_target_version(config.target, Some(ProtocolVersion::new(16, 5))).is_ok()
    );
    assert!(
        validate_receiver_target_version(config.target, Some(ProtocolVersion::new(16, 6))).is_ok()
    );
    let defaults: BridgeReceiverConfig = serde_json::from_str("{}").expect("old config");
    assert_eq!(defaults.target, BridgeReceiverTarget::Ordinary);
}
