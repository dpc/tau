use std::cell::RefCell;
use std::collections::BTreeMap;
use std::error::Error as _;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::mpsc;
use std::time::Duration;

use tau_config::settings::{TauRuntimeSocketAccess, TauStateAccess};

use super::*;
use crate::event::{ComponentIngress as PathComponentIngress, ComponentIngressCapacity};

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
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .expect("extension context must retain the underlying OS error");
    assert_eq!(os_error.kind(), std::io::ErrorKind::NotFound);
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
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .expect("custom spawn failure must retain its OS source");
    assert!(diagnostic.ends_with(&os_error.to_string()));
}
