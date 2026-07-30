use std::path::PathBuf;
use std::sync::atomic as path_std_sync_atomic;
use std::{process as path_std_process, time as path_std_time};

use super::*;

/// Ensures fuzzy git completion ranks a matching file path ahead of unrelated
/// files so prompt completion stays useful in larger repositories.
#[test]
fn fuzzy_match_git_files_ranks_path_matches() {
    let files = vec![
        "crates/tau-cli-term/src/completion.rs".to_owned(),
        "README.md".to_owned(),
        "crates/tau/src/main.rs".to_owned(),
    ];

    let matches = fuzzy_match_git_files("completion", &files);

    assert_eq!(
        matches.first(),
        Some(&"crates/tau-cli-term/src/completion.rs")
    );
}

/// Protects the replacement paths shown for `./` fuzzy completion: local files
/// keep a friendly `./` prefix while repository files outside the current
/// directory remain reachable through relative parent paths.
#[test]
fn dotslash_display_path_keeps_local_prefix_and_allows_parent_paths() {
    let root = PathBuf::from("/repo");
    let cwd = root.join("crates/tau-cli-term");

    assert_eq!(
        dotslash_display_path("crates/tau-cli-term/src/lib.rs", &root, &cwd),
        "./src/lib.rs"
    );
    assert_eq!(
        dotslash_display_path("Cargo.toml", &root, &cwd),
        "../../Cargo.toml"
    );
}

/// Ensures over-limit git output kills and reaps the child promptly so a noisy
/// repository command cannot wedge prompt completion.
#[test]
fn git_bounded_stdout_kills_child_on_overflow() {
    let mut command = path_std_process::Command::new("sh");
    command
        .arg("-c")
        .arg("yes git-overflow")
        .stdout(path_std_process::Stdio::piped())
        .stderr(path_std_process::Stdio::null());

    let start = path_std_time::Instant::now();
    let error = crate::run_with_bounded_stdout(
        &mut command,
        None,
        1024,
        path_std_time::Duration::from_secs(5),
        crate::ProcessOwnership::ProcessGroup,
    )
    .expect_err("over-limit stdout should fail");

    assert!(error.contains("stdout exceeded"));
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
}

/// Ensures failed git enumeration is cached for the current directory so an
/// over-limit or non-repository result does not rerun git on every keystroke.
#[test]
fn git_repo_files_caches_negative_result_for_cwd() {
    let dir = tempfile::tempdir().expect("tempdir");
    if let Ok(mut cache) = CACHE.lock() {
        *cache = None;
    }
    ENUMERATE_GIT_FILES_CALLS.store(0, path_std_sync_atomic::Ordering::SeqCst);

    assert!(git_repo_files(dir.path()).is_none());

    let cache = CACHE.lock().expect("cache lock");
    let cached = cache.as_ref().expect("negative result should be cached");
    assert_eq!(cached.cwd, dir.path());
    assert!(cached.result.is_none());
    drop(cache);

    assert!(git_repo_files(dir.path()).is_none());
    assert_eq!(
        ENUMERATE_GIT_FILES_CALLS.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "second same-cwd failure should use the negative cache"
    );
}
