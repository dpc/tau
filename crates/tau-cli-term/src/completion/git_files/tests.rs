use std::path::PathBuf;
use std::sync::atomic as path_std_sync_atomic;
use std::time as path_std_time;

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

/// Ensures a failed enumeration is cached briefly, then retries after the
/// negative-cache lifetime without relying on a wall-clock sleep.
#[test]
fn git_repo_files_caches_negative_result_then_retries_after_ttl() {
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

    let mut cache = CACHE.lock().expect("cache lock");
    let cached = cache
        .as_mut()
        .expect("negative result should remain cached");
    cached.cached_at = path_std_time::Instant::now() - NEGATIVE_CACHE_TTL;
    drop(cache);

    assert!(git_repo_files(dir.path()).is_none());
    assert_eq!(
        ENUMERATE_GIT_FILES_CALLS.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "expired negative cache should retry enumeration"
    );
}
