//! Per-session configuration for the shell/file extension.

use std::path as path_std_path;

#[cfg(test)]
mod tests;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use globset::{GlobBuilder, GlobMatcher};
use serde::de::Error as _;

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
            user_command_timeout_secs: 60 * 60,
            extra_env: BTreeMap::new(),
            non_interactive_pager: true,
            allowlist: None,
        }
    }
}

/// Preserve the semantic difference between an absent allowlist and every
/// present value, including rejecting explicit null as malformed.
fn deserialize_shell_allowlist<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<ShellAllowRule>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    <Vec<ShellAllowRule> as serde::Deserialize>::deserialize(deserializer).map(Some)
}

/// One conjunctive workdir-and-command allowlist rule.
#[derive(Clone, Debug)]
struct ShellAllowRule {
    /// Authored absolute workdir glob retained for denial diagnostics.
    workdir: String,
    /// Authored raw shell-language command glob retained for denial
    /// diagnostics.
    command: String,
    /// Compiled workdir matcher with component-aware separators.
    workdir_matcher: GlobMatcher,
    /// Compiled command matcher with separators treated as ordinary characters.
    command_matcher: GlobMatcher,
}

impl<'de> serde::Deserialize<'de> for ShellAllowRule {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        /// Strict authored representation of one allowlist rule.
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawRule {
            /// Absolute workdir glob.
            workdir: String,
            /// Raw shell-language command glob.
            command: String,
        }

        let raw = RawRule::deserialize(deserializer)?;
        if !Path::new(&raw.workdir).is_absolute() {
            return Err(D::Error::custom(
                "shell allowlist workdir glob must be absolute",
            ));
        }
        let workdir_matcher = GlobBuilder::new(&raw.workdir)
            .literal_separator(true)
            .backslash_escape(true)
            .build()
            .map_err(|error| {
                D::Error::custom(format!(
                    "invalid shell allowlist workdir glob `{}`: {error}",
                    raw.workdir
                ))
            })?
            .compile_matcher();
        let command_matcher = GlobBuilder::new(&raw.command)
            .literal_separator(false)
            .backslash_escape(true)
            .build()
            .map_err(|error| {
                D::Error::custom(format!(
                    "invalid shell allowlist command glob `{}`: {error}",
                    raw.command
                ))
            })?
            .compile_matcher();
        Ok(Self {
            workdir: raw.workdir,
            command: raw.command,
            workdir_matcher,
            command_matcher,
        })
    }
}

impl ShellConfig {
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
        if rules.iter().any(|rule| {
            rule.workdir_matcher.is_match(&canonical_cwd) && rule.command_matcher.is_match(command)
        }) {
            return Ok(Some(canonical_cwd));
        }
        let mut message = format!(
            "shell command denied by configured allowlist: no rule matched workdir {} and command\nallowed command/workdir glob pairs:",
            canonical_cwd.display()
        );
        if rules.is_empty() {
            message.push_str("\n- none");
        } else {
            for rule in rules {
                let command = serde_json::to_string(&rule.command)
                    .expect("serializing a string to JSON cannot fail");
                let workdir = serde_json::to_string(&rule.workdir)
                    .expect("serializing a string to JSON cannot fail");
                let _ = write!(&mut message, "\n- command: {command}\n  workdir: {workdir}");
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
                let cwd = path_std_path::Path::new(cwd);
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
