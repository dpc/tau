//! Per-session configuration for the shell/file extension.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use crate::isolation::{apply_command_isolation, apply_read_only_cwd_mount};
use crate::shell_process::ShellProcess;

/// Pager variables protected at the shared model/user shell spawn boundary.
///
/// Governed by
/// `DECISION-tau-ext-shell-non-interactive-pager-environment`.
const NON_INTERACTIVE_PAGER_ENV: [(&str, &str); 5] = [
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GH_PAGER", "cat"),
    ("JJ_PAGER", "cat"),
    ("SYSTEMD_PAGER", "cat"),
];

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ExtConfig {
    /// Current working directory the extension switches to after receiving its
    /// startup configuration. After configuration it becomes the frozen
    /// missing-key fallback for per-agent instance workdirs.
    pub(crate) working_directory: Option<PathBuf>,
    pub(crate) shell: ShellConfig,
    pub(crate) dir_lock: DirLockConfig,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct DirLockConfig {
    /// Controls the agent-visible `dir_lock` tool and whether mutating
    /// ext-shell tools participate in directory update locking. Disabled by
    /// default; set to true to opt in.
    pub(crate) enable: bool,
    /// Backend used to store directory lock state.
    pub(crate) backend: DirLockBackendConfig,
    /// Optional filesystem backend state directory. When omitted, ext-shell
    /// uses a private directory below `$XDG_RUNTIME_DIR` or a verified
    /// private temp fallback.
    pub(crate) state_dir: Option<PathBuf>,
    /// Enforce inferred read-only shell mode by bind-mounting the tool working
    /// directory read-only inside the child namespace when supported by the
    /// tool.
    ///
    /// This only applies when directory locking is enabled. Without directory
    /// locking, shell tools run as read-write commands and no read-only bind is
    /// attempted.
    pub(crate) enforce_ro_bind: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DirLockBackendConfig {
    /// Process-local lock state, preserving the historical behavior.
    #[default]
    Memory,
    /// Host/user-local shared registry coordinated with filesystem locks.
    Filesystem,
}

impl Default for DirLockConfig {
    fn default() -> Self {
        Self {
            enable: false,
            backend: DirLockBackendConfig::Memory,
            state_dir: None,
            enforce_ro_bind: true,
        }
    }
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ShellConfig {
    /// Executable used for `shell` tool invocations and `!`/`!!` UI
    /// commands. It is invoked as `<command> -c <user command>`.
    command: String,
    /// argv prefix prepended before the shell command. The effective
    /// argv is `prefix ++ [command, "-c", user_command]`.
    prefix: Vec<String>,
    /// Maximum wall-clock seconds a user-initiated `!`/`!!` shell
    /// command may run before it is killed. Tool-side shell calls
    /// have their own per-call `timeout` argument; this one bounds
    /// the UI path where the agent isn't driving the timeout.
    pub(crate) user_command_timeout_secs: u64,
    /// Extra environment variables injected into shell-tool / `!`
    /// command children, applied after the inherited environment so
    /// they override or supplement it. Keys with an empty value still
    /// clear the variable in the child env. Protected pager variables
    /// override this map unless `non_interactive_pager` is false. Does
    /// not affect the `rg` child used by `grep`.
    extra_env: BTreeMap<String, String>,
    /// Whether Tau overrides common pager variables with `cat` after
    /// `extra_env`. This defaults to true. Setting it to false is the single
    /// explicit opt-out from the protected pager environment.
    non_interactive_pager: bool,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            command: "sh".to_owned(),
            prefix: Vec::new(),
            user_command_timeout_secs: 60 * 60,
            extra_env: BTreeMap::new(),
            non_interactive_pager: true,
        }
    }
}

impl ShellConfig {
    fn command_for(&self, command: &str) -> Command {
        let mut argv = self.prefix.clone();
        argv.push(self.command.clone());
        let Some((program, args)) = argv.split_first() else {
            // `command` default is non-empty, and serde default prevents
            // this for missing config. An explicit empty string is still
            // a bad config; let spawn fail with a useful OS error.
            return Command::new("");
        };
        let mut child_cmd = Command::new(program);
        child_cmd.args(args).arg("-c").arg(command);
        child_cmd
    }

    /// Applies ordinary configured environment followed by the protected pager
    /// overlay, unless the user explicitly opted out.
    fn apply_environment(&self, child_cmd: &mut Command) {
        for (key, value) in &self.extra_env {
            if value.is_empty() {
                child_cmd.env_remove(key);
            } else {
                child_cmd.env(key, value);
            }
        }
        if self.non_interactive_pager {
            child_cmd.envs(NON_INTERACTIVE_PAGER_ENV);
        }
    }

    /// Single spawn point for shell-style child processes: builds the
    /// configured shell invocation, attaches platform shell endpoints, applies
    /// command isolation, and optionally sets a working directory.
    /// Used by both the agent-facing `shell` tool and the user-facing
    /// `!`/`!!` path so they can't silently diverge on isolation.
    pub(crate) fn spawn_isolated(
        &self,
        command: &str,
        cwd: Option<&str>,
        read_only_cwd: bool,
        enforce_ro_bind: bool,
    ) -> std::io::Result<ShellProcess> {
        let mut child_cmd = self.command_for(command);
        if let Some(cwd) = cwd {
            child_cmd.current_dir(cwd);
        }
        apply_command_isolation(&mut child_cmd);
        let read_only_warning = if read_only_cwd && enforce_ro_bind {
            let mount_cwd = cwd.map_or_else(std::env::current_dir, |cwd| {
                let cwd = std::path::Path::new(cwd);
                if cwd.is_absolute() {
                    Ok(cwd.to_path_buf())
                } else {
                    std::env::current_dir().map(|current| current.join(cwd))
                }
            })?;
            apply_read_only_cwd_mount(&mut child_cmd, &mount_cwd)?
        } else {
            None
        };
        self.apply_environment(&mut child_cmd);
        let child = ShellProcess::spawn(&mut child_cmd);
        if let Some(read_only_warning) = read_only_warning {
            read_only_warning.log_after_spawn();
        }
        child
    }
}

#[cfg(test)]
mod tests {
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
}
