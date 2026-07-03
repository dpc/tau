use std::io::BufReader;
use std::os::unix::net::UnixStream;

use tau_config::settings::TauDirs;
use tau_proto::{HarnessOutputMessage, PeerInputReader};
use tempfile::TempDir;

use super::*;
use crate::harness::Harness;

/// Ensures the synchronous daemon-message helper subscribes only to the
/// concrete trace events it consumes, instead of receiving every future event
/// in broad protocol categories.
#[test]
fn daemon_message_trace_subscription_uses_no_prefix_selectors() {
    let selectors = daemon_message_event_selectors();

    let expected = [
        EventName::AGENT_PROMPT_CREATED,
        EventName::PROVIDER_RESPONSE_FINISHED,
        EventName::TOOL_PROGRESS,
        EventName::SHELL_COMMAND_PROGRESS,
        EventName::HARNESS_NOTICE,
        EventName::EXTENSION_STARTING,
        EventName::EXTENSION_READY,
        EventName::EXTENSION_EXITED,
        EventName::EXTENSION_RESTARTING,
    ]
    .into_iter()
    .map(EventSelector::Exact)
    .collect::<Vec<_>>();

    assert_eq!(selectors, expected);
}

/// Ensures startup failures are reported to the initial UI through the Tau
/// protocol, rather than requiring the UI to scrape harness stderr logs.
#[test]
fn startup_error_is_sent_as_protocol_disconnect() {
    let (harness_end, ui_end) = UnixStream::pair().expect("stream pair");
    let error = std::io::Error::other("missing startup setting");

    send_initial_client_startup_error(
        Some(InitialClientStartupErrorOutput::Stream(harness_end)),
        &error,
    );

    let mut reader = PeerInputReader::new(BufReader::new(ui_end));
    let message = reader
        .read_message()
        .expect("read disconnect frame")
        .expect("disconnect frame");
    let HarnessOutputMessage::Disconnect(disconnect) = message else {
        panic!("expected disconnect frame");
    };
    let reason = disconnect.reason.expect("disconnect reason");
    assert!(reason.contains("harness startup failed"));
    assert!(reason.contains("missing startup setting"));
}

/// Ensures daemon-owned startup failures after the initial UI has been accepted
/// are routed through the normal connection writer and flushed before the
/// process can exit, rather than falling back to EOF or racing a side-channel
/// write.
#[test]
fn post_accept_startup_error_is_sent_through_normal_writer() {
    fn echo_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
        crate::harness::run_echo_provider(r, w).map_err(|e| e.to_string())
    }

    let td = TempDir::new().expect("tempdir");
    let state_dir = td.path().join("state");
    let dirs = TauDirs {
        config_dir: Some(td.path().join("config")),
        state_dir: Some(td.path().join("runtime")),
    };
    let mut harness = Harness::new_with_provider(
        &state_dir,
        dirs,
        echo_runner,
        echo_tools(),
        "s1",
        tau_proto::SessionStartReason::Initial,
        tau_core::SessionPersistenceMode::Durable,
    )
    .expect("harness");
    let (server_end, ui_end) = UnixStream::pair().expect("stream pair");
    let client_id = harness.accept_client(server_end).expect("accept client");
    let mut pre_accept_stream = None;

    let result = notify_startup_error_after_accept::<(), _>(
        Err(std::io::Error::other("marker write failed")),
        &mut pre_accept_stream,
        &mut harness,
        Some(&client_id),
    );

    assert!(result.is_err());
    let mut reader = PeerInputReader::new(BufReader::new(ui_end));
    let message = reader
        .read_message()
        .expect("read disconnect frame")
        .expect("disconnect frame");
    let HarnessOutputMessage::Disconnect(disconnect) = message else {
        panic!("expected disconnect frame");
    };
    let reason = disconnect.reason.expect("disconnect reason");
    assert!(reason.contains("harness startup failed"));
    assert!(reason.contains("marker write failed"));
}
