use super::*;

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
