use std::io::{BufReader, BufWriter, Error, ErrorKind};
use std::os::unix::net::UnixListener;
use std::time::Duration;

use super::*;

/// Writer that accepts frame bytes but fails the durability-like flush
/// boundary.
struct FlushFails {
    /// Bytes accepted before the simulated flush failure.
    bytes: Vec<u8>,
}

impl std::io::Write for FlushFails {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(Error::new(ErrorKind::BrokenPipe, "flush failed"))
    }
}

/// Already-unloaded retry is successful while a busy target remains a clear
/// rejection.
#[test]
fn typed_outcomes_preserve_idempotent_success() {
    assert!(classify_outcome(UnloadSessionAgentOutcome::AlreadyUnloaded).is_ok());
    assert!(classify_outcome(UnloadSessionAgentOutcome::Unloaded).is_ok());
    let error =
        classify_outcome(UnloadSessionAgentOutcome::AgentBusy).expect_err("busy target must fail");
    assert!(error.to_string().contains("agent_busy"));
}

/// Every post-send transport ambiguity tells operators that retry is safe.
#[test]
fn indeterminate_failures_use_required_retry_wording() {
    let error = indeterminate_error("daemon disconnected");
    assert!(error.to_string().contains("outcome unknown; retry safely"));
}

/// A frame accepted by the writer but rejected during flush remains
/// indeterminate.
#[test]
fn flush_failure_after_frame_write_is_indeterminate() {
    let mut writer = tau_proto::HarnessInputWriter::new(FlushFails { bytes: Vec::new() });
    let error = send_request(
        &mut writer,
        HarnessInputMessage::UnloadSessionAgent(UnloadSessionAgent {
            request_id: "request-1".to_owned(),
            session_id: "session-1".parse().expect("session"),
            agent_id: "agent-1".parse().expect("agent"),
        }),
    )
    .expect_err("flush must fail");
    assert!(error.to_string().contains("outcome unknown; retry safely"));
}

/// The real socket RPC sends the exact requested session and agent and accepts
/// success.
#[test]
fn exact_incident_command_succeeds_over_socket() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("harness.sock");
    let listener = UnixListener::bind(&path).expect("listener");
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let read_stream = stream.try_clone().expect("clone");
        let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(read_stream));
        let mut writer = tau_proto::HarnessOutputWriter::new(BufWriter::new(stream));
        writer
            .write_message(&HarnessOutputMessage::SessionAccepted(
                tau_proto::SessionAccepted {
                    session_id: "tau-zulip-bot".parse().expect("session"),
                },
            ))
            .expect("admit");
        writer.flush().expect("flush admission");
        let _hello = reader.read_message().expect("hello").expect("hello frame");
        let HarnessInputMessage::UnloadSessionAgent(request) = reader
            .read_message()
            .expect("request")
            .expect("request frame")
        else {
            panic!("expected unload request");
        };
        assert_eq!(request.session_id.as_str(), "tau-zulip-bot");
        assert_eq!(request.agent_id.as_str(), "zulip-bot-ngMK");
        writer
            .write_message(&HarnessOutputMessage::UnloadSessionAgentResult(
                tau_proto::UnloadSessionAgentResult {
                    request_id: request.request_id,
                    session_id: request.session_id,
                    agent_id: request.agent_id,
                    outcome: UnloadSessionAgentOutcome::Unloaded,
                },
            ))
            .expect("result");
        writer.flush().expect("flush result");
    });
    let result = request_at_socket(
        &path,
        &"tau-zulip-bot".parse().expect("session"),
        &"zulip-bot-ngMK".parse().expect("agent"),
        Duration::from_secs(1),
    );
    assert!(result.is_ok());
    server.join().expect("server");
}

/// EOF after the daemon receives the unload frame is reported as an unknown
/// outcome.
#[test]
fn post_send_eof_is_indeterminate() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("harness.sock");
    let listener = UnixListener::bind(&path).expect("listener");
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let read_stream = stream.try_clone().expect("clone");
        let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(read_stream));
        let mut writer = tau_proto::HarnessOutputWriter::new(BufWriter::new(stream));
        writer
            .write_message(&HarnessOutputMessage::SessionAccepted(
                tau_proto::SessionAccepted {
                    session_id: "tau-zulip-bot".parse().expect("session"),
                },
            ))
            .expect("admit");
        writer.flush().expect("flush");
        let _ = reader.read_message().expect("hello");
        let _ = reader.read_message().expect("request");
    });
    let error = request_at_socket(
        &path,
        &"tau-zulip-bot".parse().expect("session"),
        &"zulip-bot-ngMK".parse().expect("agent"),
        Duration::from_secs(1),
    )
    .expect_err("EOF is unknown");
    assert!(error.to_string().contains("outcome unknown; retry safely"));
    server.join().expect("server");
}
