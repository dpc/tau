use tempfile::TempDir;

use super::*;

/// Ensures omission preserves unrestricted behavior without requiring the
/// supplied cwd to exist.
#[test]
fn absent_allowlist_does_not_touch_or_restrict_the_workdir() {
    let config = ShellConfig::default();
    assert_eq!(
        config
            .authorize("anything", Path::new("/definitely/missing/tau-cwd"))
            .expect("absent allowlist is unrestricted"),
        None
    );
}

/// Ensures command and workdir globs match as one conjunctive rule rather than
/// combining halves from different entries.
#[test]
fn allowlist_rules_bind_command_and_workdir_as_pairs() {
    let temp = TempDir::new().expect("tempdir");
    let other = TempDir::new().expect("other tempdir");
    let config: ShellConfig = serde_json::from_value(serde_json::json!({
        "allowlist": [
            {
                "workdir": temp.path().display().to_string(),
                "command": "cargo *"
            },
            {
                "workdir": other.path().display().to_string(),
                "command": "jj status"
            }
        ]
    }))
    .expect("parse allowlist");

    assert!(
        config
            .authorize("cargo test", temp.path())
            .expect("matching pair")
            .is_some()
    );
    let error = config
        .authorize("jj status", temp.path())
        .expect_err("split pair must not authorize");
    assert!(error.contains("allowed command/workdir glob pairs:"));
    assert!(error.contains(r#"command: "cargo *""#));
    assert!(error.contains(&format!(r#"workdir: "{}""#, temp.path().display())));
}

/// Ensures command matching is anchored over the raw string while treating
/// separators and newlines as ordinary characters.
#[test]
fn command_globs_match_the_whole_raw_multiline_string() {
    let temp = TempDir::new().expect("tempdir");
    let config: ShellConfig = serde_json::from_value(serde_json::json!({
        "allowlist": [{
            "workdir": temp.path().display().to_string(),
            "command": "printf *"
        }]
    }))
    .expect("parse allowlist");

    assert!(
        config
            .authorize("printf a/b\nprintf second", temp.path())
            .expect("star spans separators and newline")
            .is_some()
    );
    assert!(
        config
            .authorize("x printf value", temp.path())
            .expect_err("glob is whole-string anchored")
            .contains("denied by configured allowlist")
    );
    assert!(
        config
            .authorize("Printf value", temp.path())
            .expect_err("matching is case-sensitive")
            .contains("denied by configured allowlist")
    );
}

/// Ensures workdir stars remain component-aware while double stars cross
/// multiple canonical path components.
#[test]
fn workdir_globs_distinguish_single_and_recursive_stars() {
    let temp = TempDir::new().expect("tempdir");
    let nested = temp.path().join("one/two");
    std::fs::create_dir_all(&nested).expect("create nested cwd");
    let single: ShellConfig = serde_json::from_value(serde_json::json!({
        "allowlist": [{
            "workdir": format!("{}/*", temp.path().display()),
            "command": "*"
        }]
    }))
    .expect("single-star config");
    let recursive: ShellConfig = serde_json::from_value(serde_json::json!({
        "allowlist": [{
            "workdir": format!("{}/**", temp.path().display()),
            "command": "*"
        }]
    }))
    .expect("recursive config");

    assert!(
        single
            .authorize("true", &nested)
            .expect_err("single star cannot cross components")
            .contains("denied")
    );
    assert!(
        recursive
            .authorize("true", &nested)
            .expect("double star crosses components")
            .is_some()
    );
}

/// Ensures an explicitly empty allowlist denies every command and discloses
/// that no paired rules are configured.
#[test]
fn empty_allowlist_denies_all_with_stable_diagnostic() {
    let temp = TempDir::new().expect("tempdir");
    let config: ShellConfig =
        serde_json::from_value(serde_json::json!({ "allowlist": [] })).expect("parse allowlist");
    let error = config
        .authorize("echo denied", temp.path())
        .expect_err("empty allowlist denies all");
    assert_eq!(
        error,
        format!(
            "shell command denied by configured allowlist: no rule matched workdir {} and command\nallowed command/workdir glob pairs:\n- none",
            temp.path().canonicalize().expect("canonical").display()
        )
    );
}

/// Ensures malformed, incomplete, and relative allowlist rules fail during
/// configuration instead of silently widening execution.
#[test]
fn malformed_allowlist_rules_fail_configuration() {
    for (value, expected) in [
        (serde_json::json!({ "allowlist": null }), "sequence"),
        (
            serde_json::json!({ "allowlist": [{ "workdir": "/tmp/**" }] }),
            "command",
        ),
        (
            serde_json::json!({ "allowlist": [{ "workdir": "relative/**", "command": "*" }] }),
            "must be absolute",
        ),
        (
            serde_json::json!({ "allowlist": [{ "workdir": "/tmp/[", "command": "*" }] }),
            "workdir glob",
        ),
        (
            serde_json::json!({ "allowlist": [{ "workdir": "/tmp/**", "command": "[" }] }),
            "command glob",
        ),
        (
            serde_json::json!({ "allowlist": [{ "workdir": "/tmp/**", "command": "*", "extra": true }] }),
            "unknown field",
        ),
    ] {
        let error = serde_json::from_value::<ShellConfig>(value).expect_err("malformed rule");
        assert!(error.to_string().contains(expected), "{error}");
    }
}

/// Ensures empty extra_env values implement the documented clear-variable
/// semantics instead of passing an empty string through to the child.
#[test]
fn empty_extra_env_removes_child_variable() {
    let mut extra_env = BTreeMap::new();
    extra_env.insert("HOME".to_owned(), String::new());
    let config = ShellConfig {
        extra_env,
        ..Default::default()
    };

    let output = config
        .command_for("printf \"${HOME+set}\"")
        .env_remove("HOME")
        .output()
        .expect("spawn shell");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");

    let output = config
        .spawn_isolated("printf \"${HOME+set}\"", None, false, false)
        .expect("spawn isolated shell")
        .child
        .wait_with_output()
        .expect("wait shell");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
}

/// Ensures the protected overlay wins over both inherited and ordinary
/// configured values while preserving TERM and unrelated pager variables.
#[test]
fn non_interactive_pager_overlay_has_final_precedence_and_narrow_scope() {
    let config: ShellConfig = serde_json::from_value(serde_json::json!({
        "extra_env": {
            "PAGER": "configured-pager",
            "GIT_PAGER": "configured-git-pager",
            "GH_PAGER": "configured-gh-pager",
            "SYSTEMD_PAGER": "configured-systemd-pager",
            "TERM": "tau-term",
            "JJ_PAGER": "configured-jj-pager",
            "MANPAGER": "configured-man-pager",
            "BAT_PAGER": "configured-bat-pager"
        }
    }))
    .expect("parse shell config");
    let mut command = config.command_for(
        "printf '%s\\n' \"$PAGER\" \"$GIT_PAGER\" \"$GH_PAGER\" \
         \"$SYSTEMD_PAGER\" \"$TERM\" \"$JJ_PAGER\" \"$MANPAGER\" \"$BAT_PAGER\"",
    );
    command
        .env("PAGER", "inherited-pager")
        .env("GIT_PAGER", "inherited-git-pager");
    config.apply_environment(&mut command);

    let output = command.output().expect("run environment probe");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "cat\ncat\ncat\ncat\ntau-term\ncat\nconfigured-man-pager\nconfigured-bat-pager\n"
    );
}

/// Ensures the full shared preparation sequence preserves an inherited TERM
/// even when ordinary shell configuration does not mention TERM.
#[test]
fn shell_isolation_preserves_inherited_term_by_default() {
    let config = ShellConfig::default();
    let mut command = config.command_for("printf '%s' \"$TERM\"");
    command.env("TERM", "inherited-test-term");
    apply_command_isolation(&mut command);
    config.apply_environment(&mut command);

    let output = command.output().expect("run inherited TERM probe");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "inherited-test-term"
    );
}

/// Ensures the documented opt-out is explicit and leaves ordinary
/// `extra_env` pager and TERM choices intact.
#[test]
fn non_interactive_pager_opt_out_preserves_configured_environment() {
    let config: ShellConfig = serde_json::from_value(serde_json::json!({
        "non_interactive_pager": false,
        "extra_env": {
            "PAGER": "custom-pager",
            "GIT_PAGER": "custom-git-pager",
            "TERM": "custom-term"
        }
    }))
    .expect("parse shell config");
    let mut command = config.command_for("printf '%s\\n' \"$PAGER\" \"$GIT_PAGER\" \"$TERM\"");
    config.apply_environment(&mut command);

    let output = command.output().expect("run opt-out probe");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "custom-pager\ncustom-git-pager\ncustom-term\n"
    );
}

/// Ensures directory-lock backend config keeps memory as the default while
/// accepting the opt-in filesystem backend and state directory.
#[test]
fn dir_lock_backend_config_defaults_memory_and_parses_filesystem() {
    assert_eq!(
        ExtConfig::default().dir_lock.backend,
        DirLockBackendConfig::Memory
    );

    let config: ExtConfig = serde_json::from_value(serde_json::json!({
        "dir_lock": {
            "enable": true,
            "backend": "filesystem",
            "state_dir": "/tmp/tau-dir-locks"
        }
    }))
    .expect("parse dir_lock backend config");

    assert_eq!(config.dir_lock.backend, DirLockBackendConfig::Filesystem);
    assert_eq!(
        config.dir_lock.state_dir,
        Some(PathBuf::from("/tmp/tau-dir-locks"))
    );
}
