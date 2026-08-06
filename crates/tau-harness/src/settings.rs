//! Loading and resolving harness/extension configuration on startup.
//!
//! Owns the resolved-configuration types ([`Config`], [`CoreConfig`],
//! [`CoreMode`], [`ExtensionConfig`]), the built-in extension list, and
//! the resolver that merges the user's
//! [`tau_config::settings::HarnessSettings`] on top of the built-ins. The wire
//! schema for `harness.yaml` lives in `tau-config`; this module turns that
//! schema into something the harness can spawn.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Duration;
use std::{fmt, sync as path_std_sync};

use tau_config::settings as path_tau_config_settings;
use tau_config::settings::{
    BuiltinComponentIdentity, ExtensionCliOverride, ExtensionEntry, ExtensionSecretEntry,
    HarnessConfigCliOverride, HarnessSettings, ProfileName, RoleCliOverride, SettingsError,
};

const TEST_DUMMY_EXTENSION_NAME: &str = "test-dummy";

/// The resolved harness configuration handed to the daemon.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    /// Core harness runtime settings.
    pub core: CoreConfig,
    /// Enabled extensions that should be spawned unless skipped later by
    /// secrets.
    pub extensions: BTreeMap<String, ExtensionConfig>,
    /// Mandatory startup diagnostics for optional extensions skipped during
    /// configuration or for every non-hidden Tau-state policy.
    pub extension_startup_diagnostics: Vec<ExtensionStartupDiagnostic>,
}

/// Replayable startup diagnostic for an extension startup decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionStartupDiagnostic {
    /// Extension config key that the diagnostic is about.
    pub extension: String,
    /// User-visible explanation safe to publish as mandatory `harness.notice`.
    pub message: String,
    /// Typed decision that selects the notice kind and presentation.
    pub kind: ExtensionStartupDiagnosticKind,
}

/// Reason the harness publishes one extension startup diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtensionStartupDiagnosticKind {
    /// An optional extension could not start and was skipped.
    OptionalSkip,
    /// A non-default Tau-state recovery policy is active for an extension.
    StateAccess {
        /// Configuration layer that selected the effective policy.
        source: TauStateAccessSource,
    },
}

/// Configuration layer that selected an extension's Tau-state policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TauStateAccessSource {
    /// The global `tau_state_access` setting selected the policy.
    GlobalConfiguration,
    /// The extension's `tau_state_access` setting selected the policy.
    InstanceConfiguration,
    /// The emergency environment force selected the policy for every extension.
    EnvironmentForce,
}

impl TauStateAccessSource {
    /// Return the stable user-facing description of this configuration layer.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::GlobalConfiguration => "global harness configuration",
            Self::InstanceConfiguration => "extension configuration",
            Self::EnvironmentForce => "process-wide TAU_EXTENSION_TAU_STATE_ACCESS",
        }
    }
}

/// Resolved core configuration values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoreConfig {
    pub mode: CoreMode,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            mode: CoreMode::Embedded,
        }
    }
}

/// Minimal runtime mode selection for the harness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoreMode {
    Embedded,
    Daemon,
}

/// One configured extension process, after merging built-in defaults
/// and user overrides. Ready to spawn.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub role: Option<String>,
    /// Resolved Tau-owned component authority, absent for external commands.
    pub component: Option<BuiltinComponentIdentity>,
    /// Immutable structural tool prefix assigned to this instance.
    pub tool_prefix: Option<tau_proto::ToolNamePrefix>,
    /// Whether harness startup requires this extension to initialize.
    pub require: bool,
    /// Maximum time from successful supervised spawn until the extension sends
    /// its initial `Ready`.
    pub startup_timeout: Duration,
    /// Current working directory used when starting the extension process. When
    /// absent, the child inherits the harness process working directory.
    pub cwd: Option<PathBuf>,
    /// Config object handed to the extension via
    /// `LifecycleConfigure`. Defaults to an empty object so
    /// extensions always see a value.
    pub config: serde_json::Value,
    /// Secret declarations authorized for this extension.
    pub secrets: BTreeMap<String, ExtensionSecretEntry>,
    /// Effective Tau-state presentation for this supervised extension.
    pub tau_state_access: path_tau_config_settings::TauStateAccess,
}

/// Built-in extension shipped with `tau`. Used by
/// [`resolve_extensions`] to seed the table before applying user
/// overrides. argv = `prefix ++ command ++ suffix`.
pub struct BuiltinExtension {
    pub name: String,
    pub prefix: Vec<String>,
    pub command: Vec<String>,
    pub suffix: Vec<String>,
    pub role: Option<String>,
    /// Built-in default current working directory for this extension.
    pub cwd: Option<PathBuf>,
    pub enable: bool,
    /// Whether this built-in must initialize when enabled.
    pub require: bool,
    /// Maximum time from successful supervised spawn until this built-in sends
    /// its initial `Ready`.
    pub startup_timeout: Duration,
    /// Built-in default config for this extension, merged below any
    /// user-provided `config: { … }` object in `harness.yaml`.
    pub config: serde_json::Value,
    /// Built-in secret declarations for this extension.
    pub secrets: BTreeMap<String, ExtensionSecretEntry>,
}

/// Error returned by [`resolve_extensions`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolveExtensionsError {
    /// A required enabled extension has no valid command-slot executable.
    /// Optional entries with the same invalid command slot are omitted with
    /// startup diagnostics instead. An argv wrapper prefix cannot satisfy this
    /// requirement.
    EmptyCommand(String),
    /// An extension startup deadline is outside Tau's bounded configuration
    /// range.
    InvalidStartupTimeout {
        /// Configured extension identity.
        name: String,
        /// Configured timeout in seconds.
        seconds: u64,
    },
    /// A CLI override named an extension absent from built-ins and user config.
    UnknownCliOverride(String),
    /// The public environment override named an unavailable extension.
    UnknownEnvironmentOverride(String),
}

impl fmt::Display for ResolveExtensionsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCommand(name) => write!(
                f,
                "required extension {name:?} has an empty `extensions.{name}.command`; set it to an executable, omit it and set a non-empty `extensions.{name}.suffix` to run a Tau subcommand, or disable the extension",
            ),
            Self::InvalidStartupTimeout { name, seconds } => write!(
                f,
                "extension `{name}` has invalid `extensions.{name}.startup_timeout_seconds` value {seconds}; expected an integer from 1 through 3600"
            ),
            Self::UnknownCliOverride(name) => {
                write!(f, "unknown extension in CLI override: `{name}`")
            }
            Self::UnknownEnvironmentOverride(name) => write!(
                f,
                "unknown extension in {}: `{name}`",
                tau_config::settings::TAU_ENABLE_EXTENSIONS_ENV
            ),
        }
    }
}

impl std::error::Error for ResolveExtensionsError {}

#[derive(Debug)]
struct ResolvedExtension {
    prefix: Vec<String>,
    /// Presence-aware command slot. `None` permits current-Tau piggybacking
    /// when `suffix` is nonempty; `Some([])` is an explicitly invalid empty
    /// command.
    command: Option<Vec<String>>,
    suffix: Vec<String>,
    enable: bool,
    require: bool,
    startup_timeout: Duration,
    role: Option<String>,
    /// Tau-owned component authority retained only while the resolved
    /// command provenance remains built-in/current-Tau and its suffix
    /// identifies that exact component. Explicit command replacement clears
    /// it.
    component: Option<BuiltinComponentIdentity>,
    tool_prefix: Option<tau_proto::ToolNamePrefix>,
    cwd: Option<PathBuf>,
    config: serde_json::Value,
    secrets: BTreeMap<String, ExtensionSecretEntry>,
    /// Effective policy after global and instance configuration resolution.
    tau_state_access: path_tau_config_settings::TauStateAccess,
}

/// Merge user-provided `extensions` entries on top of the supplied
/// built-in extensions and produce a flat list of [`ExtensionConfig`]s ready
/// for the harness to spawn.
/// Per-key merging:
/// - Field-level overlay for built-in keys: only fields the user explicitly set
///   (`Some(_)` after deserialization) replace the built-in's value. Absent
///   fields keep the built-in's defaults.
/// - User keys not in the built-in list are added as-is. Their `enable` and
///   `require` fields both default to `true`.
/// - Entries with a resolved `enable: false` are dropped before command
///   validation, secret resolution, and spawn.
/// - A nonempty explicit command is preserved. An omitted command with a
///   nonempty suffix uses the current Tau executable; an explicit empty command
///   or omitted command with an empty suffix is invalid. `prefix` wraps a valid
///   command but cannot replace it.
/// - Enabled required entries with invalid command slots are fatal. Enabled
///   optional entries with invalid command slots are omitted and reported
///   through diagnostics by
///   [`resolve_extensions_with_cli_overrides_and_diagnostics`].
///
/// Returns `Err` for enabled required entries that end up without a valid
/// command-slot executable after the merge. Disabled user-added entries are
/// inert and are
/// dropped before command validation. This wrapper discards diagnostics for
/// optional skipped entries; startup code should call
/// [`resolve_extensions_with_cli_overrides_and_diagnostics`] when those
/// diagnostics must be surfaced to users.
pub fn resolve_extensions(
    settings: &HarnessSettings,
    builtins: Vec<BuiltinExtension>,
) -> Result<Vec<ExtensionConfig>, ResolveExtensionsError> {
    resolve_extensions_with_cli_overrides(settings, builtins, &[])
}

pub fn resolve_extensions_with_cli_overrides(
    settings: &HarnessSettings,
    builtins: Vec<BuiltinExtension>,
    cli_overrides: &[ExtensionCliOverride],
) -> Result<Vec<ExtensionConfig>, ResolveExtensionsError> {
    Ok(
        resolve_extensions_with_cli_overrides_and_diagnostics(settings, builtins, cli_overrides)?
            .extensions,
    )
}

/// Resolved extension list with optional-extension startup diagnostics.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResolvedExtensions {
    /// Enabled extensions to spawn.
    pub extensions: Vec<ExtensionConfig>,
    /// Mandatory warning diagnostics for optional entries skipped during
    /// resolution.
    pub diagnostics: Vec<ExtensionStartupDiagnostic>,
}

/// Resolve extensions like [`resolve_extensions_with_cli_overrides`], while
/// also returning mandatory warning startup diagnostics for optional entries
/// skipped during resolution. Harness startup must use this variant so
/// diagnostics can be published and replayed instead of silently discarded.
pub fn resolve_extensions_with_cli_overrides_and_diagnostics(
    settings: &HarnessSettings,
    builtins: Vec<BuiltinExtension>,
    cli_overrides: &[ExtensionCliOverride],
) -> Result<ResolvedExtensions, ResolveExtensionsError> {
    resolve_extensions_with_environment_and_cli_overrides(settings, builtins, &[], cli_overrides)
}

fn resolve_extensions_with_environment_and_cli_overrides(
    settings: &HarnessSettings,
    builtins: Vec<BuiltinExtension>,
    environment_names: &[String],
    cli_overrides: &[ExtensionCliOverride],
) -> Result<ResolvedExtensions, ResolveExtensionsError> {
    // Keep the config → environment → CLI ordering aligned with
    // `SPEC-tau-harness-extension-lifecycle`.
    let (order, entries) = seed_builtin_extension_entries(builtins, settings.tau_state_access);
    let (order, mut entries) = apply_user_extension_entries(settings, order, entries);
    for (name, entry) in &settings.extensions {
        if entry.tau_state_access.is_none()
            && let Some(resolved) = entries.get_mut(name)
        {
            resolved.tau_state_access = settings.tau_state_access;
        }
    }
    for name in environment_names {
        let entry = entries
            .get_mut(name)
            .ok_or_else(|| ResolveExtensionsError::UnknownEnvironmentOverride(name.clone()))?;
        entry.enable = true;
    }
    let entries = apply_extension_cli_overrides(entries, cli_overrides)?;
    resolved_extension_entries(order, entries)
}

fn seed_builtin_extension_entries(
    builtins: Vec<BuiltinExtension>,
    tau_state_access: path_tau_config_settings::TauStateAccess,
) -> (Vec<String>, HashMap<String, ResolvedExtension>) {
    let order: Vec<String> = builtins.iter().map(|b| b.name.clone()).collect();
    let entries = builtins
        .into_iter()
        .map(|b| {
            let name = b.name.clone();
            (name, ResolvedExtension::from_builtin(b, tau_state_access))
        })
        .collect();

    (order, entries)
}

impl ResolvedExtension {
    fn from_builtin(
        builtin: BuiltinExtension,
        tau_state_access: path_tau_config_settings::TauStateAccess,
    ) -> Self {
        let component = BuiltinComponentIdentity::from_tau_owned_suffix(&builtin.suffix);
        Self {
            prefix: builtin.prefix,
            command: Some(builtin.command),
            suffix: builtin.suffix,
            enable: builtin.enable,
            require: builtin.require,
            startup_timeout: builtin.startup_timeout,
            role: builtin.role,
            component,
            tool_prefix: None,
            cwd: builtin.cwd,
            config: builtin.config,
            secrets: builtin.secrets,
            tau_state_access,
        }
    }

    fn from_user_entry(user: &ExtensionEntry) -> Self {
        Self {
            prefix: user.prefix.clone().unwrap_or_default(),
            command: user.command.clone(),
            suffix: user.suffix.clone().unwrap_or_default(),
            enable: user.enable.unwrap_or(true),
            require: user.require.unwrap_or(true),
            startup_timeout: Duration::from_secs(
                user.startup_timeout_seconds
                    .unwrap_or(DEFAULT_EXTENSION_STARTUP_TIMEOUT_SECONDS),
            ),
            role: user.role.clone(),
            component: user
                .command
                .is_none()
                .then(|| {
                    BuiltinComponentIdentity::from_tau_owned_suffix(
                        user.suffix.as_deref().unwrap_or_default(),
                    )
                })
                .flatten(),
            tool_prefix: user.tool_prefix.clone().flatten(),
            cwd: user.cwd.clone().flatten(),
            config: user
                .config
                .clone()
                .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new())),
            secrets: user.secrets.clone().unwrap_or_default(),
            tau_state_access: user.tau_state_access.unwrap_or_default(),
        }
    }

    fn apply_user_entry(&mut self, user: &ExtensionEntry) {
        if let Some(prefix) = user.prefix.as_ref() {
            self.prefix = prefix.clone();
        }
        if let Some(command) = user.command.as_ref() {
            self.command = Some(command.clone());
            self.component = None;
            // Setting `command` replaces the built-in's full argv tail.
            // `suffix` is cleared so users overriding only `command`
            // don't accidentally inherit the built-in's subcommand
            // tokens (e.g. `["component", "ext-provider-builtin"]`). Users
            // who want to keep them must set `suffix` explicitly below.
            self.suffix = Vec::new();
        }
        if let Some(suffix) = user.suffix.as_ref() {
            self.component = (self.component.is_some() || self.command.is_none())
                .then(|| BuiltinComponentIdentity::from_tau_owned_suffix(suffix))
                .flatten();
            self.suffix = suffix.clone();
        }
        if let Some(enable) = user.enable {
            self.enable = enable;
        }
        if let Some(require) = user.require {
            self.require = require;
        }
        if let Some(seconds) = user.startup_timeout_seconds {
            self.startup_timeout = Duration::from_secs(seconds);
        }
        if let Some(role) = user.role.as_ref() {
            self.role = Some(role.clone());
        }
        if let Some(tool_prefix) = user.tool_prefix.as_ref() {
            self.tool_prefix.clone_from(tool_prefix);
        }
        if let Some(cwd) = user.cwd.as_ref() {
            self.cwd = cwd.clone();
        }
        if let Some(over) = user.config.clone() {
            self.config = merge_json(self.config.take(), over);
        }
        if let Some(secrets) = user.secrets.as_ref() {
            self.secrets.extend(secrets.clone());
        }
        if let Some(tau_state_access) = user.tau_state_access {
            self.tau_state_access = tau_state_access;
        }
    }

    fn into_enabled_extension_config(
        self,
        name: String,
    ) -> Result<Option<ExtensionConfig>, ResolveExtensionsError> {
        let timeout_seconds = self.startup_timeout.as_secs();
        if timeout_seconds == 0 || 3_600 < timeout_seconds {
            return Err(ResolveExtensionsError::InvalidStartupTimeout {
                name,
                seconds: timeout_seconds,
            });
        }
        let command = match self.command {
            Some(command) if !command.is_empty() => command,
            None if !self.suffix.is_empty() => vec![current_tau_executable()],
            Some(_) | None => {
                if self.require {
                    return Err(ResolveExtensionsError::EmptyCommand(name));
                }
                return Ok(None);
            }
        };
        let mut argv = self.prefix;
        argv.extend(command);
        argv.extend(self.suffix);
        let (program, args) =
            split_extension_argv(argv).expect("validated command makes argv non-empty");

        Ok(Some(ExtensionConfig {
            name,
            command: program,
            args,
            role: self.role,
            component: self.component,
            tool_prefix: self.tool_prefix,
            require: self.require,
            startup_timeout: self.startup_timeout,
            cwd: self.cwd,
            config: self.config,
            secrets: self.secrets,
            tau_state_access: self.tau_state_access,
        }))
    }
}

fn apply_user_extension_entries(
    settings: &HarnessSettings,
    mut order: Vec<String>,
    mut entries: HashMap<String, ResolvedExtension>,
) -> (Vec<String>, HashMap<String, ResolvedExtension>) {
    let mut user_keys: Vec<&String> = settings.extensions.keys().collect();
    user_keys.sort();
    for name in user_keys {
        let user: &ExtensionEntry = &settings.extensions[name];
        match entries.get_mut(name) {
            Some(existing) => {
                existing.apply_user_entry(user);
            }
            None => {
                order.push(name.clone());
                entries.insert(name.clone(), ResolvedExtension::from_user_entry(user));
            }
        }
    }

    (order, entries)
}

fn apply_extension_cli_overrides(
    mut entries: HashMap<String, ResolvedExtension>,
    cli_overrides: &[ExtensionCliOverride],
) -> Result<HashMap<String, ResolvedExtension>, ResolveExtensionsError> {
    for override_ in cli_overrides {
        match override_ {
            ExtensionCliOverride::Enable(extension_name) => {
                let entry = entries.get_mut(extension_name).ok_or_else(|| {
                    ResolveExtensionsError::UnknownCliOverride(extension_name.clone())
                })?;
                entry.enable = true;
            }
            ExtensionCliOverride::Disable(extension_name) => {
                let entry = entries.get_mut(extension_name).ok_or_else(|| {
                    ResolveExtensionsError::UnknownCliOverride(extension_name.clone())
                })?;
                entry.enable = false;
            }
            ExtensionCliOverride::EnableAll => {
                for (name, entry) in entries.iter_mut() {
                    if name == TEST_DUMMY_EXTENSION_NAME {
                        continue;
                    }
                    entry.enable = true;
                }
            }
            ExtensionCliOverride::DisableAll => {
                for entry in entries.values_mut() {
                    entry.enable = false;
                }
            }
        }
    }

    Ok(entries)
}

fn resolved_extension_entries(
    order: Vec<String>,
    mut entries: HashMap<String, ResolvedExtension>,
) -> Result<ResolvedExtensions, ResolveExtensionsError> {
    let mut extensions = Vec::new();
    let mut diagnostics = Vec::new();
    for name in order {
        let entry = entries.remove(&name).expect("seeded above");
        if !entry.enable {
            continue;
        }
        match entry.into_enabled_extension_config(name.clone())? {
            Some(extension) => extensions.push(extension),
            None => push_optional_empty_command_diagnostic(name, &mut diagnostics),
        }
    }
    Ok(ResolvedExtensions {
        extensions,
        diagnostics,
    })
}

fn split_extension_argv(argv: Vec<String>) -> Option<(String, Vec<String>)> {
    let (program, args) = argv.split_first()?;
    Some((program.clone(), args.to_vec()))
}

fn current_tau_executable() -> String {
    std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "tau".to_owned())
}

fn push_optional_empty_command_diagnostic(
    name: String,
    diagnostics: &mut Vec<ExtensionStartupDiagnostic>,
) {
    tracing::warn!(
        target: "tau_harness::startup",
        extension = %name,
        "optional extension did not initialize: resolved command is empty"
    );
    diagnostics.push(ExtensionStartupDiagnostic {
        extension: name.clone(),
        message: format!("optional extension {name} did not initialize"),
        kind: ExtensionStartupDiagnosticKind::OptionalSkip,
    });
}

/// Merge an arbitrary extension-owned JSON value from lower to higher
/// precedence.
///
/// Objects merge recursively. Arrays, scalars, nested nulls, and type
/// mismatches replace the lower-precedence value. Top-level `config: null`
/// remains represented as an absent [`ExtensionEntry::config`] before this
/// function is called, preserving its existing no-op behavior.
fn merge_json(base: serde_json::Value, over: serde_json::Value) -> serde_json::Value {
    match (base, over) {
        (serde_json::Value::Object(mut b), serde_json::Value::Object(o)) => {
            for (k, v) in o {
                if let Some(base_value) = b.remove(&k) {
                    b.insert(k, merge_json(base_value, v));
                } else {
                    b.insert(k, v);
                }
            }
            serde_json::Value::Object(b)
        }
        (_, over) => over,
    }
}

/// Load `harness.yaml`, falling back to defaults on parse error and
/// writing a warning to stderr. Returns the parse error too so the
/// harness can surface it in the UI without re-parsing the same file
/// from scratch.
///
/// Without the warning a malformed file silently disables every
/// user-configured extension and the only symptom is "my extension
/// isn't running" with no clue why.
pub const ROLE_CLI_OVERRIDES_ENV: &str = "TAU_ROLE_CLI_OVERRIDES";
pub const EXTENSION_CLI_OVERRIDES_ENV: &str = "TAU_EXTENSION_CLI_OVERRIDES";
pub const HARNESS_CONFIG_CLI_OVERRIDES_ENV: &str = "TAU_HARNESS_CONFIG_OVERRIDES";
pub const STARTUP_ROLE_ENV: &str = "TAU_STARTUP_ROLE";

pub(crate) fn load_harness_settings_or_warn(
    dirs: &tau_config::settings::TauDirs,
) -> (HarnessSettings, Option<tau_config::settings::SettingsError>) {
    let role_overrides = role_cli_overrides_from_env();
    let harness_config_overrides = harness_config_overrides_from_env().unwrap_or_default();
    let profile = match tau_config::settings::selected_profile_in(dirs, None) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("tau: harness.yaml failed to parse — ignored.\n{error}");
            return (
                apply_startup_role_override(HarnessSettings::built_in()),
                Some(error),
            );
        }
    };
    load_harness_settings_with_overrides_or_warn(
        dirs,
        profile.as_ref(),
        &role_overrides,
        &harness_config_overrides,
        true,
    )
}

/// Load harness settings without consulting any startup environment transport.
///
/// This preserves config-file loading and warning behavior for hermetic daemon
/// fixtures while excluding role, harness-config, and startup-role overrides.
/// The optional error is returned alongside built-in fallback settings exactly
/// like [`load_harness_settings_or_warn`].
pub(crate) fn load_harness_settings_without_environment_or_warn(
    dirs: &tau_config::settings::TauDirs,
) -> (HarnessSettings, Option<tau_config::settings::SettingsError>) {
    let profile = match tau_config::settings::default_profile_in(dirs) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("tau: harness.yaml failed to parse — ignored.\n{error}");
            return (HarnessSettings::built_in(), Some(error));
        }
    };
    load_harness_settings_with_overrides_or_warn(dirs, profile.as_ref(), &[], &[], false)
}

fn load_harness_settings_with_overrides_or_warn(
    dirs: &tau_config::settings::TauDirs,
    profile: Option<&ProfileName>,
    role_overrides: &[RoleCliOverride],
    harness_config_overrides: &[HarnessConfigCliOverride],
    apply_startup_environment: bool,
) -> (HarnessSettings, Option<tau_config::settings::SettingsError>) {
    let result = (|| {
        if let Some(profile) = profile {
            validate_profile_extension_targets(dirs, profile)?;
        }
        tau_config::settings::load_harness_settings_with_profile_and_cli_overrides_in(
            dirs,
            profile,
            role_overrides,
            harness_config_overrides,
        )
    })();
    match result {
        Ok(settings) => (
            if apply_startup_environment {
                apply_startup_role_override(settings)
            } else {
                settings
            },
            None,
        ),
        Err(error) => {
            eprintln!("tau: harness.yaml failed to parse — ignored.\n{error}");
            (
                if apply_startup_environment {
                    apply_startup_role_override(HarnessSettings::built_in())
                } else {
                    HarnessSettings::built_in()
                },
                Some(error),
            )
        }
    }
}

/// Rejects profile extension typos before their enablement patches enter
/// config.
fn validate_profile_extension_targets(
    dirs: &tau_config::settings::TauDirs,
    profile: &ProfileName,
) -> Result<(), tau_config::settings::SettingsError> {
    let base = tau_config::settings::load_harness_settings_with_profile_and_cli_overrides_in(
        dirs,
        None,
        &[],
        &[],
    )?;
    let mut known_names = base
        .extensions
        .into_keys()
        .collect::<std::collections::HashSet<_>>();
    known_names.extend(
        builtin_extensions()
            .into_iter()
            .map(|extension| extension.name),
    );
    for extension in tau_config::settings::profile_extension_names_in(dirs, profile)? {
        if !known_names.contains(&extension) {
            return Err(SettingsError::UnknownProfileExtension {
                profile: profile.clone(),
                extension,
            });
        }
    }
    Ok(())
}

fn apply_startup_role_override(mut settings: HarnessSettings) -> HarnessSettings {
    if let Ok(role) = std::env::var(STARTUP_ROLE_ENV)
        && !role.is_empty()
    {
        settings.default_role = Some(role);
    }
    settings
}

fn role_cli_overrides_from_env() -> Vec<RoleCliOverride> {
    std::env::var(ROLE_CLI_OVERRIDES_ENV)
        .ok()
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or_default()
}

fn harness_config_overrides_from_env() -> Result<Vec<HarnessConfigCliOverride>, serde_json::Error> {
    std::env::var(HARNESS_CONFIG_CLI_OVERRIDES_ENV)
        .ok()
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .map(|overrides| overrides.unwrap_or_default())
}

pub(crate) fn parse_extension_cli_overrides_transport(
    value: Option<std::ffi::OsString>,
) -> Result<Vec<ExtensionCliOverride>, Box<dyn std::error::Error>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let value = value.into_string().map_err(|_| {
        format!("{EXTENSION_CLI_OVERRIDES_ENV} internal transport must be valid UTF-8 JSON")
    })?;
    serde_json::from_str(&value).map_err(|error| {
        format!("malformed {EXTENSION_CLI_OVERRIDES_ENV} internal transport: {error}").into()
    })
}

/// The set of extensions the harness ships with by default.
///
/// Each entry's `command` is `[<current-exe>]` and `suffix` is
/// `["component", <name>]`, so a fresh `tau` install with no
/// `harness.yaml` runs the in-binary provider and tool extensions out
/// of the box. Users can override individual fields
/// (or set `enable: false`) per entry in `harness.yaml` under
/// `extensions: { name: { … } }`.
///
/// The list itself lives in `config/built-in.extensions.json5` and is
/// embedded into the binary via `include_str!`; `built_in_extension_defs`
/// performs the parse step.
#[must_use]
pub fn builtin_extensions() -> Vec<BuiltinExtension> {
    let tau_binary = current_tau_executable();

    built_in_extension_defs()
        .iter()
        .map(|def| BuiltinExtension {
            name: def.name.clone(),
            prefix: def.prefix.clone().unwrap_or_default(),
            command: def
                .command
                .clone()
                .unwrap_or_else(|| vec![tau_binary.clone()]),
            suffix: def.suffix.clone().unwrap_or_default(),
            role: def.role.clone(),
            cwd: def.cwd.clone(),
            enable: def.enable,
            require: def.require,
            startup_timeout: Duration::from_secs(def.startup_timeout_seconds),
            config: def.config.clone(),
            secrets: def.secrets.clone().unwrap_or_default(),
        })
        .collect()
}

const BUILT_IN_EXTENSIONS_JSON5: &str = include_str!("../config/built-in.extensions.json5");

/// Wire schema for one entry in `built-in.extensions.json5`. `command`
/// is optional — when omitted, [`builtin_extensions`] substitutes
/// `[<current-exe>]` so the built-in runs the tau binary itself.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BuiltInExtensionDef {
    pub name: String,
    #[serde(default)]
    pub prefix: Option<Vec<String>>,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub suffix: Option<Vec<String>>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    pub enable: bool,
    #[serde(default = "default_true")]
    pub require: bool,
    /// Initial readiness deadline in seconds, defaulting to Tau's general
    /// extension startup budget.
    #[serde(default = "default_extension_startup_timeout_seconds")]
    pub startup_timeout_seconds: u64,
    pub config: serde_json::Value,
    #[serde(default)]
    pub secrets: Option<BTreeMap<String, ExtensionSecretEntry>>,
}

fn default_true() -> bool {
    true
}

/// Default initial readiness deadline for ordinary extensions.
pub const DEFAULT_EXTENSION_STARTUP_TIMEOUT_SECONDS: u64 = 2;

fn default_extension_startup_timeout_seconds() -> u64 {
    DEFAULT_EXTENSION_STARTUP_TIMEOUT_SECONDS
}

pub(crate) fn built_in_extension_defs() -> &'static [BuiltInExtensionDef] {
    static B: std::sync::LazyLock<Vec<BuiltInExtensionDef>> = path_std_sync::LazyLock::new(|| {
        json5::from_str(BUILT_IN_EXTENSIONS_JSON5).unwrap_or_else(|err| {
            panic!(
                "tau ships with malformed built-in.extensions.json5: {err}\n\
                 this is a bug; please report it"
            )
        })
    });
    &B
}

#[must_use]
pub fn default_config() -> Config {
    // `resolve_extensions` is fallible only for enabled required entries with
    // invalid command slots. The built-in settings have no user entries or
    // overrides, and the hard-coded `builtin_extensions()` list resolves to
    // non-empty commands, so the failure path is unreachable.
    let extensions = match resolve_extensions(&HarnessSettings::built_in(), builtin_extensions()) {
        Ok(extensions) => extensions,
        Err(err) => unreachable!("built-in extensions resolve cleanly: {err}"),
    };

    Config {
        core: CoreConfig {
            mode: CoreMode::Embedded,
        },
        extensions: extensions
            .into_iter()
            .map(|extension| (extension.name.clone(), extension))
            .collect(),
        extension_startup_diagnostics: Vec::new(),
    }
}

pub fn validate_cli_overrides(
    role_overrides: &[RoleCliOverride],
    extension_overrides: &[ExtensionCliOverride],
    harness_config_overrides: &[HarnessConfigCliOverride],
) -> Result<(), Box<dyn std::error::Error>> {
    let profile = tau_config::settings::selected_profile(None)?;
    validate_cli_overrides_with_profile(
        profile.as_ref(),
        role_overrides,
        extension_overrides,
        harness_config_overrides,
    )
}

/// Validates CLI overrides against one explicitly selected configuration
/// profile.
pub fn validate_cli_overrides_with_profile(
    profile: Option<&ProfileName>,
    role_overrides: &[RoleCliOverride],
    extension_overrides: &[ExtensionCliOverride],
    harness_config_overrides: &[HarnessConfigCliOverride],
) -> Result<(), Box<dyn std::error::Error>> {
    let dirs = path_tau_config_settings::TauDirs::default();
    let settings = load_settings_for_cli_overrides_in(
        &dirs,
        profile,
        role_overrides,
        harness_config_overrides,
    )?;
    resolve_extensions_with_cli_overrides(&settings, builtin_extensions(), extension_overrides)?;
    Ok(())
}

/// Validates public environment extension enables followed by ordered CLI
/// extension overrides against the effective configured extension table.
pub fn validate_extension_environment_and_cli_overrides(
    environment_names: &[String],
    cli_overrides: &[ExtensionCliOverride],
    role_overrides: &[RoleCliOverride],
    harness_config_overrides: &[HarnessConfigCliOverride],
) -> Result<(), Box<dyn std::error::Error>> {
    let profile = tau_config::settings::selected_profile(None)?;
    validate_extension_environment_and_cli_overrides_with_profile(
        profile.as_ref(),
        environment_names,
        cli_overrides,
        role_overrides,
        harness_config_overrides,
    )
}

/// Validates extension environment and CLI overrides for an explicit profile.
pub fn validate_extension_environment_and_cli_overrides_with_profile(
    profile: Option<&ProfileName>,
    environment_names: &[String],
    cli_overrides: &[ExtensionCliOverride],
    role_overrides: &[RoleCliOverride],
    harness_config_overrides: &[HarnessConfigCliOverride],
) -> Result<(), Box<dyn std::error::Error>> {
    let dirs = path_tau_config_settings::TauDirs::default();
    let settings = load_settings_for_cli_overrides_in(
        &dirs,
        profile,
        role_overrides,
        harness_config_overrides,
    )?;
    resolve_extensions_with_environment_and_cli_overrides(
        &settings,
        builtin_extensions(),
        environment_names,
        cli_overrides,
    )?;
    Ok(())
}

fn load_settings_for_cli_overrides_in(
    dirs: &tau_config::settings::TauDirs,
    profile: Option<&ProfileName>,
    role_overrides: &[RoleCliOverride],
    harness_config_overrides: &[HarnessConfigCliOverride],
) -> Result<HarnessSettings, Box<dyn std::error::Error>> {
    if let Some(profile) = profile {
        validate_profile_extension_targets(dirs, profile)?;
    }
    match tau_config::settings::load_harness_settings_with_profile_and_cli_overrides_in(
        dirs,
        profile,
        role_overrides,
        harness_config_overrides,
    ) {
        Ok(settings) => Ok(apply_startup_role_override(settings)),
        Err(
            error @ (path_tau_config_settings::SettingsError::UnknownRoleCliOverride(_)
            | path_tau_config_settings::SettingsError::UnknownProfile(_)),
        ) => Err(Box::new(error)),
        Err(error) => {
            if !harness_config_overrides.is_empty() {
                eprintln!("tau: harness.yaml failed to parse — ignored.\n{error}");
                let fallback_dirs = tau_config::settings::TauDirs {
                    config_dir: None,
                    state_dir: dirs.state_dir.clone(),
                };
                return tau_config::settings::load_harness_settings_with_profile_and_cli_overrides_in(
                    &fallback_dirs,
                    profile,
                    role_overrides,
                    harness_config_overrides,
                )
                .map(apply_startup_role_override)
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error>);
            }
            eprintln!("tau: harness.yaml failed to parse — ignored.\n{error}");
            Ok(apply_startup_role_override(HarnessSettings::built_in()))
        }
    }
}

pub(crate) fn resolve_config(
    _explicit_path: Option<&std::path::Path>,
) -> Result<Config, Box<dyn std::error::Error>> {
    let dirs = path_tau_config_settings::TauDirs::default();
    resolve_config_in(&dirs)
}

pub(crate) fn resolve_config_with_extension_cli_overrides(
    extension_overrides: &[ExtensionCliOverride],
) -> Result<Config, Box<dyn std::error::Error>> {
    let dirs = path_tau_config_settings::TauDirs::default();
    resolve_config_in_with_extension_cli_overrides(&dirs, extension_overrides)
}

pub(crate) fn resolve_config_in(
    dirs: &tau_config::settings::TauDirs,
) -> Result<Config, Box<dyn std::error::Error>> {
    resolve_config_in_with_extension_cli_overrides(dirs, &[])
}

/// Resolve one explicit directory layout without process-environment startup
/// transports. Deterministic embedded and daemon callers use this to prevent
/// ambient CLI compatibility variables from altering generated configuration.
pub(crate) fn resolve_config_in_without_environment(
    dirs: &tau_config::settings::TauDirs,
) -> Result<Config, Box<dyn std::error::Error>> {
    let settings = tau_config::settings::load_harness_settings_in(dirs)?;
    let resolved_extensions = resolve_extensions_with_environment_and_cli_overrides(
        &settings,
        builtin_extensions(),
        &[],
        &[],
    )?;
    Ok(config_from_resolved_extensions(resolved_extensions))
}

fn resolve_config_in_with_extension_cli_overrides(
    dirs: &tau_config::settings::TauDirs,
    extension_overrides: &[ExtensionCliOverride],
) -> Result<Config, Box<dyn std::error::Error>> {
    // Extensions live in `harness.yaml` under `extensions: { ... }`.
    // We start from the built-in provider + tools defaults and apply the
    // user's overrides on top; a malformed harness.yaml falls back
    // to defaults rather than failing the whole startup, but we warn
    // on stderr so the user can see why their config is being
    // ignored.
    let role_overrides = role_cli_overrides_from_env();
    let harness_config_overrides = harness_config_overrides_from_env()?;
    let profile = tau_config::settings::selected_profile_in(dirs, None)?;
    let settings = load_settings_for_cli_overrides_in(
        dirs,
        profile.as_ref(),
        &role_overrides,
        &harness_config_overrides,
    )?;
    let environment_names = tau_config::settings::parse_enable_extensions_env(std::env::var_os(
        tau_config::settings::TAU_ENABLE_EXTENSIONS_ENV,
    ))?;
    let resolved_extensions = resolve_extensions_with_environment_and_cli_overrides(
        &settings,
        builtin_extensions(),
        &environment_names,
        extension_overrides,
    )?;
    let mut resolved_extensions = resolved_extensions;
    let tau_state_access_force = tau_config::settings::parse_tau_state_access_env(
        std::env::var_os(tau_config::settings::TAU_EXTENSION_TAU_STATE_ACCESS_ENV),
    )?;
    apply_tau_state_access_force(&mut resolved_extensions, tau_state_access_force);
    let mut config = config_from_resolved_extensions(resolved_extensions);
    append_tau_state_access_diagnostics(&mut config, &settings, tau_state_access_force);
    Ok(config)
}

/// Apply the emergency process-wide state-access override after configuration
/// resolution.
fn apply_tau_state_access_force(
    resolved_extensions: &mut ResolvedExtensions,
    tau_state_access_force: Option<path_tau_config_settings::TauStateAccess>,
) {
    if let Some(tau_state_access) = tau_state_access_force {
        for extension in &mut resolved_extensions.extensions {
            extension.tau_state_access = tau_state_access;
        }
    }
}

/// Append mandatory diagnostics for each Tau-state recovery policy.
fn append_tau_state_access_diagnostics(
    config: &mut Config,
    settings: &HarnessSettings,
    tau_state_access_force: Option<path_tau_config_settings::TauStateAccess>,
) {
    for extension in config.extensions.values().filter(|extension| {
        extension.tau_state_access != path_tau_config_settings::TauStateAccess::Hidden
    }) {
        let source = match tau_state_access_force {
            Some(_) => TauStateAccessSource::EnvironmentForce,
            None if settings
                .extensions
                .get(&extension.name)
                .and_then(|entry| entry.tau_state_access)
                .is_some() =>
            {
                TauStateAccessSource::InstanceConfiguration
            }
            None => TauStateAccessSource::GlobalConfiguration,
        };
        config
            .extension_startup_diagnostics
            .push(ExtensionStartupDiagnostic {
                extension: extension.name.clone(),
                message: format!(
                    "extension `{}` uses Tau-state access `{}` from {}",
                    extension.name,
                    extension.tau_state_access,
                    source.description()
                ),
                kind: ExtensionStartupDiagnosticKind::StateAccess { source },
            });
    }
}

fn config_from_resolved_extensions(resolved_extensions: ResolvedExtensions) -> Config {
    Config {
        core: CoreConfig {
            mode: CoreMode::Embedded,
        },
        extensions: resolved_extensions
            .extensions
            .into_iter()
            .map(|extension| (extension.name.clone(), extension))
            .collect(),
        extension_startup_diagnostics: resolved_extensions
            .diagnostics
            .into_iter()
            .map(|diagnostic| ExtensionStartupDiagnostic {
                extension: diagnostic.extension,
                message: diagnostic.message,
                kind: ExtensionStartupDiagnosticKind::OptionalSkip,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests;
