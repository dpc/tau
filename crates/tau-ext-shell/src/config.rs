//! Per-session configuration for the shell/file extension.

mod shell_allowlist;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[cfg(test)]
use shell_allowlist::{
    MAX_SHELL_ALLOWLIST_COMPILE_BYTES, MAX_SHELL_ALLOWLIST_DESCRIPTION_BYTES,
    MAX_SHELL_ALLOWLIST_PATTERN_BYTES, MAX_SHELL_ALLOWLIST_RULES,
};
use shell_allowlist::{ShellAllowRule, deserialize_shell_allowlist};

use crate::isolation::{apply_command_isolation, apply_read_only_cwd_mount};
use crate::shell_process::ShellProcess;

/// Pager variables protected at the shared model/user shell spawn boundary.
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
    /// Maximum wall-clock time a user-initiated `!`/`!!` shell command may run
    /// before it is killed. The `user_command_timeout_secs` configuration field
    /// decodes its integer seconds into this duration. Tool-side shell calls
    /// have their own per-call `timeout` argument; this one bounds the UI path
    /// where the agent isn't driving the timeout.
    #[serde(
        rename = "user_command_timeout_secs",
        deserialize_with = "deserialize_user_command_timeout_secs"
    )]
    pub(crate) user_command_timeout: Duration,
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
    /// Optional best-effort allowlist for shell-style command surfaces.
    ///
    /// Absence preserves unrestricted execution. A present empty list denies
    /// every command.
    #[serde(default, deserialize_with = "deserialize_shell_allowlist")]
    allowlist: Option<Vec<ShellAllowRule>>,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            command: "sh".to_owned(),
            prefix: Vec::new(),
            user_command_timeout: Duration::from_secs(60 * 60),
            extra_env: BTreeMap::new(),
            non_interactive_pager: true,
            allowlist: None,
        }
    }
}

/// Decode the integer-second user shell timeout configuration into its internal
/// duration.
fn deserialize_user_command_timeout_secs<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(deserializer).map(Duration::from_secs)
}

impl ShellConfig {
    /// Returns the model-visible description of the effective allowlist when
    /// command enforcement is enabled.
    ///
    /// The rendered selector set deliberately preserves matcher types while
    /// sorting and de-duplicating authored rules. Enforcement retains its
    /// authored rule vector unchanged.
    pub(crate) fn allowlist_prompt_fragment(&self) -> Option<String> {
        let rules = self.allowlist.as_ref()?;
        let selectors = rules
            .iter()
            .map(ShellAllowRule::prompt_selector)
            .collect::<BTreeSet<_>>();
        let mut fragment = String::from(
            "\n\n### Shell command allowlist\n\n\
             Shell command enforcement is enabled. A raw shell command and its \
             canonical effective workdir must both match one selector pair:",
        );
        if selectors.is_empty() {
            fragment.push_str("\n- none (all shell commands are denied)");
        } else {
            for selector in selectors {
                let _ = write!(fragment, "\n- {selector}");
            }
        }
        Some(fragment)
    }

    /// Authorize one submitted shell string in its effective cwd.
    ///
    /// Returns `None` without touching the filesystem when no allowlist is
    /// configured, preserving unrestricted execution behavior. With an
    /// allowlist, returns the canonical cwd that was actually matched.
    pub(crate) fn authorize(&self, command: &str, cwd: &Path) -> Result<Option<PathBuf>, String> {
        let Some(rules) = &self.allowlist else {
            return Ok(None);
        };
        let canonical_cwd = cwd.canonicalize().map_err(|error| {
            format!(
                "failed to resolve shell command workdir {}: {error}",
                cwd.display()
            )
        })?;
        if !canonical_cwd.is_dir() {
            return Err(format!(
                "shell command workdir is not a directory: {}",
                canonical_cwd.display()
            ));
        }
        if canonical_cwd.to_str().is_none() {
            return Err(
                "shell command workdir is not valid UTF-8 and cannot be matched losslessly"
                    .to_owned(),
            );
        }
        let canonical_cwd_text = canonical_cwd
            .to_str()
            .expect("UTF-8 workdirs returned after explicit validation");
        if rules
            .iter()
            .any(|rule| rule.matches(canonical_cwd_text, command))
        {
            return Ok(Some(canonical_cwd));
        }
        let mut message = format!(
            "shell command denied by configured allowlist: no rule matched workdir {} and command\nallowed command/workdir rule pairs:",
            canonical_cwd.display()
        );
        if rules.is_empty() {
            message.push_str("\n- none");
        } else {
            for rule in rules {
                rule.append_diagnostic(&mut message);
            }
        }
        Err(message)
    }

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
        cwd: Option<&Path>,
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
