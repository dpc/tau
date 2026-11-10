use std::io::Cursor;
use std::num::NonZeroU64;

use super::*;

/// Both entry points enforce version support before enqueueing. Supported
/// callers share correlation and emit distinct policies through one wire API.
#[test]
fn acceptance_modes_are_gated_before_transport_and_preserved_on_wire() {
    let (sender, receiver) = crate::writer_thread::writer_channel();
    let handle = ClientHandle::new(sender);
    let writer = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        crate::writer_thread::run_writer(&mut bytes, receiver).expect("writer");
        bytes
    });
    handle.send_ready(None).expect("ready");
    let client = MessageAcceptanceClient::new(handle.clone());
    let session: SessionId = "session".parse().expect("session");
    let report = MessageDelivered::new(
        RawMessagePublisherId::new("bridge"),
        tau_proto::MessageAgentTarget::new("agent"),
        tau_proto::MessageFactId::new("message"),
        tau_proto::MessageParty {
            stable_id: "sender".to_owned(),
            display_name: None,
            sender_auth: None,
            sender_trust: None,
        },
        None,
        "body",
    );
    for version in [
        None,
        Some(ProtocolVersion::new(23, 1)),
        Some(ProtocolVersion::new(24, 0)),
    ] {
        assert!(
            client
                .start_request(session.clone(), report.clone(), version)
                .is_err()
        );
        assert!(
            client
                .start_existing_request(session.clone(), report.clone(), version)
                .is_err()
        );
    }
    let first = client
        .start_request(
            session.clone(),
            report.clone(),
            Some(ProtocolVersion::new(23, 2)),
        )
        .expect("admit new");
    let mut noticed = report;
    noticed.incoming_notice = Some(
        tau_proto::MessageIncomingNotice::new("endpoint".to_owned(), "local guidance".to_owned())
            .expect("notice"),
    );
    assert!(
        client
            .start_request(
                session.clone(),
                noticed.clone(),
                Some(ProtocolVersion::new(23, 2))
            )
            .is_err()
    );
    assert!(
        client
            .start_existing_request(
                session.clone(),
                noticed.clone(),
                Some(ProtocolVersion::new(23, 2))
            )
            .is_err()
    );
    let second = client
        .start_existing_request(
            session.clone(),
            noticed.clone(),
            Some(ProtocolVersion::new(23, 3)),
        )
        .expect("existing only");
    noticed.connection_id = NonZeroU64::new(7);
    for minor in [2, 3] {
        let version = Some(ProtocolVersion::new(23, minor));
        assert!(
            client
                .start_request(session.clone(), noticed.clone(), version)
                .is_err()
        );
        assert!(
            client
                .start_existing_request(session.clone(), noticed.clone(), version)
                .is_err()
        );
    }
    let third = client
        .start_request(session, noticed, Some(tau_proto::PROTOCOL_VERSION))
        .expect("connection display");
    assert_ne!(first, second);
    handle.shutdown().expect("shutdown");
    let mut reader =
        tau_proto::HarnessInputReader::new(Cursor::new(writer.join().expect("writer")));
    assert!(matches!(
        reader.read_message().expect("ready"),
        Some(HarnessInputMessage::Ready(_))
    ));
    for (request_id, mode) in [
        (first, MessageAcceptanceMode::AcceptNew),
        (second, MessageAcceptanceMode::ExistingOnly),
        (third, MessageAcceptanceMode::AcceptNew),
    ] {
        let Some(HarnessInputMessage::MessageAcceptanceRequest(request)) =
            reader.read_message().expect("request")
        else {
            panic!("directed acceptance");
        };
        assert_eq!(request.request_id, request_id);
        assert_eq!(request.mode, mode);
        assert_eq!(
            request.report.incoming_notice.is_some(),
            request.report.connection_id.is_some() || mode == MessageAcceptanceMode::ExistingOnly
        );
    }
    assert!(reader.read_message().expect("end").is_none());
}

/// Unknown and older peers must never receive an unsupported request shape.
#[test]
fn unsupported_versions_fail_closed() {
    for version in [
        None,
        Some(ProtocolVersion::new(23, 1)),
        Some(ProtocolVersion::new(22, 99)),
        Some(ProtocolVersion::new(24, 0)),
    ] {
        assert!(validate_version(version).is_err());
    }
    assert!(validate_version(Some(ProtocolVersion::new(23, 2))).is_ok());
    assert!(validate_version(Some(ProtocolVersion::new(23, 3))).is_ok());
}
