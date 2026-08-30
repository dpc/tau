use std::io::{BufReader, BufWriter};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tau_proto::{
    ClientKind, Disconnect, Event, EventName, EventSelector, HarnessInputMessage,
    HarnessOutputMessage, Hello, PROTOCOL_VERSION, PeerInputReader, PeerOutputWriter, SessionId,
    Subscribe,
};

/// Owns an initial-UI child and reaps it if a test exits before clean shutdown.
struct InitialUiChild {
    /// Spawned component process.
    process: Child,
}

impl InitialUiChild {
    /// Waits for clean shutdown only until the supplied timeout, then reaps the
    /// child so a broken lifecycle cannot hang the test suite.
    fn wait_for_exit(&mut self, timeout: Duration) -> Result<ExitStatus, String> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.process.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) if Instant::now() >= deadline => {
                    self.stop();
                    return Err("initial-UI child did not exit after disconnect".to_owned());
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) => {
                    self.stop();
                    return Err(format!("query initial-UI child: {error}"));
                }
            }
        }
    }

    /// Stops and reaps a child that cannot complete the protocol under test.
    fn stop(&mut self) {
        if !matches!(self.process.try_wait(), Ok(Some(_))) {
            let _ = self.process.kill();
            let _ = self.process.wait();
        }
    }
}

impl Drop for InitialUiChild {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Builds an initial-UI harness command isolated from the test runner's
/// configuration and launch environment.
fn initial_ui_stdio_command(
    tau_bin: &str,
    temp: &tempfile::TempDir,
    config_home: &Path,
    state_home: &Path,
    runtime_dir: &Path,
) -> Command {
    let home = temp.path().join("home");
    let cache_home = temp.path().join("cache");
    std::fs::create_dir_all(&home).expect("mkdir home");
    std::fs::create_dir_all(&cache_home).expect("mkdir cache");

    let mut command = Command::new(tau_bin);
    command
        .env_clear()
        .arg("component")
        .arg("harness")
        .arg("--initial-ui-stdio")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_STATE_HOME", state_home)
        .env("XDG_CACHE_HOME", cache_home)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("LANG", "C.UTF-8")
        .env("TERM", "xterm-256color")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

fn write_initial_ui_handshake(writer: &mut PeerOutputWriter<BufWriter<std::process::ChildStdin>>) {
    writer
        .write_message(&HarnessInputMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "tau-chat".parse().expect("valid chat client name"),
            client_kind: ClientKind::Ui,
            expected_session_id: None,
            capabilities: Vec::new(),
        }))
        .expect("write hello");
    writer
        .write_message(&HarnessInputMessage::Subscribe(Subscribe {
            historical_selectors: vec![EventSelector::Exact(EventName::SESSION_REPLAY_COMPLETE)],
            live_selectors: Vec::new(),
        }))
        .expect("write subscribe");
    writer.flush().expect("flush handshake");
}

/// A real spawned initial-stdio process emits exactly one welcome only when the
/// conversational launch marker is present.
#[test]
fn initial_ui_introduction_notice_requires_conversational_launch() {
    for (eligible, expected) in [(true, 1), (false, 0)] {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_home = temp.path().join("config");
        let state_home = temp.path().join("state");
        let runtime_dir = temp.path().join("runtime");
        std::fs::create_dir_all(config_home.join("tau")).expect("mkdir config");
        std::fs::create_dir_all(&state_home).expect("mkdir state");
        std::fs::create_dir_all(&runtime_dir).expect("mkdir runtime");
        std::fs::write(
            config_home.join("tau/harness.yaml"),
            "extensions:\n  provider-builtin:\n    enable: false\n  core-shell:\n    enable: false\n",
        )
        .expect("write minimal harness config");

        let tau_bin = std::env::var("CARGO_BIN_EXE_tau").expect("CARGO_BIN_EXE_tau");
        let mut command =
            initial_ui_stdio_command(&tau_bin, &temp, &config_home, &state_home, &runtime_dir);
        command.env("TAU_SESSION_ID", format!("introduction-{eligible}"));
        if eligible {
            command.env(tau_harness::INITIAL_UI_INTRODUCTION_NOTICE_ENV, "1");
        }
        let mut child = command.spawn().expect("spawn harness");
        let mut writer = PeerOutputWriter::new(BufWriter::new(child.stdin.take().expect("stdin")));
        write_initial_ui_handshake(&mut writer);
        let stdout = child.stdout.take().expect("stdout");
        let (sender, receiver) = mpsc::channel();
        let reader_thread = std::thread::spawn(move || {
            let mut reader = PeerInputReader::new(BufReader::new(stdout));
            while let Ok(Some(message)) = reader.read_message() {
                if sender.send(message).is_err() {
                    break;
                }
            }
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut introductions = 0;
        let mut startup_complete = false;
        while Instant::now() < deadline && (!startup_complete || introductions < expected) {
            let message = match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(message) => message,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("initial UI reader exited before startup completed")
                }
            };
            match message {
                HarnessOutputMessage::Deliver(delivery) => match delivery.event() {
                    Event::HarnessNotice(notice)
                        if notice.kind == tau_proto::notice_kind::HARNESS_INTRODUCTION =>
                    {
                        introductions += 1;
                    }
                    Event::SessionReplayComplete(_) => startup_complete = true,
                    _ => {}
                },
                HarnessOutputMessage::Disconnect(disconnect) => {
                    panic!(
                        "initial UI disconnected before startup completed: {:?}",
                        disconnect.reason
                    );
                }
                _ => {}
            }
        }
        assert!(startup_complete, "spawned harness did not finish startup");
        std::thread::sleep(Duration::from_millis(100));
        while let Ok(message) = receiver.try_recv() {
            if matches!(
                message,
                HarnessOutputMessage::Deliver(ref delivery)
                    if matches!(
                        delivery.event(),
                        Event::HarnessNotice(notice)
                            if notice.kind == tau_proto::notice_kind::HARNESS_INTRODUCTION
                    )
            ) {
                introductions += 1;
            }
        }
        assert_eq!(introductions, expected);

        writer
            .write_message(&HarnessInputMessage::Disconnect(Disconnect {
                reason: Some("test complete".to_owned()),
            }))
            .expect("write disconnect");
        writer.flush().expect("flush disconnect");
        drop(writer);
        let _ = child.wait().expect("wait child");
        reader_thread.join().expect("reader thread");
    }
}

/// A metadata-publication failure occurs after harness construction but before
/// welcome eligibility is consumed, so the initial UI receives only disconnect.
#[test]
fn late_startup_failure_does_not_emit_introduction_notice() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let runtime_dir = temp.path().join("runtime");
    std::fs::create_dir_all(config_home.join("tau")).expect("mkdir config");
    std::fs::create_dir_all(&state_home).expect("mkdir state");
    std::fs::create_dir_all(&runtime_dir).expect("mkdir runtime");
    std::fs::write(
        config_home.join("tau/harness.yaml"),
        "extensions:\n  provider-builtin:\n    enable: false\n  core-shell:\n    enable: false\n",
    )
    .expect("write minimal harness config");

    let tau_bin = std::env::var("CARGO_BIN_EXE_tau").expect("CARGO_BIN_EXE_tau");
    let instance = "0123456789abcdef";
    let mut child =
        initial_ui_stdio_command(&tau_bin, &temp, &config_home, &state_home, &runtime_dir)
            .env("TAU_SESSION_ID", "late-startup-failure")
            .env("TAU_HARNESS_INSTANCE_ID", instance)
            .env(tau_harness::INITIAL_UI_INTRODUCTION_NOTICE_ENV, "1")
            .spawn()
            .expect("spawn harness");
    let metadata = runtime_dir
        .join("tau/harnesses")
        .join(format!("{}-{instance}.json", child.id()));
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline
        && !metadata
            .parent()
            .expect("metadata parent")
            .try_exists()
            .expect("check runtime directory")
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::create_dir(&metadata).expect("block metadata file replacement");

    let mut writer = PeerOutputWriter::new(BufWriter::new(child.stdin.take().expect("stdin")));
    write_initial_ui_handshake(&mut writer);
    let mut reader = PeerInputReader::new(BufReader::new(child.stdout.take().expect("stdout")));
    let mut introductions = 0;
    let disconnect = loop {
        let message = reader
            .read_message()
            .expect("read startup result")
            .expect("startup result");
        match message {
            HarnessOutputMessage::Deliver(delivery) => {
                if matches!(
                    delivery.event(),
                    Event::HarnessNotice(notice)
                        if notice.kind == tau_proto::notice_kind::HARNESS_INTRODUCTION
                ) {
                    introductions += 1;
                }
            }
            HarnessOutputMessage::Disconnect(disconnect) => break disconnect,
            _ => {}
        }
    };
    assert_eq!(introductions, 0);
    assert!(
        disconnect
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("harness startup failed"))
    );
    drop(writer);
    assert!(!child.wait().expect("wait child").success());
}

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
    let mut child =
        initial_ui_stdio_command(&tau_bin, &temp, &config_home, &state_home, &runtime_dir)
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
    std::fs::write(session_dir.join("events.cbor"), b"").expect("write ordinary journal");
    std::fs::write(session_dir.join("restore-events.cbor"), b"").expect("write restore journal");
    std::fs::remove_dir_all(&session_dir).expect("delete selected session before startup lock");

    let tau_bin = std::env::var("CARGO_BIN_EXE_tau").expect("CARGO_BIN_EXE_tau");
    let mut child =
        initial_ui_stdio_command(&tau_bin, &temp, &config_home, &state_home, &runtime_dir)
            .env("TAU_SESSION_ID", "deleted-session")
            .env("TAU_SESSION_STATUS", "resumed")
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

/// A configured resumed harness completes initial-UI startup and leaves its
/// relay target in the existing session without allocating another session.
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
    std::fs::write(session_dir.join("events.cbor"), b"").expect("write ordinary journal");
    std::fs::write(session_dir.join("restore-events.cbor"), b"").expect("write restore journal");

    let tau_bin = std::env::var("CARGO_BIN_EXE_tau").expect("CARGO_BIN_EXE_tau");
    let mut child = InitialUiChild {
        process: initial_ui_stdio_command(&tau_bin, &temp, &config_home, &state_home, &runtime_dir)
            .env("TAU_SESSION_ID", "resumed-session")
            .env("TAU_SESSION_STATUS", "resumed")
            .spawn()
            .expect("spawn resumed tau harness"),
    };
    let expected_session = SessionId::parse("resumed-session").expect("valid expected session id");
    let mut writer =
        PeerOutputWriter::new(BufWriter::new(child.process.stdin.take().expect("stdin")));
    write_initial_ui_handshake(&mut writer);
    let stdout = child.process.stdout.take().expect("stdout");
    let (sender, receiver) = mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let mut reader = PeerInputReader::new(BufReader::new(stdout));
        while let Ok(Some(message)) = reader.read_message() {
            if sender.send(message).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    let startup = loop {
        if Instant::now() >= deadline {
            break Err("resumed harness did not complete startup".to_owned());
        }
        let message = match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(message) => message,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err("resumed harness disconnected before completing startup".to_owned());
            }
        };
        match message {
            HarnessOutputMessage::Deliver(delivery) => {
                if let Event::SessionReplayComplete(replay) = delivery.event() {
                    if replay.session_id == expected_session && replay.error.is_none() {
                        break Ok(());
                    }
                    break Err(format!(
                        "resumed harness completed the wrong or failed replay: {replay:?}"
                    ));
                }
            }
            HarnessOutputMessage::Disconnect(disconnect) => {
                break Err(format!(
                    "resumed harness disconnected before completing startup: {:?}",
                    disconnect.reason
                ));
            }
            _ => {}
        }
    };
    if let Err(error) = startup {
        drop(writer);
        child.stop();
        let _ = reader_thread.join();
        panic!("{error}");
    }

    let harness_log = session_dir.join("logs/tau-harness.log");
    assert!(
        harness_log.is_file(),
        "configured resume must create the parent relay target"
    );
    let mut sessions = std::fs::read_dir(state_home.join("tau/sessions"))
        .expect("read sessions")
        .map(|entry| entry.expect("read session").file_name())
        .collect::<Vec<_>>();
    sessions.sort();
    assert_eq!(sessions, [std::ffi::OsString::from("resumed-session")]);

    let shutdown = writer
        .write_message(&HarnessInputMessage::Disconnect(Disconnect {
            reason: Some("test complete".to_owned()),
        }))
        .map_err(|error| format!("write clean shutdown: {error}"))
        .and_then(|()| {
            writer
                .flush()
                .map_err(|error| format!("flush clean shutdown: {error}"))
        });
    drop(writer);
    let status = match shutdown {
        Ok(()) => child.wait_for_exit(Duration::from_secs(10)),
        Err(error) => {
            child.stop();
            Err(error)
        }
    };
    reader_thread.join().expect("reader thread");
    let status = status.unwrap_or_else(|error| panic!("{error}"));
    assert!(
        status.success(),
        "resumed initial-UI harness must exit cleanly after disconnect"
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
    let mut child =
        initial_ui_stdio_command(&tau_bin, &temp, &config_home, &state_home, &runtime_dir)
            .env("TAU_SESSION_ID", "actual-session")
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
