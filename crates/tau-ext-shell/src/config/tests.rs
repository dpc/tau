use super::*;

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
