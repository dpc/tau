use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Duration;
use std::{fs, thread};

use tau_config::settings::TauDirs;
use tau_proto::{HarnessOutputMessage, PeerInputReader};
use tempfile::TempDir;

use super::*;
use crate::harness::Harness;

/// Runtime harness startup captures one canonical directory identity so symlink
/// aliases and parent components cannot leak into live-session responses.
#[test]
fn project_root_identity_is_canonical_and_requires_a_directory() {
    let temp = TempDir::new().expect("tempdir");
    let project = temp.path().join("project");
    let alias = temp.path().join("alias");
    let file = temp.path().join("file");
    fs::create_dir(&project).expect("project directory");
    std::os::unix::fs::symlink(&project, &alias).expect("project symlink");
    fs::write(&file, b"not a directory").expect("test file");

    assert_eq!(
        canonical_project_root(&alias).expect("canonical project root"),
        project.canonicalize().expect("canonical project directory")
    );
    assert!(
        canonical_project_root(&file)
            .expect_err("file project root")
            .to_string()
            .contains("not a directory")
    );
}

/// Protects the component startup source boundary: direct entrypoints ignore
/// inherited private transport while only spawned initial-UI children consume
/// it.
#[test]
fn component_launch_selects_private_transport_only_for_spawned_child() {
    assert!(
        !ComponentLaunch::Direct(Vec::new()).uses_spawned_transport(),
        "bare/direct harness entrypoints use typed overrides"
    );
    assert!(ComponentLaunch::SpawnedInitialUiStdio.uses_spawned_transport());
    let malformed = Some("not-json".into());
    let direct = vec![tau_config::settings::ExtensionCliOverride::Disable(
        "std-pim".to_owned(),
    )];
    assert_eq!(
        ComponentLaunch::Direct(direct.clone())
            .extension_overrides(malformed.clone())
            .expect("direct launch ignores inherited private transport"),
        direct
    );
    assert!(
        ComponentLaunch::SpawnedInitialUiStdio
            .extension_overrides(malformed)
            .is_err(),
        "spawned child must fail closed on malformed private transport"
    );
}

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

/// Ensures dropping the listener forwarder wakes an idle poll/accept wait
/// without forwarding the internal wake endpoint as a harness client.
#[test]
fn listener_forwarder_drop_wakes_idle_accept() {
    let td = TempDir::new().expect("tempdir");
    let socket_path = td.path().join("daemon.sock");
    let listener = bind_listener(&socket_path).expect("bind listener");
    let (forwarder, rx) = spawn_waiting_test_forwarder(&listener);

    drop_forwarder_with_timeout(forwarder);

    assert!(
        rx.try_recv().is_err(),
        "wake endpoint must not be delivered as a daemon client"
    );
}

/// Ensures listener forwarder shutdown uses its owned wake endpoint rather than
/// relying on the daemon socket pathname to remain present.
#[test]
fn listener_forwarder_drop_wakes_after_socket_path_removed() {
    let td = TempDir::new().expect("tempdir");
    let socket_path = td.path().join("daemon.sock");
    let listener = bind_listener(&socket_path).expect("bind listener");
    let (forwarder, rx) = spawn_waiting_test_forwarder(&listener);

    fs::remove_file(&socket_path).expect("remove daemon socket path");
    drop_forwarder_with_timeout(forwarder);

    assert!(
        rx.try_recv().is_err(),
        "wake endpoint must not be delivered as a daemon client"
    );
}

/// Ensures listener forwarder shutdown is tied to the owned accept thread even
/// when another listener later occupies the same daemon socket pathname.
#[test]
fn listener_forwarder_drop_wakes_after_socket_path_replaced() {
    let td = TempDir::new().expect("tempdir");
    let socket_path = td.path().join("daemon.sock");
    let listener = bind_listener(&socket_path).expect("bind listener");
    let (forwarder, rx) = spawn_waiting_test_forwarder(&listener);

    fs::remove_file(&socket_path).expect("remove daemon socket path");
    let _replacement = bind_listener(&socket_path).expect("bind replacement listener");
    drop_forwarder_with_timeout(forwarder);

    assert!(
        rx.try_recv().is_err(),
        "wake endpoint must not be delivered as a daemon client"
    );
}

/// Ensures shutdown wakeup wins over a listener that is already receiving
/// clients, so lifecycle control cannot be starved by accept draining.
#[test]
fn listener_forwarder_drop_wakes_while_accept_ready() {
    let td = TempDir::new().expect("tempdir");
    let socket_path = td.path().join("daemon.sock");
    let listener = bind_listener(&socket_path).expect("bind listener");
    let (forwarder, rx) = spawn_waiting_test_forwarder(&listener);
    let (stop_tx, stop_rx) = mpsc::channel();
    let connector_path = socket_path.clone();
    let connector = thread::spawn(move || {
        while stop_rx.try_recv().is_err() {
            let _ = UnixStream::connect(&connector_path);
        }
    });
    rx.recv_timeout(Duration::from_secs(1))
        .expect("forwarder should accept at least one traffic client");

    let (done_tx, done_rx) = mpsc::channel();
    let drop_join = thread::spawn(move || {
        drop(forwarder);
        let _ = done_tx.send(());
    });
    let drop_result = done_rx.recv_timeout(Duration::from_secs(1));
    let _ = stop_tx.send(());
    connector.join().expect("connector should not panic");
    drop_result.expect("forwarder drop should not wait for accept traffic to quiesce");
    drop_join.join().expect("drop thread should not panic");
}

fn spawn_waiting_test_forwarder(
    listener: &SocketListener,
) -> (ListenerForwarder, mpsc::Receiver<HarnessEvent>) {
    let (tx, rx) = mpsc::channel();
    let (before_wait_tx, before_wait_rx) = mpsc::channel();
    let forwarder = ListenerForwarder::spawn_for_test(
        listener.try_clone_raw_listener().expect("clone listener"),
        tx,
        before_wait_tx,
    )
    .expect("spawn listener forwarder");
    before_wait_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("forwarder should reach poll wait");
    (forwarder, rx)
}

fn drop_forwarder_with_timeout(forwarder: ListenerForwarder) {
    let (done_tx, done_rx) = mpsc::channel();
    let join = thread::spawn(move || {
        drop(forwarder);
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("forwarder drop should wake and join accept thread");
    join.join().expect("drop thread should not panic");
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

/// Ensures pre-resolved entrypoints cannot silently accept the environment
/// bypass and enforce any exact extension allowlist before construction.
#[test]
fn pre_resolved_daemon_rejects_environment_bypass_and_enforces_allowlist() {
    let config = crate::settings::default_config();
    let bypass = ServeOptions::builder()
        .ignore_startup_environment(true)
        .build();
    assert!(validate_pre_resolved_serve_options(&bypass, &config).is_err());

    let mismatch = ServeOptions::builder()
        .allowed_extensions(std::collections::BTreeSet::from(["not-configured".into()]))
        .build();
    let error = validate_pre_resolved_serve_options(&mismatch, &config)
        .expect_err("pre-resolved allowlist mismatch must fail before harness construction");
    assert!(
        error
            .to_string()
            .contains("resolved extensions differ from deterministic allowlist")
    );
}
