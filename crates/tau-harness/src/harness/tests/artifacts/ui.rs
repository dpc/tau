use std::io::BufReader;
use std::os::unix::net::UnixStream;

use super::*;

/// Exercises the production initial-stdio adapter and ordinary socket adapter
/// with the same Hello and directed RPC, without adding any UI participant.
#[test]
fn ui_artifact_upload_uses_existing_initial_and_attached_ingress() {
    for initial in [true, false] {
        let temp = TempDir::new().expect("private root");
        let mut h = quiet_provider_harness(temp.path()).expect("harness");
        let (connection, mut reader) = admitted_ui(&mut h, initial);
        let clients = h.ui_runtime.client_writers.len();
        assert_eq!(
            ui_rpc(
                &mut h,
                &connection,
                &mut reader,
                ArtifactOp::Available,
                true
            ),
            Ok(ArtifactValue::Done)
        );
        let bytes = b"normalized\nwall of text".to_vec();
        let mut upload = tau_client::ArtifactUpload::new(bytes).expect("bounded original");
        while let Some(op) = upload.next_op() {
            upload
                .accept(ui_rpc(&mut h, &connection, &mut reader, op, true).expect("upload RPC"))
                .expect("valid progress");
        }
        let key = upload.descriptor().expect("final descriptor").key.clone();
        assert_eq!(
            ui_rpc(
                &mut h,
                &connection,
                &mut reader,
                ArtifactOp::Stat { key },
                false
            ),
            Err(ArtifactError::Permission)
        );
        assert_eq!(h.ui_runtime.client_writers.len(), clients);
        h.handle_client_message(
            &connection,
            HarnessInputMessage::GetCurrentSession(tau_proto::GetCurrentSession {
                request_id: "after-upload".to_owned(),
            }),
        )
        .expect("ordinary UI request");
        assert!(matches!(
            reader.read_message().expect("ordinary UI reply"),
            Some(HarnessOutputMessage::CurrentSessionResult(_))
        ));
        h.handle_disconnect(&connection);
        assert!(!h.ui_runtime.artifact_admissions.contains_key(&connection));
    }
}

/// Memory-only, stale session generations, wrong session, and oversize chunks
/// fail before artifact worker/root access even on an authenticated UI.
#[test]
fn ui_artifact_admission_rejects_memory_stale_and_oversize_requests() {
    let temp = TempDir::new().expect("private root");
    let mut h = echo_harness_memory_only(temp.path()).expect("memory-only harness");
    let (connection, mut reader) = admitted_ui(&mut h, true);
    assert_eq!(
        ui_rpc(
            &mut h,
            &connection,
            &mut reader,
            ArtifactOp::Available,
            false
        ),
        Err(ArtifactError::Permission)
    );
    assert!(h.runtime_io.artifacts.is_none());
    assert!(!temp.path().join("artifacts").exists());
    h.handle_disconnect(&connection);

    let mut h = quiet_provider_harness(temp.path()).expect("persistent harness");
    let (connection, mut reader) = admitted_ui(&mut h, false);
    assert_eq!(
        ui_rpc(
            &mut h,
            &connection,
            &mut reader,
            ArtifactOp::Write {
                upload: "unallocated".parse().expect("upload id"),
                offset: 0,
                bytes: vec![0; tau_proto::ARTIFACT_CHUNK_BYTES + 1],
            },
            false
        ),
        Err(ArtifactError::Invalid)
    );
    h.session_runtime.current_session_generation = h
        .session_runtime
        .current_session_generation
        .saturating_next();
    assert_eq!(
        ui_rpc(
            &mut h,
            &connection,
            &mut reader,
            ArtifactOp::Available,
            false
        ),
        Err(ArtifactError::SessionMismatch)
    );
    assert!(h.runtime_io.artifacts.is_none());
    assert!(!temp.path().join("artifacts").exists());
}

/// Builds the two real ingress adapters, then consumes exact-session admission.
fn admitted_ui(
    h: &mut Harness,
    initial: bool,
) -> (
    tau_proto::ConnectionId,
    tau_proto::HarnessOutputReader<BufReader<UnixStream>>,
) {
    let (server, client) = UnixStream::pair().expect("socket pair");
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let connection = if initial {
        h.accept_initial_ui_io(server.try_clone().expect("reader clone"), server)
            .expect("initial UI admission")
    } else {
        h.accept_client(server).expect("attached UI admission")
    };
    let session = h.session_runtime.current_session_id.clone();
    h.handle_client_message(
        &connection,
        HarnessInputMessage::Hello(tau_proto::Hello {
            declaration_inspection: false,
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::test_extension_name("paste-test"),
            client_kind: tau_proto::ClientKind::Ui,
            expected_session_id: Some(session),
            capabilities: Vec::new(),
        }),
    )
    .expect("Hello admission");
    h.ui_runtime.pending_socket_admission.remove(&connection);
    let mut reader = tau_proto::HarnessOutputReader::new(BufReader::new(client));
    assert!(matches!(
        reader.read_message().expect("Hello reply"),
        Some(HarnessOutputMessage::SessionAccepted(_))
    ));
    assert!(h.is_attached_socket_ui(&connection));
    (connection, reader)
}

/// Runs one request through the UI dispatcher, optionally driving bounded I/O.
fn ui_rpc(
    h: &mut Harness,
    connection: &tau_proto::ConnectionId,
    reader: &mut tau_proto::HarnessOutputReader<BufReader<UnixStream>>,
    op: ArtifactOp,
    worker: bool,
) -> Result<ArtifactValue, ArtifactError> {
    let request = request(h, op);
    h.handle_client_message(connection, HarnessInputMessage::ArtifactRequest(request))
        .expect("artifact request");
    if worker {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline);
            if let Ok(HarnessEvent::Command(command)) =
                h.runtime_io.rx.recv_timeout(Duration::from_millis(50))
            {
                let completed =
                    matches!(command, crate::event::HarnessCommand::ArtifactCompleted(_));
                h.handle_harness_command(command)
                    .expect("worker completion");
                if completed {
                    break;
                }
            }
        }
    }
    let Some(HarnessOutputMessage::ArtifactResult(result)) =
        reader.read_message().expect("artifact reply")
    else {
        panic!("directed artifact result")
    };
    assert_eq!(result.request_id.as_str(), "artifact-test");
    result.result
}
