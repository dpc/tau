use super::*;

/// Ensures workdir glob compilation retains the shared matcher-size bound
/// even though normal configuration rejects this pattern at the earlier
/// authored-size boundary.
#[test]
fn workdir_glob_compilation_enforces_the_matcher_size_bound() {
    let pattern = "?".repeat(MAX_SHELL_ALLOWLIST_COMPILE_BYTES);
    assert_eq!(
        compile_workdir_glob(&pattern).expect_err("oversized matcher"),
        "shell allowlist workdir glob compilation must not exceed 262144 bytes"
    );
}

/// Ensures command glob compilation retains the shared matcher-size bound
/// even though normal configuration rejects this pattern at the earlier
/// authored-size boundary.
#[test]
fn command_glob_compilation_enforces_the_matcher_size_bound() {
    let pattern = "?".repeat(MAX_SHELL_ALLOWLIST_COMPILE_BYTES);
    assert_eq!(
        compile_command_glob(&pattern).expect_err("oversized matcher"),
        "shell allowlist command glob compilation must not exceed 262144 bytes"
    );
}
