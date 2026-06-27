use std::io::BufReader;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use tau_proto::{HarnessOutputMessage, PeerInputReader};

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
