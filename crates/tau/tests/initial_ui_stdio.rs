use std::io::{BufReader, BufWriter};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tau_proto::{
    ClientKind, HarnessInputMessage, HarnessOutputMessage, Hello, PROTOCOL_VERSION,
    PeerInputReader, PeerOutputWriter, SessionId,
};

/// Ensures a real `tau component harness --initial-ui-stdio` child flushes a
/// fatal startup disconnect all the way to child stdout before exiting. This
/// protects the child-process stdio path, not just the in-process UnixStream
/// writer.
#[test]
fn initial_ui_stdio_startup_error_reaches_child_stdout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let runtime_dir = temp.path().join("runtime");
    let tau_config_dir = config_home.join("tau");
    std::fs::create_dir_all(&tau_config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_home).expect("mkdir state");
    std::fs::create_dir_all(&runtime_dir).expect("mkdir runtime");
    std::fs::write(
        tau_config_dir.join("harness.yaml"),
        r#"
extensions:
  startup-secret-test:
    command: [tau]
    secrets:
      missing_token: {}
"#,
    )
    .expect("write harness config");

    let tau_bin = std::env::var("CARGO_BIN_EXE_tau").expect("CARGO_BIN_EXE_tau");
    let mut child = Command::new(tau_bin)
        .arg("component")
        .arg("harness")
        .arg("--initial-ui-stdio")
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tau harness");
    let _stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("stdout");
    let (sender, receiver) = mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let mut reader = PeerInputReader::new(BufReader::new(stdout));
        let _ = sender.send(reader.read_message());
    });

    let message = match receiver.recv_timeout(Duration::from_secs(10)) {
        Ok(message) => message
            .expect("read startup disconnect")
            .expect("startup disconnect"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader_thread.join();
            panic!("timed out waiting for startup disconnect on child stdout");
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("startup disconnect reader thread exited without reporting a result");
        }
    };
    reader_thread.join().expect("reader thread");
    let HarnessOutputMessage::Disconnect(disconnect) = message else {
        panic!("expected disconnect frame");
    };
    let reason = disconnect.reason.expect("disconnect reason");
    assert!(reason.contains("harness startup failed"));
    assert!(reason.contains("missing_token"));

    let status = child.wait().expect("wait child");
    assert!(!status.success());
}

/// A resumed harness process must revalidate the selected session under its
/// non-creating lock path and report deletion without recreating the target.
#[test]
fn resumed_harness_process_does_not_recreate_deleted_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let runtime_dir = temp.path().join("runtime");
    let tau_config_dir = config_home.join("tau");
    let session_dir = state_home.join("tau/sessions/deleted-session");
    std::fs::create_dir_all(&tau_config_dir).expect("mkdir config");
    std::fs::create_dir_all(&session_dir).expect("mkdir selected session");
    std::fs::create_dir_all(&runtime_dir).expect("mkdir runtime");
    std::fs::write(session_dir.join("lock"), b"").expect("write session lock");
    std::fs::write(
        session_dir.join("meta.json"),
        br#"{"created_at":1,"last_touched":1}"#,
    )
    .expect("write session metadata");
    std::fs::remove_dir_all(&session_dir).expect("delete selected session before startup lock");

    let tau_bin = std::env::var("CARGO_BIN_EXE_tau").expect("CARGO_BIN_EXE_tau");
    let mut child = Command::new(tau_bin)
        .arg("component")
        .arg("harness")
        .arg("--initial-ui-stdio")
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("TAU_SESSION_ID", "deleted-session")
        .env("TAU_SESSION_STATUS", "resumed")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn resumed tau harness");
    let _stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("stdout");
    let (sender, receiver) = mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let mut reader = PeerInputReader::new(BufReader::new(stdout));
        let _ = sender.send(reader.read_message());
    });

    let message = match receiver.recv_timeout(Duration::from_secs(10)) {
        Ok(message) => message
            .expect("read startup disconnect")
            .expect("startup disconnect"),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader_thread.join();
            panic!("failed waiting for resume deletion result: {error}");
        }
    };
    reader_thread.join().expect("reader thread");
    let HarnessOutputMessage::Disconnect(disconnect) = message else {
        panic!("expected disconnect frame");
    };
    assert!(disconnect.reason.as_deref().is_some_and(
        |reason| reason.contains("deleted-session") && reason.contains("no longer exists")
    ));
    assert!(!session_dir.exists());

    let status = child.wait().expect("wait child");
    assert!(!status.success());
}

/// The configured harness path used by the public component creates the
/// resumed stderr relay target only after it has acquired the existing lock.
#[test]
fn resumed_configured_harness_process_creates_relay_log() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let runtime_dir = temp.path().join("runtime");
    let tau_config_dir = config_home.join("tau");
    let session_dir = state_home.join("tau/sessions/resumed-session");
    std::fs::create_dir_all(&tau_config_dir).expect("mkdir config");
    std::fs::create_dir_all(&session_dir).expect("mkdir selected session");
    std::fs::create_dir_all(&runtime_dir).expect("mkdir runtime");
    std::fs::write(session_dir.join("lock"), b"").expect("write session lock");
    std::fs::write(
        session_dir.join("meta.json"),
        br#"{"created_at":1,"last_touched":1}"#,
    )
    .expect("write session metadata");

    let tau_bin = std::env::var("CARGO_BIN_EXE_tau").expect("CARGO_BIN_EXE_tau");
    let mut child = Command::new(tau_bin)
        .arg("component")
        .arg("harness")
        .arg("--initial-ui-stdio")
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("TAU_SESSION_ID", "resumed-session")
        .env("TAU_SESSION_STATUS", "resumed")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn resumed tau harness");
    let harness_log = session_dir.join("logs/tau-harness.log");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !harness_log.exists() {
        if child.try_wait().expect("query child").is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        harness_log.exists(),
        "configured resume must create the parent relay target"
    );
}

/// A real harness process must reject an attach handshake whose expected
/// session differs from the process's actual bound session.
#[test]
fn harness_process_rejects_attach_session_mismatch() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let runtime_dir = temp.path().join("runtime");
    std::fs::create_dir_all(config_home.join("tau")).expect("mkdir config");
    std::fs::create_dir_all(&state_home).expect("mkdir state");
    std::fs::create_dir_all(&runtime_dir).expect("mkdir runtime");

    let tau_bin = std::env::var("CARGO_BIN_EXE_tau").expect("CARGO_BIN_EXE_tau");
    let mut child = Command::new(tau_bin)
        .arg("component")
        .arg("harness")
        .arg("--initial-ui-stdio")
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("TAU_SESSION_ID", "actual-session")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tau harness");
    let mut writer = PeerOutputWriter::new(BufWriter::new(child.stdin.take().expect("stdin")));
    writer
        .write_message(&HarnessInputMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "attach-test".parse().expect("valid client name"),
            client_kind: ClientKind::Ui,
            expected_session_id: Some(
                SessionId::parse("requested-session").expect("valid session id"),
            ),
            capabilities: Vec::new(),
        }))
        .expect("write attach hello");
    writer.flush().expect("flush attach hello");

    let stdout = child.stdout.take().expect("stdout");
    let (sender, receiver) = mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let mut reader = PeerInputReader::new(BufReader::new(stdout));
        let _ = sender.send(reader.read_message());
    });
    let message = match receiver.recv_timeout(Duration::from_secs(10)) {
        Ok(message) => message
            .expect("read handshake response")
            .expect("handshake response"),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader_thread.join();
            panic!("failed waiting for attach mismatch result: {error}");
        }
    };
    reader_thread.join().expect("reader thread");
    let HarnessOutputMessage::Disconnect(disconnect) = message else {
        panic!("expected disconnect frame");
    };
    let reason = disconnect.reason.expect("disconnect reason");
    assert!(reason.contains("requested-session"));
    assert!(reason.contains("actual-session"));
    assert!(reason.contains("tau attach requested-session"));

    drop(writer);
    let status = child.wait().expect("wait child");
    assert!(!status.success());
}
