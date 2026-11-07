use std::io::Cursor;

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
        Some(ProtocolVersion::new(16, 5)),
        Some(ProtocolVersion::new(16, 4)),
        Some(ProtocolVersion::new(15, 99)),
        Some(ProtocolVersion::new(17, 0)),
        Some(ProtocolVersion::new(18, 0)),
        Some(ProtocolVersion::new(PROTOCOL_VERSION.major - 1, u32::MAX)),
        Some(ProtocolVersion::new(PROTOCOL_VERSION.major + 1, 0)),
    ] {
        assert!(validate_receiver_target_version(config.target, version).is_err());
        assert!(validate_receiver_target_version(BridgeReceiverTarget::Ordinary, version).is_ok());
    }
    for version in [
        PROTOCOL_VERSION,
        ProtocolVersion::new(PROTOCOL_VERSION.major, u32::MAX),
    ] {
        assert!(validate_receiver_target_version(config.target, Some(version)).is_ok());
    }
    let defaults: BridgeReceiverConfig = serde_json::from_str("{}").expect("old config");
    assert_eq!(defaults.target, BridgeReceiverTarget::Ordinary);
}

/// Supported canonical-main requests reach the writer unchanged; missing/skewed
/// versions produce no frame and ordinary requests still need no version.
#[test]
fn canonical_main_transport_rejects_skew_before_enqueueing() {
    let (sender, receiver) = crate::writer_thread::writer_channel();
    let handle = ClientHandle::new(sender);
    let writer = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        crate::writer_thread::run_writer(&mut bytes, receiver).expect("writer");
        bytes
    });
    handle.send_ready(None).expect("ready");
    let client = BridgeReceiverClient::new(handle.clone());
    let session = SessionId::parse("session").expect("session");
    let mode = BridgeReceiverMode::Select {
        preferred_agent_id: None,
    };
    for version in [
        None,
        Some(ProtocolVersion::new(PROTOCOL_VERSION.major - 1, 0)),
        Some(ProtocolVersion::new(PROTOCOL_VERSION.major + 1, 0)),
    ] {
        assert!(
            client
                .start_request_with_target(
                    session.clone(),
                    None,
                    mode.clone(),
                    BridgeReceiverTarget::CanonicalMain,
                    version,
                )
                .is_err()
        );
    }
    let request_id = client
        .start_request_with_target(
            session.clone(),
            None,
            mode.clone(),
            BridgeReceiverTarget::CanonicalMain,
            Some(PROTOCOL_VERSION),
        )
        .expect("current protocol");
    let ordinary_id = client
        .start_request(session.clone(), None, mode.clone())
        .expect("ordinary request without version");
    handle.shutdown().expect("shutdown");
    let bytes = writer.join().expect("join writer");
    let mut reader = tau_proto::HarnessInputReader::new(Cursor::new(bytes));
    assert!(matches!(
        reader.read_message().expect("ready frame"),
        Some(HarnessInputMessage::Ready(_))
    ));
    for (request_id, target) in [
        (request_id, BridgeReceiverTarget::CanonicalMain),
        (ordinary_id, BridgeReceiverTarget::Ordinary),
    ] {
        assert_eq!(
            reader.read_message().expect("request frame"),
            Some(HarnessInputMessage::BridgeReceiverRequest(
                BridgeReceiverRequest {
                    request_id,
                    session_id: session.clone(),
                    role: None,
                    target,
                    mode: mode.clone(),
                }
            ))
        );
    }
    assert!(reader.read_message().expect("end of frames").is_none());
}
