//! Per-session configuration for the shell/file extension.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use crate::isolation::{apply_command_isolation, apply_read_only_cwd_mount};

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ExtConfig {
    /// Current working directory the extension switches to after receiving its
    /// startup configuration. Relative paths used by ext-shell tools are
    /// resolved from this directory unless a per-call cwd overrides them.
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
    /// they override or supplement it. Use this to set a custom
    /// `PAGER` or adjust paths. Keys with an empty value still clear
    /// the variable in the child env. Does not affect the `rg` child
    /// used by `grep`.
    extra_env: BTreeMap<String, String>,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            command: "sh".to_owned(),
            prefix: Vec::new(),
            user_command_timeout_secs: 60 * 60,
            extra_env: BTreeMap::new(),
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

    /// Single spawn point for shell-style child processes: builds the
    /// configured shell invocation, attaches piped stdio, applies
    /// command isolation, and optionally sets a working directory.
    /// Used by both the agent-facing `shell` tool and the user-facing
    /// `!`/`!!` path so they can't silently diverge on isolation.
    pub(crate) fn spawn_isolated(
        &self,
        command: &str,
        cwd: Option<&str>,
        read_only_cwd: bool,
        enforce_ro_bind: bool,
    ) -> std::io::Result<std::process::Child> {
        let mut child_cmd = self.command_for(command);
        child_cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
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
        for (key, value) in &self.extra_env {
            if value.is_empty() {
                child_cmd.env_remove(key);
            } else {
                child_cmd.env(key, value);
            }
        }
        let child = child_cmd.spawn();
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
            .wait_with_output()
            .expect("wait shell");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "");
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
