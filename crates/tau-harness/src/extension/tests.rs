use std::cell::RefCell;
use std::collections::BTreeMap;
use std::error::Error as _;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use tau_config::settings::{TauRuntimeSocketAccess, TauStateAccess};

use super::*;
use crate::event::{ComponentIngress as PathComponentIngress, ComponentIngressCapacity};
use crate::extension_stderr_mirror::{ExtensionStderrIdentity, ExtensionStderrMirror};

/// A blocked destructor models work that begins after a runner returns but
/// before its Rust thread has actually terminated.
struct BlockingTeardown {
    /// Test-controlled release for the blocked thread teardown.
    release: mpsc::Receiver<()>,
    /// Completion signal sent after the controlled teardown resumes.
    exited: mpsc::Sender<()>,
}

thread_local! {
    /// Teardown that runs after a runner function returned but before its
    /// native thread can complete.
    static BLOCKING_THREAD_LOCAL_TEARDOWN: RefCell<Option<BlockingTeardown>> = const { RefCell::new(None) };
}

#[cfg(unix)]
const RAW_CHILD_SCRUB_TEST: &str =
    "extension::tests::supervised_child_scrub_handles_raw_environment_keys";

/// Proves the supervised-child launch scrub removes exact raw secret keys,
/// ignores unrelated non-Unicode entries, and still launches the child.
#[cfg(unix)]
#[test]
fn supervised_child_scrub_handles_raw_environment_keys() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;
    use std::process::Command;

    match std::env::var("TAU_RAW_CHILD_TEST_ACTION").as_deref() {
        Ok("observer") => {
            assert!(
                !std::env::vars_os().any(|(key, _)| {
                    tau_config::secret_sources::is_secret_environment_key(&key)
                }),
                "supervised child inherited a raw named-secret key"
            );
            assert!(
                std::env::vars_os()
                    .any(|(key, _)| key.as_encoded_bytes().starts_with(b"UNRELATED_")),
                "scrub must leave unrelated raw entries untouched"
            );
        }
        Ok("supervisor") => {
            let mut child = Command::new(std::env::current_exe().expect("current test executable"));
            child
                .args(["--exact", RAW_CHILD_SCRUB_TEST, "--nocapture"])
                .env("TAU_RAW_CHILD_TEST_ACTION", "observer");
            scrub_secret_environment(&mut child);
            let output = child.output().expect("launch scrubbed child observer");
            assert!(
                output.status.success(),
                "scrubbed child observer failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(other) => panic!("unknown raw child test action: {other}"),
        Err(_) => {
            let output = Command::new(std::env::current_exe().expect("current test executable"))
                .args(["--exact", RAW_CHILD_SCRUB_TEST, "--nocapture"])
                .env_clear()
                .env("TAU_RAW_CHILD_TEST_ACTION", "supervisor")
                .env(
                    OsString::from_vec(b"TAU_SECRET_CHILD\xff".to_vec()),
                    "raw-child-secret",
                )
                .env(
                    OsString::from_vec(b"UNRELATED_\xff".to_vec()),
                    OsString::from_vec(b"value\xff".to_vec()),
                )
                .output()
                .expect("launch isolated supervisor");
            assert!(
                output.status.success(),
                "isolated supervisor failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

impl Drop for BlockingTeardown {
    fn drop(&mut self) {
        let _ = self.release.recv();
        let _ = self.exited.send(());
    }
}

/// Releases every controlled teardown even if a test assertion panics.
struct BlockingTeardownRelease {
    /// Senders that unblock every controlled teardown.
    releases: Vec<mpsc::Sender<()>>,
    /// Completion receivers that prove the released threads left TLS teardown.
    exited: Vec<mpsc::Receiver<()>>,
}

impl Drop for BlockingTeardownRelease {
    fn drop(&mut self) {
        for release in &self.releases {
            let _ = release.send(());
        }
        for exited in &self.exited {
            let _ = exited.recv_timeout(Duration::from_secs(1));
        }
    }
}

/// A private-file failure permanently suppresses only that child's mirror while
/// the drain loop continues attempting every later raw chunk.
#[test]
fn raw_sink_failure_disables_only_logger_mirror_and_keeps_draining() {
    struct ChunkReader {
        chunks: std::collections::VecDeque<Vec<u8>>,
    }
    impl io::Read for ChunkReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some(chunk) = self.chunks.pop_front() else {
                return Ok(0);
            };
            buf[..chunk.len()].copy_from_slice(&chunk);
            Ok(chunk.len())
        }
    }
    #[derive(Default)]
    struct FailingRaw {
        writes: Vec<Vec<u8>>,
        flushes: usize,
    }
    impl io::Write for FailingRaw {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes.push(bytes.to_vec());
            if self.writes.len() == 1 {
                Err(io::Error::other("injected raw failure"))
            } else {
                Ok(bytes.len())
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }
    #[derive(Clone)]
    struct MirrorOutput(Arc<Mutex<Vec<u8>>>);
    impl io::Write for MirrorOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("mirror output")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let output = Arc::new(Mutex::new(Vec::new()));
    let mirror = ExtensionStderrMirror::with_writer_and_capacity(MirrorOutput(output.clone()), 8);
    let logger = mirror.logger(ExtensionStderrIdentity::new(
        crate::test_extension_name("raw-failure"),
        0,
        12,
    ));
    let mut reader = ChunkReader {
        chunks: [b"first\n".to_vec(), b"second\n".to_vec()].into(),
    };
    let mut raw = FailingRaw::default();
    drain_extension_stderr(&mut reader, &mut raw, Some(logger));
    drop(mirror);
    assert_eq!(raw.writes, vec![b"first\n".to_vec(), b"second\n".to_vec()]);
    assert_eq!(raw.flushes, 2, "every raw write attempt retains its flush");
    assert!(output.lock().expect("mirror output").is_empty());
}

/// A private-file flush failure disables only that logger's mirror while later
/// raw chunks and their flush attempts continue.
#[test]
fn raw_flush_failure_disables_only_logger_mirror_and_keeps_draining() {
    struct TwoChunks {
        next: usize,
    }
    impl io::Read for TwoChunks {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let chunks: [&[u8]; 2] = [b"first\n", b"second\n"];
            let Some(chunk) = chunks.get(self.next) else {
                return Ok(0);
            };
            self.next += 1;
            buf[..chunk.len()].copy_from_slice(chunk);
            Ok(chunk.len())
        }
    }
    #[derive(Default)]
    struct FlushFailsOnce {
        bytes: Vec<u8>,
        flushes: usize,
    }
    impl io::Write for FlushFailsOnce {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.flushes == 1 {
                Err(io::Error::other("injected raw flush failure"))
            } else {
                Ok(())
            }
        }
    }
    let mirror_output = Arc::new(Mutex::new(Vec::new()));
    #[derive(Clone)]
    struct Output(Arc<Mutex<Vec<u8>>>);
    impl io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("output lock").extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mirror = ExtensionStderrMirror::with_writer_and_capacity(Output(mirror_output.clone()), 8);
    let logger = mirror.logger(ExtensionStderrIdentity::new(
        crate::test_extension_name("raw-flush-failure"),
        0,
        15,
    ));
    let mut reader = TwoChunks { next: 0 };
    let mut raw = FlushFailsOnce::default();
    drain_extension_stderr(&mut reader, &mut raw, Some(logger));
    drop(mirror);
    assert_eq!(raw.bytes, b"first\nsecond\n");
    assert_eq!(raw.flushes, 2);
    assert!(mirror_output.lock().expect("output lock").is_empty());
}

/// A read error is not child EOF and must discard the pending mirror suffix so
/// it cannot masquerade as a complete `boundary=eof` record.
#[test]
fn stderr_read_error_does_not_emit_eof_boundary() {
    struct ErrorAfterBytes {
        emitted: bool,
    }
    impl io::Read for ErrorAfterBytes {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.emitted {
                return Err(io::Error::other("injected read failure"));
            }
            self.emitted = true;
            buf[..7].copy_from_slice(b"partial");
            Ok(7)
        }
    }
    #[derive(Clone)]
    struct MirrorOutput(Arc<Mutex<Vec<u8>>>);
    impl io::Write for MirrorOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("mirror output")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let output = Arc::new(Mutex::new(Vec::new()));
    let mirror = ExtensionStderrMirror::with_writer_and_capacity(MirrorOutput(output.clone()), 8);
    let logger = mirror.logger(ExtensionStderrIdentity::new(
        crate::test_extension_name("read-error"),
        0,
        13,
    ));
    let mut reader = ErrorAfterBytes { emitted: false };
    let mut raw = Vec::new();
    drain_extension_stderr(&mut reader, &mut raw, Some(logger));
    drop(mirror);
    assert_eq!(raw, b"partial");
    assert!(output.lock().expect("mirror output").is_empty());
}

/// A worker blocked in its inherited-stderr sink cannot backpressure the raw
/// drain even after the bounded mirror queue saturates.
#[test]
fn blocked_mirror_sink_does_not_block_raw_stderr_drain() {
    struct BlockingWriter {
        entered: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
        blocked: bool,
    }
    impl io::Write for BlockingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if !self.blocked {
                self.blocked = true;
                self.entered.send(()).expect("announce blocked mirror sink");
                self.release.recv().expect("release mirror sink");
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let mirror = ExtensionStderrMirror::with_writer_and_capacity(
        BlockingWriter {
            entered: entered_tx,
            release: release_rx,
            blocked: false,
        },
        1,
    );
    let identity = ExtensionStderrIdentity::new(crate::test_extension_name("blocked-drain"), 0, 14);
    let mut blocker = mirror.logger(identity.clone());
    blocker.feed(b"occupy-worker\n");
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("mirror worker reached blocked sink");
    let input = b"raw-a\nraw-b\nraw-c\n";
    let mut reader = io::Cursor::new(input);
    let mut raw = Vec::new();
    drain_extension_stderr(&mut reader, &mut raw, Some(mirror.logger(identity)));
    assert_eq!(raw, input, "raw drain stalled or lost bytes at saturation");
    release_tx.send(()).expect("release mirror worker");
}

/// A process-stderr flush failure disables the one shared mirror while two
/// independent authoritative raw sinks continue receiving complete bytes.
#[test]
fn mirror_flush_failure_is_global_but_raw_loggers_continue() {
    struct FlushFailure {
        attempted: mpsc::SyncSender<()>,
        failed: mpsc::Sender<()>,
    }
    impl io::Write for FlushFailure {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.attempted.send(()).expect("announce flush failure");
            Err(io::Error::other("injected mirror flush failure"))
        }
    }
    impl Drop for FlushFailure {
        fn drop(&mut self) {
            let _ = self.failed.send(());
        }
    }
    let (attempted_tx, attempted_rx) = mpsc::sync_channel(0);
    let (failed_tx, failed_rx) = mpsc::channel();
    let mirror = ExtensionStderrMirror::with_writer_and_capacity(
        FlushFailure {
            attempted: attempted_tx,
            failed: failed_tx,
        },
        4,
    );
    let identity =
        |name, pid| ExtensionStderrIdentity::new(crate::test_extension_name(name), 0, pid);
    let first_bytes = b"first raw file\n";
    let mut first_reader = io::Cursor::new(first_bytes);
    let mut first_raw = Vec::new();
    drain_extension_stderr(
        &mut first_reader,
        &mut first_raw,
        Some(mirror.logger(identity("first-raw", 21))),
    );
    attempted_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("mirror worker attempted flush");
    failed_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("mirror worker disabled sink and exited");
    let second_bytes = b"second raw file\n";
    let mut second_reader = io::Cursor::new(second_bytes);
    let mut second_raw = Vec::new();
    drain_extension_stderr(
        &mut second_reader,
        &mut second_raw,
        Some(mirror.logger(identity("second-raw", 22))),
    );
    assert_eq!(first_raw, first_bytes);
    assert_eq!(second_raw, second_bytes);
}

/// Bounded joins must detach threads that remain alive in post-run teardown,
/// and multiple detached handles must consume one shared deadline rather than
/// serializing the grace period.
#[test]
fn in_process_join_detaches_threads_blocked_in_teardown_at_shared_deadline() {
    fn blocked_thread(
        release: mpsc::Receiver<()>,
        exited: mpsc::Sender<()>,
        ready: mpsc::Sender<()>,
    ) -> InProcessThreadHandle {
        let thread = std::thread::spawn(move || {
            BLOCKING_THREAD_LOCAL_TEARDOWN.with(|teardown| {
                *teardown.borrow_mut() = Some(BlockingTeardown { release, exited });
            });
            let _ = ready.send(());
            Ok(())
        });
        InProcessThreadHandle { thread }
    }

    let (first_release_tx, first_release_rx) = mpsc::channel();
    let (first_exited_tx, first_exited_rx) = mpsc::channel();
    let (first_ready_tx, first_ready_rx) = mpsc::channel();
    let (second_release_tx, second_release_rx) = mpsc::channel();
    let (second_exited_tx, second_exited_rx) = mpsc::channel();
    let (second_ready_tx, second_ready_rx) = mpsc::channel();
    let teardown_release = BlockingTeardownRelease {
        releases: vec![first_release_tx, second_release_tx],
        exited: vec![first_exited_rx, second_exited_rx],
    };
    let handles = vec![
        blocked_thread(first_release_rx, first_exited_tx, first_ready_tx),
        blocked_thread(second_release_rx, second_exited_tx, second_ready_tx),
    ];
    first_ready_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("first runner returned");
    second_ready_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("second runner returned");

    let deadline = Instant::now() + Duration::from_millis(20);
    let (outcomes_tx, outcomes_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let outcomes = handles
            .into_iter()
            .map(|handle| {
                handle
                    .start_join()
                    .expect("start detached join reaper")
                    .wait_until(deadline)
            })
            .collect::<Vec<_>>();
        let _ = outcomes_tx.send(outcomes);
    });
    let outcomes = match outcomes_rx.recv_timeout(Duration::from_secs(1)) {
        Ok(outcomes) => outcomes,
        Err(error) => panic!("bounded join did not return: {error}"),
    };
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome, InProcessJoinOutcome::TimedOut))
    );
    drop(teardown_release);
}

/// Proves the persistent Provider mount source contains the exact retained
/// Configure bytes and is read-only before namespace setup.
#[test]
fn provider_settings_mount_materializes_exact_read_only_snapshot() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("snapshot");
    let files = BTreeMap::from([
        ("a.json".to_owned(), b"{\"a\":1}".to_vec()),
        ("b.json".to_owned(), b"{\"b\":2}".to_vec()),
    ]);

    materialize_provider_settings_snapshot(&root, &files).expect("materialize snapshot");

    for (name, contents) in files {
        let path = root.join(name);
        assert_eq!(std::fs::read(&path).expect("profile"), contents);
        assert_eq!(
            std::fs::metadata(path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o400
        );
    }
    assert_eq!(
        std::fs::metadata(root)
            .expect("root metadata")
            .permissions()
            .mode()
            & 0o777,
        0o500
    );
}

fn test_extension_config(cwd: Option<PathBuf>) -> ExtensionConfig {
    ExtensionConfig {
        tool_prefix: None,
        name: "test-extension".to_owned(),
        command: "tau-test-extension".to_owned(),
        args: vec!["--stdio".to_owned()],
        role: None,
        component: None,
        require: true,
        startup_timeout: Duration::from_secs(2),
        cwd,
        config: serde_json::json!({}),
        secrets: BTreeMap::new(),
        tau_state_access: TauStateAccess::Legacy,
        tau_runtime_socket_access: TauRuntimeSocketAccess::Hidden,
    }
}

#[test]
fn extension_stderr_log_path_rejects_unsafe_extension_names() {
    // Extension names originate in user-authored harness config. The
    // stderr log path is constructed before the Configure handshake, so it
    // must reject traversal and absolute-path names on its own.
    let sessions_dir = Path::new("/tmp/tau-sessions");
    assert_eq!(
        extension_stderr_log_path(sessions_dir, "session-1", "std-email")
            .expect("safe extension name"),
        sessions_dir
            .join("session-1")
            .join("logs")
            .join("std-email.log")
    );

    for name in ["", "../x", "a/b", "/tmp/x", ".", ".."] {
        assert!(
            extension_stderr_log_path(sessions_dir, "session-1", name).is_err(),
            "{name:?} must be rejected before building the log path"
        );
    }
}

#[test]
fn supervised_command_uses_configured_cwd() {
    // The cwd field is resolved from harness.yaml and must affect the OS
    // child process, not the LifecycleConfigure payload sent after spawn.
    let cwd = PathBuf::from("/tmp/tau-extension-cwd");
    let config = test_extension_config(Some(cwd.clone()));

    let (command, _) = supervised_command(
        &config,
        &ClientKind::Tool,
        None,
        Path::new("/tmp/tau-state"),
        false,
        &Default::default(),
    )
    .expect("build supervised command");

    assert_eq!(command.get_current_dir(), Some(cwd.as_path()));
}

/// Proves persistent launch preparation creates a mount root only for
/// providers, preventing ordinary tool extensions from populating the
/// providers tree.
#[test]
fn persistent_launch_prepares_settings_mount_only_for_provider() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = temp.path().join("state");
    std::fs::create_dir(&state).expect("state root");

    assert_eq!(
        prepare_provider_settings_mount(&state, "std-shell", &ClientKind::Tool, false)
            .expect("prepare tool launch"),
        None
    );
    assert!(!state.join("providers").exists());

    let provider_root =
        prepare_provider_settings_mount(&state, "provider-work", &ClientKind::Provider, false)
            .expect("prepare provider launch")
            .expect("provider settings root");
    assert_eq!(provider_root, state.join("providers/provider-work"));
    assert!(provider_root.is_dir());
    assert_eq!(
        std::fs::symlink_metadata(&provider_root)
            .expect("provider settings metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );

    let memory_state = temp.path().join("memory-state");
    std::fs::create_dir(&memory_state).expect("memory-only state root");
    assert_eq!(
        prepare_provider_settings_mount(
            &memory_state,
            "provider-memory",
            &ClientKind::Provider,
            true,
        )
        .expect("prepare memory-only provider launch"),
        None
    );
    assert!(!memory_state.join("providers").exists());
}

/// Ensures a built-in instance's failed executable is actionable without
/// exposing arguments, extension config, or an absent cwd.
#[test]
fn builtin_spawn_failure_is_contextual_and_secret_safe() {
    let mut config = test_extension_config(None);
    config.name = "provider-builtin".to_owned();
    config.command = format!("/tau-test-missing-extension-{}", std::process::id());
    config.args = vec!["--token=argument-secret".to_owned()];
    config.config = serde_json::json!({"token": "config-secret"});

    let (tx, _rx) = mpsc::channel();
    let (_ingress, ingress_tx) =
        PathComponentIngress::new(tx.clone(), ComponentIngressCapacity::One);
    let error = match spawn_supervised(
        &config,
        ClientKind::Provider,
        None,
        &tx,
        &ingress_tx,
        Path::new("/tmp/tau-state"),
        false,
        &Default::default(),
        None,
        0,
    ) {
        Ok(_) => panic!("missing built-in executable must fail"),
        Err(error) => error,
    };
    let diagnostic = error.to_string();

    assert!(diagnostic.contains("extension instance \"provider-builtin\""));
    assert!(diagnostic.contains("`command` executable"));
    assert!(diagnostic.contains(&config.command));
    assert!(!diagnostic.contains("cwd"));
    assert!(!diagnostic.contains("argument-secret"));
    assert!(!diagnostic.contains("config-secret"));
    assert!(
        error.source().is_some(),
        "harness error must retain spawn source"
    );
    let os_error = error
        .source()
        .and_then(|source| source.source())
        .and_then(|source| source.downcast_ref::<io::Error>())
        .expect("extension context must retain the underlying OS error");
    assert_eq!(os_error.kind(), io::ErrorKind::NotFound);
    assert!(diagnostic.ends_with(&os_error.to_string()));
}

/// Ensures a custom instance's invalid configured cwd is included while every
/// unrelated or potentially secret field stays out and long fields are bounded.
#[test]
fn custom_spawn_failure_includes_only_relevant_bounded_context() {
    let current_exe = std::env::current_exe().expect("current test executable");
    let long_name = format!("custom-{}-instance-secret", "x".repeat(300));
    let cwd = PathBuf::from(format!(
        "/tau-test-missing-cwd-{}-{}",
        std::process::id(),
        "y".repeat(300)
    ));
    let mut config = test_extension_config(Some(cwd));
    config.name = long_name;
    config.command = format!("{}{}", current_exe.to_string_lossy(), "z".repeat(300));
    config.args = vec!["argument-secret".to_owned()];
    config.config = serde_json::json!({"secret": "config-secret"});

    let (tx, _rx) = mpsc::channel();
    let (_ingress, ingress_tx) =
        PathComponentIngress::new(tx.clone(), ComponentIngressCapacity::One);
    let error = match spawn_supervised(
        &config,
        ClientKind::Tool,
        None,
        &tx,
        &ingress_tx,
        Path::new("/tmp/tau-state"),
        false,
        &Default::default(),
        None,
        0,
    ) {
        Ok(_) => panic!("missing custom cwd must fail"),
        Err(error) => error,
    };
    let diagnostic = error.to_string();

    assert!(diagnostic.contains("configured cwd"));
    assert!(diagnostic.contains("custom-"));
    assert!(diagnostic.contains("tau-test-missing-cwd"));
    assert!(diagnostic.contains('…'), "long context must be truncated");
    assert!(
        diagnostic.len() < 1_500,
        "spawn diagnostic must remain bounded: {} bytes",
        diagnostic.len()
    );
    assert!(!diagnostic.contains("instance-secret"));
    assert!(!diagnostic.contains("argument-secret"));
    assert!(!diagnostic.contains("config-secret"));
    let os_error = error
        .source()
        .and_then(|source| source.source())
        .and_then(|source| source.downcast_ref::<io::Error>())
        .expect("custom spawn failure must retain its OS source");
    assert!(diagnostic.ends_with(&os_error.to_string()));
}
