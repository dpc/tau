use std::sync::mpsc;

use tau_proto::{ArtifactError, ArtifactOp, ArtifactRequest, ArtifactValue};

use super::*;
use crate::event::ChannelSink;
use crate::event_log::EventLog;
use crate::harness::{ExtensionActivationStage, ExtensionFrameAdmission};

/// A stalled configured recipient is disconnected on artifact egress overflow;
/// other connections remain live and late completions cannot revive the peer.
#[test]
fn artifact_egress_overflow_disconnects_only_the_stalled_recipient() {
    let temp = TempDir::new().expect("private root");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let connection = crate::test_connection_id("stalled-artifacts");
    let log = EventLog::new();
    let (writer_tx, _writer_rx) = mpsc::channel();
    let sink = ChannelSink::new(
        &writer_tx,
        Arc::clone(&log),
        h.runtime_io.tx.clone(),
        connection.clone(),
    )
    .expect("stalled writer follower");
    h.runtime_io.bus.connect(Connection::new(
        PendingConnectionMetadata {
            id: Some(connection.clone()),
            name: crate::test_extension_name("stalled-artifacts"),
            kind: tau_proto::ClientKind::Tool,
            origin: ConnectionOrigin::InMemory,
        },
        Box::new(sink),
    ));
    mark_connected_test_extension_configured(
        &mut h,
        "stalled-artifacts",
        "artifact-tool",
        tau_proto::ClientKind::Tool,
    );
    let _healthy = connect_test_client(&mut h, "healthy", tau_proto::ClientKind::Ui);
    for _ in 0..8 {
        h.send_artifact_result(
            &connection,
            tau_proto::ArtifactResult {
                request_id: "retry".parse().expect("request"),
                result: Ok(ArtifactValue::Chunk {
                    offset: 0,
                    bytes: vec![1; tau_proto::ARTIFACT_CHUNK_BYTES],
                    eof: false,
                }),
            },
        );
        assert!(h.runtime_io.bus.connection(&connection).is_some());
    }
    for _ in 0..2 {
        h.send_artifact_result(
            &connection,
            tau_proto::ArtifactResult {
                request_id: "retry".parse().expect("request"),
                result: Err(ArtifactError::Busy),
            },
        );
        assert!(h.runtime_io.bus.connection(&connection).is_none());
        assert!(
            h.runtime_io
                .bus
                .connection(&crate::test_connection_id("healthy"))
                .is_some()
        );
    }
}

fn request(h: &Harness, op: ArtifactOp) -> ArtifactRequest {
    ArtifactRequest {
        request_id: "artifact-test".parse().expect("valid request"),
        expected_session_id: h.session_runtime.current_session_id.clone(),
        op,
    }
}

fn rpc(
    h: &mut Harness,
    sink: &Arc<Mutex<Vec<RoutedFrame>>>,
    op: ArtifactOp,
) -> Result<ArtifactValue, ArtifactError> {
    assert_eq!(
        h.extensions.entries["artifact-test"].state,
        path_crate_extension::ExtensionState::Ready
    );
    assert!(
        !h.extensions
            .ready_received
            .contains(&crate::test_connection_id("artifact-test"))
    );
    let request = request(h, op);
    h.handle_extension_message(
        &crate::test_connection_id("artifact-test"),
        HarnessInputMessage::ArtifactRequest(request),
    )
    .expect("artifact request admission");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        {
            let mut frames = sink.lock().expect("response sink");
            if let Some(index) = frames
                .iter()
                .position(|frame| matches!(&frame.frame, HarnessOutputMessage::ArtifactResult(_)))
            {
                let frame = frames.remove(index);
                let HarnessOutputMessage::ArtifactResult(result) = frame.frame else {
                    unreachable!()
                };
                assert_eq!(result.request_id.as_str(), "artifact-test");
                return result.result;
            }
        }
        assert!(Instant::now() < deadline);
        if let Ok(HarnessEvent::Command(command)) =
            h.runtime_io.rx.recv_timeout(Duration::from_millis(100))
        {
            h.handle_harness_command(command)
                .expect("worker completion");
        }
    }
}

/// Complete a configured extension's real Ready transition for artifact RPC
/// admission tests.
fn connect_activated_artifact_extension(h: &mut Harness) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let connection_id = crate::test_connection_id("artifact-test");
    let sink = connect_test_client(h, "artifact-test", tau_proto::ClientKind::Tool);
    mark_connected_test_extension_configured(
        h,
        "artifact-test",
        "artifact-tool",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut(&connection_id)
        .expect("configured artifact extension")
        .state = path_crate_extension::ExtensionState::Handshaking;
    h.extensions
        .activation_staging
        .insert(connection_id.clone(), ExtensionActivationStage::default());
    h.extensions.initial_tool_preflight_complete = true;
    h.handle_extension_message(&connection_id, TestMessage::Ready(Default::default()))
        .expect("Ready activation");
    sink
}

/// Artifact RPC remains illegal before Ready and isolates the configured peer
/// without starting artifact storage work or returning a directed result.
#[test]
fn artifact_request_before_ready_remains_a_protocol_failure() {
    let temp = TempDir::new().expect("private root");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let connection_id = crate::test_connection_id("artifact-test");
    let sink = connect_test_client(&mut h, "artifact-test", tau_proto::ClientKind::Tool);
    mark_connected_test_extension_configured(
        &mut h,
        "artifact-test",
        "artifact-tool",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut(&connection_id)
        .expect("configured artifact extension")
        .state = path_crate_extension::ExtensionState::Handshaking;

    h.handle_extension_message(
        &connection_id,
        HarnessInputMessage::ArtifactRequest(request(&h, ArtifactOp::Available)),
    )
    .expect("protocol failure is isolated");

    assert_eq!(
        h.extensions.entries["artifact-test"].state,
        path_crate_extension::ExtensionState::Disconnected
    );
    assert!(h.runtime_io.artifacts.is_none());
    assert!(
        sink.lock()
            .expect("response sink")
            .iter()
            .all(|frame| { !matches!(frame.frame, HarnessOutputMessage::ArtifactResult(_)) })
    );
}

/// Real Ready activation must admit Artifact RPC in steady state without
/// disconnecting its configured extension; bounded worker completion and
/// directed sink routing then compose without journaling bytes.
#[test]
fn artifact_rpc_roundtrip_is_directed_and_allows_persistent_ephemeral_sessions() {
    let temp = TempDir::new().expect("private root");
    let mut h = quiet_provider_harness_ephemeral(temp.path()).expect("ephemeral harness");
    let sink = connect_activated_artifact_extension(&mut h);
    assert_eq!(
        rpc(&mut h, &sink, ArtifactOp::Available),
        Ok(ArtifactValue::Done)
    );
    assert!(
        h.runtime_io
            .bus
            .connection(&crate::test_connection_id("artifact-test"))
            .is_some()
    );
    let bytes = b"original binary\0\xff".to_vec();
    let mut upload = tau_client::ArtifactUpload::new(bytes.clone()).expect("bounded original");
    while let Some(op) = upload.next_op() {
        upload
            .accept(rpc(&mut h, &sink, op).expect("upload RPC"))
            .expect("upload response");
    }
    let key = upload
        .descriptor()
        .expect("published descriptor")
        .key
        .clone();
    let mut download = tau_client::ArtifactDownload::new(key);
    while let Some(op) = download.next_op() {
        download
            .accept(rpc(&mut h, &sink, op).expect("read RPC"))
            .expect("read response");
    }
    assert_eq!(download.into_bytes().expect("verified original"), bytes);
    assert!(temp.path().join("artifacts").exists());
    assert!(
        !temp.path().join("sessions/s1").exists(),
        "explicit artifacts do not enable ephemeral journals"
    );
}

/// Memory-only mode rejects availability and writes before constructing a
/// worker or inspecting/cleaning an already existing persistent artifact
/// domain.
#[test]
fn artifact_memory_only_admission_never_touches_persistent_store() {
    let temp = TempDir::new().expect("private root");
    let root = temp.path().join("artifacts");
    std::fs::create_dir(&root).expect("seed artifact domain");
    std::fs::write(root.join("sentinel"), b"preserve").expect("seed sentinel");
    let mut h = echo_harness_memory_only(temp.path()).expect("memory-only harness");
    let sink = connect_ready_configured_extension(
        &mut h,
        "artifact-test",
        "artifact-tool",
        tau_proto::ClientKind::Tool,
    );
    assert_eq!(
        rpc(&mut h, &sink, ArtifactOp::Available),
        Err(ArtifactError::Permission)
    );
    assert_eq!(
        rpc(
            &mut h,
            &sink,
            ArtifactOp::Begin {
                size: tau_proto::ArtifactSize::new(1).expect("size")
            }
        ),
        Err(ArtifactError::Permission)
    );
    assert!(h.runtime_io.artifacts.is_none());
    assert_eq!(std::fs::read_dir(&root).expect("untouched root").count(), 1);
    assert_eq!(
        std::fs::read(root.join("sentinel")).expect("untouched sentinel"),
        b"preserve"
    );
}

/// UI/remote-style clients do not gain Artifact authority; exact session
/// mismatch and oversized payloads fail before creating a store or worker.
#[test]
fn artifact_admission_preserves_peer_session_and_frame_boundaries() {
    let temp = TempDir::new().expect("private root");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let client = connect_test_client(&mut h, "unconfigured", tau_proto::ClientKind::Ui);
    let message = HarnessInputMessage::ArtifactRequest(request(
        &h,
        ArtifactOp::Begin {
            size: tau_proto::ArtifactSize::new(1).expect("size"),
        },
    ));
    h.handle_client_message(&crate::test_connection_id("unconfigured"), message)
        .expect("client request");
    assert!(client.lock().expect("client sink").is_empty());
    assert!(h.runtime_io.artifacts.is_none());
    let sink = connect_ready_configured_extension(
        &mut h,
        "artifact-test",
        "artifact-tool",
        tau_proto::ClientKind::Tool,
    );
    let mut mismatch = request(&h, ArtifactOp::Available);
    mismatch.expected_session_id = "other-session".parse().expect("session identifier");
    let admission = ExtensionFrameAdmission {
        session_id: h.session_runtime.current_session_id.clone(),
        session_generation: h.session_runtime.current_session_generation,
    };
    h.handle_artifact_request(
        &crate::test_connection_id("artifact-test"),
        mismatch,
        admission,
    );
    let frames = sink.lock().expect("response sink");
    assert!(frames.iter().any(|frame| matches!(&frame.frame, HarnessOutputMessage::ArtifactResult(result) if result.result == Err(ArtifactError::SessionMismatch))));
    drop(frames);
    sink.lock().expect("response sink").clear();
    assert_eq!(
        rpc(
            &mut h,
            &sink,
            ArtifactOp::Write {
                upload: "upload".parse().expect("valid upload"),
                offset: 0,
                bytes: vec![0; tau_proto::ARTIFACT_CHUNK_BYTES + 1]
            }
        ),
        Err(ArtifactError::Invalid)
    );
    assert!(h.runtime_io.artifacts.is_none());
    assert!(!temp.path().join("artifacts").exists());
}

/// Returning to the same textual session must not revive a deferred operation's
/// old generation authority or create an artifact worker.
#[test]
fn artifact_stale_admission_generation_cannot_reenter_same_session() {
    let temp = TempDir::new().expect("private root");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let sink = connect_ready_configured_extension(
        &mut h,
        "artifact-test",
        "artifact-tool",
        tau_proto::ClientKind::Tool,
    );
    let admission = ExtensionFrameAdmission {
        session_id: h.session_runtime.current_session_id.clone(),
        session_generation: h.session_runtime.current_session_generation,
    };
    let request = request(
        &h,
        ArtifactOp::Begin {
            size: tau_proto::ArtifactSize::new(1).expect("size"),
        },
    );
    h.session_runtime.current_session_generation = h
        .session_runtime
        .current_session_generation
        .saturating_next();
    h.handle_artifact_request(
        &crate::test_connection_id("artifact-test"),
        request,
        admission,
    );
    assert!(sink.lock().expect("response sink").iter().any(|frame| matches!(&frame.frame,
        HarnessOutputMessage::ArtifactResult(result) if result.result == Err(ArtifactError::SessionMismatch))));
    assert!(h.runtime_io.artifacts.is_none());
    assert!(!temp.path().join("artifacts").exists());
}
