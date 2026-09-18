#[cfg(unix)]
use std::fs::Permissions;

use super::*;

/// A process-local permissive umask must not make a newly initialized UI
/// directory or diagnostic log accessible to other accounts.
#[cfg(unix)]
#[test]
fn init_creates_private_diagnostics_under_permissive_umask() {
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Command;

    const CHILD_MARKER: &str = "TAU_UI_LOGGING_PERMISSION_CHILD";
    const ROOT_ENV: &str = "TAU_UI_LOGGING_PERMISSION_ROOT";
    const TEST_NAME: &str =
        "ui_logging::tests::init_creates_private_diagnostics_under_permissive_umask";

    if std::env::var_os(CHILD_MARKER).is_some() {
        let root = PathBuf::from(std::env::var_os(ROOT_ENV).expect("child state root"));
        let logging = init(&root).expect("initialize private UI logging");
        let parent_mode = std::fs::metadata(root.join("uis"))
            .expect("UI parent metadata")
            .permissions()
            .mode()
            & 0o777;
        let dir_mode = std::fs::metadata(logging.dir())
            .expect("private UI directory metadata")
            .permissions()
            .mode()
            & 0o777;
        let log_mode = std::fs::metadata(logging.log_path())
            .expect("private UI log metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(parent_mode, 0o777);
        assert_eq!(dir_mode, 0o700);
        assert_eq!(log_mode, 0o600);
        return;
    }

    let root = tempfile::tempdir().expect("temporary state root");
    std::fs::set_permissions(root.path(), Permissions::from_mode(0o777))
        .expect("make state root traversable");
    let test_executable = std::env::current_exe().expect("current test executable");
    let status = Command::new("sh")
        .args([
            "-c",
            "umask 000; exec \"$1\" --exact \"$2\" --nocapture",
            "sh",
        ])
        .arg(test_executable)
        .arg(TEST_NAME)
        .env(CHILD_MARKER, "1")
        .env(ROOT_ENV, root.path())
        .status()
        .expect("run isolated permissive-umask test");
    assert!(status.success(), "permission child failed: {status}");
}

/// An existing per-UI leaf must be rejected without changing its permissions
/// or adopting its contents.
#[cfg(unix)]
#[test]
fn private_ui_directory_creation_rejects_existing_leaf() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().expect("temporary state root");
    let existing = root.path().join("uis/ui-collision");
    std::fs::create_dir_all(&existing).expect("existing UI directory");
    std::fs::set_permissions(&existing, Permissions::from_mode(0o755))
        .expect("set existing directory mode");
    std::fs::write(existing.join("marker"), "preserve").expect("existing marker");

    let error = create_private_ui_dir(root.path(), "ui-collision")
        .expect_err("existing UI directory must be rejected");

    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::metadata(&existing)
            .expect("existing directory metadata")
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        std::fs::read_to_string(existing.join("marker")).expect("existing marker"),
        "preserve"
    );
}

/// An existing UI log must be rejected without changing its permissions or
/// truncating its diagnostic bytes.
#[cfg(unix)]
#[test]
fn private_ui_log_creation_rejects_existing_file() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().expect("temporary UI directory");
    let log_path = root.path().join("ui.log");
    std::fs::write(&log_path, "preserve\n").expect("existing UI log");
    std::fs::set_permissions(&log_path, Permissions::from_mode(0o644))
        .expect("set existing log mode");

    let error = create_private_ui_log(&log_path).expect_err("existing UI log must be rejected");

    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::metadata(&log_path)
            .expect("existing log metadata")
            .permissions()
            .mode()
            & 0o777,
        0o644
    );
    assert_eq!(
        std::fs::read_to_string(&log_path).expect("existing log contents"),
        "preserve\n"
    );
}

/// A disabled tracing filter must not suppress the mandatory bounded
/// foreground-restoration evidence written directly to the private UI log.
#[test]
fn restoration_evidence_bypasses_disabled_trace_filter() {
    let dir = tempfile::tempdir().expect("temporary UI directory");
    let log_path = dir.path().join("ui.log");
    std::fs::write(&log_path, "# tau ui log\n").expect("seed UI log");
    let diagnostic_file = File::options()
        .append(true)
        .open(&log_path)
        .expect("open diagnostic writer");
    let logging = UiLogging {
        ui_id: "ui-test".to_owned(),
        dir: dir.path().to_owned(),
        log_path: log_path.clone(),
        diagnostic_writer: Some(SharedUiLogWriter::new(diagnostic_file)),
    };
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("off"))
        .with_writer(io::sink)
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        logging.write_foreground_restoration_failure(
            tau_cli_term::ForegroundRestorationDiagnostic::tcsetpgrp_unconfirmed(libc::ENOTTY),
        );
    });

    let log = std::fs::read_to_string(log_path).expect("read UI log");
    assert!(log.contains(
        "terminal_foreground_restoration_failure restoration_class=tcsetpgrp-unconfirmed"
    ));
    assert!(log.contains(&format!("restoration_errno={}", libc::ENOTTY)));
}

/// A restoration failure without a syscall errno uses the fixed bounded
/// `restoration_errno=none` representation.
#[test]
fn restoration_evidence_records_absent_errno_as_none() {
    let dir = tempfile::tempdir().expect("temporary UI directory");
    let log_path = dir.path().join("ui.log");
    std::fs::write(&log_path, "# tau ui log\n").expect("seed UI log");
    let diagnostic_file = File::options()
        .append(true)
        .open(&log_path)
        .expect("open diagnostic writer");
    let logging = UiLogging {
        ui_id: "ui-test".to_owned(),
        dir: dir.path().to_owned(),
        log_path: log_path.clone(),
        diagnostic_writer: Some(SharedUiLogWriter::new(diagnostic_file)),
    };

    logging.write_foreground_restoration_fields("initial-foreground-mismatch", None);

    let log = std::fs::read_to_string(log_path).expect("read UI log");
    assert!(log.contains(
        "terminal_foreground_restoration_failure restoration_class=initial-foreground-mismatch restoration_errno=none"
    ));
}

/// A later normal trace write must append after, rather than overwrite, the
/// mandatory restoration record written through the shared diagnostic path.
#[test]
fn trace_after_restoration_evidence_preserves_both_complete_lines() {
    let dir = tempfile::tempdir().expect("temporary UI directory");
    let log_path = dir.path().join("ui.log");
    std::fs::write(&log_path, "# tau ui log\n").expect("seed UI log");
    let log_file = File::options()
        .append(true)
        .open(&log_path)
        .expect("open shared log writer");
    let log_writer = SharedUiLogWriter::new(log_file);
    let logging = UiLogging {
        ui_id: "ui-test".to_owned(),
        dir: dir.path().to_owned(),
        log_path: log_path.clone(),
        diagnostic_writer: Some(log_writer.clone()),
    };
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .without_time()
        .with_ansi(false)
        .with_writer(log_writer)
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        logging.write_foreground_restoration_failure(
            tau_cli_term::ForegroundRestorationDiagnostic::tcsetpgrp_unconfirmed(libc::EPERM),
        );
        tracing::info!(target: "tau_cli::ui", reason = "foreground-ownership-unconfirmed", "terminal UI exiting");
    });

    let log = std::fs::read_to_string(log_path).expect("read UI log");
    let lines = log.lines().collect::<Vec<_>>();
    assert!(lines.iter().any(|line| {
        *line
            == format!(
                "terminal_foreground_restoration_failure restoration_class=tcsetpgrp-unconfirmed restoration_errno={}",
                libc::EPERM
            )
    }));
    assert!(lines.iter().any(|line| {
        line.contains("terminal UI exiting")
            && line.contains("reason=\"foreground-ownership-unconfirmed\"")
    }));
}
