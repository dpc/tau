use std::io as path_std_io;
use std::io::{BufReader, BufWriter};
use std::os::unix as path_std_os_unix;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tempfile::TempDir;

mod support;
use support::isolated_runtime_dir;

const FAKE_HARNESS_TIMEOUT: Duration = Duration::from_secs(3);

/// Returns the bundled Tau binary under Cargo's integration-test contract.
fn tau_bin() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_tau")
        .map(PathBuf::from)
        .expect("CARGO_BIN_EXE_tau")
}

/// Creates a private runtime root suitable for Tau's same-user socket policy.
fn runtime_root(temp: &TempDir) -> PathBuf {
    isolated_runtime_dir(temp.path())
}

/// Runs one isolated `tau session list` command.
fn run_list(runtime: &Path, cwd: &Path, arguments: &[&str]) -> Output {
    let root = cwd;
    Command::new(tau_bin())
        .args(["session", "list"])
        .args(arguments)
        .current_dir(cwd)
        .env_clear()
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", runtime)
        .output()
        .expect("run tau session list")
}

/// Serves one bounded fake current-session control exchange.
fn serve_current_session(
    listener: tau_socket::SocketListener,
    session_id: tau_proto::SessionId,
    project_root: PathBuf,
) -> Result<(), String> {
    let raw_listener = listener
        .try_clone_raw_listener()
        .map_err(|error| error.to_string())?;
    raw_listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + FAKE_HARNESS_TIMEOUT;
    let stream = loop {
        match raw_listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == path_std_io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err("timed out waiting for session-list probe".to_owned());
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(format!("accept session-list probe: {error}")),
        }
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    stream
        .set_read_timeout(Some(remaining))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(remaining))
        .map_err(|error| error.to_string())?;
    let reader_stream = stream.try_clone().map_err(|error| error.to_string())?;
    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(reader_stream));
    let mut writer = tau_proto::HarnessOutputWriter::new(BufWriter::new(stream));
    match reader.read_message().map_err(|error| error.to_string())? {
        Some(tau_proto::HarnessInputMessage::Hello(hello))
            if hello.expected_session_id.as_ref() == Some(&session_id) => {}
        other => return Err(format!("expected probe hello, got {other:?}")),
    }
    writer
        .write_message(&tau_proto::HarnessOutputMessage::SessionAccepted(
            tau_proto::SessionAccepted {
                session_id: session_id.clone(),
            },
        ))
        .map_err(|error| error.to_string())?;
    writer.flush().map_err(|error| error.to_string())?;
    let request = match reader.read_message().map_err(|error| error.to_string())? {
        Some(tau_proto::HarnessInputMessage::GetCurrentSession(request)) => request,
        other => return Err(format!("expected current-session request, got {other:?}")),
    };
    writer
        .write_message(&tau_proto::HarnessOutputMessage::CurrentSessionResult(
            tau_proto::CurrentSessionResult {
                request_id: request.request_id,
                session_id,
                project_root,
            },
        ))
        .map_err(|error| error.to_string())?;
    writer.flush().map_err(|error| error.to_string())
}

/// Empty runtime inspection returns the two compatible empty forms and creates
/// no Tau runtime state.
#[test]
fn empty_runtime_is_successful_and_inspection_only() {
    let temp = TempDir::new().expect("tempdir");
    let runtime = runtime_root(&temp);

    let bare = run_list(&runtime, temp.path(), &[]);
    let json = run_list(&runtime, temp.path(), &["--json"]);

    assert!(bare.status.success());
    assert!(bare.stdout.is_empty());
    assert!(bare.stderr.is_empty());
    assert!(json.status.success());
    assert_eq!(json.stdout, b"[]\n");
    assert!(json.stderr.is_empty());
    assert!(!runtime.join("tau").exists());
    for directory in ["home", "config", "state", "cache"] {
        assert!(
            !temp.path().join(directory).exists(),
            "inspection created isolated {directory} state"
        );
    }
}

/// Invalid filters follow clap's exit-2 path and never write stdout that
/// automation could mistake for an empty snapshot.
#[test]
fn invalid_directory_exits_two_without_stdout() {
    let temp = TempDir::new().expect("tempdir");
    let runtime = runtime_root(&temp);
    let output = run_list(&runtime, temp.path(), &["--dir", "missing", "--json"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
    assert!(!runtime.join("tau").exists());
}

/// A relative symlink filter resolves from caller CWD and selects the canonical
/// root returned by a responsive harness rather than adjacent metadata.
#[test]
fn relative_directory_filter_returns_live_json_record() {
    let temp = TempDir::new().expect("tempdir");
    let runtime = runtime_root(&temp);
    let project = temp.path().join("project");
    let alias = temp.path().join("alias");
    std::fs::create_dir(&project).expect("project directory");
    path_std_os_unix::fs::symlink(&project, &alias).expect("project symlink");
    let project = project.canonicalize().expect("canonical project");
    let session_id = "live-session"
        .parse::<tau_proto::SessionId>()
        .expect("known-safe SessionId must be valid");
    let mut claim = tau_harness::runtime_dir::claim_session_in(&runtime, &project, &session_id)
        .expect("claim fake harness runtime");
    claim
        .reclaim_stale_socket()
        .expect("reclaim stale fake harness socket");
    let listener =
        tau_socket::SocketListener::bind_fresh(claim.socket_path()).expect("fake harness listener");
    claim.publish(false).expect("publish fake harness claim");
    let response_session_id = session_id.clone();
    let response_root = project.clone();
    let (server_tx, server_rx) = mpsc::sync_channel(1);
    let daemon = std::thread::spawn(move || {
        let _ = server_tx.send(serve_current_session(
            listener,
            response_session_id,
            response_root,
        ));
    });

    let output = run_list(&runtime, temp.path(), &["--dir", "alias", "--json"]);

    let server_result = server_rx
        .recv_timeout(FAKE_HARNESS_TIMEOUT + Duration::from_secs(1))
        .expect("bounded fake harness result");
    daemon.join().expect("fake harness");
    server_result.expect("fake current-session exchange");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(output.stderr.is_empty());
    let records: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("session list JSON");
    assert_eq!(
        records,
        serde_json::json!([{
            "session_id": "live-session",
            "project_root": project,
        }])
    );
}

/// A runtime scan I/O failure returns nonzero with empty stdout and does not
/// mutate the structurally invalid candidate path.
#[test]
fn runtime_scan_failure_has_no_partial_stdout_or_cleanup() {
    let temp = TempDir::new().expect("tempdir");
    let runtime = runtime_root(&temp);
    let claims = runtime.join("tau/harnesses/claims");
    std::fs::create_dir_all(claims.parent().expect("Tau runtime root")).expect("Tau runtime root");
    std::fs::write(&claims, b"sentinel").expect("non-directory claims path");

    let output = run_list(&runtime, temp.path(), &["--json"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
    assert!(
        claims.is_file(),
        "inspection must not replace the invalid runtime entry"
    );
    assert_eq!(
        std::fs::read(&claims).expect("runtime sentinel"),
        b"sentinel",
        "inspection must not rewrite the invalid runtime entry"
    );
}
