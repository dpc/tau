use super::*;

/// Ensures diagnostic setup fails before spawn, preventing an unowned daemon
/// when its stderr destination cannot be opened.
#[test]
fn diagnostic_creation_precedes_daemon_spawn() {
    let root = tempfile::TempDir::new().expect("test-owned temporary root");
    let stderr_path = root.path().join("daemon.stderr");
    std::fs::create_dir(&stderr_path).expect("directory stderr destination");
    let mut command = Command::new(root.path().join("absent-daemon"));

    let error = create_diagnostic_then_spawn(&mut command, &stderr_path)
        .expect_err("directory diagnostic destination must fail before absent executable");

    assert_eq!(error.raw_os_error(), Some(nix::libc::EISDIR));
}
