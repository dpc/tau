//! Standard email extension policy, state, and IMAP/SMTP backend core.
//!
//! Policy and approval checks stay in the synchronous command engine so fake
//! tests and the real network backend exercise the same redaction and
//! no-partial-send behavior.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::rc as path_std_rc;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::DateTime;
use globset::{Glob, GlobMatcher};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::google_oauth::{
    is_valid_pkce_verifier, parse_installed_app_redirect_url, validate_loopback_redirect_uri,
};
use crate::storage::{FsStorage, SharedStorage, StorageCreateError, file_name};

mod real_backend;
use real_backend::RealEmailBackend;

const READ_BODY_MAX_BYTES: usize = 64 * 1024;
const READ_BODY_MAX_LINES: usize = 1000;
const LIST_MAX_LIMIT: usize = 100;
const DEFAULT_LIST_LIMIT: u32 = LIST_MAX_LIMIT as u32;
const DEFAULT_RECENT_DAYS: u32 = 7;
const MAX_RECENT_DAYS: u32 = 365;
const DEFAULT_FOLDER: &str = "INBOX";
const MAX_DISPLAY_LINE_CHARS: usize = 256;
const MAX_HEADER_VALUE_CHARS: usize = 512;
const MAX_ADDRESS_CHARS: usize = 320;
const MAX_ATTACHMENT_NAME_CHARS: usize = 256;
const MAX_FLAGS: usize = 32;
const MAX_RECIPIENTS: usize = 256;
const MAX_BACKEND_ERROR_CHARS: usize = 512;
const UNAPPROVED_SUBJECT_PREVIEW_MAX_CHARS: usize = 96;
const UNAPPROVED_BODY_PREVIEW_MAX_CHARS: usize = 2000;
const EXTERNAL_UNTRUSTED_MESSAGE_TAG: &str = "external_unstrusted_message";
const EMAIL_LOG_DEFAULT_LIMIT: usize = 20;
const EMAIL_LOG_MAX_LIMIT: usize = 200;
const EMAIL_LOG_TITLE_MAX_CHARS: usize = 80;
const ACCESS_FULL: &str = "full";
const ACCESS_PREVIEW: &str = "preview";
const ACCESS_NONE: &str = "none";
// Persisted schemas stay at zero per `GATE-no-backward-compatibility`.
const STATE_SCHEMA: u32 = 0;
const GOOGLE_AUTH_SCHEMA: u32 = 0;
const GOOGLE_AUTH_PENDING_SCHEMA: u32 = 0;
const MAX_GOOGLE_AUTH_FIELD_CHARS: usize = 4096;

use tau_proto::{
    ACTION_SCHEMA_VERSION, ActionArg, ActionArgKind, ActionChoice, ActionCommand, ActionError,
    ActionInvoke, ActionOutput, ActionResult, ActionSchema, CborValue, Event, PromptFragment,
    PromptPriority, ToolError, ToolExample, ToolExampleSelector, ToolProgress, ToolResult,
    ToolSpec, ToolStarted, ToolUseState, ToolUseStats, ToolUseStatus,
};

/// `tracing` target for events emitted from this extension.
pub const LOG_TARGET: &str = "email";

/// Legacy envelope tool name for email commands.
pub const TOOL_NAME: &str = "email";

/// Prefix for model-visible split email command tools.
pub const TOOL_PREFIX: &str = "email_";

const EMAIL_COMMANDS: &[&str] = &[
    "list_folders",
    "list_recent",
    "read",
    "request_access",
    "mark_read",
    "mark_unread",
    "star",
    "unstar",
    "trash",
    "send",
];
/// Run the extension over stdio.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_client::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

/// Run the extension over the supplied reader/writer pair.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    tau_client::TauExtensionRunner::new(EmailExtension).run(
        reader,
        writer,
        RuntimeState::default(),
    )?;
    Ok(())
}

/// Tau-client declaration for the legacy email-only PIM runner.
struct EmailExtension;

impl tau_client::TauExtension for EmailExtension {
    type State = RuntimeState;

    fn name(&self) -> &'static str {
        "tau-ext-pim"
    }

    fn register(self, builder: &mut tau_client::ExtensionBuilder<Self::State>) {
        register_tools_with_prompt_fragment(
            builder,
            email_tool_specs(),
            "email_read",
            email_prompt_fragment(),
        );
        builder
            .action_provider()
            .ready_message("email extension ready")
            .configure_raw(|cx| {
                let Some(state_dir) = cx.configure.state_dir.clone() else {
                    let message = "email extension requires Configure.state_dir".to_owned();
                    cx.state.reject(message.clone());
                    return Err(tau_client::ClientError::handler(message));
                };
                let storage = Rc::new(FsStorage::new(state_dir)) as SharedStorage;
                cx.state
                    .configure(cx.configure.clone(), storage)
                    .map_err(tau_client::ClientError::handler)?;
                cx.handle.emit(Event::ActionSchemaDeclared(
                    tau_proto::ActionSchemaDeclared {
                        schema: email_action_schema_with_accounts(
                            &cx.state.google_auth_account_ids(),
                        ),
                    },
                ))
            })
            .on_raw_live(
                tau_proto::EventSelector::Exact(tau_proto::EventName::ACTION_INVOKE),
                |cx| {
                    let Event::ActionInvoke(invoke) = cx.event().clone() else {
                        return Ok(());
                    };
                    let event = cx.state.dispatch_action(invoke);
                    cx.handle.emit(event)
                },
            );
    }
}

fn register_tools_with_prompt_fragment(
    builder: &mut tau_client::ExtensionBuilder<RuntimeState>,
    tools: Vec<ToolSpec>,
    prompt_tool_name: &str,
    prompt_fragment: PromptFragment,
) {
    for tool in tools {
        let prompt_fragment = if tool.name.as_str() == prompt_tool_name {
            Some(prompt_fragment.clone())
        } else {
            None
        };
        builder.tool_with_group_and_prompt_fragment(tool, None, prompt_fragment, |cx| {
            let wire_tool_name = cx.invoke.tool_name.clone();
            let mut local_invoke = cx.invoke.clone();
            local_invoke.tool_name = cx.local_tool_name().clone();
            cx.handle
                .report_tool_progress(initial_progress(cx.invoke, cx.local_tool_name()))?;
            let mut outcome =
                tau_client::ToolTerminalOutcome::try_from(cx.state.dispatch(local_invoke))
                    .map_err(|_| {
                        tau_client::ClientError::handler(
                            "email dispatch returned a non-terminal tool event",
                        )
                    })?;
            *outcome.tool_name_mut() = wire_tool_name;
            cx.handle.report_tool_terminal(outcome)
        });
    }
}

/// Top-level email extension configuration.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmailExtensionConfig {
    /// Harness-level enable flag. Disabled by default for safe configuration.
    pub enable: bool,
    /// Configured email accounts. Account IDs must be unique.
    pub accounts: Vec<AccountConfig>,
    /// Global incoming/outgoing allow policy.
    pub policy: PolicyConfig,
}

/// One configured email account.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountConfig {
    /// Stable account identifier used in config and flattened folder ids.
    pub id: String,
    /// Per-account enable flag. Accounts are disabled unless explicitly
    /// enabled.
    ///
    /// `enabled` was a mistaken old spelling. Keep it as a little bandaid for
    /// reading old config during migration.
    #[serde(alias = "enabled")]
    pub enable: bool,
    /// Optional display name for user-facing account lists.
    pub display_name: Option<String>,
    /// Configured From identity for outgoing sends.
    pub from: String,
    /// Optional IMAP settings used by list and read commands.
    pub imap: Option<ImapConfig>,
    /// Optional SMTP settings used by send commands.
    pub smtp: Option<SmtpConfig>,
    /// Optional authentication settings. Secrets are loaded at use time and are
    /// never returned by tools.
    pub auth: Option<AuthConfig>,
    /// Per-account folder visibility policy.
    pub folders: FolderPolicy,
}

/// TLS mode used by IMAP and SMTP connections.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// Connect with TLS immediately.
    #[default]
    #[serde(alias = "implicit")]
    Required,
    /// Connect in plaintext and require a successful STARTTLS upgrade before
    /// credentials or message content are sent.
    #[serde(alias = "start_tls")]
    StartTls,
    /// Use plaintext only. This is intended for trusted local relays or test
    /// servers.
    None,
}

/// IMAP connection configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImapConfig {
    /// Server host name.
    pub host: Option<String>,
    /// Server port.
    pub port: u16,
    /// TLS policy.
    pub tls: TlsMode,
    /// Login user name.
    pub login: Option<String>,
    /// Whole-operation timeout in seconds.
    pub timeout_seconds: u64,
}

impl Default for ImapConfig {
    fn default() -> Self {
        Self {
            host: None,
            port: 993,
            tls: TlsMode::Required,
            login: None,
            timeout_seconds: 30,
        }
    }
}

/// SMTP connection configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SmtpConfig {
    /// Server host name.
    pub host: Option<String>,
    /// Server port.
    pub port: u16,
    /// TLS policy.
    pub tls: TlsMode,
    /// Login user name.
    pub login: Option<String>,
    /// Whole-operation timeout in seconds.
    pub timeout_seconds: u64,
}

impl Default for SmtpConfig {
    fn default() -> Self {
        Self {
            host: None,
            port: 587,
            tls: TlsMode::StartTls,
            login: None,
            timeout_seconds: 30,
        }
    }
}

/// Authentication method for account credentials.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    /// Read a password from a configured Tau secret.
    #[default]
    Password,
    /// Deprecated command-based password source.
    Command,
    /// Do not configure SMTP authentication. IMAP still requires a password.
    None,
    /// Google OAuth2/XOAUTH2 authentication.
    Oauth2,
}

/// OAuth2 provider for an email account.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EmailOauth2Provider {
    /// Google Gmail IMAP/SMTP using XOAUTH2.
    Google,
}

/// Authentication configuration. Secrets are loaded at use time and are never
/// returned in tool output.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// Authentication method.
    pub method: AuthMethod,
    /// Name of the Tau secret containing this account's password.
    pub password_secret: Option<String>,
    /// Deprecated environment variable password source.
    pub password_env: Option<String>,
    /// Deprecated password command.
    pub command: Option<Vec<String>>,
    /// Deprecated alias for `command`.
    pub password_command: Option<Vec<String>>,
    /// Deprecated OAuth token command placeholder.
    pub oauth2_token_command: Option<Vec<String>>,
    /// OAuth2 provider. The MVP supports only `google`.
    pub provider: Option<EmailOauth2Provider>,
    /// Secret containing the Google OAuth client id.
    pub client_id_secret: Option<String>,
    /// Optional secret containing the Google OAuth client secret.
    pub client_secret_secret: Option<String>,
    /// Optional secret containing a manually provisioned Google refresh token.
    pub refresh_token_secret: Option<String>,
}

/// Folder visibility policy for an account.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FolderPolicy {
    /// Glob patterns for visible/selectable folders. Empty means no folders
    /// visible.
    pub allow: Vec<String>,
    /// Optional special Sent folder placeholder.
    pub special_sent: Option<String>,
}

/// Global email policy.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    /// Config-defined incoming sender allow patterns.
    pub incoming_allow: Vec<String>,
    /// Authentication policy required before incoming sender allow patterns can
    /// auto-allow message content.
    pub incoming_auth: IncomingAuthPolicyConfig,
    /// Config-defined outgoing recipient allow patterns.
    pub outgoing_allow: Vec<String>,
    /// Whether persisted state allowlists may extend config policy.
    pub allow_state_policy_extensions: bool,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            incoming_allow: Vec::new(),
            incoming_auth: IncomingAuthPolicyConfig::default(),
            outgoing_allow: Vec::new(),
            allow_state_policy_extensions: true,
        }
    }
}

/// Authentication evidence required for incoming sender allow policy.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IncomingAuthPolicyConfig {
    /// Require trusted `Authentication-Results` alignment before incoming
    /// allowlists can auto-read. Defaults to true so missing configuration
    /// fails closed.
    pub require: bool,
    /// Exact authserv-id values whose server-provided `Authentication-Results`
    /// headers may be trusted for incoming auto-read decisions.
    pub trusted_authserv_ids: Vec<String>,
    /// Whether trusted aligned DMARC pass alone may satisfy incoming auth.
    /// Defaults to false so unaware users require an aligned DKIM pass.
    pub allow_dmarc_only: bool,
}

impl Default for IncomingAuthPolicyConfig {
    fn default() -> Self {
        Self {
            require: true,
            trusted_authserv_ids: Vec::new(),
            allow_dmarc_only: false,
        }
    }
}

impl EmailExtensionConfig {
    /// Validate and compile this configuration into a policy runtime.
    pub fn validate(self) -> Result<ValidatedConfig, String> {
        let mut ids = BTreeSet::new();
        let mut accounts = BTreeMap::new();
        let mut account_order = Vec::new();
        for account in self.accounts {
            if account.id.trim().is_empty() {
                return Err("account id must not be empty".to_owned());
            }
            if account.id.contains('/') {
                return Err("account id must not contain `/`".to_owned());
            }
            if !ids.insert(account.id.clone()) {
                return Err(format!("duplicate account id `{}`", account.id));
            }
            if account.from.trim().is_empty() {
                return Err(format!(
                    "account `{}` from identity must not be empty",
                    account.id
                ));
            }
            for pat in &account.folders.allow {
                validate_folder_pattern(pat)?;
            }
            if let Some(folder) = &account.folders.special_sent {
                validate_folder_pattern(folder)?;
            }
            account_order.push(account.id.clone());
            accounts.insert(
                account.id.clone(),
                ValidatedAccount::from_config(account, self.enable)?,
            );
        }
        Ok(ValidatedConfig {
            enable: self.enable,
            accounts,
            account_order,
            policy: ValidatedPolicy {
                incoming_allow: compile_address_patterns(&self.policy.incoming_allow)?,
                incoming_auth: validate_incoming_auth_policy(self.policy.incoming_auth)?,
                outgoing_allow: compile_address_patterns(&self.policy.outgoing_allow)?,
                allow_state_policy_extensions: self.policy.allow_state_policy_extensions,
            },
        })
    }
}

/// Validated extension configuration with compiled policy matchers.
pub struct ValidatedConfig {
    /// Harness-level enable flag.
    pub enable: bool,
    /// Accounts keyed by configured account ID.
    pub accounts: BTreeMap<String, ValidatedAccount>,
    /// Account IDs in configuration order. Used for deterministic defaults
    /// when a model omits the account argument.
    pub account_order: Vec<String>,
    /// Compiled global policy.
    pub policy: ValidatedPolicy,
}

/// Validated account configuration.
pub struct ValidatedAccount {
    /// Stable account identifier used in config and flattened folder ids.
    pub id: String,
    /// Whether this account is enabled.
    pub enable: bool,
    /// Optional display name.
    pub display_name: Option<String>,
    /// Normalized From address for spoof checks.
    pub from_normalized: String,
    /// Original From identity for display.
    pub from_identity: String,
    /// Validated IMAP settings when configured.
    pub imap: Option<ValidatedImapConfig>,
    /// Validated SMTP settings when configured.
    pub smtp: Option<ValidatedSmtpConfig>,
    /// Validated account authentication settings.
    pub auth: Option<ValidatedAuthConfig>,
    /// Compiled folder allowlist.
    pub folders: ValidatedFolderPolicy,
}

impl TryFrom<AccountConfig> for ValidatedAccount {
    type Error = String;

    fn try_from(value: AccountConfig) -> Result<Self, Self::Error> {
        Self::from_config(value, true)
    }
}

impl ValidatedAccount {
    /// Return true when this account has IMAP settings.
    pub fn imap_configured(&self) -> bool {
        self.imap.is_some()
    }

    /// Return true when this account has SMTP settings.
    pub fn smtp_configured(&self) -> bool {
        self.smtp.is_some()
    }
}

/// Validated IMAP connection settings.
#[derive(Clone)]
pub struct ValidatedImapConfig {
    /// Server host name.
    pub host: String,
    /// Server port.
    pub port: u16,
    /// TLS policy.
    pub tls: TlsMode,
    /// Login user name.
    pub login: String,
    /// Whole-operation timeout in seconds.
    pub timeout_seconds: u64,
}

/// Validated SMTP connection settings.
#[derive(Clone)]
pub struct ValidatedSmtpConfig {
    /// Server host name.
    pub host: String,
    /// Server port.
    pub port: u16,
    /// TLS policy.
    pub tls: TlsMode,
    /// Login user name.
    pub login: String,
    /// Whole-operation timeout in seconds.
    pub timeout_seconds: u64,
}

/// Validated authentication settings.
#[derive(Clone)]
pub struct ValidatedAuthConfig {
    /// Authentication method.
    pub method: AuthMethod,
    /// Name of the Tau secret containing the password.
    pub password_secret: Option<String>,
    /// OAuth2 provider for OAuth accounts.
    pub provider: Option<EmailOauth2Provider>,
    /// Secret containing the Google OAuth client id.
    pub client_id_secret: Option<String>,
    /// Optional secret containing the Google OAuth client secret.
    pub client_secret_secret: Option<String>,
    /// Optional secret containing a pre-provisioned Google refresh token.
    pub refresh_token_secret: Option<String>,
}

impl ValidatedAccount {
    fn from_config(value: AccountConfig, extension_enabled: bool) -> Result<Self, String> {
        let matchers = value
            .folders
            .allow
            .iter()
            .map(|pat| {
                Glob::new(pat)
                    .map(|glob| glob.compile_matcher())
                    .map_err(|e| e.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let imap = validate_imap_config(&value.id, value.imap)?;
        let smtp = validate_smtp_config(&value.id, value.smtp)?;
        let account_enabled = value.enable;
        let auth = if extension_enabled && account_enabled {
            let auth = validate_auth_config(&value.id, value.auth)?;
            validate_account_auth_support(&value.id, imap.is_some(), auth.as_ref())?;
            auth
        } else {
            inactive_auth_config(value.auth)
        };
        Ok(Self {
            id: value.id,
            enable: account_enabled,
            display_name: value.display_name,
            from_normalized: normalize_address(&value.from)
                .ok_or_else(|| "from identity must contain an email address".to_owned())?,
            from_identity: value.from,
            imap,
            smtp,
            auth,
            folders: ValidatedFolderPolicy { matchers },
        })
    }
}

fn validate_imap_config(
    account_id: &str,
    config: Option<ImapConfig>,
) -> Result<Option<ValidatedImapConfig>, String> {
    let Some(config) = config else {
        return Ok(None);
    };
    let host = required_config_string(config.host, account_id, "imap.host")?;
    let login = required_config_string(config.login, account_id, "imap.login")?;
    validate_timeout(account_id, "imap.timeout_seconds", config.timeout_seconds)?;
    Ok(Some(ValidatedImapConfig {
        host,
        port: config.port,
        tls: config.tls,
        login,
        timeout_seconds: config.timeout_seconds,
    }))
}

fn validate_smtp_config(
    account_id: &str,
    config: Option<SmtpConfig>,
) -> Result<Option<ValidatedSmtpConfig>, String> {
    let Some(config) = config else {
        return Ok(None);
    };
    let host = required_config_string(config.host, account_id, "smtp.host")?;
    let login = required_config_string(config.login, account_id, "smtp.login")?;
    validate_timeout(account_id, "smtp.timeout_seconds", config.timeout_seconds)?;
    Ok(Some(ValidatedSmtpConfig {
        host,
        port: config.port,
        tls: config.tls,
        login,
        timeout_seconds: config.timeout_seconds,
    }))
}

fn validate_auth_config(
    account_id: &str,
    config: Option<AuthConfig>,
) -> Result<Option<ValidatedAuthConfig>, String> {
    let Some(config) = config else {
        return Ok(None);
    };
    reject_deprecated_auth_config(account_id, &config)?;
    match config.method {
        AuthMethod::Command => Err(migration_error(account_id, "auth.method command")),
        AuthMethod::Oauth2 => validate_oauth2_auth_config(account_id, config),
        AuthMethod::Password | AuthMethod::None => {
            validate_non_oauth2_auth_config(account_id, config)
        }
    }
}

fn reject_deprecated_auth_config(account_id: &str, config: &AuthConfig) -> Result<(), String> {
    if config.password_env.is_some() {
        return Err(migration_error(account_id, "auth.password_env"));
    }
    if config.command.is_some() {
        return Err(migration_error(account_id, "auth.command"));
    }
    if config.password_command.is_some() {
        return Err(migration_error(account_id, "auth.password_command"));
    }
    if config.oauth2_token_command.is_some() {
        return Err(migration_error(account_id, "auth.oauth2_token_command"));
    }
    Ok(())
}

fn validate_oauth2_auth_config(
    account_id: &str,
    config: AuthConfig,
) -> Result<Option<ValidatedAuthConfig>, String> {
    if config.password_secret.is_some() {
        return Err(format!(
            "account `{account_id}` auth.password_secret is only valid for password auth"
        ));
    }
    if config.provider != Some(EmailOauth2Provider::Google) {
        return Err(format!(
            "account `{account_id}` auth.provider must be google for oauth2 email auth"
        ));
    }
    let client_id_secret = config.client_id_secret.ok_or_else(|| {
        format!("account `{account_id}` auth.client_id_secret is required for oauth2 google auth")
    })?;
    validate_secret_name(account_id, "auth.client_id_secret", &client_id_secret)?;
    validate_optional_secret_name(
        account_id,
        "auth.client_secret_secret",
        config.client_secret_secret.as_deref(),
    )?;
    validate_optional_secret_name(
        account_id,
        "auth.refresh_token_secret",
        config.refresh_token_secret.as_deref(),
    )?;
    Ok(Some(ValidatedAuthConfig {
        method: config.method,
        password_secret: None,
        provider: config.provider,
        client_id_secret: Some(client_id_secret),
        client_secret_secret: config.client_secret_secret,
        refresh_token_secret: config.refresh_token_secret,
    }))
}

fn validate_non_oauth2_auth_config(
    account_id: &str,
    config: AuthConfig,
) -> Result<Option<ValidatedAuthConfig>, String> {
    if config.provider.is_some()
        || config.client_id_secret.is_some()
        || config.client_secret_secret.is_some()
        || config.refresh_token_secret.is_some()
    {
        return Err(format!(
            "account `{account_id}` OAuth fields require auth.method oauth2"
        ));
    }
    if matches!(config.method, AuthMethod::Password) && config.password_secret.is_none() {
        return Err(format!(
            "account `{account_id}` auth.password_secret is required for password auth; declare the secret under the enabled extension's secrets and set auth.password_secret to that name"
        ));
    }
    validate_optional_secret_name(
        account_id,
        "auth.password_secret",
        config.password_secret.as_deref(),
    )?;
    Ok(Some(ValidatedAuthConfig {
        method: config.method,
        password_secret: config.password_secret,
        provider: None,
        client_id_secret: None,
        client_secret_secret: None,
        refresh_token_secret: None,
    }))
}

fn validate_optional_secret_name(
    account_id: &str,
    field: &str,
    secret: Option<&str>,
) -> Result<(), String> {
    if let Some(secret) = secret {
        validate_secret_name(account_id, field, secret)?;
    }
    Ok(())
}

fn inactive_auth_config(config: Option<AuthConfig>) -> Option<ValidatedAuthConfig> {
    config.map(|config| ValidatedAuthConfig {
        method: config.method,
        password_secret: config.password_secret,
        provider: config.provider,
        client_id_secret: config.client_id_secret,
        client_secret_secret: config.client_secret_secret,
        refresh_token_secret: config.refresh_token_secret,
    })
}

fn migration_error(account_id: &str, field: &str) -> String {
    format!(
        "account `{account_id}` {field} is no longer supported; declare the password under the enabled extension's secrets and set auth.password_secret to that secret name"
    )
}

fn validate_account_auth_support(
    account_id: &str,
    imap_configured: bool,
    auth: Option<&ValidatedAuthConfig>,
) -> Result<(), String> {
    if !imap_configured {
        return Ok(());
    }
    match auth.map(|auth| auth.method) {
        Some(AuthMethod::Password) => Ok(()),
        Some(AuthMethod::Command) => Err(migration_error(account_id, "auth.method command")),
        Some(AuthMethod::None) => Err(format!(
            "account `{account_id}` auth.method none is not supported for IMAP accounts"
        )),
        None => Err(format!(
            "account `{account_id}` IMAP configuration requires auth.method password or oauth2"
        )),
        Some(AuthMethod::Oauth2) => Ok(()),
    }
}

fn validate_secret_name(account_id: &str, field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("account `{account_id}` {field} must not be empty"));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(format!(
            "account `{account_id}` {field} may only contain ASCII letters, digits, '_', '-', '.'"
        ));
    }
    Ok(())
}

fn validate_incoming_auth_policy(
    config: IncomingAuthPolicyConfig,
) -> Result<ValidatedIncomingAuthPolicy, String> {
    let mut trusted_authserv_ids = BTreeSet::new();
    for id in config.trusted_authserv_ids {
        let normalized = validate_authserv_id(&id)?;
        trusted_authserv_ids.insert(normalized);
    }
    Ok(ValidatedIncomingAuthPolicy {
        require: config.require,
        trusted_authserv_ids,
        allow_dmarc_only: config.allow_dmarc_only,
    })
}

fn validate_authserv_id(id: &str) -> Result<String, String> {
    let trimmed = id.trim();
    if trimmed.is_empty()
        || trimmed.chars().any(char::is_control)
        || trimmed.contains([';', ',', ' ', '\t', '\r', '\n', '<', '>'])
    {
        return Err(format!("invalid incoming_auth trusted authserv-id `{id}`"));
    }
    Ok(trimmed.to_ascii_lowercase())
}

fn validate_config_secrets(
    config: &ValidatedConfig,
    secrets: &BTreeMap<String, tau_proto::SecretValue>,
) -> Result<(), String> {
    if !config.enable {
        return Ok(());
    }
    for account in config.accounts.values() {
        if !account.enable {
            continue;
        }
        let Some(auth) = account.auth.as_ref() else {
            continue;
        };
        if matches!(auth.method, AuthMethod::Password) {
            let secret = auth.password_secret.as_deref().ok_or_else(|| {
                format!(
                    "account `{}` auth.password_secret is required for password auth",
                    account.id
                )
            })?;
            if !secrets.contains_key(secret) {
                return Err(format!(
                    "account `{}` auth.password_secret `{secret}` was not provided in Configure.secrets; declare it under the enabled extension's secrets",
                    account.id
                ));
            }
        } else if matches!(auth.method, AuthMethod::Oauth2) {
            let client_id_secret = auth.client_id_secret.as_deref().ok_or_else(|| {
                format!(
                    "account `{}` auth.client_id_secret is required for oauth2 auth",
                    account.id
                )
            })?;
            for (field, secret) in [
                ("auth.client_id_secret", Some(client_id_secret)),
                (
                    "auth.client_secret_secret",
                    auth.client_secret_secret.as_deref(),
                ),
                (
                    "auth.refresh_token_secret",
                    auth.refresh_token_secret.as_deref(),
                ),
            ] {
                let Some(secret) = secret else { continue };
                if !secrets.contains_key(secret) {
                    return Err(format!(
                        "account `{}` {field} `{secret}` was not provided in Configure.secrets; declare it under the enabled extension's secrets",
                        account.id
                    ));
                }
            }
        }
    }
    Ok(())
}

fn validate_timeout(account_id: &str, field: &str, seconds: u64) -> Result<(), String> {
    if seconds == 0 {
        return Err(format!("account `{account_id}` {field} must be positive"));
    }
    Ok(())
}

fn required_config_string(
    value: Option<String>,
    account_id: &str,
    field: &str,
) -> Result<String, String> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(format!("account `{account_id}` {field} must not be empty")),
    }
}

/// Compiled folder allowlist.
pub struct ValidatedFolderPolicy {
    /// Glob matchers that decide folder visibility.
    pub matchers: Vec<GlobMatcher>,
}

impl ValidatedFolderPolicy {
    /// Return true if the folder is visible for this account.
    pub fn allows(&self, folder: &str) -> bool {
        !self.matchers.is_empty() && self.matchers.iter().any(|matcher| matcher.is_match(folder))
    }
}

/// Compiled global policy.
pub struct ValidatedPolicy {
    /// Compiled config incoming allow patterns.
    pub incoming_allow: Vec<AddressPattern>,
    /// Compiled authentication policy for incoming allow decisions.
    pub incoming_auth: ValidatedIncomingAuthPolicy,
    /// Compiled config outgoing allow patterns.
    pub outgoing_allow: Vec<AddressPattern>,
    /// Whether persisted state policy extensions are enabled.
    pub allow_state_policy_extensions: bool,
}

/// Validated authentication policy for incoming allow decisions.
pub struct ValidatedIncomingAuthPolicy {
    /// Whether incoming allow decisions require trusted authentication
    /// evidence.
    pub require: bool,
    /// Lowercase trusted authserv-id values.
    pub trusted_authserv_ids: BTreeSet<String>,
    /// Whether aligned DMARC pass alone can satisfy incoming auth.
    pub allow_dmarc_only: bool,
}

/// A normalized address pattern.
pub enum AddressPattern {
    /// Exact normalized `local@domain` match.
    Exact { pattern: String },
    /// Whole-address glob match.
    Glob {
        pattern: String,
        matcher: GlobMatcher,
    },
    /// Whole-address regex match, compiled as `^(?:pattern)$`.
    Regex { pattern: String, regex: Regex },
}

impl AddressPattern {
    /// Compile a user/config pattern string.
    pub fn compile(input: &str) -> Result<Self, String> {
        if input.trim().is_empty() {
            return Err("allow pattern must not be empty".to_owned());
        }
        if input
            .chars()
            .any(|ch| ch.is_control() || is_unsafe_format_control(ch))
        {
            return Err("allow pattern must not contain control characters".to_owned());
        }
        if let Some(regex) = input.strip_prefix("re:") {
            let compiled = Regex::new(&format!("^(?:{regex})$"))
                .map_err(|error| format!("invalid regex pattern `{input}`: {error}"))?;
            return Ok(Self::Regex {
                pattern: input.to_owned(),
                regex: compiled,
            });
        }
        if input.contains('*') || input.contains('?') {
            let pattern = input.to_ascii_lowercase();
            let matcher = Glob::new(&pattern)
                .map_err(|error| format!("invalid glob pattern `{input}`: {error}"))?
                .compile_matcher();
            return Ok(Self::Glob { pattern, matcher });
        }
        let normalized = normalize_address(input)
            .ok_or_else(|| format!("invalid exact address pattern `{input}`"))?;
        Ok(Self::Exact {
            pattern: normalized,
        })
    }

    /// Return true when this pattern matches the normalized address/header.
    pub fn matches(&self, address: &str) -> bool {
        let Some(normalized) = normalize_address(address) else {
            return false;
        };
        match self {
            Self::Exact { pattern } => pattern == &normalized,
            Self::Glob { matcher, .. } => matcher.is_match(&normalized),
            Self::Regex { regex, .. } => regex.is_match(&normalized),
        }
    }

    fn pattern_text(&self) -> &str {
        match self {
            Self::Exact { pattern } | Self::Glob { pattern, .. } | Self::Regex { pattern, .. } => {
                pattern
            }
        }
    }
}

/// Match result for policy decisions.
pub struct PolicyDecision {
    /// Whether the operation is allowed without a new approval.
    pub allowed: bool,
    /// Machine-readable reason.
    pub reason: String,
    /// Matching pattern text when allowed by policy.
    pub matched_pattern: Option<String>,
}

impl PolicyDecision {
    fn allowed(pattern: Option<String>) -> Self {
        Self {
            allowed: true,
            reason: "allowed".to_owned(),
            matched_pattern: pattern,
        }
    }

    fn denied(reason: &str) -> Self {
        Self {
            allowed: false,
            reason: reason.to_owned(),
            matched_pattern: None,
        }
    }
}

fn compile_address_patterns(patterns: &[String]) -> Result<Vec<AddressPattern>, String> {
    patterns
        .iter()
        .map(|pattern| AddressPattern::compile(pattern))
        .collect()
}

fn incoming_auth_decision(
    message: &BackendMessage,
    policy: &ValidatedIncomingAuthPolicy,
) -> PolicyDecision {
    if message.source_truncated && message.body_text.is_empty() {
        return PolicyDecision::denied("auth truncated");
    }
    if message.auth_results.is_empty() {
        return PolicyDecision::denied("auth missing");
    }
    let Some(visible_domain) = normalize_address(&message.from).and_then(|address| {
        address
            .split_once('@')
            .map(|(_, domain)| domain.to_ascii_lowercase())
    }) else {
        return PolicyDecision::denied("auth unaligned");
    };
    // Authentication-Results headers below the newest one are attacker-controlled
    // unless the trusted MTA strips them. Trust only the topmost parsed header so
    // a forged lower header cannot override the server's result.
    let Some(evidence) = message.auth_results.first() else {
        return PolicyDecision::denied("auth missing");
    };
    if !policy
        .trusted_authserv_ids
        .contains(&evidence.authserv_id.to_ascii_lowercase())
    {
        return PolicyDecision::denied("untrusted auth server");
    }

    let mut saw_pass = false;
    let mut saw_aligned_dmarc = false;
    if evidence.dmarc_result.as_deref() == Some("pass") {
        saw_pass = true;
        if evidence
            .dmarc_header_from
            .as_deref()
            .is_some_and(|domain| domain.eq_ignore_ascii_case(&visible_domain))
        {
            saw_aligned_dmarc = true;
        }
    }
    if evidence.dkim_result.as_deref() == Some("pass") {
        saw_pass = true;
        if evidence
            .dkim_header_d
            .as_deref()
            .is_some_and(|domain| domain.eq_ignore_ascii_case(&visible_domain))
        {
            return PolicyDecision::allowed(Some("auth".to_owned()));
        }
    }
    if policy.allow_dmarc_only && saw_aligned_dmarc {
        return PolicyDecision::allowed(Some("auth".to_owned()));
    }
    if saw_aligned_dmarc {
        PolicyDecision::denied("dkim missing")
    } else if saw_pass {
        PolicyDecision::denied("auth unaligned")
    } else {
        PolicyDecision::denied("auth failed")
    }
}

/// Normalize an email address/header to lowercase `local@domain` for policy
/// matching.
pub fn normalize_address(input: &str) -> Option<String> {
    let raw = input.trim();
    let candidate = if let (Some(start), Some(end)) = (raw.rfind('<'), raw.rfind('>')) {
        if start < end {
            &raw[start + 1..end]
        } else {
            raw
        }
    } else {
        raw
    };
    let candidate = candidate.trim().trim_matches('"');
    let (local, domain) = candidate.split_once('@')?;
    if local.is_empty()
        || domain.is_empty()
        || candidate.contains(char::is_whitespace)
        || candidate
            .chars()
            .any(|ch| ch.is_control() || is_unsafe_format_control(ch))
        || candidate.matches('@').count() != 1
    {
        return None;
    }
    Some(format!(
        "{}@{}",
        local.to_ascii_lowercase(),
        domain.to_ascii_lowercase()
    ))
}

fn validate_folder_pattern(pattern: &str) -> Result<(), String> {
    if pattern.trim().is_empty() {
        return Err("folder allow pattern must not be empty".to_owned());
    }
    if pattern.contains('\0') || pattern.split('/').any(|part| part == "..") {
        return Err(format!("invalid folder allow pattern `{pattern}`"));
    }
    Glob::new(pattern).map_err(|error| format!("invalid folder glob `{pattern}`: {error}"))?;
    Ok(())
}

fn validate_mailbox_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty()
        || MAX_HEADER_VALUE_CHARS < name.chars().count()
        || name
            .chars()
            .any(|ch| ch.is_control() || is_unsafe_format_control(ch))
    {
        return Err("folder name is invalid".to_owned());
    }
    Ok(())
}

#[derive(Default, Serialize, Deserialize)]
struct PolicyFile {
    schema: u32,
    patterns: Vec<StatePattern>,
}

/// One persisted allowlist pattern.
#[derive(Clone, Serialize, Deserialize)]
pub struct StatePattern {
    /// Pattern kind: exact, glob, or regex.
    pub kind: String,
    /// User-added pattern text.
    pub pattern: String,
    /// Creation timestamp placeholder.
    pub created_at: String,
    /// Creator marker such as cli or test.
    pub created_by: String,
    /// Optional human note.
    pub note: Option<String>,
}

/// Stored OAuth authorization for one Gmail email account.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct EmailGoogleStoredAuth {
    /// Stored auth schema version.
    pub(crate) schema: u32,
    /// Configured email account id.
    pub(crate) account: String,
    /// Google OAuth refresh token.
    pub(crate) refresh_token: String,
}

impl std::fmt::Debug for EmailGoogleStoredAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmailGoogleStoredAuth")
            .field("schema", &self.schema)
            .field("account", &self.account)
            .field("refresh_token", &"<redacted>")
            .finish()
    }
}

impl EmailGoogleStoredAuth {
    /// Build a stored OAuth auth record.
    pub(crate) fn new(account: &str, refresh_token: &str) -> Self {
        Self {
            schema: GOOGLE_AUTH_SCHEMA,
            account: account.to_owned(),
            refresh_token: refresh_token.to_owned(),
        }
    }
}

/// Pending Google authorization request for one email account.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct EmailGooglePendingAuth {
    /// Pending auth schema version.
    pub(crate) schema: u32,
    /// Configured email account id.
    pub(crate) account: String,
    /// Flow-specific private pending auth data.
    #[serde(flatten)]
    pub(crate) flow: EmailGooglePendingAuthFlow,
}

/// Flow-specific private pending Google authorization data.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "flow", rename_all = "snake_case")]
pub(crate) enum EmailGooglePendingAuthFlow {
    /// Installed-app authorization-code flow with a manually pasted loopback
    /// URL.
    InstalledApp {
        /// OAuth state parameter expected in the pasted loopback URL.
        state: String,
        /// RFC 7636 PKCE verifier used only during token exchange.
        pkce_verifier: String,
        /// Exact redirect URI saved at start and reused during token exchange.
        redirect_uri: String,
        /// Unix timestamp in milliseconds when this request was created.
        created_at_unix_ms: u64,
        /// Unix timestamp in milliseconds when this request expires.
        expires_at_unix_ms: u64,
    },
}

impl std::fmt::Debug for EmailGooglePendingAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmailGooglePendingAuth")
            .field("schema", &self.schema)
            .field("account", &self.account)
            .field("flow", &"<redacted>")
            .finish()
    }
}

impl std::fmt::Debug for EmailGooglePendingAuthFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InstalledApp { .. } => f.write_str("InstalledApp(<redacted>)"),
        }
    }
}

impl EmailGooglePendingAuth {
    /// Build a pending Google installed-app authorization record.
    pub(crate) fn installed_app(
        account: &str,
        state: &str,
        pkce_verifier: &str,
        redirect_uri: &str,
        expires_in_secs: u64,
    ) -> Self {
        let created_at_unix_ms = current_unix_millis();
        Self {
            schema: GOOGLE_AUTH_PENDING_SCHEMA,
            account: account.to_owned(),
            flow: EmailGooglePendingAuthFlow::InstalledApp {
                state: state.to_owned(),
                pkce_verifier: pkce_verifier.to_owned(),
                redirect_uri: redirect_uri.to_owned(),
                created_at_unix_ms,
                expires_at_unix_ms: created_at_unix_ms
                    .saturating_add(expires_in_secs.saturating_mul(1000)),
            },
        }
    }

    /// Return true when this pending authorization has expired.
    pub(crate) fn expired(&self) -> bool {
        self.expires_at_unix_ms() <= current_unix_millis()
    }

    /// Return installed-app pending data.
    pub(crate) fn installed_app_data(&self) -> InstalledAppPendingAuth<'_> {
        match &self.flow {
            EmailGooglePendingAuthFlow::InstalledApp {
                state,
                pkce_verifier,
                redirect_uri,
                ..
            } => InstalledAppPendingAuth {
                state,
                pkce_verifier,
                redirect_uri,
            },
        }
    }

    fn expires_at_unix_ms(&self) -> u64 {
        match &self.flow {
            EmailGooglePendingAuthFlow::InstalledApp {
                expires_at_unix_ms, ..
            } => *expires_at_unix_ms,
        }
    }
}

/// Borrowed installed-app pending authorization fields.
pub(crate) struct InstalledAppPendingAuth<'a> {
    /// OAuth state expected in the pasted redirect URL.
    pub(crate) state: &'a str,
    /// PKCE verifier saved at start.
    pub(crate) pkce_verifier: &'a str,
    /// Exact redirect URI saved at start.
    pub(crate) redirect_uri: &'a str,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct EmailLogEntry {
    schema: u32,
    ts_unix_ms: u64,
    kind: String,
    command: String,
    status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    folder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    to: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    title_redacted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    approval_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Persistent policy/approval state under the injected extension storage root.
#[derive(Clone)]
pub struct StateStore {
    storage: SharedStorage,
    /// Root state directory provided by the harness, kept for tests and
    /// diagnostics.
    pub state_dir: PathBuf,
}

impl StateStore {
    /// Create the state directory and marker file if needed.
    pub fn open(state_dir: PathBuf) -> Result<Self, String> {
        Self::open_with_storage(
            state_dir.clone(),
            path_std_rc::Rc::new(FsStorage::new(state_dir)),
        )
    }

    /// Create the state marker file if needed using the supplied storage
    /// backend.
    pub(crate) fn open_with_storage(
        state_dir: PathBuf,
        storage: SharedStorage,
    ) -> Result<Self, String> {
        let store = Self { storage, state_dir };
        store.write_json(
            "state-v0.json",
            &serde_json::json!({"schema": STATE_SCHEMA}),
        )?;
        Ok(store)
    }

    fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, String> {
        self.storage.read_file(path)
    }

    fn write_json<T: Serialize>(&self, path: &str, value: &T) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
        self.storage.write_file(path, bytes)
    }

    fn create_json<T: Serialize>(&self, path: &str, value: &T) -> Result<(), CreateNewJsonError> {
        let bytes = serde_json::to_vec_pretty(value)
            .map_err(|error| CreateNewJsonError::Other(error.to_string()))?;
        self.storage
            .create_file(path, bytes)
            .map_err(|error| match error {
                StorageCreateError::AlreadyExists => CreateNewJsonError::AlreadyExists,
                StorageCreateError::Other(message) => CreateNewJsonError::Other(message),
            })
    }

    fn file_exists(&self, path: &str) -> Result<bool, String> {
        self.storage.file_exists(path)
    }

    /// Load persisted incoming allow patterns.
    pub fn load_incoming_allow(&self) -> Result<Vec<AddressPattern>, String> {
        self.load_allow_file("incoming-allow.json")
    }

    /// Load persisted outgoing allow patterns.
    pub fn load_outgoing_allow(&self) -> Result<Vec<AddressPattern>, String> {
        self.load_allow_file("outgoing-allow.json")
    }

    /// Save persisted incoming allow pattern records.
    pub fn save_incoming_allow_records(&self, records: &[StatePattern]) -> Result<(), String> {
        self.save_allow_file("incoming-allow.json", records)
    }

    /// Save persisted outgoing allow pattern records.
    pub fn save_outgoing_allow_records(&self, records: &[StatePattern]) -> Result<(), String> {
        self.save_allow_file("outgoing-allow.json", records)
    }

    /// Append one persisted incoming allow pattern record.
    pub fn append_incoming_allow_record(&self, record: StatePattern) -> Result<(), String> {
        let mut records = self.load_allow_records("incoming-allow.json")?;
        records.push(record);
        self.save_incoming_allow_records(&records)
    }

    /// Append one persisted outgoing allow pattern record.
    pub fn append_outgoing_allow_record(&self, record: StatePattern) -> Result<(), String> {
        let mut records = self.load_allow_records("outgoing-allow.json")?;
        records.push(record);
        self.save_outgoing_allow_records(&records)
    }

    /// Load pending incoming read approvals in deterministic order.
    pub fn list_pending_incoming(&self) -> Result<Vec<IncomingApproval>, String> {
        self.list_incoming_approvals("pending")
    }

    /// Load pending outgoing send approvals in deterministic order.
    pub fn list_pending_outgoing(&self) -> Result<Vec<OutgoingApproval>, String> {
        self.list_outgoing_approvals("pending")
    }

    /// Load one pending incoming read approval by id.
    pub fn pending_incoming_by_id(&self, id: &str) -> Result<IncomingApproval, String> {
        self.load_incoming_approval("pending", id)
    }

    /// Load one pending outgoing send approval by id.
    pub fn pending_outgoing_by_id(&self, id: &str) -> Result<OutgoingApproval, String> {
        self.load_outgoing_approval("pending", id)
    }

    /// Load one approved incoming read approval by id.
    pub fn approved_incoming_by_id(&self, id: &str) -> Result<IncomingApproval, String> {
        self.load_incoming_approval("approved", id)
    }

    /// Load one denied incoming read approval by id.
    pub fn denied_incoming_by_id(&self, id: &str) -> Result<IncomingApproval, String> {
        self.load_incoming_approval("denied", id)
    }

    /// Load one approved outgoing send approval by id.
    pub fn approved_outgoing_by_id(&self, id: &str) -> Result<OutgoingApproval, String> {
        self.load_outgoing_approval("approved", id)
    }

    /// Load one denied outgoing send approval by id.
    pub fn denied_outgoing_by_id(&self, id: &str) -> Result<OutgoingApproval, String> {
        self.load_outgoing_approval("denied", id)
    }

    fn append_email_log(&self, entry: &EmailLogEntry) -> Result<(), String> {
        let path = self.email_log_path();
        let mut bytes = serde_json::to_vec(entry).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        self.storage.append_file(&path, bytes)
    }

    fn recent_email_log(&self, limit: usize) -> Result<Vec<EmailLogEntry>, String> {
        let path = self.email_log_path();
        let Some(bytes) = self.read_file(&path)? else {
            return Ok(Vec::new());
        };
        let mut entries = VecDeque::new();
        for line in BufReader::new(bytes.as_slice()).lines() {
            let line = line.map_err(|error| format!("failed to read {path}: {error}"))?;
            if line.trim().is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<EmailLogEntry>(&line) else {
                continue;
            };
            if entry.schema != STATE_SCHEMA {
                continue;
            }
            entries.push_back(entry);
            while limit < entries.len() {
                entries.pop_front();
            }
        }
        Ok(entries.into_iter().collect())
    }

    /// Store a Google OAuth refresh token for one email account.
    pub(crate) fn save_google_refresh_token(
        &self,
        account: &str,
        refresh_token: &str,
    ) -> Result<(), String> {
        let auth = EmailGoogleStoredAuth::new(account, refresh_token);
        validate_google_stored_auth(&auth, Some(account))?;
        self.write_json(&self.google_auth_path(account), &auth)
    }

    /// Load a stored Google OAuth refresh token for one email account.
    pub(crate) fn google_refresh_token(&self, account: &str) -> Result<Option<String>, String> {
        let path = self.google_auth_path(account);
        let Some(bytes) = self.read_file(&path)? else {
            return Ok(None);
        };
        let auth: EmailGoogleStoredAuth = serde_json::from_slice(&bytes)
            .map_err(|error| format!("failed to parse {path}: {error}"))?;
        validate_google_stored_auth(&auth, Some(account))?;
        Ok(Some(auth.refresh_token))
    }

    /// Store a pending Google authorization request for email.
    pub(crate) fn save_pending_google_auth(
        &self,
        pending: &EmailGooglePendingAuth,
    ) -> Result<(), String> {
        validate_google_pending_auth(pending, Some(&pending.account))?;
        self.write_json(&self.google_pending_auth_path(&pending.account), pending)
    }

    /// Load a pending Google authorization request for email.
    pub(crate) fn pending_google_auth(
        &self,
        account: &str,
    ) -> Result<EmailGooglePendingAuth, String> {
        let path = self.google_pending_auth_path(account);
        let Some(bytes) = self.read_file(&path)? else {
            return Err(format!(
                "no pending Google authorization for account `{account}`"
            ));
        };
        let pending: EmailGooglePendingAuth = serde_json::from_slice(&bytes).map_err(|_| {
            "pending Google email authorization is unsupported or corrupt; run `:email auth google start <account>` again".to_owned()
        })?;
        validate_google_pending_auth(&pending, Some(account))?;
        Ok(pending)
    }

    /// Clear a pending Google authorization request for email.
    pub(crate) fn clear_pending_google_auth(&self, account: &str) -> Result<(), String> {
        self.storage
            .delete_file(&self.google_pending_auth_path(account))
    }

    fn email_log_path(&self) -> String {
        "logs/email.jsonl".to_owned()
    }

    fn google_auth_path(&self, account: &str) -> String {
        format!("auth/email/google/{}.json", account_file_stem(account))
    }

    fn google_pending_auth_path(&self, account: &str) -> String {
        format!(
            "auth/email/google-pending/{}.json",
            account_file_stem(account)
        )
    }

    fn load_allow_file(&self, name: &str) -> Result<Vec<AddressPattern>, String> {
        self.load_allow_records(name)?
            .iter()
            .map(|record| match record.kind.as_str() {
                "exact" | "glob" => AddressPattern::compile(&record.pattern),
                "regex" => AddressPattern::compile(&format!("re:{}", record.pattern)),
                other => Err(format!("unsupported policy pattern kind `{other}`")),
            })
            .collect()
    }

    fn load_allow_records(&self, name: &str) -> Result<Vec<StatePattern>, String> {
        let path = format!("policy/{name}");
        let Some(bytes) = self.read_file(&path)? else {
            return Ok(Vec::new());
        };
        let file: PolicyFile = serde_json::from_slice(&bytes)
            .map_err(|error| format!("failed to parse {path}: {error}"))?;
        if file.schema != STATE_SCHEMA {
            return Err(format!(
                "unsupported policy schema {} in {path}",
                file.schema
            ));
        }
        Ok(file.patterns)
    }

    fn save_allow_file(&self, name: &str, records: &[StatePattern]) -> Result<(), String> {
        let file = PolicyFile {
            schema: STATE_SCHEMA,
            patterns: records.to_vec(),
        };
        self.write_json(&format!("policy/{name}"), &file)
    }

    fn list_approvals<T: for<'de> Deserialize<'de>>(
        &self,
        kind: &str,
        status: &str,
    ) -> Result<Vec<T>, String> {
        let dir = format!("approvals/{kind}/{status}");
        self.storage
            .list_files(&dir)?
            .into_iter()
            .filter(|entry| !entry.is_dir && entry.path.ends_with(".json"))
            .map(|entry| {
                let bytes = self
                    .read_file(&entry.path)?
                    .ok_or_else(|| format!("approval file `{}` not found", entry.path))?;
                serde_json::from_slice(&bytes)
                    .map_err(|error| format!("failed to parse {}: {error}", entry.path))
            })
            .collect()
    }

    fn load_approval<T: for<'de> Deserialize<'de>>(
        &self,
        kind: &str,
        status: &str,
        id: &str,
    ) -> Result<T, String> {
        let path = self.approval_path(kind, status, id)?;
        let Some(bytes) = self.read_file(&path)? else {
            return Err(format!("approval `{id}` not found"));
        };
        serde_json::from_slice(&bytes).map_err(|error| format!("failed to parse {path}: {error}"))
    }

    fn list_incoming_approvals(&self, status: &str) -> Result<Vec<IncomingApproval>, String> {
        self.list_approvals::<IncomingApproval>("incoming", status)?
            .into_iter()
            .map(|approval| {
                validate_incoming_approval(&approval, status, None)?;
                Ok(approval)
            })
            .collect()
    }

    fn list_outgoing_approvals(&self, status: &str) -> Result<Vec<OutgoingApproval>, String> {
        self.list_approvals::<OutgoingApproval>("outgoing", status)?
            .into_iter()
            .map(|approval| {
                validate_outgoing_approval(&approval, status, None)?;
                Ok(approval)
            })
            .collect()
    }

    fn load_incoming_approval(&self, status: &str, id: &str) -> Result<IncomingApproval, String> {
        let approval = self.load_approval("incoming", status, id)?;
        validate_incoming_approval(&approval, status, Some(id))?;
        Ok(approval)
    }

    fn load_outgoing_approval(&self, status: &str, id: &str) -> Result<OutgoingApproval, String> {
        let approval = self.load_approval("outgoing", status, id)?;
        validate_outgoing_approval(&approval, status, Some(id))?;
        Ok(approval)
    }

    fn sending_outgoing_by_id(&self, id: &str) -> Result<OutgoingApproval, String> {
        self.load_outgoing_approval("sending", id)
    }

    /// Return an existing pending incoming approval or create it atomically.
    pub fn pending_incoming(&self, request: &IncomingApproval) -> Result<String, String> {
        loop {
            for approval in self.list_pending_incoming()? {
                if incoming_approval_matches_target(&approval, request) {
                    return Ok(approval.id);
                }
            }
            let mut request = request.clone();
            request.id = self.next_approval_id("incoming")?;
            let path = self.approval_path("incoming", "pending", &request.id)?;
            match self.create_json(&path, &request) {
                Ok(()) => return Ok(request.id),
                Err(CreateNewJsonError::AlreadyExists) => continue,
                Err(CreateNewJsonError::Other(message)) => return Err(message),
            }
        }
    }

    /// Return an existing pending outgoing approval or create it atomically.
    pub fn pending_outgoing(&self, request: &OutgoingApproval) -> Result<String, String> {
        loop {
            for approval in self.list_pending_outgoing()? {
                if outgoing_approval_matches_message(&approval, request) {
                    return Ok(approval.id);
                }
            }
            let mut request = request.clone();
            request.id = self.next_approval_id("outgoing")?;
            let path = self.approval_path("outgoing", "pending", &request.id)?;
            match self.create_json(&path, &request) {
                Ok(()) => return Ok(request.id),
                Err(CreateNewJsonError::AlreadyExists) => continue,
                Err(CreateNewJsonError::Other(message)) => return Err(message),
            }
        }
    }

    /// Mark an incoming approval ID as approved by moving/writing it to
    /// approved.
    pub fn approve_incoming(&self, id: &str) -> Result<(), String> {
        self.approve("incoming", id)
    }

    /// Mark an incoming approval ID as denied by moving/writing it to denied.
    pub fn deny_incoming(&self, id: &str) -> Result<(), String> {
        self.deny("incoming", id)
    }

    /// Mark an outgoing approval ID as approved by moving/writing it to
    /// approved.
    pub fn approve_outgoing(&self, id: &str) -> Result<(), String> {
        self.approve("outgoing", id)
    }

    /// Mark an outgoing approval ID as denied by moving/writing it to denied.
    pub fn deny_outgoing(&self, id: &str) -> Result<(), String> {
        self.deny("outgoing", id)
    }

    fn outgoing_pending_exists(&self, id: &str) -> Result<bool, String> {
        self.file_exists(&self.approval_path("outgoing", "pending", id)?)
    }

    fn outgoing_sending_exists(&self, id: &str) -> Result<bool, String> {
        self.file_exists(&self.approval_path("outgoing", "sending", id)?)
    }

    fn outgoing_denied_exists(&self, id: &str) -> Result<bool, String> {
        self.file_exists(&self.approval_path("outgoing", "denied", id)?)
    }

    fn claim_outgoing(&self, id: &str) -> Result<OutgoingApproval, String> {
        if self.outgoing_denied_exists(id)? {
            return Err(format!(
                "Outgoing email {id} is denied; refusing to approve or send it."
            ));
        }
        let approval = self.pending_outgoing_by_id(id)?;
        let mut sending = approval.clone();
        sending.status = "sending".to_owned();
        let sending_path = self.approval_path("outgoing", "sending", id)?;
        match self.create_json(&sending_path, &sending) {
            Ok(()) => {}
            Err(CreateNewJsonError::AlreadyExists) => {
                return Err(format!("approval `{id}` is already being sent"));
            }
            Err(CreateNewJsonError::Other(message)) => return Err(message),
        }
        let pending_path = self.approval_path("outgoing", "pending", id)?;
        if let Err(error) = self.storage.delete_file(&pending_path) {
            let _ = self.storage.delete_file(&sending_path);
            return Err(error);
        }
        Ok(approval)
    }

    fn complete_outgoing(&self, id: &str, message_id: &str) -> Result<(), String> {
        let mut approval = self.sending_outgoing_by_id(id)?;
        approval.status = "approved".to_owned();
        approval.sent_message_id = Some(safe_model_line(message_id, MAX_HEADER_VALUE_CHARS));
        let approved_path = self.approval_path("outgoing", "approved", id)?;
        match self.create_json(&approved_path, &approval) {
            Ok(()) | Err(CreateNewJsonError::AlreadyExists) => {}
            Err(CreateNewJsonError::Other(message)) => return Err(message),
        }
        self.storage
            .delete_file(&self.approval_path("outgoing", "sending", id)?)
    }

    fn approve(&self, kind: &str, id: &str) -> Result<(), String> {
        self.move_pending_approval(kind, id, "approved")
    }

    fn deny(&self, kind: &str, id: &str) -> Result<(), String> {
        self.move_pending_approval(kind, id, "denied")
    }

    fn move_pending_approval(&self, kind: &str, id: &str, new_status: &str) -> Result<(), String> {
        validate_approval_id(id)?;
        let from = self.approval_path(kind, "pending", id)?;
        let to = self.approval_path(kind, new_status, id)?;
        if let Some(bytes) = self.read_file(&from)? {
            let mut record: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|error| format!("failed to parse {from}: {error}"))?;
            validate_approval_record(&record, kind, "pending", id)?;
            record["status"] = serde_json::Value::String(new_status.to_owned());
            match self.create_json(&to, &record) {
                Ok(()) | Err(CreateNewJsonError::AlreadyExists) => {}
                Err(CreateNewJsonError::Other(message)) => return Err(message),
            }
            self.storage.delete_file(&from)
        } else if self.file_exists(&to)? {
            Ok(())
        } else {
            Err(format!("approval `{id}` not found"))
        }
    }

    fn incoming_approved_exact(&self, target: &IncomingTarget, metadata: &BackendMessage) -> bool {
        self.list_incoming_approvals("approved")
            .is_ok_and(|approvals| {
                approvals.iter().any(|approval| {
                    incoming_approval_matches_target_tuple(approval, target)
                        && incoming_approval_matches_message_metadata(approval, metadata)
                })
            })
    }

    fn incoming_denied_exact(&self, target: &IncomingTarget, metadata: &BackendMessage) -> bool {
        self.list_incoming_approvals("denied")
            .is_ok_and(|approvals| {
                approvals.iter().any(|approval| {
                    incoming_approval_matches_target_tuple(approval, target)
                        && incoming_approval_matches_message_metadata(approval, metadata)
                })
            })
    }

    fn outgoing_approved_exact(&self, message: &OutgoingMessage) -> bool {
        self.list_outgoing_approvals("approved")
            .is_ok_and(|approvals| {
                approvals
                    .iter()
                    .any(|approval| outgoing_approval_matches_outgoing_message(approval, message))
            })
    }

    fn approval_path(&self, kind: &str, status: &str, id: &str) -> Result<String, String> {
        approval_prefix(kind)?;
        validate_approval_id(id)?;
        Ok(format!("approvals/{kind}/{status}/{id}.json"))
    }

    fn next_approval_id(&self, kind: &str) -> Result<String, String> {
        let mut max_id = 0_u64;
        for status in ["pending", "sending", "approved", "denied"] {
            let dir = format!("approvals/{kind}/{status}");
            for entry in self.storage.list_files(&dir)? {
                if entry.is_dir || !entry.path.ends_with(".json") {
                    continue;
                }
                let Some(stem) = file_name(&entry.path).and_then(|name| name.strip_suffix(".json"))
                else {
                    continue;
                };
                if let Ok(id) = stem.parse::<u64>()
                    && max_id < id
                {
                    max_id = id;
                }
            }
        }
        Ok((max_id + 1).to_string())
    }
}

fn approval_prefix(kind: &str) -> Result<&'static str, String> {
    match kind {
        "incoming" => Ok("in"),
        "outgoing" => Ok("out"),
        _ => Err(format!("invalid approval kind `{kind}`")),
    }
}

fn validate_approval_id(id: &str) -> Result<(), String> {
    let Ok(value) = id.parse::<u64>() else {
        return Err(format!("invalid approval id `{id}`"));
    };
    if value == 0 || id.contains(['/', '\\', '\0']) || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("invalid approval id `{id}`"));
    }
    Ok(())
}

fn validate_google_stored_auth(
    auth: &EmailGoogleStoredAuth,
    expected_account: Option<&str>,
) -> Result<(), String> {
    if auth.schema != GOOGLE_AUTH_SCHEMA {
        return Err("Google auth has unsupported schema".to_owned());
    }
    validate_google_auth_account(&auth.account, expected_account)?;
    if !is_safe_persisted_line(&auth.refresh_token, MAX_GOOGLE_AUTH_FIELD_CHARS) {
        return Err("Google auth refresh token contains unsafe text".to_owned());
    }
    Ok(())
}

fn validate_google_pending_auth(
    pending: &EmailGooglePendingAuth,
    expected_account: Option<&str>,
) -> Result<(), String> {
    if pending.schema != GOOGLE_AUTH_PENDING_SCHEMA {
        return Err("pending Google auth has unsupported schema".to_owned());
    }
    validate_google_auth_account(&pending.account, expected_account)?;
    match &pending.flow {
        EmailGooglePendingAuthFlow::InstalledApp {
            state,
            pkce_verifier,
            redirect_uri,
            created_at_unix_ms,
            expires_at_unix_ms,
        } => {
            for (field, value) in [
                ("state", state.as_str()),
                ("pkce_verifier", pkce_verifier.as_str()),
                ("redirect_uri", redirect_uri.as_str()),
            ] {
                if !is_safe_persisted_line(value, MAX_GOOGLE_AUTH_FIELD_CHARS) {
                    return Err(format!(
                        "pending Google auth field `{field}` contains unsafe text"
                    ));
                }
            }
            if !is_valid_pkce_verifier(pkce_verifier) {
                return Err("pending Google auth PKCE verifier is invalid".to_owned());
            }
            validate_loopback_redirect_uri(redirect_uri)?;
            if *created_at_unix_ms == 0 || *expires_at_unix_ms <= *created_at_unix_ms {
                return Err("pending Google auth timing is invalid".to_owned());
            }
        }
    }
    Ok(())
}

fn validate_google_auth_account(
    account: &str,
    expected_account: Option<&str>,
) -> Result<(), String> {
    if account.trim().is_empty() || !is_safe_persisted_line(account, MAX_DISPLAY_LINE_CHARS) {
        return Err("Google auth account id is invalid".to_owned());
    }
    if let Some(expected_account) = expected_account
        && account != expected_account
    {
        return Err("Google auth account id mismatch".to_owned());
    }
    Ok(())
}

fn account_file_stem(account: &str) -> String {
    let digest = blake3::hash(account.as_bytes());
    digest.to_hex()[..16].to_owned()
}

fn is_safe_persisted_line(value: &str, max_chars: usize) -> bool {
    value.chars().count() <= max_chars
        && !value
            .chars()
            .any(|ch| ch.is_control() || is_unsafe_format_control(ch))
}

fn is_unapproved_subject_preview_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, ',' | ';' | '.' | ' ' | '-')
}

fn is_safe_unapproved_subject_preview(value: &str) -> bool {
    value.chars().count() <= UNAPPROVED_SUBJECT_PREVIEW_MAX_CHARS
        && value.chars().all(is_unapproved_subject_preview_char)
}

fn validate_optional_persisted_line(
    value: Option<&String>,
    field: &str,
    max_chars: usize,
) -> Result<(), String> {
    if let Some(value) = value
        && !is_safe_persisted_line(value, max_chars)
    {
        return Err(format!("approval field `{field}` contains unsafe text"));
    }
    Ok(())
}

fn validate_incoming_approval(
    approval: &IncomingApproval,
    expected_status: &str,
    expected_id: Option<&str>,
) -> Result<(), String> {
    if approval.schema != STATE_SCHEMA {
        return Err(format!("approval `{}` has unsupported schema", approval.id));
    }
    validate_approval_id(&approval.id)?;
    if let Some(expected_id) = expected_id
        && approval.id != expected_id
    {
        return Err(format!(
            "approval `{expected_id}` has mismatched embedded id"
        ));
    }
    if approval.kind != "incoming_read" {
        return Err(format!(
            "approval `{}` has mismatched embedded kind",
            approval.id
        ));
    }
    if approval.status != expected_status {
        return Err(format!(
            "approval `{}` has mismatched embedded status",
            approval.id
        ));
    }
    if approval.account.trim().is_empty()
        || approval.folder.trim().is_empty()
        || !is_single_uid(&approval.uid)
        || !is_safe_persisted_line(&approval.account, MAX_HEADER_VALUE_CHARS)
        || !is_safe_persisted_line(&approval.folder, MAX_HEADER_VALUE_CHARS)
        || !is_safe_persisted_line(&approval.uidvalidity, MAX_HEADER_VALUE_CHARS)
        || !is_safe_persisted_line(&approval.from, MAX_ADDRESS_CHARS)
        || !is_safe_persisted_line(&approval.date, MAX_HEADER_VALUE_CHARS)
        || !is_safe_unapproved_subject_preview(&approval.subject_preview)
        || !is_safe_persisted_line(&approval.reason, MAX_HEADER_VALUE_CHARS)
    {
        return Err(format!(
            "approval `{}` contains unsafe metadata",
            approval.id
        ));
    }
    validate_optional_persisted_line(
        approval.message_id.as_ref(),
        "message_id",
        MAX_HEADER_VALUE_CHARS,
    )
}

fn validate_outgoing_approval(
    approval: &OutgoingApproval,
    expected_status: &str,
    expected_id: Option<&str>,
) -> Result<(), String> {
    validate_outgoing_approval_identity(approval, expected_status, expected_id)?;
    validate_outgoing_approval_metadata(approval)?;
    validate_outgoing_optional_fields(approval)?;
    validate_outgoing_recipient_lists(approval)
}

fn validate_outgoing_approval_identity(
    approval: &OutgoingApproval,
    expected_status: &str,
    expected_id: Option<&str>,
) -> Result<(), String> {
    if approval.schema != STATE_SCHEMA {
        return Err(format!("approval `{}` has unsupported schema", approval.id));
    }
    validate_approval_id(&approval.id)?;
    if let Some(expected_id) = expected_id
        && approval.id != expected_id
    {
        return Err(format!(
            "approval `{expected_id}` has mismatched embedded id"
        ));
    }
    if approval.kind != "outgoing_send" {
        return Err(format!(
            "approval `{}` has mismatched embedded kind",
            approval.id
        ));
    }
    if approval.status != expected_status {
        return Err(format!(
            "approval `{}` has mismatched embedded status",
            approval.id
        ));
    }
    Ok(())
}

fn validate_outgoing_approval_metadata(approval: &OutgoingApproval) -> Result<(), String> {
    if outgoing_approval_has_unsafe_metadata(approval) {
        return Err(format!(
            "approval `{}` contains unsafe metadata",
            approval.id
        ));
    }
    Ok(())
}

fn outgoing_approval_has_unsafe_metadata(approval: &OutgoingApproval) -> bool {
    approval.account.trim().is_empty()
        || approval.from.trim().is_empty()
        || approval.to.is_empty()
        || !is_safe_persisted_line(&approval.account, MAX_HEADER_VALUE_CHARS)
        || !is_safe_persisted_line(&approval.from, MAX_HEADER_VALUE_CHARS)
        || !is_safe_persisted_line(&approval.subject, MAX_HEADER_VALUE_CHARS)
        || !is_safe_persisted_line(&approval.reason, MAX_HEADER_VALUE_CHARS)
        || !outgoing_body_within_persisted_limits(&approval.body_text)
}

fn outgoing_body_within_persisted_limits(body_text: &str) -> bool {
    body_text.len() <= READ_BODY_MAX_BYTES && body_text.lines().count() <= READ_BODY_MAX_LINES
}

fn validate_outgoing_optional_fields(approval: &OutgoingApproval) -> Result<(), String> {
    validate_optional_persisted_line(approval.reply_to.as_ref(), "reply_to", MAX_ADDRESS_CHARS)?;
    validate_optional_persisted_line(
        approval.in_reply_to.as_ref(),
        "in_reply_to",
        MAX_HEADER_VALUE_CHARS,
    )?;
    validate_optional_persisted_line(
        approval.sent_message_id.as_ref(),
        "sent_message_id",
        MAX_HEADER_VALUE_CHARS,
    )?;
    Ok(())
}

fn validate_outgoing_recipient_lists(approval: &OutgoingApproval) -> Result<(), String> {
    validate_recipient_values(&approval.to)?;
    validate_recipient_values(&approval.cc)?;
    validate_recipient_values(&approval.bcc)?;
    validate_recipient_values(&approval.blocked_recipients)
}

fn validate_recipient_values(values: &[String]) -> Result<(), String> {
    if MAX_RECIPIENTS < values.len() {
        return Err("approval contains too many recipients".to_owned());
    }
    for value in values {
        if normalize_address(value).is_none() || !is_safe_persisted_line(value, MAX_ADDRESS_CHARS) {
            return Err("approval contains an invalid recipient".to_owned());
        }
    }
    Ok(())
}

fn incoming_approval_matches_target(left: &IncomingApproval, right: &IncomingApproval) -> bool {
    left.account == right.account
        && left.folder == right.folder
        && left.uid == right.uid
        && left.uidvalidity == right.uidvalidity
        && left.from == right.from
        && left.date == right.date
        && left.message_id == right.message_id
}

fn incoming_approval_matches_target_tuple(
    approval: &IncomingApproval,
    target: &IncomingTarget,
) -> bool {
    approval.account == safe_model_line(&target.account, MAX_HEADER_VALUE_CHARS)
        && approval.folder == safe_model_line(&target.folder, MAX_HEADER_VALUE_CHARS)
        && approval.uid == safe_model_line(&target.uid, MAX_HEADER_VALUE_CHARS)
        && approval.uidvalidity == safe_model_line(&target.uidvalidity, MAX_HEADER_VALUE_CHARS)
}

fn incoming_approval_matches_message_metadata(
    approval: &IncomingApproval,
    metadata: &BackendMessage,
) -> bool {
    approval.from == incoming_approval_from(metadata)
        && approval.date == safe_model_line(&metadata.date, MAX_HEADER_VALUE_CHARS)
        && approval.message_id == incoming_approval_message_id(metadata)
}

fn incoming_approval_from(message: &BackendMessage) -> String {
    safe_model_line(
        &normalize_address(&message.from).unwrap_or_else(|| message.from.clone()),
        MAX_ADDRESS_CHARS,
    )
}

fn incoming_approval_message_id(message: &BackendMessage) -> Option<String> {
    message
        .message_id
        .as_deref()
        .map(|message_id| safe_model_line(message_id, MAX_HEADER_VALUE_CHARS))
}

fn outgoing_approval_matches_message(left: &OutgoingApproval, right: &OutgoingApproval) -> bool {
    left.account == right.account
        && left.from == right.from
        && left.to == right.to
        && left.cc == right.cc
        && left.bcc == right.bcc
        && left.subject == right.subject
        && left.body_text == right.body_text
        && left.reply_to == right.reply_to
        && left.in_reply_to == right.in_reply_to
}

fn outgoing_approval_matches_outgoing_message(
    approval: &OutgoingApproval,
    message: &OutgoingMessage,
) -> bool {
    approval.account == message.account
        && approval.from == message.from
        && approval.to == message.to
        && approval.cc == message.cc
        && approval.bcc == message.bcc
        && approval.subject == message.subject
        && approval.body_text == message.body_text
        && approval.reply_to == message.reply_to
        && approval.in_reply_to == message.in_reply_to
}

fn validate_approval_record(
    record: &serde_json::Value,
    kind: &str,
    expected_status: &str,
    id: &str,
) -> Result<(), String> {
    let expected_kind = match kind {
        "incoming" => "incoming_read",
        "outgoing" => "outgoing_send",
        _ => return Err(format!("invalid approval kind `{kind}`")),
    };
    if record.get("schema").and_then(serde_json::Value::as_u64) != Some(u64::from(STATE_SCHEMA)) {
        return Err(format!("approval `{id}` has unsupported schema"));
    }
    let field = |name: &str| record.get(name).and_then(serde_json::Value::as_str);
    if field("id") != Some(id) {
        return Err(format!("approval `{id}` has mismatched embedded id"));
    }
    if field("kind") != Some(expected_kind) {
        return Err(format!("approval `{id}` has mismatched embedded kind"));
    }
    if field("status") != Some(expected_status) {
        return Err(format!("approval `{id}` has mismatched embedded status"));
    }
    Ok(())
}

#[derive(Debug)]
enum CreateNewJsonError {
    AlreadyExists,
    Other(String),
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// IMAP flag mutation requested by safe message-management commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageFlagMutation {
    /// Add the IMAP `\\Seen` flag.
    AddSeen,
    /// Remove the IMAP `\\Seen` flag.
    RemoveSeen,
    /// Add the IMAP `\\Flagged` flag.
    AddFlagged,
    /// Remove the IMAP `\\Flagged` flag.
    RemoveFlagged,
}

impl MessageFlagMutation {
    fn imap_store_query(self) -> &'static str {
        match self {
            Self::AddSeen => "+FLAGS.SILENT (\\Seen)",
            Self::RemoveSeen => "-FLAGS.SILENT (\\Seen)",
            Self::AddFlagged => "+FLAGS.SILENT (\\Flagged)",
            Self::RemoveFlagged => "-FLAGS.SILENT (\\Flagged)",
        }
    }
}

/// Classified failure from one SMTP submission attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EmailSendFailure {
    /// Tau can prove the SMTP server did not accept the message for delivery.
    NotDispatched(String),
    /// SMTP submission began but Tau did not receive a conclusive acceptance or
    /// rejection result.
    OutcomeUnknown(String),
}

/// Minimal backend abstraction used by command handlers.
pub trait EmailBackend {
    /// Capture the current time for one recent-message listing operation.
    fn current_time(&self) -> SystemTime {
        SystemTime::now()
    }
    /// List folders known to the backend for an account.
    fn list_folders(&self, account: &str) -> Result<Vec<BackendFolder>, String>;
    /// List messages known to the backend for an internal account/folder.
    fn list_messages(&self, account: &str, folder: &str) -> Result<Vec<BackendMessage>, String>;
    /// List one redaction-safe metadata page in descending UID order.
    fn list_messages_by_uid_page(
        &self,
        account: &str,
        folder: &str,
        limit: usize,
        offset: usize,
    ) -> Result<BackendMessagePage, String> {
        let messages = self.list_messages(account, folder)?;
        paged_messages(messages, limit, offset)
    }
    /// List one redaction-safe metadata page whose IMAP internal timestamps are
    /// within the rolling duration ending at `now`.
    fn list_recent_messages_page(
        &self,
        account: &str,
        folder: &str,
        limit: usize,
        offset: usize,
        days: u32,
        now: SystemTime,
    ) -> Result<BackendMessagePage, String> {
        let messages = self.list_messages(account, folder)?;
        paged_recent_messages(messages, limit, offset, days, now)
    }
    /// Fetch metadata needed for incoming policy checks without fetching the
    /// full body when the backend can support that.
    fn message_metadata(
        &self,
        account: &str,
        folder: &str,
        uid: &str,
    ) -> Result<BackendMessage, String> {
        self.read_message(account, folder, uid)
    }
    /// Read one message by UID after policy has allowed the content fetch.
    fn read_message(
        &self,
        account: &str,
        folder: &str,
        uid: &str,
    ) -> Result<BackendMessage, String>;
    /// Add or remove a standard IMAP flag for one message by UID.
    fn update_message_flags(
        &mut self,
        account: &str,
        folder: &str,
        uid: &str,
        mutation: MessageFlagMutation,
    ) -> Result<(), String>;
    /// Move one message by UID from the selected folder to the account's Trash
    /// mailbox.
    fn move_message_to_trash(
        &mut self,
        account: &str,
        folder: &str,
        uid: &str,
    ) -> Result<String, String>;
    /// Send one already-approved outgoing message.
    fn send_message(&mut self, message: &OutgoingMessage) -> Result<String, EmailSendFailure>;
    /// Start Google OAuth installed-app authorization and return
    /// authorization_url, state, pkce_verifier, redirect_uri, and
    /// expires_in_secs.
    fn start_google_installed_app_auth(
        &self,
        _account: &str,
    ) -> Result<(String, String, String, String, u64), String> {
        Err("auth_error: Google email OAuth is not available in this backend".to_owned())
    }
    /// Finish Google OAuth installed-app authorization and return
    /// refresh_token, access_token, and expires_in_secs.
    fn finish_google_installed_app_auth(
        &self,
        _account: &str,
        _code: &str,
        _pkce_verifier: &str,
        _redirect_uri: &str,
    ) -> Result<(String, Option<String>, Option<u64>), String> {
        Err("auth_error: Google email OAuth is not available in this backend".to_owned())
    }
    /// Prime any backend access-token cache with a fresh Google access token.
    fn prime_google_access_token_cache(
        &self,
        _account: &str,
        _access_token: String,
        _expires_in_secs: Option<u64>,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// One page of backend message metadata.
pub struct BackendMessagePage {
    /// Messages in display order.
    pub messages: Vec<BackendMessage>,
    /// Opaque next cursor when more data is available.
    pub next_cursor: Option<String>,
    /// Whether additional messages are available after this page.
    pub truncated: bool,
}

/// Backend folder metadata.
#[derive(Clone)]
pub struct BackendFolder {
    /// Folder name.
    pub name: String,
    /// Hierarchy delimiter.
    pub delimiter: String,
    /// Whether the folder is selectable.
    pub selectable: bool,
}

/// Backend attachment metadata. Attachment content is intentionally not exposed
/// by the Phase E backend.
#[derive(Clone, Default)]
pub struct BackendAttachment {
    /// Optional attachment file name.
    pub filename: Option<String>,
    /// Optional MIME content type, such as `application/pdf`.
    pub content_type: Option<String>,
    /// Decoded attachment size when known.
    pub size_bytes: Option<u64>,
}

/// Parsed server-provided Authentication-Results evidence used internally for
/// incoming auto-read policy. Raw authentication headers are never exposed in
/// model-visible tool output.
#[derive(Clone, Default)]
pub struct AuthenticationResultsEvidence {
    /// Authserv-id that produced the Authentication-Results header.
    pub authserv_id: String,
    /// DMARC result token, such as `pass` or `fail`.
    pub dmarc_result: Option<String>,
    /// DMARC `header.from` domain reported by the trusted server.
    pub dmarc_header_from: Option<String>,
    /// DKIM result token, such as `pass` or `fail`.
    pub dkim_result: Option<String>,
    /// DKIM `header.d` domain reported by the trusted server.
    pub dkim_header_d: Option<String>,
}

/// Backend message fixture/metadata.
#[derive(Clone)]
pub struct BackendMessage {
    /// IMAP UID or fixture UID.
    pub uid: String,
    /// UIDVALIDITY used to bind approvals exactly.
    pub uidvalidity: String,
    /// Message date string.
    pub date: String,
    /// Sender header/address.
    pub from: String,
    /// Recipient addresses.
    pub to: Vec<String>,
    /// CC recipient addresses.
    pub cc: Vec<String>,
    /// Message subject.
    pub subject: String,
    /// Message body text. Metadata-only fetches leave this empty.
    pub body_text: String,
    /// Whether the backend could not inspect the complete source message or
    /// metadata headers, such as after a bounded fetch or RFC822 parse failure.
    pub source_truncated: bool,
    /// Minimal flags.
    pub flags: Vec<String>,
    /// Whether the message appears to have attachments.
    pub has_attachments: bool,
    /// Attachment metadata, populated only after an allowed full read.
    pub attachments: Vec<BackendAttachment>,
    /// Optional Message-ID header.
    pub message_id: Option<String>,
    /// Parsed Authentication-Results evidence in header order, newest first.
    /// Raw auth headers are never exposed to model-visible output.
    pub auth_results: Vec<AuthenticationResultsEvidence>,
}

/// Exact incoming read approval target.
#[derive(Serialize, Deserialize)]
pub struct IncomingTarget {
    /// Account ID.
    pub account: String,
    /// Folder name.
    pub folder: String,
    /// Message UID.
    pub uid: String,
    /// Mailbox UIDVALIDITY.
    pub uidvalidity: String,
}

/// Persisted incoming approval record.
#[derive(Clone, Serialize, Deserialize)]
pub struct IncomingApproval {
    /// Schema version.
    pub schema: u32,
    /// Opaque stable approval ID.
    pub id: String,
    /// Approval kind.
    pub kind: String,
    /// Approval status.
    pub status: String,
    /// Account ID.
    pub account: String,
    /// Folder name.
    pub folder: String,
    /// Message UID.
    pub uid: String,
    /// Mailbox UIDVALIDITY.
    pub uidvalidity: String,
    /// Sender address/header.
    pub from: String,
    /// Message date.
    pub date: String,
    /// Optional Message-ID captured to detect stale or overwritten approvals.
    #[serde(default)]
    pub message_id: Option<String>,
    /// Whether subject is redacted in the approval-required tool output.
    pub subject_redacted: bool,
    /// Sanitized subject preview visible before approval. This is deliberately
    /// lossy and does not participate in approval matching.
    #[serde(default)]
    pub subject_preview: String,
    /// Denial/approval reason.
    pub reason: String,
}

/// Outgoing message submitted by the tool.
#[derive(Serialize, Deserialize)]
pub struct OutgoingMessage {
    /// Account ID.
    pub account: String,
    /// From identity.
    pub from: String,
    /// To recipients.
    pub to: Vec<String>,
    /// CC recipients.
    pub cc: Vec<String>,
    /// BCC recipients; stored for approval but never leaked in unrelated
    /// outputs.
    pub bcc: Vec<String>,
    /// Subject.
    pub subject: String,
    /// Body text.
    pub body_text: String,
    /// Optional Reply-To header.
    pub reply_to: Option<String>,
    /// Optional In-Reply-To message identifier.
    pub in_reply_to: Option<String>,
}

/// Raw arguments supplied to the outgoing send command before validation.
struct SendArgs {
    /// Requested account id, or `None` to use the configured send account.
    account: Option<String>,
    /// Requested From value, or `None` to use the account identity.
    from: Option<String>,
    /// To recipients.
    to: Vec<String>,
    /// CC recipients.
    cc: Vec<String>,
    /// BCC recipients.
    bcc: Vec<String>,
    /// Subject header value.
    subject: String,
    /// Plain text body.
    body_text: String,
    /// Optional Reply-To recipient-like header.
    reply_to: Option<String>,
    /// Optional In-Reply-To message identifier.
    in_reply_to: Option<String>,
}

/// Persisted outgoing approval record.
#[derive(Clone, Serialize, Deserialize)]
pub struct OutgoingApproval {
    /// Schema version.
    pub schema: u32,
    /// Opaque stable approval ID.
    pub id: String,
    /// Approval kind.
    pub kind: String,
    /// Approval status.
    pub status: String,
    /// Account ID.
    pub account: String,
    /// From identity.
    pub from: String,
    /// To recipients.
    pub to: Vec<String>,
    /// CC recipients.
    pub cc: Vec<String>,
    /// BCC recipients.
    pub bcc: Vec<String>,
    /// Subject.
    pub subject: String,
    /// Body text.
    pub body_text: String,
    /// Optional Reply-To header.
    pub reply_to: Option<String>,
    /// Optional In-Reply-To message identifier.
    pub in_reply_to: Option<String>,
    /// Recipients blocked by current policy.
    pub blocked_recipients: Vec<String>,
    /// Denial/approval reason.
    pub reason: String,
    /// SMTP Message-ID recorded after a successful approval send.
    #[serde(default)]
    pub sent_message_id: Option<String>,
}

fn outgoing_approval_message(approval: &OutgoingApproval) -> OutgoingMessage {
    OutgoingMessage {
        account: approval.account.clone(),
        from: approval.from.clone(),
        to: approval.to.clone(),
        cc: approval.cc.clone(),
        bcc: approval.bcc.clone(),
        subject: approval.subject.clone(),
        body_text: approval.body_text.clone(),
        reply_to: approval.reply_to.clone(),
        in_reply_to: approval.in_reply_to.clone(),
    }
}

fn stable_id<T: Serialize>(prefix: &str, value: &T) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let hash = blake3::hash(&bytes);
    format!("{prefix}_{}", &hash.to_hex()[..24])
}

struct BodyTruncation {
    body_text: String,
    truncated: bool,
    total_lines: u64,
    total_bytes: u64,
    shown_lines: u64,
    shown_bytes: u64,
}

fn truncate_body(body: &str) -> BodyTruncation {
    let total_bytes = body.len();
    let total_lines = body.lines().count();
    let mut shown = String::new();
    let mut truncated = false;

    for (line_index, line) in body.split_inclusive('\n').enumerate() {
        if line_index == READ_BODY_MAX_LINES {
            truncated = true;
            break;
        }
        if READ_BODY_MAX_BYTES < shown.len().saturating_add(line.len()) {
            let remaining = READ_BODY_MAX_BYTES.saturating_sub(shown.len());
            let mut boundary = 0usize;
            for (idx, ch) in line.char_indices() {
                let next = idx + ch.len_utf8();
                if remaining < next {
                    break;
                }
                boundary = next;
            }
            shown.push_str(&line[..boundary]);
            truncated = true;
            break;
        }
        shown.push_str(line);
    }

    if shown.len() < total_bytes {
        truncated = true;
    }

    BodyTruncation {
        shown_lines: shown.lines().count() as u64,
        shown_bytes: shown.len() as u64,
        body_text: shown,
        truncated,
        total_lines: total_lines as u64,
        total_bytes: total_bytes as u64,
    }
}

fn is_unsafe_format_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
    )
}

fn push_escaped_char(out: &mut String, ch: char, multiline: bool) {
    match ch {
        '\n' if multiline => out.push('\n'),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\u{1b}' => out.push_str("\\e"),
        '\u{7f}' => out.push_str("\\x7f"),
        ch if (ch as u32) <= 0x1f || (0x80..=0x9f).contains(&(ch as u32)) => {
            out.push_str(&format!("\\u{{{:04x}}}", ch as u32));
        }
        ch if is_unsafe_format_control(ch) => {
            out.push_str(&format!("\\u{{{:04x}}}", ch as u32));
        }
        ch => out.push(ch),
    }
}

fn safe_text(value: &str, max_chars: usize, multiline: bool) -> String {
    let mut out = String::new();
    for (index, ch) in value.chars().enumerate() {
        if index == max_chars {
            out.push('…');
            break;
        }
        push_escaped_char(&mut out, ch, multiline);
    }
    out
}

fn safe_display_line(value: &str) -> String {
    safe_text(value, MAX_DISPLAY_LINE_CHARS, false)
}

fn safe_oauth_url_display(value: &str) -> String {
    safe_text(value, MAX_GOOGLE_AUTH_FIELD_CHARS, false)
}

fn safe_display_text(value: &str) -> String {
    safe_text(value, READ_BODY_MAX_BYTES, true)
}

fn safe_model_text(value: &str, max_chars: usize) -> String {
    safe_text(value, max_chars, true)
}

fn safe_model_line(value: &str, max_chars: usize) -> String {
    safe_text(value, max_chars, false)
}

struct SimplifiedEmailContent {
    text: String,
    source: &'static str,
}

struct UnapprovedEmailPreview {
    text: String,
    source: &'static str,
    truncated: bool,
}

fn simplify_email_content(raw: &str) -> SimplifiedEmailContent {
    let source = email_body_source(raw);
    let text = match source {
        "html" => simplify_html_email(raw),
        "empty" => String::new(),
        _ => simplify_plain_email(raw),
    };
    let text = neutralize_email_angle_brackets(&text);
    SimplifiedEmailContent { text, source }
}

fn unapproved_email_preview(raw: &str) -> UnapprovedEmailPreview {
    let simplified = simplify_email_content(raw);
    let (text, truncated) = sanitize_unapproved_email_preview(&simplified.text);
    UnapprovedEmailPreview {
        text,
        source: simplified.source,
        truncated,
    }
}

fn wrap_external_untrusted_message(body: &str) -> String {
    format!(
        "<{EXTERNAL_UNTRUSTED_MESSAGE_TAG}>\n{}\n</{EXTERNAL_UNTRUSTED_MESSAGE_TAG}>",
        body.trim()
    )
}

fn email_body_source(raw: &str) -> &'static str {
    let trimmed = raw.trim_start();
    if trimmed.is_empty() {
        return "empty";
    }
    let probe = trimmed
        .chars()
        .take(2048)
        .collect::<String>()
        .to_ascii_lowercase();
    if probe.contains("<html")
        || probe.contains("<body")
        || probe.contains("<div")
        || probe.contains("<p")
        || probe.contains("<br")
        || probe.contains("<table")
        || probe.contains("<a ")
        || probe.contains("</")
    {
        "html"
    } else {
        "text"
    }
}

fn simplify_html_email(raw: &str) -> String {
    let mut html = remove_html_comments(raw);
    for tag in ["script", "style", "head", "svg"] {
        html = remove_html_block(&html, tag);
    }
    let mut out = String::new();
    let chars = html.chars().collect::<Vec<_>>();
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] == '<' {
            let start = index + 1;
            let Some(end_offset) = chars[start..].iter().position(|ch| *ch == '>') else {
                out.push(' ');
                break;
            };
            let end = start + end_offset;
            let tag = chars[start..end].iter().collect::<String>();
            let closing = tag.trim_start().starts_with('/');
            let name = html_tag_name(&tag);
            if !closing && is_html_link_tag(&name) {
                out.push_str(" LINK ");
            } else if is_html_block_tag(&name) {
                out.push('\n');
            }
            index = end + 1;
            continue;
        }
        out.push(chars[index]);
        index += 1;
    }
    simplify_plain_email(&decode_html_entities_basic(&out))
}

fn simplify_plain_email(raw: &str) -> String {
    let replaced = replace_links_in_text(raw);
    normalize_email_text(&replaced)
}

fn remove_html_comments(raw: &str) -> String {
    let mut out = String::new();
    let mut rest = raw;
    loop {
        let Some(start) = rest.find("<!--") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let after_start = start + 4;
        let Some(end) = rest[after_start..].find("-->") else {
            break;
        };
        rest = &rest[after_start + end + 3..];
    }
    out
}

fn remove_html_block(raw: &str, tag: &str) -> String {
    let mut out = String::new();
    let lower = raw.to_ascii_lowercase();
    let mut cursor = 0usize;
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    while let Some(relative_start) = lower[cursor..].find(&open) {
        let start = cursor + relative_start;
        out.push_str(&raw[cursor..start]);
        let search_from = start + open.len();
        let Some(relative_end) = lower[search_from..].find(&close) else {
            cursor = raw.len();
            break;
        };
        cursor = search_from + relative_end + close.len();
    }
    out.push_str(&raw[cursor..]);
    out
}

fn html_tag_name(tag: &str) -> String {
    tag.trim_start()
        .trim_start_matches('/')
        .trim_start()
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn is_html_link_tag(name: &str) -> bool {
    matches!(name, "a" | "area" | "link")
}

fn is_html_block_tag(name: &str) -> bool {
    matches!(
        name,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "br"
            | "caption"
            | "dd"
            | "div"
            | "dt"
            | "figcaption"
            | "footer"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hr"
            | "li"
            | "main"
            | "p"
            | "section"
            | "table"
            | "td"
            | "th"
            | "tr"
    )
}

fn decode_html_entities_basic(raw: &str) -> String {
    let mut out = String::new();
    let chars = raw.chars().collect::<Vec<_>>();
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] == '&'
            && let Some(end_offset) = chars[index + 1..].iter().take(16).position(|ch| *ch == ';')
        {
            let end = index + 1 + end_offset;
            let entity = chars[index + 1..end].iter().collect::<String>();
            if let Some(decoded) = decode_html_entity(&entity) {
                out.push(decoded);
                index = end + 1;
                continue;
            }
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

fn decode_html_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => decode_numeric_html_entity(entity),
    }
}

fn decode_numeric_html_entity(entity: &str) -> Option<char> {
    if let Some(hex) = entity
        .strip_prefix("#x")
        .or_else(|| entity.strip_prefix("#X"))
    {
        return u32::from_str_radix(hex, 16).ok().and_then(char::from_u32);
    }
    let decimal = entity.strip_prefix('#')?;
    decimal.parse::<u32>().ok().and_then(char::from_u32)
}

fn replace_links_in_text(raw: &str) -> String {
    let mut out = String::new();
    let mut token = String::new();
    for ch in raw.chars() {
        if ch.is_whitespace() {
            flush_link_token(&mut out, &mut token);
            out.push(ch);
        } else {
            token.push(ch);
        }
    }
    flush_link_token(&mut out, &mut token);
    out
}

fn flush_link_token(out: &mut String, token: &mut String) {
    if token.is_empty() {
        return;
    }
    if is_link_token(token) {
        out.push_str("LINK");
    } else {
        out.push_str(token);
    }
    token.clear();
}

fn is_link_token(token: &str) -> bool {
    let trimmed = token.trim_matches(|ch: char| {
        matches!(
            ch,
            '(' | ')' | '[' | ']' | '<' | '>' | ',' | '.' | ';' | ':'
        )
    });
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("www.")
        || lower.starts_with("mailto:")
}

fn normalize_email_text(raw: &str) -> String {
    let mut lines = Vec::new();
    let mut previous_blank = false;
    for line in raw.replace("\r\n", "\n").replace('\r', "\n").lines() {
        let trimmed = line.trim();
        if should_stop_email_text(trimmed, !lines.is_empty()) {
            break;
        }
        if trimmed.starts_with('>') {
            continue;
        }
        let collapsed = collapse_inline_whitespace(trimmed);
        if collapsed.is_empty() {
            if !previous_blank && !lines.is_empty() {
                lines.push(String::new());
                previous_blank = true;
            }
            continue;
        }
        lines.push(collapsed);
        previous_blank = false;
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

fn should_stop_email_text(line: &str, saw_content: bool) -> bool {
    if line == "--" || line == "-- " {
        return true;
    }
    let lower = line.to_ascii_lowercase();
    (saw_content && lower.starts_with("on ") && lower.contains(" wrote:"))
        || (saw_content && lower.starts_with("from:") && lower.contains('@'))
        || lower.starts_with("sent from my ")
        || lower.contains("confidentiality notice")
        || lower.starts_with("this email and any attachments")
}

fn collapse_inline_whitespace(raw: &str) -> String {
    let mut out = String::new();
    let mut previous_space = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            if !previous_space {
                out.push(' ');
                previous_space = true;
            }
        } else {
            out.push(ch);
            previous_space = false;
        }
    }
    out.trim().to_owned()
}

fn neutralize_email_angle_brackets(raw: &str) -> String {
    raw.chars()
        .map(|ch| match ch {
            '<' => '‹',
            '>' => '›',
            _ => ch,
        })
        .collect()
}

fn sanitize_unapproved_email_preview(raw: &str) -> (String, bool) {
    let mut out = String::new();
    let mut previous_space = true;
    let mut written = 0usize;
    let mut truncated = false;
    for ch in raw.chars() {
        if UNAPPROVED_BODY_PREVIEW_MAX_CHARS <= written {
            truncated = true;
            break;
        }
        let mapped = if ch.is_ascii_alphanumeric() || matches!(ch, ',' | '.' | '-') {
            ch
        } else {
            ' '
        };
        if mapped == ' ' {
            if previous_space {
                continue;
            }
            out.push(' ');
            previous_space = true;
        } else {
            out.push(mapped);
            previous_space = false;
        }
        written += 1;
    }
    if out.ends_with(' ') {
        out.pop();
    }
    (out, truncated)
}

fn unapproved_subject_preview(value: &str) -> String {
    let mut out = String::new();
    let mut chars = 0usize;
    let mut last_was_space = false;
    for ch in value.chars() {
        if UNAPPROVED_SUBJECT_PREVIEW_MAX_CHARS <= chars {
            break;
        }
        let ch = if is_unapproved_subject_preview_char(ch) {
            ch
        } else {
            ' '
        };
        if ch == ' ' {
            if out.is_empty() || last_was_space {
                continue;
            }
            last_was_space = true;
        } else {
            last_was_space = false;
        }
        out.push(ch);
        chars += 1;
    }
    if last_was_space {
        out.pop();
    }
    out
}

fn safe_model_vec(values: Vec<String>, max_items: usize, max_chars: usize) -> Vec<String> {
    values
        .into_iter()
        .take(max_items)
        .map(|value| safe_model_line(&value, max_chars))
        .collect()
}

fn safe_display_join<'a>(values: impl IntoIterator<Item = &'a String>, separator: &str) -> String {
    values
        .into_iter()
        .map(|value| safe_display_line(value))
        .collect::<Vec<_>>()
        .join(separator)
}

fn parse_cursor(cursor: Option<&str>) -> Result<usize, String> {
    match cursor {
        Some(cursor) => cursor
            .parse::<usize>()
            .map_err(|_| "cursor must be a non-negative integer offset".to_owned()),
        None => Ok(0),
    }
}

fn paged_messages(
    messages: Vec<BackendMessage>,
    limit: usize,
    offset: usize,
) -> Result<BackendMessagePage, String> {
    let truncated = offset.saturating_add(limit) < messages.len();
    let next_cursor = truncated.then(|| offset.saturating_add(limit).to_string());
    Ok(BackendMessagePage {
        messages: messages.into_iter().skip(offset).take(limit).collect(),
        next_cursor,
        truncated,
    })
}

/// Return the inclusive lower bound for a rolling recent-message duration.
pub(super) fn recent_cutoff(now: SystemTime, days: u32) -> Result<SystemTime, String> {
    now.checked_sub(Duration::from_secs(u64::from(days) * 24 * 60 * 60))
        .ok_or_else(|| {
            "internal_error: recent-message cutoff is before system time range".to_owned()
        })
}

fn paged_recent_messages(
    messages: Vec<BackendMessage>,
    limit: usize,
    offset: usize,
    days: u32,
    now: SystemTime,
) -> Result<BackendMessagePage, String> {
    let cutoff = recent_cutoff(now, days)?;
    let messages = messages
        .into_iter()
        .filter_map(|message| {
            let timestamp = DateTime::parse_from_rfc3339(&message.date)
                .map_err(|_| {
                    "invalid_data: IMAP message is missing a usable internal timestamp".to_owned()
                })
                .map(SystemTime::from);
            match timestamp {
                Ok(timestamp) if cutoff <= timestamp && timestamp <= now => Some(Ok(message)),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    paged_messages(messages, limit, offset)
}

fn format_folder_line(account_id: &str, folder: BackendFolder) -> CborValue {
    let flags = if folder.selectable {
        "selectable"
    } else {
        "noselect"
    };
    CborValue::Text(format!(
        "{} {}",
        flatten_folder_id(account_id, &folder.name),
        flags
    ))
}

fn format_read_headers(
    from: &str,
    message: &BackendMessage,
    subject_name: &str,
    subject: &str,
    trusted: bool,
    source: &str,
) -> String {
    let recipients = |values: &[String]| {
        let values = safe_model_vec(values.to_vec(), MAX_RECIPIENTS, MAX_ADDRESS_CHARS);
        if values.is_empty() {
            "-".to_owned()
        } else {
            values.join(";")
        }
    };
    let message_id = message
        .message_id
        .as_deref()
        .map(|value| safe_model_line(value, MAX_HEADER_VALUE_CHARS))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "-".to_owned());
    let subject = safe_model_line(subject, MAX_HEADER_VALUE_CHARS);
    let subject = if subject.is_empty() { "-" } else { &subject };
    format!(
        "from={}, to={}, cc={}, date={}, {subject_name}={}, message_id={}, trusted={}, source={}, simplified=true",
        list_token(from, MAX_ADDRESS_CHARS),
        recipients(&message.to),
        recipients(&message.cc),
        list_token(&message.date, MAX_HEADER_VALUE_CHARS),
        subject,
        message_id,
        trusted,
        source,
    )
}

fn header_text_field<'a>(data: &'a CborValue, name: &str) -> Option<&'a str> {
    let headers = cbor_text_field(data, "headers")?;
    let prefix = format!("{name}=");
    let value = headers
        .split(", ")
        .find_map(|part| part.strip_prefix(&prefix))?;
    (value != "-").then_some(value)
}

fn format_list_message_line(message: BackendMessage, access: &str) -> String {
    let readable = access == ACCESS_FULL;
    let mut flags = safe_model_vec(message.flags, MAX_FLAGS, MAX_HEADER_VALUE_CHARS);
    if !readable {
        flags.push("redacted".to_owned());
    }
    let flags = if flags.is_empty() {
        "-".to_owned()
    } else {
        flags.join(",")
    };
    let attachments = if readable {
        let has_attachments = message.has_attachments || !message.attachments.is_empty();
        if message.attachments.is_empty() && has_attachments {
            "1+".to_owned()
        } else {
            message.attachments.len().to_string()
        }
    } else {
        "?".to_owned()
    };
    let subject = if readable {
        safe_model_line(&message.subject, MAX_HEADER_VALUE_CHARS)
    } else {
        unapproved_subject_preview(&message.subject)
    };
    let subject = if subject.is_empty() {
        "-".to_owned()
    } else {
        subject
    };
    format!(
        "{} {} {} {} {} {} {}",
        list_token(&message.uid, MAX_HEADER_VALUE_CHARS),
        list_token(&message.date, MAX_HEADER_VALUE_CHARS),
        list_token(
            &normalize_address(&message.from).unwrap_or(message.from),
            MAX_ADDRESS_CHARS,
        ),
        list_token(&flags, MAX_HEADER_VALUE_CHARS),
        access,
        attachments,
        subject
    )
}

fn list_token(value: &str, max_chars: usize) -> String {
    use std::fmt::Write as _;
    let value = safe_model_line(value, max_chars);
    let mut token = String::new();
    for ch in value.chars() {
        if ch.is_whitespace() || ch == '%' {
            let mut buf = [0; 4];
            for byte in ch.encode_utf8(&mut buf).as_bytes() {
                let _ = write!(&mut token, "%{byte:02X}");
            }
        } else {
            token.push(ch);
        }
    }
    if token.is_empty() {
        "-".to_owned()
    } else {
        token
    }
}

fn is_single_uid(uid: &str) -> bool {
    uid.parse::<u32>()
        .is_ok_and(|value| 0 < value && uid.bytes().all(|byte| byte.is_ascii_digit()))
}

type IncomingAccessTarget = (
    String,
    BackendMessage,
    IncomingTarget,
    &'static str,
    PolicyDecision,
);

struct Engine<B> {
    config: ValidatedConfig,
    state: StateStore,
    backend: B,
}

impl<B: EmailBackend> Engine<B> {
    fn dispatch(&mut self, command: EmailCommand) -> CborValue {
        let log_command = command.clone();
        let result = match command {
            EmailCommand::ListFolders { .. } => self.list_folders(),
            EmailCommand::ListByUid {
                account,
                folder,
                limit,
                cursor,
            } => self.list_by_uid(&account, &folder, limit, cursor.as_deref()),
            EmailCommand::ListRecent {
                account,
                folder,
                limit,
                cursor,
                days,
            } => self.list_recent(&account, &folder, limit, cursor.as_deref(), days),
            EmailCommand::Read {
                account,
                folder,
                uid,
            } => self.read(&account, &folder, &uid),
            EmailCommand::RequestFull {
                account,
                folder,
                uid,
            } => self.request_access(&account, &folder, &uid),
            EmailCommand::ManageMessage {
                command,
                account,
                folder,
                uid,
            } => self.manage_message(command, &account, &folder, &uid),
            EmailCommand::Trash {
                account,
                folder,
                uid,
            } => self.trash(&account, &folder, &uid),
            EmailCommand::Send {
                account,
                from,
                to,
                cc,
                bcc,
                subject,
                body_text,
                reply_to,
                in_reply_to,
            } => self.send(
                account,
                from,
                to,
                cc,
                bcc,
                subject,
                body_text,
                reply_to,
                in_reply_to,
            ),
        };
        self.append_email_log_for_command(&log_command, &result);
        result
    }

    fn append_email_log_for_command(&self, command: &EmailCommand, result: &CborValue) {
        let Some(entry) = self.email_log_entry(command, result) else {
            return;
        };
        if let Err(_message) = self.state.append_email_log(&entry) {
            tracing::warn!(target: LOG_TARGET, "failed to append email log");
        }
    }

    fn email_log_entry(&self, command: &EmailCommand, result: &CborValue) -> Option<EmailLogEntry> {
        let kind = email_log_kind(command)?;
        let data = cbor_field(result, "data").or_else(|| {
            let details =
                cbor_field(result, "error").and_then(|error| cbor_field(error, "details"))?;
            match details {
                CborValue::Map(entries) if entries.is_empty() => None,
                _ => Some(details),
            }
        });
        let mut entry = EmailLogEntry {
            schema: STATE_SCHEMA,
            ts_unix_ms: current_unix_millis(),
            kind: kind.to_owned(),
            command: email_command_name(command).to_owned(),
            status: email_log_status(result),
            account: None,
            folder: None,
            uid: None,
            access: data
                .and_then(|data| cbor_text_field(data, "access"))
                .map(str::to_owned),
            from: None,
            to: Vec::new(),
            title: None,
            title_redacted: false,
            approval_id: data
                .and_then(|data| cbor_text_field(data, "approval_id"))
                .map(str::to_owned),
            message_count: data.and_then(|data| cbor_array_len(data, "messages")),
            reason: email_log_reason(result),
        };

        match command {
            EmailCommand::ListByUid {
                account, folder, ..
            }
            | EmailCommand::ListRecent {
                account, folder, ..
            } => {
                entry.account = log_account(data, Some(account.as_str()));
                entry.folder = log_field(data, "folder", Some(folder.as_str()));
            }
            EmailCommand::Read {
                account,
                folder,
                uid,
            }
            | EmailCommand::RequestFull {
                account,
                folder,
                uid,
            } => {
                entry.account = log_account(data, Some(account.as_str()));
                entry.folder = log_field(data, "folder", Some(folder.as_str()));
                entry.uid = log_field(data, "uid", Some(uid.as_str()));
                if entry.access.is_none() {
                    entry.access = match email_log_status(result).as_str() {
                        "ok" => Some(ACCESS_FULL.to_owned()),
                        ACCESS_PREVIEW => Some(ACCESS_PREVIEW.to_owned()),
                        "approval_required" => Some(ACCESS_NONE.to_owned()),
                        _ => None,
                    };
                }
                entry.from = data
                    .and_then(|data| cbor_text_field(data, "from"))
                    .map(str::to_owned)
                    .or_else(|| {
                        data.and_then(|data| header_text_field(data, "from"))
                            .map(str::to_owned)
                    });
                if let Some(data) = data {
                    if let Some(subject) = header_text_field(data, "subject") {
                        entry.title = Some(email_log_title(subject));
                    } else if let Some(subject) = cbor_text_field(data, "subject_preview")
                        .or_else(|| header_text_field(data, "subject_preview"))
                    {
                        entry.title = Some(email_log_title(subject));
                        entry.title_redacted = true;
                    }
                }
            }
            EmailCommand::ManageMessage {
                account,
                folder,
                uid,
                ..
            }
            | EmailCommand::Trash {
                account,
                folder,
                uid,
            } => {
                entry.account = log_account(data, Some(account.as_str()));
                entry.folder = log_field(data, "folder", Some(folder.as_str()));
                entry.uid = log_field(data, "uid", Some(uid.as_str()));
            }
            EmailCommand::Send {
                account,
                from,
                to,
                cc,
                subject,
                ..
            } => {
                let resolved_account =
                    self.resolve_send_account_id(account.clone(), from.as_deref());
                entry.account = log_account(data, resolved_account.as_deref());
                entry.from = from
                    .as_deref()
                    .map(|value| safe_model_line(value, MAX_ADDRESS_CHARS))
                    .or_else(|| {
                        entry
                            .account
                            .as_deref()
                            .and_then(|account| self.config.accounts.get(account))
                            .map(|account| {
                                safe_model_line(&account.from_identity, MAX_ADDRESS_CHARS)
                            })
                    });
                entry.to = to
                    .iter()
                    .chain(cc.iter())
                    .take(MAX_RECIPIENTS)
                    .map(|recipient| safe_model_line(recipient, MAX_ADDRESS_CHARS))
                    .collect();
                entry.title = Some(email_log_title(subject));
            }
            EmailCommand::ListFolders { .. } => return None,
        }
        Some(entry)
    }

    fn resolve_account_id<'a>(&'a self, command: &str, id: &'a str) -> Result<&'a str, CborValue> {
        if !id.is_empty() {
            return Ok(id);
        }
        self.config
            .account_order
            .first()
            .map(String::as_str)
            .ok_or_else(|| error_envelope(Some(command), "account_not_found", "account not found"))
    }

    fn account(&self, command: &str, id: &str) -> Result<&ValidatedAccount, CborValue> {
        if !self.config.enable {
            return Err(error_envelope(
                Some(command),
                "account_disabled",
                "email extension is disabled",
            ));
        }
        let account = self.config.accounts.get(id).ok_or_else(|| {
            error_envelope(Some(command), "account_not_found", "account not found")
        })?;
        if !account.enable {
            return Err(error_envelope(
                Some(command),
                "account_disabled",
                "account is disabled",
            ));
        }
        Ok(account)
    }

    fn list_folders(&self) -> CborValue {
        let mut visible = Vec::new();
        for account_id in &self.config.account_order {
            let account = match self.account("list_folders", account_id) {
                Ok(account) => account,
                Err(_) => continue,
            };
            if !account.imap_configured() {
                continue;
            }
            let folders = match self.backend.list_folders(account_id) {
                Ok(folders) => folders,
                Err(message) => {
                    return backend_error_envelope(Some("list_folders"), "network_error", &message);
                }
            };
            visible.extend(
                folders
                    .into_iter()
                    .filter(|f| account.folders.allows(&f.name))
                    .map(|folder| format_folder_line(account_id, folder)),
            );
        }
        ok_line_envelope(
            "list_folders",
            "ok",
            vec![("format", CborValue::Text("folder flags".to_owned()))],
            cbor_map(vec![
                ("format", CborValue::Text("folder flags".to_owned())),
                ("folders", CborValue::Array(visible.clone())),
            ]),
            &visible,
        )
    }

    fn list_by_uid(
        &self,
        account_id: &str,
        folder: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> CborValue {
        self.list_messages(
            "list_by_uid",
            account_id,
            folder,
            limit,
            cursor,
            |backend, account_id, folder, limit, offset| {
                backend.list_messages_by_uid_page(account_id, folder, limit, offset)
            },
        )
    }

    fn list_recent(
        &self,
        account_id: &str,
        folder: &str,
        limit: u32,
        cursor: Option<&str>,
        days: u32,
    ) -> CborValue {
        let now = self.backend.current_time();
        self.list_messages(
            "list_recent",
            account_id,
            folder,
            limit,
            cursor,
            |backend, account_id, folder, limit, offset| {
                backend.list_recent_messages_page(account_id, folder, limit, offset, days, now)
            },
        )
    }

    fn list_messages(
        &self,
        command: &'static str,
        account_id: &str,
        folder: &str,
        limit: u32,
        cursor: Option<&str>,
        list_page: impl FnOnce(&B, &str, &str, usize, usize) -> Result<BackendMessagePage, String>,
    ) -> CborValue {
        let account_id = match self.resolve_account_id(command, account_id) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let account = match self.account(command, account_id) {
            Ok(a) => a,
            Err(e) => return e,
        };
        if let Err(message) = validate_mailbox_name(folder) {
            return error_envelope(Some(command), "invalid_input", &message);
        }
        if !account.folders.allows(folder) {
            return error_envelope(
                Some(command),
                "folder_not_allowed",
                "folder is not whitelisted for this account",
            );
        }
        let offset = match parse_cursor(cursor) {
            Ok(offset) => offset,
            Err(message) => return error_envelope(Some(command), "invalid_input", &message),
        };
        let limit = (limit as usize).min(LIST_MAX_LIMIT);
        let page = match list_page(&self.backend, account_id, folder, limit, offset) {
            Ok(page) => page,
            Err(message) => {
                return backend_error_envelope(Some(command), "network_error", &message);
            }
        };
        let next_cursor = page
            .next_cursor
            .map(CborValue::Text)
            .unwrap_or(CborValue::Null);
        let truncated = page.truncated;
        let data: Vec<CborValue> = page
            .messages
            .into_iter()
            .map(|message| {
                let target = IncomingTarget {
                    account: account_id.to_owned(),
                    folder: folder.to_owned(),
                    uid: message.uid.clone(),
                    uidvalidity: message.uidvalidity.clone(),
                };
                let (access, _decision) = self.incoming_effective_access(&target, &message);
                CborValue::Text(format_list_message_line(message, access))
            })
            .collect();
        ok_line_envelope(
            command,
            "ok",
            vec![
                (
                    "folder",
                    CborValue::Text(safe_model_line(
                        &flatten_folder_id(account_id, folder),
                        MAX_HEADER_VALUE_CHARS,
                    )),
                ),
                (
                    "format",
                    CborValue::Text("uid date from flags access attachments subject...".to_owned()),
                ),
                ("next_cursor", next_cursor.clone()),
                ("truncated", CborValue::Bool(truncated)),
            ],
            cbor_map(vec![
                (
                    "folder",
                    CborValue::Text(safe_model_line(
                        &flatten_folder_id(account_id, folder),
                        MAX_HEADER_VALUE_CHARS,
                    )),
                ),
                (
                    "format",
                    CborValue::Text("uid date from flags access attachments subject...".to_owned()),
                ),
                ("messages", CborValue::Array(data.clone())),
                ("next_cursor", next_cursor),
                ("truncated", CborValue::Bool(truncated)),
            ]),
            &data,
        )
    }

    fn incoming_access_target(
        &self,
        command: &str,
        account_id: &str,
        folder: &str,
        uid: &str,
    ) -> Result<IncomingAccessTarget, CborValue> {
        let account_id = self.validate_message_target(command, account_id, folder, uid)?;
        let metadata = match self.backend.message_metadata(&account_id, folder, uid) {
            Ok(message) => message,
            Err(message) if message.contains("not implemented") => {
                return Err(error_envelope(Some(command), "internal_error", &message));
            }
            Err(message)
                if backend_error_code(&message, "network_error") == "message_not_found" =>
            {
                return Err(error_envelope(
                    Some(command),
                    "message_not_found",
                    "message not found",
                ));
            }
            Err(message) => {
                return Err(backend_error_envelope(
                    Some(command),
                    "network_error",
                    &message,
                ));
            }
        };
        let target = IncomingTarget {
            account: account_id.clone(),
            folder: folder.to_owned(),
            uid: uid.to_owned(),
            uidvalidity: metadata.uidvalidity.clone(),
        };
        let (access, decision) = self.incoming_effective_access(&target, &metadata);
        Ok((account_id, metadata, target, access, decision))
    }

    fn read(&self, account_id: &str, folder: &str, uid: &str) -> CborValue {
        let (account_id, metadata, _target, access, decision) =
            match self.incoming_access_target("read", account_id, folder, uid) {
                Ok(target) => target,
                Err(error) => return error,
            };
        if access == ACCESS_NONE {
            return error_envelope_with_details(
                Some("read"),
                "approval_required",
                "full access to this email requires approval; use email_request_access to request user approval",
                cbor_map(vec![
                    ("access", CborValue::Text(ACCESS_NONE.to_owned())),
                    ("requested_access", CborValue::Text(ACCESS_FULL.to_owned())),
                    ("kind", CborValue::Text("incoming_read".to_owned())),
                    (
                        "folder",
                        CborValue::Text(safe_model_line(
                            &flatten_folder_id(&account_id, folder),
                            MAX_HEADER_VALUE_CHARS,
                        )),
                    ),
                    (
                        "uid",
                        CborValue::Text(safe_model_line(uid, MAX_HEADER_VALUE_CHARS)),
                    ),
                    ("from", CborValue::Text(incoming_approval_from(&metadata))),
                    (
                        "date",
                        CborValue::Text(safe_model_line(&metadata.date, MAX_HEADER_VALUE_CHARS)),
                    ),
                    ("subject", CborValue::Null),
                    (
                        "subject_preview",
                        CborValue::Text(unapproved_subject_preview(&metadata.subject)),
                    ),
                    ("subject_redacted", CborValue::Bool(true)),
                    ("reason", CborValue::Text(decision.reason.clone())),
                ]),
            );
        }
        if access == ACCESS_FULL {
            let msg = match self.backend.read_message(&account_id, folder, uid) {
                Ok(message) => message,
                Err(message)
                    if backend_error_code(&message, "network_error") == "message_not_found" =>
                {
                    return error_envelope(Some("read"), "message_not_found", "message not found");
                }
                Err(message) => {
                    return backend_error_envelope(Some("read"), "network_error", &message);
                }
            };
            if msg.uid != metadata.uid || msg.uidvalidity != metadata.uidvalidity {
                return error_envelope(Some("read"), "message_not_found", "message not found");
            }
            let simplified = simplify_email_content(&msg.body_text);
            let wrapped_body = wrap_external_untrusted_message(&simplified.text);
            let trusted = decision.matched_pattern.as_deref() != Some("approval");
            let from = normalize_address(&msg.from).unwrap_or_else(|| msg.from.clone());
            let truncate = truncate_body(&wrapped_body);
            let attachments = msg
                .attachments
                .iter()
                .enumerate()
                .map(|(index, attachment)| format_attachment_line(index, attachment))
                .map(CborValue::Text)
                .collect();
            return ok_envelope(
                "read",
                "ok",
                cbor_map(vec![
                    (
                        "folder",
                        CborValue::Text(safe_model_line(
                            &flatten_folder_id(&account_id, folder),
                            MAX_HEADER_VALUE_CHARS,
                        )),
                    ),
                    (
                        "headers",
                        CborValue::Text(format_read_headers(
                            &from,
                            &msg,
                            "subject",
                            &msg.subject,
                            trusted,
                            simplified.source,
                        )),
                    ),
                    (
                        "body_text",
                        CborValue::Text(safe_model_text(&truncate.body_text, READ_BODY_MAX_BYTES)),
                    ),
                    (
                        "body_truncated",
                        CborValue::Bool(truncate.truncated || msg.source_truncated),
                    ),
                    (
                        "body_total_lines",
                        CborValue::Integer(truncate.total_lines.into()),
                    ),
                    (
                        "body_total_bytes",
                        CborValue::Integer(truncate.total_bytes.into()),
                    ),
                    (
                        "body_shown_lines",
                        CborValue::Integer(truncate.shown_lines.into()),
                    ),
                    (
                        "body_shown_bytes",
                        CborValue::Integer(truncate.shown_bytes.into()),
                    ),
                    (
                        "format",
                        CborValue::Text(
                            "attachment_index filename content_type size_bytes".to_owned(),
                        ),
                    ),
                    ("attachments", CborValue::Array(attachments)),
                ]),
            );
        }
        let preview_message = match self.backend.read_message(&account_id, folder, uid) {
            Ok(message) => message,
            Err(message)
                if backend_error_code(&message, "network_error") == "message_not_found" =>
            {
                return error_envelope(Some("read"), "message_not_found", "message not found");
            }
            Err(message) => {
                return backend_error_envelope(Some("read"), "network_error", &message);
            }
        };
        if preview_message.uid != metadata.uid
            || preview_message.uidvalidity != metadata.uidvalidity
        {
            return error_envelope(Some("read"), "message_not_found", "message not found");
        }
        let preview = unapproved_email_preview(&preview_message.body_text);
        let wrapped_preview = wrap_external_untrusted_message(&preview.text);
        let preview_from = normalize_address(&preview_message.from)
            .unwrap_or_else(|| preview_message.from.clone());
        let preview_truncated = preview.truncated || preview_message.source_truncated;
        ok_envelope(
            "read",
            ACCESS_PREVIEW,
            cbor_map(vec![
                (
                    "folder",
                    CborValue::Text(safe_model_line(
                        &flatten_folder_id(&account_id, folder),
                        MAX_HEADER_VALUE_CHARS,
                    )),
                ),
                (
                    "headers",
                    CborValue::Text(format_read_headers(
                        &preview_from,
                        &preview_message,
                        "subject_preview",
                        &unapproved_subject_preview(&preview_message.subject),
                        false,
                        preview.source,
                    )),
                ),
                (
                    "body_preview",
                    CborValue::Text(safe_model_text(&wrapped_preview, READ_BODY_MAX_BYTES)),
                ),
                ("body_preview_truncated", CborValue::Bool(preview_truncated)),
                ("reason", CborValue::Text(decision.reason.clone())),
                ("policy", policy_cbor(&decision)),
                (
                    "message",
                    CborValue::Text(
                        "Preview only. Use email_request_access for user approval before reading the full message."
                            .to_owned(),
                    ),
                ),
            ]),
        )
    }

    fn request_access(&self, account_id: &str, folder: &str, uid: &str) -> CborValue {
        let (account_id, metadata, target, access, decision) =
            match self.incoming_access_target("request_access", account_id, folder, uid) {
                Ok(target) => target,
                Err(error) => return error,
            };
        if access == ACCESS_FULL {
            return ok_envelope(
                "request_access",
                "already_full",
                cbor_map(vec![
                    (
                        "folder",
                        CborValue::Text(safe_model_line(
                            &flatten_folder_id(&account_id, folder),
                            MAX_HEADER_VALUE_CHARS,
                        )),
                    ),
                    (
                        "message",
                        CborValue::Text("Access already available. Use email read.".to_owned()),
                    ),
                ]),
            );
        }
        let approval = IncomingApproval {
            schema: STATE_SCHEMA,
            id: String::new(),
            kind: "incoming_read".to_owned(),
            status: "pending".to_owned(),
            account: safe_model_line(&account_id, MAX_HEADER_VALUE_CHARS),
            folder: safe_model_line(folder, MAX_HEADER_VALUE_CHARS),
            uid: safe_model_line(uid, MAX_HEADER_VALUE_CHARS),
            uidvalidity: safe_model_line(&target.uidvalidity, MAX_HEADER_VALUE_CHARS),
            from: incoming_approval_from(&metadata),
            date: safe_model_line(&metadata.date, MAX_HEADER_VALUE_CHARS),
            message_id: incoming_approval_message_id(&metadata),
            subject_redacted: true,
            subject_preview: unapproved_subject_preview(&metadata.subject),
            reason: decision.reason.clone(),
        };
        match self.state.pending_incoming(&approval) {
            Ok(_id) => ok_envelope(
                "request_access",
                "approval_required",
                cbor_map(vec![
                    (
                        "folder",
                        CborValue::Text(safe_model_line(
                            &flatten_folder_id(&account_id, folder),
                            MAX_HEADER_VALUE_CHARS,
                        )),
                    ),
                    (
                        "message",
                        CborValue::Text(
                            "Access requested. When approved, read again to fetch full content."
                                .to_owned(),
                        ),
                    ),
                ]),
            ),
            Err(message) => error_envelope(Some("request_access"), "internal_error", &message),
        }
    }

    fn validate_message_target(
        &self,
        command: &str,
        account_id: &str,
        folder: &str,
        uid: &str,
    ) -> Result<String, CborValue> {
        let account_id = self.resolve_account_id(command, account_id)?;
        let account = self.account(command, account_id)?;
        if let Err(message) = validate_mailbox_name(folder) {
            return Err(error_envelope(Some(command), "invalid_input", &message));
        }
        if !account.folders.allows(folder) {
            return Err(error_envelope(
                Some(command),
                "folder_not_allowed",
                "folder is not whitelisted for this account",
            ));
        }
        if !is_single_uid(uid) {
            return Err(error_envelope(
                Some(command),
                "invalid_input",
                "uid must be a positive integer",
            ));
        }
        Ok(account_id.to_owned())
    }

    fn manage_message(
        &mut self,
        command: MessageManagementCommand,
        account_id: &str,
        folder: &str,
        uid: &str,
    ) -> CborValue {
        let command_name = command.command_name();
        let account_id = match self.validate_message_target(command_name, account_id, folder, uid) {
            Ok(account_id) => account_id,
            Err(error) => return error,
        };
        match self
            .backend
            .update_message_flags(&account_id, folder, uid, command.mutation())
        {
            Ok(()) => ok_envelope(
                command_name,
                command.status_name(),
                cbor_map(vec![
                    (
                        "folder",
                        CborValue::Text(safe_model_line(
                            &flatten_folder_id(&account_id, folder),
                            MAX_HEADER_VALUE_CHARS,
                        )),
                    ),
                    (
                        "message",
                        CborValue::Text(command.success_message().to_owned()),
                    ),
                ]),
            ),
            Err(message) if backend_error_code(&message, "imap_error") == "message_not_found" => {
                error_envelope(Some(command_name), "message_not_found", "message not found")
            }
            Err(message) => backend_error_envelope(Some(command_name), "imap_error", &message),
        }
    }

    fn trash(&mut self, account_id: &str, folder: &str, uid: &str) -> CborValue {
        let command = "trash";
        let account_id = match self.validate_message_target(command, account_id, folder, uid) {
            Ok(account_id) => account_id,
            Err(error) => return error,
        };
        match self.backend.move_message_to_trash(&account_id, folder, uid) {
            Ok(_trash_folder) => ok_envelope(
                command,
                "moved_to_trash",
                cbor_map(vec![
                    (
                        "folder",
                        CborValue::Text(safe_model_line(
                            &flatten_folder_id(&account_id, folder),
                            MAX_HEADER_VALUE_CHARS,
                        )),
                    ),
                    (
                        "message",
                        CborValue::Text("Message moved to trash.".to_owned()),
                    ),
                ]),
            ),
            Err(message) if backend_error_code(&message, "imap_error") == "message_not_found" => {
                error_envelope(Some(command), "message_not_found", "message not found")
            }
            Err(message) => backend_error_envelope(Some(command), "imap_error", &message),
        }
    }
    fn resolve_send_account_id(
        &self,
        account: Option<String>,
        from: Option<&str>,
    ) -> Option<String> {
        if account.is_some() {
            return account;
        }
        if let Some(from) = from {
            let from_trimmed = from.trim();
            let from_normalized = normalize_address(from);
            if let Some(id) = self.config.account_order.iter().find(|id| {
                let Some(account) = self.config.accounts.get(*id) else {
                    return false;
                };
                account.enable
                    && account.smtp_configured()
                    && (from_trimmed == account.from_identity
                        || from_normalized.as_deref() == Some(account.from_normalized.as_str()))
            }) {
                return Some(id.clone());
            }
        }
        self.config.account_order.iter().find_map(|id| {
            let account = self.config.accounts.get(id)?;
            if account.enable && account.smtp_configured() {
                Some(id.clone())
            } else {
                None
            }
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn send(
        &mut self,
        account: Option<String>,
        from: Option<String>,
        to: Vec<String>,
        cc: Vec<String>,
        bcc: Vec<String>,
        subject: String,
        body_text: String,
        reply_to: Option<String>,
        in_reply_to: Option<String>,
    ) -> CborValue {
        let message = match self.outgoing_message_from_send_args(SendArgs {
            account,
            from,
            to,
            cc,
            bcc,
            subject,
            body_text,
            reply_to,
            in_reply_to,
        }) {
            Ok(message) => message,
            Err(error) => return error,
        };
        self.send_outgoing_message(&message)
    }

    fn outgoing_message_from_send_args(
        &self,
        args: SendArgs,
    ) -> Result<OutgoingMessage, CborValue> {
        let account_id =
            match self.resolve_send_account_id(args.account.clone(), args.from.as_deref()) {
                Some(id) => id,
                None => {
                    return Err(error_envelope(
                        Some("send"),
                        "account_not_found",
                        "account not found",
                    ));
                }
            };
        let account_cfg = self.account("send", &account_id)?;
        if !account_cfg.smtp_configured() {
            return Err(error_envelope(
                Some("send"),
                "smtp_error",
                "account has no SMTP configuration",
            ));
        }
        validate_send_from(args.from.as_deref(), account_cfg)?;
        let from_identity = account_cfg.from_identity.clone();
        validate_send_inputs(&args)?;
        Ok(OutgoingMessage {
            account: account_id,
            from: from_identity,
            to: args.to,
            cc: args.cc,
            bcc: args.bcc,
            subject: args.subject,
            body_text: args.body_text,
            reply_to: args.reply_to,
            in_reply_to: args.in_reply_to,
        })
    }

    fn send_outgoing_message(&mut self, message: &OutgoingMessage) -> CborValue {
        let blocked = self.blocked_recipients(message);
        if blocked.is_empty() {
            return self.send_allowed_outgoing_message(message);
        }
        if self.state.outgoing_approved_exact(message) {
            return already_sent_outgoing_envelope();
        }
        self.queue_outgoing_approval(message, blocked)
    }

    fn send_allowed_outgoing_message(&mut self, message: &OutgoingMessage) -> CborValue {
        match self.backend.send_message(message) {
            Ok(_id) => ok_envelope("send", "sent", cbor_map(vec![])),
            Err(EmailSendFailure::NotDispatched(message)) => {
                backend_error_envelope(Some("send"), "smtp_error", &message)
            }
            Err(EmailSendFailure::OutcomeUnknown(_message)) => smtp_outcome_unknown_envelope(),
        }
    }

    fn queue_outgoing_approval(
        &self,
        message: &OutgoingMessage,
        blocked: Vec<String>,
    ) -> CborValue {
        let approval = OutgoingApproval {
            schema: STATE_SCHEMA,
            id: String::new(),
            kind: "outgoing_send".to_owned(),
            status: "pending".to_owned(),
            account: message.account.clone(),
            from: message.from.clone(),
            to: message.to.clone(),
            cc: message.cc.clone(),
            bcc: message.bcc.clone(),
            subject: message.subject.clone(),
            body_text: message.body_text.clone(),
            reply_to: message.reply_to.clone(),
            in_reply_to: message.in_reply_to.clone(),
            blocked_recipients: blocked,
            reason: "recipient_not_whitelisted".to_owned(),
            sent_message_id: None,
        };
        match self.state.pending_outgoing(&approval) {
            Ok(_id) => ok_envelope(
                "send",
                "approval_required",
                cbor_map(vec![(
                    "message",
                    CborValue::Text("Message pending approval.".to_owned()),
                )]),
            ),
            Err(message) => error_envelope(Some("send"), "internal_error", &message),
        }
    }

    fn dispatch_action(&mut self, action_id: &str, argv: &[String]) -> Result<String, String> {
        match action_id {
            "email.auth.google.start" => {
                require_one_arg(argv).and_then(|account| self.action_auth_google_start(account))
            }
            "email.auth.google.finish" => self.action_auth_google_finish_args(argv),
            "email.out.list" => require_no_args(argv).and_then(|()| self.action_out_list()),
            "email.out.open" => require_one_arg(argv).and_then(|id| self.action_out_open(id)),
            "email.out.approve" => self.action_out_approve_args(argv),
            "email.out.deny" => {
                require_approval_ids(argv).and_then(|ids| self.action_out_deny_many(&ids))
            }
            "email.out.whitelist" => {
                require_one_arg(argv).and_then(|pattern| self.action_out_whitelist(pattern))
            }
            "email.in.list" => require_no_args(argv).and_then(|()| self.action_in_list()),
            "email.in.open" => require_one_arg(argv).and_then(|id| self.action_in_open(id)),
            "email.in.approve" => self.action_in_approve_args(argv),
            "email.in.deny" => {
                require_approval_ids(argv).and_then(|ids| self.action_in_deny_many(&ids))
            }
            "email.in.whitelist" => {
                require_one_arg(argv).and_then(|pattern| self.action_in_whitelist(pattern))
            }
            "email.log.last" => parse_log_limit(argv).and_then(|limit| self.action_log_last(limit)),
            _ => Err(format!("unsupported email action `{action_id}`")),
        }
    }

    fn action_auth_google_start(&self, account_id: &str) -> Result<String, String> {
        let account = self.google_oauth_state_account(account_id)?;
        let (authorization_url, state, pkce_verifier, redirect_uri, expires_in_secs) =
            self.backend.start_google_installed_app_auth(&account.id)?;
        let pending = EmailGooglePendingAuth::installed_app(
            &account.id,
            &state,
            &pkce_verifier,
            &redirect_uri,
            expires_in_secs,
        );
        self.state.save_pending_google_auth(&pending)?;
        Ok(format!(
            "Google email authorization started for account {}.\nOpen this URL:\n{}\nApprove access in the browser. The browser will fail to connect to localhost; copy the full final address-bar URL and run:\n:email auth google finish {} <copied-url>\nExpires in {} second(s).",
            safe_display_line(&account.id),
            safe_oauth_url_display(&authorization_url),
            safe_display_line(&account.id),
            expires_in_secs,
        ))
    }

    fn action_auth_google_finish_args(&mut self, argv: &[String]) -> Result<String, String> {
        match argv {
            [account_id, redirect_url]
                if !account_id.trim().is_empty() && !redirect_url.trim().is_empty() =>
            {
                self.action_auth_google_finish(account_id, redirect_url)
            }
            [_, _] => Err("action arguments must not be empty".to_owned()),
            [] | [_] => Err("missing required action argument".to_owned()),
            _ => Err("too many action arguments".to_owned()),
        }
    }

    fn action_auth_google_finish(
        &mut self,
        account_id: &str,
        redirect_url: &str,
    ) -> Result<String, String> {
        let account_id = self.google_oauth_state_account(account_id)?.id.clone();
        let pending = self.state.pending_google_auth(&account_id)?;
        if pending.expired() {
            self.state.clear_pending_google_auth(&account_id)?;
            return Err(format!(
                "Google authorization for account `{}` expired; run `:email auth google start {}` again",
                safe_display_line(&account_id),
                safe_display_line(&account_id)
            ));
        }
        let installed_app = pending.installed_app_data();
        let redirect = parse_installed_app_redirect_url(
            redirect_url,
            installed_app.redirect_uri,
            installed_app.state,
        )?;
        let (refresh_token, access_token, expires_in_secs) =
            self.backend.finish_google_installed_app_auth(
                &account_id,
                &redirect.code,
                installed_app.pkce_verifier,
                installed_app.redirect_uri,
            )?;
        self.state
            .save_google_refresh_token(&account_id, &refresh_token)?;
        self.state.clear_pending_google_auth(&account_id)?;
        if let Some(access_token) = access_token
            && let Err(_message) = self.backend.prime_google_access_token_cache(
                &account_id,
                access_token,
                expires_in_secs,
            )
        {
            tracing::warn!(target: LOG_TARGET, "failed to prime Google email access token cache");
        }
        Ok(format!(
            "Google email authorization stored for account {}.",
            safe_display_line(&account_id)
        ))
    }

    fn google_oauth_state_account(&self, account_id: &str) -> Result<&ValidatedAccount, String> {
        if !self.config.enable {
            return Err("email extension is disabled".to_owned());
        }
        let account = self
            .config
            .accounts
            .get(account_id)
            .ok_or_else(|| format!("email account `{account_id}` not found"))?;
        if !account.enable {
            return Err(format!("email account `{account_id}` is disabled"));
        }
        let Some(auth) = &account.auth else {
            return Err(format!(
                "email account `{account_id}` has no auth configuration"
            ));
        };
        match (
            auth.method,
            auth.provider,
            auth.refresh_token_secret.as_deref(),
        ) {
            (AuthMethod::Oauth2, Some(EmailOauth2Provider::Google), None) => Ok(account),
            (AuthMethod::Oauth2, Some(EmailOauth2Provider::Google), Some(_)) => Err(format!(
                "email account `{}` already uses refresh_token_secret; remove it before using `:email auth google`",
                account.id
            )),
            _ => Err(format!(
                "email account `{}` is not configured for oauth2 provider google",
                account.id
            )),
        }
    }

    fn action_log_last(&self, limit: usize) -> Result<String, String> {
        let entries = self.state.recent_email_log(limit)?;
        if entries.is_empty() {
            return Ok("No email log entries.".to_owned());
        }
        let mut lines = vec![format!("Last {} email log entry(s):", entries.len())];
        for entry in entries.iter().rev() {
            lines.push(format_email_log_entry(entry));
        }
        Ok(lines.join("\n"))
    }

    fn approvable_pending_outgoing(&self) -> Result<Vec<OutgoingApproval>, String> {
        let mut approvals = Vec::new();
        for approval in self.state.list_pending_outgoing()? {
            if !self.state.outgoing_denied_exists(&approval.id)? {
                approvals.push(approval);
            }
        }
        Ok(approvals)
    }

    fn action_out_list(&self) -> Result<String, String> {
        let approvals = self.approvable_pending_outgoing()?;
        if approvals.is_empty() {
            return Ok("No pending outgoing email approvals.".to_owned());
        }
        let mut lines = vec![format!(
            "{} pending outgoing email approval(s):",
            approvals.len()
        )];
        for approval in approvals {
            let visible_blocked = approval
                .blocked_recipients
                .iter()
                .filter(|recipient| !approval.bcc.contains(recipient))
                .cloned()
                .collect::<Vec<_>>();
            lines.push(format!(
                "{} account={} to={} cc={} blocked={} subject={}",
                safe_display_line(&approval.id),
                safe_display_line(&approval.account),
                safe_display_join(&approval.to, ","),
                safe_display_join(&approval.cc, ","),
                safe_display_join(&visible_blocked, ","),
                safe_display_line(&approval.subject)
            ));
        }
        Ok(lines.join("\n"))
    }

    fn action_out_open(&self, id: &str) -> Result<String, String> {
        validate_approval_id(id)?;
        if self.state.outgoing_denied_exists(id)? {
            let approval = self.state.denied_outgoing_by_id(id)?;
            return Ok(format!(
                "Outgoing approval {id} is denied. subject={} to={}",
                safe_display_line(&approval.subject),
                safe_display_join(&approval.to, ",")
            ));
        }
        let approval = self.state.pending_outgoing_by_id(id)?;
        Ok(format!(
            "Outgoing approval {id}\nstatus: {}\naccount: {}\nfrom: {}\nto: {}\ncc: {}\nbcc: {}\nsubject: {}\nreply_to: {}\nin_reply_to: {}\nblocked: {}\nreason: {}\n\n{}",
            safe_display_line(&approval.status),
            safe_display_line(&approval.account),
            safe_display_line(&approval.from),
            safe_display_join(&approval.to, ", "),
            safe_display_join(&approval.cc, ", "),
            safe_display_join(&approval.bcc, ", "),
            safe_display_line(&approval.subject),
            safe_display_line(approval.reply_to.as_deref().unwrap_or("")),
            safe_display_line(approval.in_reply_to.as_deref().unwrap_or("")),
            safe_display_join(&approval.blocked_recipients, ", "),
            safe_display_line(&approval.reason),
            safe_display_text(&approval.body_text)
        ))
    }

    fn action_out_approve_args(&mut self, argv: &[String]) -> Result<String, String> {
        if require_all_arg(argv)? {
            let ids = self
                .approvable_pending_outgoing()?
                .into_iter()
                .map(|approval| approval.id)
                .collect::<Vec<_>>();
            if ids.is_empty() {
                return Ok("No pending outgoing email approvals to approve.".to_owned());
            }
            return self.action_out_approve_many(&ids);
        }
        require_approval_ids(argv).and_then(|ids| self.action_out_approve_many(&ids))
    }

    fn action_out_approve_many(&mut self, ids: &[String]) -> Result<String, String> {
        if ids.len() == 1 {
            return self.action_out_approve(&ids[0]);
        }

        let mut lines = vec![format!("Approving {} outgoing email(s):", ids.len())];
        let mut errors = Vec::new();
        for id in ids {
            match self.action_out_approve(id) {
                Ok(message) => lines.push(message),
                Err(error) => {
                    lines.push(format!(
                        "Failed outgoing email {id}: {}",
                        safe_display_line(&error)
                    ));
                    errors.push(id.clone());
                }
            }
        }
        let output = lines.join("\n");
        if errors.is_empty() {
            Ok(output)
        } else {
            Err(output)
        }
    }

    fn action_out_approve(&mut self, id: &str) -> Result<String, String> {
        if self.state.outgoing_denied_exists(id)? {
            return Err(format!(
                "Outgoing email {id} is denied; refusing to approve or send it."
            ));
        }
        if self.state.outgoing_pending_exists(id)? {
            let pending = self.state.pending_outgoing_by_id(id)?;
            self.validate_outgoing_approval_for_send(&pending)?;
            let approval = self.state.claim_outgoing(id)?;
            self.validate_outgoing_approval_for_send(&approval)?;
            let message = outgoing_approval_message(&approval);
            let message_id =
                self.backend
                    .send_message(&message)
                    .map_err(|failure| match failure {
                        EmailSendFailure::NotDispatched(message) => backend_error_text(&message),
                        EmailSendFailure::OutcomeUnknown(message) => format!(
                            "{SMTP_OUTCOME_UNKNOWN_MESSAGE} Detail: {}",
                            backend_error_text(&message)
                        ),
                    })?;
            let display_message_id = safe_display_line(&message_id);
            return match self.state.complete_outgoing(id, &message_id) {
                Ok(()) => Ok(format!(
                    "Sent approved outgoing email {id}. message_id={display_message_id} subject={} to={}",
                    safe_display_line(&approval.subject),
                    safe_display_join(&approval.to, ",")
                )),
                Err(error) => Ok(format!(
                    "Sent approved outgoing email {id}, but failed to record approval: {}. message_id={display_message_id} subject={} to={}",
                    safe_display_line(&error),
                    safe_display_line(&approval.subject),
                    safe_display_join(&approval.to, ",")
                )),
            };
        }
        if self.state.outgoing_sending_exists(id)? {
            return Err(format!(
                "Outgoing email {id} is already being sent or needs manual recovery."
            ));
        }
        let approval = self.state.approved_outgoing_by_id(id)?;
        Ok(format!(
            "Outgoing email {id} is already approved/sent. subject={} to={}",
            safe_display_line(&approval.subject),
            safe_display_join(&approval.to, ",")
        ))
    }

    fn action_out_deny_many(&mut self, ids: &[String]) -> Result<String, String> {
        if ids.len() == 1 {
            return self.action_out_deny(&ids[0]);
        }

        let mut lines = vec![format!("Denying {} outgoing email(s):", ids.len())];
        let mut errors = Vec::new();
        for id in ids {
            match self.action_out_deny(id) {
                Ok(message) => lines.push(message),
                Err(error) => {
                    lines.push(format!(
                        "Failed outgoing email {id}: {}",
                        safe_display_line(&error)
                    ));
                    errors.push(id.clone());
                }
            }
        }
        let output = lines.join("\n");
        if errors.is_empty() {
            Ok(output)
        } else {
            Err(output)
        }
    }

    fn action_out_deny(&mut self, id: &str) -> Result<String, String> {
        match self.state.pending_outgoing_by_id(id) {
            Ok(approval) => {
                self.state.deny_outgoing(id)?;
                Ok(format!(
                    "Denied outgoing email {id}. subject={} to={}",
                    safe_display_line(&approval.subject),
                    safe_display_join(&approval.to, ",")
                ))
            }
            Err(pending_error) => {
                if self.state.outgoing_denied_exists(id)? {
                    let approval = self.state.denied_outgoing_by_id(id)?;
                    return Ok(format!(
                        "Outgoing email {id} is already denied. subject={} to={}",
                        safe_display_line(&approval.subject),
                        safe_display_join(&approval.to, ",")
                    ));
                }
                if self.state.approved_outgoing_by_id(id).is_ok() {
                    return Err(format!(
                        "Outgoing email {id} is already approved/sent; refusing to deny it."
                    ));
                }
                if self.state.outgoing_sending_exists(id)? {
                    return Err(format!(
                        "Outgoing email {id} is already being sent or needs manual recovery."
                    ));
                }
                Err(pending_error)
            }
        }
    }

    fn action_out_whitelist(&self, pattern: &str) -> Result<String, String> {
        self.ensure_state_policy_extensions_enabled()?;
        let record = allow_record(pattern, "approved from :email out whitelist")?;
        self.state.append_outgoing_allow_record(record)?;
        Ok(format!(
            "Added outgoing email whitelist pattern `{}`.",
            safe_display_line(pattern)
        ))
    }

    fn action_in_list(&self) -> Result<String, String> {
        let approvals = self.state.list_pending_incoming()?;
        if approvals.is_empty() {
            return Ok("No pending incoming email read approvals.".to_owned());
        }
        let mut lines = vec![format!(
            "{} pending incoming email read approval(s):",
            approvals.len()
        )];
        for approval in approvals {
            lines.push(format!(
                "{} account={} folder={} uid={} from={} date={} subject_preview={} reason={}",
                safe_display_line(&approval.id),
                safe_display_line(&approval.account),
                safe_display_line(&approval.folder),
                safe_display_line(&approval.uid),
                safe_display_line(&approval.from),
                safe_display_line(&approval.date),
                safe_display_line(&approval.subject_preview),
                safe_display_line(&approval.reason)
            ));
        }
        Ok(lines.join("\n"))
    }

    fn action_in_open(&mut self, id: &str) -> Result<String, String> {
        validate_approval_id(id)?;
        let approval = self.state.pending_incoming_by_id(id)?;
        let message = self
            .backend
            .read_message(&approval.account, &approval.folder, &approval.uid)
            .map_err(|message| backend_error_text(&message))?;
        if safe_model_line(&message.uid, MAX_HEADER_VALUE_CHARS) != approval.uid
            || safe_model_line(&message.uidvalidity, MAX_HEADER_VALUE_CHARS) != approval.uidvalidity
        {
            return Err("message not found".to_owned());
        }
        let truncate = truncate_body(&message.body_text);
        let from = incoming_approval_from(&message);
        let attachment_names = message
            .attachments
            .iter()
            .filter_map(|attachment| attachment.filename.as_ref())
            .map(|filename| safe_display_line(filename))
            .collect::<Vec<_>>()
            .join(", ");
        Ok(format!(
            include_str!("prompts/incoming_approval_open.txt"),
            id = id,
            status = safe_display_line(&approval.status),
            account = safe_display_line(&approval.account),
            folder = safe_display_line(&approval.folder),
            uid = safe_display_line(&approval.uid),
            uidvalidity = safe_display_line(&approval.uidvalidity),
            from = safe_display_line(&from),
            to = safe_display_join(&message.to, ", "),
            cc = safe_display_join(&message.cc, ", "),
            date = safe_display_line(&message.date),
            subject = safe_display_line(&message.subject),
            body_truncated = truncate.truncated || message.source_truncated,
            attachments = message.attachments.len(),
            attachment_names = safe_display_line(&attachment_names),
            reason = safe_display_line(&approval.reason),
            body = safe_display_text(&truncate.body_text),
        ))
    }

    fn action_in_approve_args(&mut self, argv: &[String]) -> Result<String, String> {
        if require_all_arg(argv)? {
            let ids = self
                .state
                .list_pending_incoming()?
                .into_iter()
                .map(|approval| approval.id)
                .collect::<Vec<_>>();
            if ids.is_empty() {
                return Ok("No pending incoming email reads to approve.".to_owned());
            }
            return self.action_in_approve_many(&ids);
        }
        require_approval_ids(argv).and_then(|ids| self.action_in_approve_many(&ids))
    }

    fn action_in_approve_many(&mut self, ids: &[String]) -> Result<String, String> {
        if ids.len() == 1 {
            return self.action_in_approve(&ids[0]);
        }

        let mut lines = vec![format!("Approving {} incoming email read(s):", ids.len())];
        let mut errors = Vec::new();
        for id in ids {
            match self.action_in_approve(id) {
                Ok(message) => lines.push(message),
                Err(error) => {
                    lines.push(format!(
                        "Failed incoming email read {id}: {}",
                        safe_display_line(&error)
                    ));
                    errors.push(id.clone());
                }
            }
        }
        let output = lines.join("\n");
        if errors.is_empty() {
            Ok(output)
        } else {
            Err(output)
        }
    }

    fn action_in_approve(&mut self, id: &str) -> Result<String, String> {
        match self.state.pending_incoming_by_id(id) {
            Ok(approval) => {
                self.state.approve_incoming(id)?;
                Ok(format!(
                    "Approved incoming email read {id}; repeat the matching email_read with folder={}/{} email_id={} to fetch content.",
                    safe_display_line(&approval.account),
                    safe_display_line(&approval.folder),
                    safe_display_line(&approval.uid)
                ))
            }
            Err(_) => {
                let approval = self.state.approved_incoming_by_id(id)?;
                Ok(format!(
                    "Incoming email read {id} is already approved; repeat the matching email_read with folder={}/{} email_id={} to fetch content.",
                    safe_display_line(&approval.account),
                    safe_display_line(&approval.folder),
                    safe_display_line(&approval.uid)
                ))
            }
        }
    }

    fn action_in_deny_many(&mut self, ids: &[String]) -> Result<String, String> {
        if ids.len() == 1 {
            return self.action_in_deny(&ids[0]);
        }

        let mut lines = vec![format!("Denying {} incoming email read(s):", ids.len())];
        let mut errors = Vec::new();
        for id in ids {
            match self.action_in_deny(id) {
                Ok(message) => lines.push(message),
                Err(error) => {
                    lines.push(format!(
                        "Failed incoming email read {id}: {}",
                        safe_display_line(&error)
                    ));
                    errors.push(id.clone());
                }
            }
        }
        let output = lines.join("\n");
        if errors.is_empty() {
            Ok(output)
        } else {
            Err(output)
        }
    }

    fn action_in_deny(&mut self, id: &str) -> Result<String, String> {
        match self.state.pending_incoming_by_id(id) {
            Ok(approval) => {
                self.state.deny_incoming(id)?;
                Ok(format!(
                    "Denied incoming email read {id}; future matching email_read requests with folder={}/{} email_id={} report access=none. Explicit email_request_access can ask again.",
                    safe_display_line(&approval.account),
                    safe_display_line(&approval.folder),
                    safe_display_line(&approval.uid)
                ))
            }
            Err(pending_error) => {
                if let Ok(approval) = self.state.denied_incoming_by_id(id) {
                    return Ok(format!(
                        "Incoming email read {id} is already denied; matching email_read requests with folder={}/{} email_id={} report access=none. Explicit email_request_access can ask again.",
                        safe_display_line(&approval.account),
                        safe_display_line(&approval.folder),
                        safe_display_line(&approval.uid)
                    ));
                }
                if self.state.approved_incoming_by_id(id).is_ok() {
                    return Err(format!(
                        "Incoming email read {id} is already approved; refusing to deny it."
                    ));
                }
                Err(pending_error)
            }
        }
    }

    fn action_in_whitelist(&self, pattern: &str) -> Result<String, String> {
        self.ensure_state_policy_extensions_enabled()?;
        let record = allow_record(pattern, "approved from :email in whitelist")?;
        self.state.append_incoming_allow_record(record)?;
        Ok(format!(
            "Added incoming email whitelist pattern `{}`.",
            safe_display_line(pattern)
        ))
    }

    fn ensure_state_policy_extensions_enabled(&self) -> Result<(), String> {
        if self.config.policy.allow_state_policy_extensions {
            Ok(())
        } else {
            Err(
                "state policy extensions are disabled; whitelist actions cannot add active policy"
                    .to_owned(),
            )
        }
    }

    fn incoming_effective_access(
        &self,
        target: &IncomingTarget,
        message: &BackendMessage,
    ) -> (&'static str, PolicyDecision) {
        let decision = self.incoming_decision(message);
        if self.state.incoming_approved_exact(target, message) {
            return (
                ACCESS_FULL,
                PolicyDecision::allowed(Some("approval".to_owned())),
            );
        }
        if self.state.incoming_denied_exact(target, message) {
            return (ACCESS_NONE, PolicyDecision::denied("user denied"));
        }
        if decision.allowed {
            return (ACCESS_FULL, decision);
        }
        (ACCESS_PREVIEW, decision)
    }

    fn incoming_decision(&self, message: &BackendMessage) -> PolicyDecision {
        let sender_decision = self.address_decision(
            &message.from,
            &self.config.policy.incoming_allow,
            |s| s.load_incoming_allow(),
            "untrusted",
        );
        if !self.config.policy.incoming_auth.require {
            return sender_decision;
        }

        let auth_decision = incoming_auth_decision(message, &self.config.policy.incoming_auth);
        if sender_decision.allowed && auth_decision.allowed {
            return PolicyDecision::allowed(sender_decision.matched_pattern);
        }

        let mut reasons = Vec::new();
        if !sender_decision.allowed {
            reasons.push(sender_decision.reason);
        }
        if !auth_decision.allowed {
            reasons.push(auth_decision.reason);
        }
        PolicyDecision::denied(&reasons.join(", "))
    }

    fn recipient_allowed(&self, recipient: &str) -> bool {
        self.address_decision(
            recipient,
            &self.config.policy.outgoing_allow,
            |s| s.load_outgoing_allow(),
            "recipient_not_whitelisted",
        )
        .allowed
    }

    fn address_decision<F>(
        &self,
        address: &str,
        config_patterns: &[AddressPattern],
        load_state: F,
        denied: &str,
    ) -> PolicyDecision
    where
        F: Fn(&StateStore) -> Result<Vec<AddressPattern>, String>,
    {
        for pattern in config_patterns {
            if pattern.matches(address) {
                return PolicyDecision::allowed(Some(pattern.pattern_text().to_owned()));
            }
        }
        if self.config.policy.allow_state_policy_extensions
            && let Ok(patterns) = load_state(&self.state)
        {
            for pattern in patterns {
                if pattern.matches(address) {
                    return PolicyDecision::allowed(Some(pattern.pattern_text().to_owned()));
                }
            }
        }
        PolicyDecision::denied(denied)
    }

    fn blocked_recipients(&self, message: &OutgoingMessage) -> Vec<String> {
        message
            .to
            .iter()
            .chain(message.cc.iter())
            .chain(message.bcc.iter())
            .chain(message.reply_to.iter())
            .filter(|r| !self.recipient_allowed(r))
            .cloned()
            .collect()
    }

    fn validate_outgoing_approval_for_send(
        &self,
        approval: &OutgoingApproval,
    ) -> Result<(), String> {
        let account = self
            .config
            .accounts
            .get(&approval.account)
            .ok_or_else(|| "approval account not found".to_owned())?;
        if !self.config.enable || !account.enable {
            return Err("approval account is disabled".to_owned());
        }
        if !account.smtp_configured() {
            return Err("approval account has no SMTP configuration".to_owned());
        }
        if approval.from != account.from_identity {
            return Err("approval from identity does not match configured account".to_owned());
        }
        let message = outgoing_approval_message(approval);
        let recomputed_blocked = self.blocked_recipients(&message);
        if recomputed_blocked != approval.blocked_recipients {
            return Err("approval recipients no longer match current policy".to_owned());
        }
        Ok(())
    }
}

fn validate_send_from(from: Option<&str>, account_cfg: &ValidatedAccount) -> Result<(), CborValue> {
    let Some(from) = from else {
        return Ok(());
    };
    if from.trim() == account_cfg.from_identity {
        return Ok(());
    }
    if !from.contains(['<', '>'])
        && normalize_address(from).as_deref() == Some(account_cfg.from_normalized.as_str())
    {
        return Ok(());
    }
    Err(error_envelope(
        Some("send"),
        "policy_denied",
        "from identity does not match configured account",
    ))
}

fn validate_send_inputs(args: &SendArgs) -> Result<(), CborValue> {
    validate_send_recipient_count(args)?;
    validate_send_recipients(args)?;
    validate_send_subject(&args.subject)?;
    validate_send_body(&args.body_text)?;
    validate_send_in_reply_to(args.in_reply_to.as_deref())
}

fn validate_send_recipient_count(args: &SendArgs) -> Result<(), CborValue> {
    let recipient_count = args
        .to
        .len()
        .saturating_add(args.cc.len())
        .saturating_add(args.bcc.len())
        .saturating_add(usize::from(args.reply_to.is_some()));
    if MAX_RECIPIENTS < recipient_count {
        return Err(error_envelope(
            Some("send"),
            "invalid_input",
            "too many recipients",
        ));
    }
    Ok(())
}

fn validate_send_recipients(args: &SendArgs) -> Result<(), CborValue> {
    let invalid = args
        .to
        .iter()
        .chain(args.cc.iter())
        .chain(args.bcc.iter())
        .chain(args.reply_to.iter())
        .any(|recipient| !is_valid_send_recipient(recipient));
    if invalid {
        return Err(error_envelope(
            Some("send"),
            "invalid_input",
            "recipient address is invalid",
        ));
    }
    Ok(())
}

fn is_valid_send_recipient(recipient: &str) -> bool {
    normalize_address(recipient).is_some()
        && recipient.chars().count() <= MAX_ADDRESS_CHARS
        && is_safe_persisted_line(recipient, MAX_ADDRESS_CHARS)
}

fn validate_send_subject(subject: &str) -> Result<(), CborValue> {
    if subject.chars().count() <= MAX_HEADER_VALUE_CHARS
        && is_safe_persisted_line(subject, MAX_HEADER_VALUE_CHARS)
    {
        return Ok(());
    }
    Err(error_envelope(
        Some("send"),
        "invalid_input",
        "subject is too large or contains unsafe characters",
    ))
}

fn validate_send_body(body_text: &str) -> Result<(), CborValue> {
    if body_text.len() <= READ_BODY_MAX_BYTES && body_text.lines().count() <= READ_BODY_MAX_LINES {
        return Ok(());
    }
    Err(error_envelope(
        Some("send"),
        "invalid_input",
        "body_text is too large",
    ))
}

fn validate_send_in_reply_to(value: Option<&str>) -> Result<(), CborValue> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.chars().count() <= MAX_HEADER_VALUE_CHARS
        && is_safe_persisted_line(value, MAX_HEADER_VALUE_CHARS)
    {
        return Ok(());
    }
    Err(error_envelope(
        Some("send"),
        "invalid_input",
        "in_reply_to is too large or contains unsafe characters",
    ))
}

fn already_sent_outgoing_envelope() -> CborValue {
    ok_envelope(
        "send",
        "already_sent",
        cbor_map(vec![(
            "message",
            CborValue::Text("Message already sent.".to_owned()),
        )]),
    )
}

fn require_no_args(argv: &[String]) -> Result<(), String> {
    if argv.is_empty() {
        Ok(())
    } else {
        Err("this email action does not accept arguments".to_owned())
    }
}

fn require_one_arg(argv: &[String]) -> Result<&str, String> {
    match argv {
        [value] if !value.trim().is_empty() => Ok(value),
        [_] => Err("action argument must not be empty".to_owned()),
        [] => Err("missing required action argument".to_owned()),
        _ => Err("too many action arguments".to_owned()),
    }
}

fn require_all_arg(argv: &[String]) -> Result<bool, String> {
    let values = argv
        .iter()
        .flat_map(|value| value.split_whitespace())
        .collect::<Vec<_>>();
    if values == ["all"] {
        return Ok(true);
    }
    if values.contains(&"all") {
        return Err("`all` must be the only action argument".to_owned());
    }
    Ok(false)
}

fn require_approval_ids(argv: &[String]) -> Result<Vec<String>, String> {
    let ids = argv
        .iter()
        .flat_map(|value| value.split_whitespace())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return Err("missing required action argument".to_owned());
    }
    for id in &ids {
        validate_approval_id(id)?;
    }
    Ok(ids)
}

fn parse_log_limit(argv: &[String]) -> Result<usize, String> {
    let limit = match argv {
        [] => EMAIL_LOG_DEFAULT_LIMIT,
        [value] if !value.trim().is_empty() => value
            .parse::<usize>()
            .map_err(|_| "log limit must be a positive integer".to_owned())?,
        [_] => return Err("log limit must not be empty".to_owned()),
        _ => return Err("too many action arguments".to_owned()),
    };
    if limit == 0 {
        return Err("log limit must be a positive integer".to_owned());
    }
    Ok(if EMAIL_LOG_MAX_LIMIT < limit {
        EMAIL_LOG_MAX_LIMIT
    } else {
        limit
    })
}

fn allow_record(pattern: &str, note: &str) -> Result<StatePattern, String> {
    let compiled = AddressPattern::compile(pattern)?;
    let kind = match compiled {
        AddressPattern::Exact { .. } => "exact",
        AddressPattern::Glob { .. } => "glob",
        AddressPattern::Regex { .. } => "regex",
    };
    let pattern = if let Some(regex) = pattern.strip_prefix("re:") {
        regex.to_owned()
    } else {
        pattern.to_owned()
    };
    Ok(StatePattern {
        kind: kind.to_owned(),
        pattern,
        created_at: "now".to_owned(),
        created_by: "cli".to_owned(),
        note: Some(note.to_owned()),
    })
}

/// Runtime state for the email module.
#[derive(Default)]
pub struct RuntimeState {
    config_state: ConfigState,
}

#[derive(Default)]
enum ConfigState {
    #[default]
    Unconfigured,
    Configured(Box<Engine<RealEmailBackend>>),
    Rejected {
        reason: String,
    },
}

impl RuntimeState {
    /// Mark the email module configuration as rejected without keeping stale
    /// state.
    pub(crate) fn reject(&mut self, reason: String) {
        self.config_state = ConfigState::Rejected { reason };
    }

    /// Return effective accounts accepted by interactive Google email OAuth.
    pub(crate) fn google_auth_account_ids(&self) -> Vec<String> {
        let ConfigState::Configured(engine) = &self.config_state else {
            return Vec::new();
        };
        if !engine.config.enable {
            return Vec::new();
        }
        engine
            .config
            .accounts
            .values()
            .filter(|account| {
                account.enable
                    && crate::is_safe_action_account_id(&account.id)
                    && matches!(
                        account.auth.as_ref(),
                        Some(ValidatedAuthConfig {
                            method: AuthMethod::Oauth2,
                            provider: Some(EmailOauth2Provider::Google),
                            refresh_token_secret: None,
                            ..
                        })
                    )
            })
            .map(|account| account.id.clone())
            .collect()
    }

    /// Configure the email module from harness-supplied extension config.
    pub(crate) fn configure(
        &mut self,
        configure: tau_proto::Configure,
        storage: SharedStorage,
    ) -> Result<(), String> {
        match self.try_configure(configure, storage) {
            Ok(engine) => {
                self.config_state = ConfigState::Configured(Box::new(engine));
                Ok(())
            }
            Err(message) => {
                self.config_state = ConfigState::Rejected {
                    reason: message.clone(),
                };
                Err(message)
            }
        }
    }

    /// Configure the email module from an already-decoded email config.
    pub(crate) fn configure_with_config(
        &mut self,
        cfg: EmailExtensionConfig,
        state_dir: Option<PathBuf>,
        secrets: BTreeMap<String, tau_proto::SecretValue>,
        storage: SharedStorage,
    ) -> Result<(), String> {
        match self.try_configure_with_config(cfg, state_dir, secrets, storage) {
            Ok(engine) => {
                self.config_state = ConfigState::Configured(Box::new(engine));
                Ok(())
            }
            Err(message) => {
                self.config_state = ConfigState::Rejected {
                    reason: message.clone(),
                };
                Err(message)
            }
        }
    }

    fn try_configure(
        &self,
        configure: tau_proto::Configure,
        storage: SharedStorage,
    ) -> Result<Engine<RealEmailBackend>, String> {
        let cfg: EmailExtensionConfig = crate::parse_config(&configure.config)?;
        self.try_configure_with_config(cfg, configure.state_dir, configure.secrets, storage)
    }

    fn try_configure_with_config(
        &self,
        cfg: EmailExtensionConfig,
        state_dir: Option<PathBuf>,
        secrets: BTreeMap<String, tau_proto::SecretValue>,
        storage: SharedStorage,
    ) -> Result<Engine<RealEmailBackend>, String> {
        let state_dir = state_dir.unwrap_or_default();
        let config = cfg.validate()?;
        validate_config_secrets(&config, &secrets)?;
        let state = StateStore::open_with_storage(state_dir, storage)?;
        let backend = RealEmailBackend::new(&config, secrets, state.clone())?;
        Ok(Engine {
            config,
            state,
            backend,
        })
    }

    /// Dispatch a model-visible `email` tool invocation.
    pub fn dispatch(&mut self, invoke: ToolStarted) -> Event {
        let invoke = invoke_with_command(invoke);
        match &mut self.config_state {
            ConfigState::Configured(engine) => match parse_command(&invoke.arguments) {
                Ok(command) => finish_tool_result(invoke, engine.dispatch(command)),
                Err(error) => tool_error(invoke, error),
            },
            ConfigState::Unconfigured => {
                let command = command_from_arguments(&invoke.arguments).map(str::to_owned);
                tool_error(
                    invoke,
                    error_envelope(
                        command.as_deref(),
                        "invalid_input",
                        "email extension has not been configured",
                    ),
                )
            }
            ConfigState::Rejected { reason } => {
                let command = command_from_arguments(&invoke.arguments).map(str::to_owned);
                tool_error(
                    invoke,
                    error_envelope(
                        command.as_deref(),
                        "invalid_input",
                        &format!("email extension configuration was rejected: {reason}"),
                    ),
                )
            }
        }
    }

    /// Dispatch a user `:email` action invocation.
    pub fn dispatch_action(&mut self, invoke: ActionInvoke) -> Event {
        let result = match &mut self.config_state {
            ConfigState::Configured(engine) => {
                engine.dispatch_action(&invoke.action_id, &invoke.argv)
            }
            ConfigState::Unconfigured => Err("email extension has not been configured".to_owned()),
            ConfigState::Rejected { reason } => Err(format!(
                "email extension configuration was rejected: {reason}"
            )),
        };
        match result {
            Ok(text) => Event::ActionResultReported(ActionResult {
                invocation_id: invoke.invocation_id,
                action_id: invoke.action_id,
                output: ActionOutput::Text { text },
            }),
            Err(message) => Event::ActionErrorReported(ActionError {
                invocation_id: invoke.invocation_id,
                action_id: invoke.action_id,
                message,
                details: None,
            }),
        }
    }
}

/// Return the model-visible split email tool specifications.
pub fn email_tool_specs() -> Vec<ToolSpec> {
    EMAIL_COMMANDS
        .iter()
        .map(|command| email_command_tool_spec(command))
        .collect()
}

/// Return the legacy model-visible email envelope tool specification.
pub fn email_tool_spec() -> ToolSpec {
    email_envelope_tool_spec(TOOL_NAME)
}

fn email_command_tool_spec(command: &str) -> ToolSpec {
    let mut spec = email_envelope_tool_spec(&format!("{TOOL_PREFIX}{command}"));
    spec.description = Some(email_tool_description(command).to_owned());
    spec.parameters = Some(email_command_parameters(command));
    spec.examples = email_command_examples(command);
    spec
}

fn email_envelope_tool_spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(name),
        model_visible_name: None,
        description: Some("Controlled email access. Folder ids are opaque values returned by email_list_folders; omit folder to use the default folder. request_access and sends can require approval; message-management commands do not.".to_owned()),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "enum": EMAIL_COMMANDS,
                    "description": "Email operation to perform."
                },
                "args": email_common_arg_properties()
            },
            "required": ["command"],
            "additionalProperties": false
        })),
        format: None,
        tags: Vec::new(),
        enabled_by_default: false,
        background_support: None,
        examples: email_envelope_examples(),
    }
}

fn example_field(name: &str, value: CborValue) -> (CborValue, CborValue) {
    (CborValue::Text(name.to_owned()), value)
}

fn cbor_text(value: &str) -> CborValue {
    CborValue::Text(value.to_owned())
}

fn cbor_int(value: i64) -> CborValue {
    CborValue::Integer(value.into())
}

fn email_example(id: &str, title: &str, arguments: CborValue) -> ToolExample {
    ToolExample {
        id: id.to_owned(),
        title: Some(title.to_owned()),
        arguments,
        note: None,
        subcommand: None,
    }
}

fn email_command_examples(command: &str) -> Vec<ToolExample> {
    let example = match command {
        "list_folders" => email_example("list-folders", "List folders", CborValue::Map(Vec::new())),
        "list_recent" => email_example(
            "list-recent",
            "List recent mail",
            CborValue::Map(vec![example_field("limit", cbor_int(10))]),
        ),
        "read" => email_example(
            "read-message",
            "Read a message",
            CborValue::Map(vec![example_field("email_id", cbor_text("msg_123"))]),
        ),
        "request_access" => email_example(
            "request-access",
            "Request full access",
            CborValue::Map(vec![example_field("email_id", cbor_text("msg_123"))]),
        ),
        "mark_read" => email_example(
            "mark-read",
            "Mark read",
            CborValue::Map(vec![example_field("email_id", cbor_text("msg_123"))]),
        ),
        "mark_unread" => email_example(
            "mark-unread",
            "Mark unread",
            CborValue::Map(vec![example_field("email_id", cbor_text("msg_123"))]),
        ),
        "star" => email_example(
            "star-message",
            "Star a message",
            CborValue::Map(vec![example_field("email_id", cbor_text("msg_123"))]),
        ),
        "unstar" => email_example(
            "unstar-message",
            "Unstar a message",
            CborValue::Map(vec![example_field("email_id", cbor_text("msg_123"))]),
        ),
        "trash" => email_example(
            "trash-message",
            "Move to trash",
            CborValue::Map(vec![example_field("email_id", cbor_text("msg_123"))]),
        ),
        "send" => email_example(
            "send-mail",
            "Send mail",
            CborValue::Map(vec![
                example_field("to", CborValue::Array(vec![cbor_text("alice@example.com")])),
                example_field("subject", cbor_text("Quick update")),
                example_field(
                    "body_text",
                    cbor_text("Hello Alice,\n\nHere is the update."),
                ),
            ]),
        ),
        _ => return Vec::new(),
    };
    vec![example]
}

fn email_envelope_examples() -> Vec<ToolExample> {
    EMAIL_COMMANDS
        .iter()
        .flat_map(|command| {
            email_command_examples(command)
                .into_iter()
                .map(|mut example| {
                    let args = email_envelope_args(command, example.arguments);
                    example.arguments = CborValue::Map(vec![
                        example_field("command", cbor_text(command)),
                        example_field("args", args),
                    ]);
                    example.subcommand = Some(ToolExampleSelector {
                        path: vec!["command".to_owned()],
                        value: cbor_text(command),
                    });
                    example
                })
        })
        .collect()
}

fn email_envelope_args(command: &str, args: CborValue) -> CborValue {
    split_tool_args_to_command_args(command, args)
}

fn email_command_parameters(command: &str) -> serde_json::Value {
    let required = match command {
        "read" | "request_access" | "mark_read" | "mark_unread" | "star" | "unstar" | "trash" => {
            serde_json::json!(["email_id"])
        }
        "send" => serde_json::json!(["to", "subject", "body_text"]),
        _ => serde_json::json!([]),
    };
    serde_json::json!({
        "type": "object",
        "description": email_args_description(command),
        "properties": email_command_properties(command),
        "required": required,
        "additionalProperties": false
    })
}

fn email_common_arg_properties() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "folder": {"type": "string", "description": "Folder id from email_list_folders. Optional for folder-targeting commands; defaults to the default folder."},
            "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Maximum messages to list. Optional; defaults to 100 and is capped at 100."},
            "days": {"type": "integer", "minimum": 1, "maximum": 365, "description": "For email_list_recent, include messages whose IMAP internal timestamp falls within the inclusive rolling N × 24-hour window ending when the operation starts. Optional; defaults to 7 and is capped at 365."},
            "cursor": {"type": "string", "description": "Pagination cursor returned by email_list_recent."},
            "email_id": {"type": "string", "description": "Message id from email list results."},
            "uid": {"type": "string", "description": "Legacy message uid for the envelope email tool."},
            "to": {"type": "array", "items": {"type": "string"}, "description": "Recipients. Required for email_send."},
            "cc": {"type": "array", "items": {"type": "string"}, "description": "Cc recipients for email_send."},
            "bcc": {"type": "array", "items": {"type": "string"}, "description": "Bcc recipients for email_send."},
            "subject": {"type": "string", "description": "Subject. Required for email_send; may be empty."},
            "body_text": {"type": "string", "description": "Plain text body. Required for email_send; may be empty."},
            "from": {"type": "string", "description": "Optional From identity; normally omit to use the account default."},
            "reply_to": {"type": ["string", "null"]},
            "in_reply_to": {"type": ["string", "null"]}
        },
        "additionalProperties": false
    })
}

fn email_command_properties(command: &str) -> serde_json::Value {
    match command {
        "list_folders" => serde_json::json!({}),
        "list_recent" => serde_json::json!({
            "folder": {"type": "string", "description": "Folder id from email_list_folders. Optional; defaults to the default folder."},
            "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Maximum messages to return. Optional; defaults to 100."},
            "days": {"type": "integer", "minimum": 1, "maximum": 365, "description": "Search messages in the inclusive rolling N × 24-hour window ending when the operation starts. Optional; defaults to 7."},
            "cursor": {"type": "string", "description": "Pagination cursor returned by email_list_recent."}

        }),
        "list_by_uid" => serde_json::json!({
            "folder": {"type": "string", "description": "Folder id from email_list_folders. Optional; defaults to the default folder."},
            "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Maximum messages to return. Optional; defaults to 100."},
            "cursor": {"type": "string", "description": "Pagination cursor."}

        }),
        "read" | "request_access" | "mark_read" | "mark_unread" | "star" | "unstar" | "trash" => {
            serde_json::json!({
                "folder": {"type": "string", "description": "Folder id from email_list_folders. Optional; defaults to the default folder."},
                "email_id": {"type": "string", "description": "Message id from email list results."}
            })
        }
        "send" => serde_json::json!({
            "to": {"type": "array", "items": {"type": "string"}, "description": "Recipient email addresses."},
            "cc": {"type": "array", "items": {"type": "string"}, "description": "Cc recipient email addresses."},
            "bcc": {"type": "array", "items": {"type": "string"}, "description": "Bcc recipient email addresses."},
            "subject": {"type": "string", "description": "Message subject; may be empty."},
            "body_text": {"type": "string", "description": "Plain text message body; may be empty."},
            "from": {"type": "string", "description": "Optional From identity; normally omit."},
            "reply_to": {"type": ["string", "null"], "description": "Optional Reply-To address."},
            "in_reply_to": {"type": ["string", "null"], "description": "Optional message id this send replies to."}
        }),
        _ => serde_json::json!({}),
    }
}

fn email_tool_description(command: &str) -> &'static str {
    match command {
        "list_folders" => {
            "List configured email folders. Returns folder ids for other email tools."
        }
        "list_recent" => {
            "List recent email messages. Optional folder, days, limit, and cursor; omit folder to use the default folder."
        }
        "read" => {
            "Read an email by email_id. Returns full content only when policy or approval allows it; otherwise returns preview or approval_required."
        }
        "request_access" => "Request user approval to read full content for one email by email_id.",
        "mark_read" => "Mark one email as read by email_id.",
        "mark_unread" => "Mark one email as unread by email_id.",
        "star" => "Star one email by email_id.",
        "unstar" => "Unstar one email by email_id.",
        "trash" => "Move one email to Trash by email_id.",
        "send" => "Send a plain-text email. May require user approval.",
        _ => "Run an email command.",
    }
}

fn email_args_description(command: &str) -> &'static str {
    match command {
        "list_folders" => "No arguments.",
        "send" => "Outgoing message fields.",
        _ => "Email target arguments. Omit folder to use the default folder.",
    }
}

/// Return the prompt fragment that teaches the model email tool policy.
pub fn email_prompt_fragment() -> PromptFragment {
    PromptFragment::new(
        "email.instructions",
        PromptPriority::new(120),
        include_str!("prompts/email_instructions.md"),
    )
}

/// Return the `:email` action schema.
pub fn email_action_schema() -> ActionSchema {
    email_action_schema_with_accounts(&[])
}

/// Return the `:email` action schema with current Google OAuth account choices.
pub(crate) fn email_action_schema_with_accounts(accounts: &[String]) -> ActionSchema {
    fn string_arg(name: &str, description: &str) -> ActionArg {
        ActionArg {
            name: name.to_owned(),
            description: description.to_owned(),
            required: true,
            suggestions: Vec::new(),
            kind: ActionArgKind::String,
        }
    }
    fn rest_string_arg(name: &str, description: &str) -> ActionArg {
        ActionArg {
            name: name.to_owned(),
            description: description.to_owned(),
            required: true,
            suggestions: Vec::new(),
            kind: ActionArgKind::RestString,
        }
    }
    fn optional_integer_arg(name: &str, description: &str) -> ActionArg {
        ActionArg {
            name: name.to_owned(),
            description: description.to_owned(),
            required: false,
            suggestions: Vec::new(),
            kind: ActionArgKind::Integer,
        }
    }
    fn leaf(name: &str, action_id: &str, description: &str, args: Vec<ActionArg>) -> ActionCommand {
        ActionCommand {
            name: name.to_owned(),
            description: description.to_owned(),
            action_id: Some(action_id.to_owned()),
            args,
            children: Vec::new(),
        }
    }
    fn group(name: &str, description: &str, children: Vec<ActionCommand>) -> ActionCommand {
        ActionCommand {
            name: name.to_owned(),
            description: description.to_owned(),
            action_id: None,
            args: Vec::new(),
            children,
        }
    }

    let id_arg = || string_arg("id", "approval id");
    let ids_arg = || rest_string_arg("ids", "one or more approval ids");
    let approve_ids_arg = || ActionArg {
        name: "ids".to_owned(),
        description: "one or more approval ids, or all".to_owned(),
        required: true,
        suggestions: vec![ActionChoice {
            value: "all".to_owned(),
            description: "All pending approvals".to_owned(),
        }],
        kind: ActionArgKind::RestString,
    };
    let pattern_arg = || string_arg("pattern", "glob or email address");
    let limit_arg =
        || optional_integer_arg("number", "number of recent log entries; defaults to 20");
    ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![ActionCommand {
            name: ":email".to_owned(),
            description: "Review, approve, deny, and audit email access".to_owned(),
            action_id: None,
            args: Vec::new(),
            children: vec![
                group(
                    "auth",
                    "Email account authorization actions",
                    vec![group(
                        "google",
                        "Google Gmail installed-app browser authorization",
                        vec![
                            leaf(
                                "start",
                                "email.auth.google.start",
                                "Start Google email authorization",
                                vec![crate::google_auth_account_arg("Email", accounts)],
                            ),
                            leaf(
                                "finish",
                                "email.auth.google.finish",
                                "Finish Google email authorization",
                                vec![
                                    crate::google_auth_account_arg("Email", accounts),
                                    rest_string_arg(
                                        "redirect_url",
                                        "Full failed loopback URL copied from the browser address bar",
                                    ),
                                ],
                            ),
                        ],
                    )],
                ),
                group(
                    "out",
                    "Outgoing email approval actions",
                    vec![
                        leaf(
                            "list",
                            "email.out.list",
                            "List pending outgoing approvals",
                            Vec::new(),
                        ),
                        leaf(
                            "open",
                            "email.out.open",
                            "Inspect a pending outgoing draft",
                            vec![id_arg()],
                        ),
                        leaf(
                            "approve",
                            "email.out.approve",
                            "Approve one or more outgoing drafts",
                            vec![approve_ids_arg()],
                        ),
                        leaf(
                            "deny",
                            "email.out.deny",
                            "Deny one or more outgoing drafts",
                            vec![ids_arg()],
                        ),
                        leaf(
                            "whitelist",
                            "email.out.whitelist",
                            "Allow outgoing recipients matching a pattern",
                            vec![pattern_arg()],
                        ),
                    ],
                ),
                group(
                    "log",
                    "Email audit log actions",
                    vec![leaf(
                        "last",
                        "email.log.last",
                        "Show recent email access and mutation log entries",
                        vec![limit_arg()],
                    )],
                ),
                group(
                    "in",
                    "Incoming email read approval actions",
                    vec![
                        leaf(
                            "list",
                            "email.in.list",
                            "List pending incoming read approvals",
                            Vec::new(),
                        ),
                        leaf(
                            "open",
                            "email.in.open",
                            "Open a pending incoming message for user review; may display email content",
                            vec![id_arg()],
                        ),
                        leaf(
                            "approve",
                            "email.in.approve",
                            "Approve one or more incoming reads",
                            vec![approve_ids_arg()],
                        ),
                        leaf(
                            "deny",
                            "email.in.deny",
                            "Deny one or more incoming reads and persist exact denials",
                            vec![ids_arg()],
                        ),
                        leaf(
                            "whitelist",
                            "email.in.whitelist",
                            "Allow incoming senders matching a pattern",
                            vec![pattern_arg()],
                        ),
                    ],
                ),
            ],
        }],
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MessageManagementCommand {
    MarkRead,
    MarkUnread,
    Star,
    Unstar,
}

impl MessageManagementCommand {
    fn command_name(self) -> &'static str {
        match self {
            Self::MarkRead => "mark_read",
            Self::MarkUnread => "mark_unread",
            Self::Star => "star",
            Self::Unstar => "unstar",
        }
    }

    fn status_name(self) -> &'static str {
        match self {
            Self::MarkRead => "marked_read",
            Self::MarkUnread => "marked_unread",
            Self::Star => "starred",
            Self::Unstar => "unstarred",
        }
    }

    fn success_message(self) -> &'static str {
        match self {
            Self::MarkRead => "Message marked read.",
            Self::MarkUnread => "Message marked unread.",
            Self::Star => "Message starred.",
            Self::Unstar => "Message unstarred.",
        }
    }

    fn mutation(self) -> MessageFlagMutation {
        match self {
            Self::MarkRead => MessageFlagMutation::AddSeen,
            Self::MarkUnread => MessageFlagMutation::RemoveSeen,
            Self::Star => MessageFlagMutation::AddFlagged,
            Self::Unstar => MessageFlagMutation::RemoveFlagged,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum EmailCommand {
    ListFolders {
        account: String,
    },
    ListByUid {
        account: String,
        folder: String,
        limit: u32,
        cursor: Option<String>,
    },
    ListRecent {
        account: String,
        folder: String,
        limit: u32,
        cursor: Option<String>,
        days: u32,
    },
    Read {
        account: String,
        folder: String,
        uid: String,
    },
    RequestFull {
        account: String,
        folder: String,
        uid: String,
    },
    ManageMessage {
        command: MessageManagementCommand,
        account: String,
        folder: String,
        uid: String,
    },
    Trash {
        account: String,
        folder: String,
        uid: String,
    },
    Send {
        account: Option<String>,
        from: Option<String>,
        to: Vec<String>,
        cc: Vec<String>,
        bcc: Vec<String>,
        subject: String,
        body_text: String,
        reply_to: Option<String>,
        in_reply_to: Option<String>,
    },
}

fn email_command_name(command: &EmailCommand) -> &'static str {
    match command {
        EmailCommand::ListFolders { .. } => "list_folders",
        EmailCommand::ListByUid { .. } => "list_by_uid",
        EmailCommand::ListRecent { .. } => "list_recent",
        EmailCommand::Read { .. } => "read",
        EmailCommand::RequestFull { .. } => "request_access",
        EmailCommand::ManageMessage { command, .. } => command.command_name(),
        EmailCommand::Trash { .. } => "trash",
        EmailCommand::Send { .. } => "send",
    }
}

fn email_log_kind(command: &EmailCommand) -> Option<&'static str> {
    match command {
        EmailCommand::ListByUid { .. }
        | EmailCommand::ListRecent { .. }
        | EmailCommand::Read { .. }
        | EmailCommand::RequestFull { .. } => Some("access"),
        EmailCommand::ManageMessage { .. }
        | EmailCommand::Trash { .. }
        | EmailCommand::Send { .. } => Some("mutable"),
        EmailCommand::ListFolders { .. } => None,
    }
}

fn email_log_status(result: &CborValue) -> String {
    cbor_text_field(result, "status")
        .or_else(|| cbor_field(result, "error").and_then(|error| cbor_text_field(error, "code")))
        .unwrap_or("unknown")
        .to_owned()
}

fn email_log_reason(result: &CborValue) -> Option<String> {
    cbor_field(result, "data")
        .and_then(|data| cbor_text_field(data, "reason"))
        .or_else(|| cbor_field(result, "error").and_then(|error| cbor_text_field(error, "message")))
        .map(|reason| safe_model_line(reason, MAX_HEADER_VALUE_CHARS))
}

fn email_log_title(title: &str) -> String {
    safe_model_line(title, EMAIL_LOG_TITLE_MAX_CHARS)
}

fn log_field(data: Option<&CborValue>, field: &str, fallback: Option<&str>) -> Option<String> {
    data.and_then(|data| cbor_text_field(data, field))
        .or(fallback.filter(|value| !value.is_empty()))
        .map(|value| safe_model_line(value, MAX_HEADER_VALUE_CHARS))
}

fn log_account(data: Option<&CborValue>, fallback: Option<&str>) -> Option<String> {
    log_field(data, "account", fallback)
}

fn format_email_log_entry(entry: &EmailLogEntry) -> String {
    let mut line = format!(
        "{} {}/{} status={}",
        entry.ts_unix_ms,
        safe_display_line(&entry.kind),
        safe_display_line(&entry.command),
        safe_display_line(&entry.status)
    );
    push_email_log_part(&mut line, "account", entry.account.as_deref());
    push_email_log_part(&mut line, "folder", entry.folder.as_deref());
    push_email_log_part(&mut line, "uid", entry.uid.as_deref());
    push_email_log_part(&mut line, "access", entry.access.as_deref());
    push_email_log_part(&mut line, "from", entry.from.as_deref());
    if !entry.to.is_empty() {
        line.push_str(" to=");
        line.push_str(&safe_display_join(&entry.to, ","));
    }
    if let Some(title) = &entry.title {
        if entry.title_redacted {
            line.push_str(" title_preview=");
        } else {
            line.push_str(" title=");
        }
        line.push_str(&safe_display_line(title));
    }
    push_email_log_part(&mut line, "approval", entry.approval_id.as_deref());
    if let Some(count) = entry.message_count {
        line.push_str(&format!(" messages={count}"));
    }
    push_email_log_part(&mut line, "reason", entry.reason.as_deref());
    line
}

fn push_email_log_part(line: &mut String, name: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        line.push(' ');
        line.push_str(name);
        line.push('=');
        line.push_str(&safe_display_line(value));
    }
}

fn command_from_arguments(arguments: &CborValue) -> Option<&str> {
    let CborValue::Map(entries) = arguments else {
        return None;
    };
    entries.iter().find_map(|(key, value)| match (key, value) {
        (CborValue::Text(key), CborValue::Text(value)) if key == "command" => Some(value.as_str()),
        _ => None,
    })
}

type CborMapEntries<'a> = &'a [(CborValue, CborValue)];
type CommandEnvelope<'a> = (String, CborMapEntries<'a>);

fn command_for_tool_name(tool_name: &str) -> Option<&'static str> {
    if tool_name == TOOL_NAME {
        return None;
    }
    let tool_suffix = tool_name.strip_prefix(TOOL_PREFIX)?;
    EMAIL_COMMANDS
        .iter()
        .find_map(|command| (*command == tool_suffix).then_some(*command))
}
fn command_arguments(command: &str, args: CborValue) -> CborValue {
    let args = split_tool_args_to_command_args(command, args);
    CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text(command.to_owned()),
        ),
        (CborValue::Text("args".to_owned()), args),
    ])
}

fn split_tool_args_to_command_args(command: &str, args: CborValue) -> CborValue {
    if !uses_email_id(command) {
        return args;
    }
    let CborValue::Map(entries) = args else {
        return args;
    };
    CborValue::Map(
        entries
            .into_iter()
            .map(|(key, value)| match key {
                CborValue::Text(key) if key == "email_id" => {
                    (CborValue::Text("uid".to_owned()), value)
                }
                other => (other, value),
            })
            .collect(),
    )
}

fn uses_email_id(command: &str) -> bool {
    matches!(
        command,
        "read" | "request_access" | "mark_read" | "mark_unread" | "star" | "unstar" | "trash"
    )
}

fn invoke_with_command(mut invoke: ToolStarted) -> ToolStarted {
    if let Some(command) = command_for_tool_name(invoke.tool_name.as_str()) {
        invoke.arguments = command_arguments(command, invoke.arguments);
    }
    invoke
}

pub(crate) fn is_tool_name(tool_name: &str) -> bool {
    command_for_tool_name(tool_name).is_some()
}

pub(crate) fn initial_display_for_tool(tool_name: &str, arguments: &CborValue) -> ToolUseState {
    if let Some(command) = command_for_tool_name(tool_name) {
        return ToolUseState {
            args: split_invocation_display_args(command, Some(arguments)).unwrap_or_default(),
            status: ToolUseStatus::InProgress,
            status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
            ..Default::default()
        };
    }
    initial_display(arguments)
}

fn parse_command(arguments: &CborValue) -> Result<EmailCommand, CborValue> {
    let (command, args) = parse_command_envelope(arguments)?;
    match command.as_str() {
        "list_folders" => parse_list_folders(&command, args),
        "list" | "list_by_uid" => parse_list_by_uid(&command, args),
        "list_recent" => parse_list_recent(&command, args),
        "read" => parse_read(&command, args),
        "request_access" => parse_request_access(&command, args),
        "mark_read" => parse_manage_message(&command, args, MessageManagementCommand::MarkRead),
        "mark_unread" => parse_manage_message(&command, args, MessageManagementCommand::MarkUnread),
        "star" => parse_manage_message(&command, args, MessageManagementCommand::Star),
        "unstar" => parse_manage_message(&command, args, MessageManagementCommand::Unstar),
        "trash" => parse_trash(&command, args),
        "send" => parse_send(&command, args),
        _ => Err(error_envelope(
            Some(&command),
            "invalid_input",
            "unsupported email command",
        )),
    }
}

fn parse_command_envelope(arguments: &CborValue) -> Result<CommandEnvelope<'_>, CborValue> {
    let CborValue::Map(entries) = arguments else {
        return Err(error_envelope(
            None,
            "invalid_input",
            "arguments must be an object",
        ));
    };
    let mut seen = BTreeSet::new();
    let command = required_string(entries, &mut seen, "command", None)?;
    let args = optional_object(entries, &mut seen, "args", Some(&command))?.unwrap_or(&[]);
    reject_extra(entries, &seen, Some(&command))?;
    Ok((command, args))
}

fn parse_list_folders(
    command: &str,
    args: &[(CborValue, CborValue)],
) -> Result<EmailCommand, CborValue> {
    reject_extra(args, &BTreeSet::new(), Some(command))?;
    Ok(EmailCommand::ListFolders {
        account: String::new(),
    })
}
fn parse_list_by_uid(
    command: &str,
    args: &[(CborValue, CborValue)],
) -> Result<EmailCommand, CborValue> {
    let mut seen = BTreeSet::new();
    let folder_arg = optional_string(args, &mut seen, "folder", Some(command))?;
    let (account, folder) = parse_flattened_folder_arg(command, folder_arg.as_deref())?;
    let limit = optional_positive_u32(args, &mut seen, "limit", Some(command))?
        .unwrap_or(DEFAULT_LIST_LIMIT);
    let cursor = optional_string(args, &mut seen, "cursor", Some(command))?;
    reject_extra(args, &seen, Some(command))?;
    Ok(EmailCommand::ListByUid {
        account,
        folder,
        limit,
        cursor,
    })
}
fn parse_list_recent(
    command: &str,
    args: &[(CborValue, CborValue)],
) -> Result<EmailCommand, CborValue> {
    let mut seen = BTreeSet::new();
    let folder_arg = optional_string(args, &mut seen, "folder", Some(command))?;
    let (account, folder) = parse_flattened_folder_arg(command, folder_arg.as_deref())?;
    let limit = optional_positive_u32(args, &mut seen, "limit", Some(command))?
        .unwrap_or(DEFAULT_LIST_LIMIT);
    let days = optional_positive_u32(args, &mut seen, "days", Some(command))?
        .unwrap_or(DEFAULT_RECENT_DAYS)
        .min(MAX_RECENT_DAYS);
    let cursor = optional_string(args, &mut seen, "cursor", Some(command))?;
    reject_extra(args, &seen, Some(command))?;
    Ok(EmailCommand::ListRecent {
        account,
        folder,
        limit,
        cursor,
        days,
    })
}
fn parse_read(command: &str, args: &[(CborValue, CborValue)]) -> Result<EmailCommand, CborValue> {
    let (account, folder, uid) = parse_message_target(command, args)?;
    Ok(EmailCommand::Read {
        account,
        folder,
        uid,
    })
}
fn parse_request_access(
    command: &str,
    args: &[(CborValue, CborValue)],
) -> Result<EmailCommand, CborValue> {
    let (account, folder, uid) = parse_message_target(command, args)?;
    Ok(EmailCommand::RequestFull {
        account,
        folder,
        uid,
    })
}
fn parse_manage_message(
    command: &str,
    args: &[(CborValue, CborValue)],
    message_command: MessageManagementCommand,
) -> Result<EmailCommand, CborValue> {
    let (account, folder, uid) = parse_message_target(command, args)?;
    Ok(EmailCommand::ManageMessage {
        command: message_command,
        account,
        folder,
        uid,
    })
}
fn parse_trash(command: &str, args: &[(CborValue, CborValue)]) -> Result<EmailCommand, CborValue> {
    let (account, folder, uid) = parse_message_target(command, args)?;
    Ok(EmailCommand::Trash {
        account,
        folder,
        uid,
    })
}
fn parse_flattened_folder_arg(
    command: &str,
    folder: Option<&str>,
) -> Result<(String, String), CborValue> {
    let Some(folder) = folder else {
        return Ok((String::new(), DEFAULT_FOLDER.to_owned()));
    };
    let Some((account, folder)) = folder.split_once('/') else {
        return Err(error_envelope(
            Some(command),
            "invalid_input",
            "folder must be a folder id from email_list_folders",
        ));
    };
    let account = crate::opaque_id::decode_component(account).map_err(|_| {
        error_envelope(
            Some(command),
            "invalid_input",
            "folder must be a folder id from email_list_folders",
        )
    })?;
    let folder = crate::opaque_id::decode_component(folder).map_err(|_| {
        error_envelope(
            Some(command),
            "invalid_input",
            "folder must be a folder id from email_list_folders",
        )
    })?;
    if account.trim().is_empty() || folder.trim().is_empty() {
        return Err(error_envelope(
            Some(command),
            "invalid_input",
            "folder must be a folder id from email_list_folders",
        ));
    }
    Ok((account, folder))
}

fn flatten_folder_id(account: &str, folder: &str) -> String {
    format!(
        "{}/{}",
        crate::opaque_id::encode_component(account),
        crate::opaque_id::encode_component(folder)
    )
}

fn parse_message_target(
    command: &str,
    args: &[(CborValue, CborValue)],
) -> Result<(String, String, String), CborValue> {
    let mut seen = BTreeSet::new();
    let folder_arg = optional_string(args, &mut seen, "folder", Some(command))?;
    let (account, folder) = parse_flattened_folder_arg(command, folder_arg.as_deref())?;
    let uid = required_string(args, &mut seen, "uid", Some(command))?;
    reject_extra(args, &seen, Some(command))?;
    Ok((account, folder, uid))
}
fn parse_send(command: &str, args: &[(CborValue, CborValue)]) -> Result<EmailCommand, CborValue> {
    let mut seen = BTreeSet::new();
    let from = optional_string(args, &mut seen, "from", Some(command))?;
    let to = required_string_array(args, &mut seen, "to", Some(command))?;
    let cc = optional_string_array(args, &mut seen, "cc", Some(command))?;
    let bcc = optional_string_array(args, &mut seen, "bcc", Some(command))?;
    let subject = required_string_allow_empty(args, &mut seen, "subject", Some(command))?;
    let body_text = required_string_allow_empty(args, &mut seen, "body_text", Some(command))?;
    let reply_to = optional_nullable_string(args, &mut seen, "reply_to", Some(command))?;
    let in_reply_to = optional_nullable_string(args, &mut seen, "in_reply_to", Some(command))?;
    reject_non_empty_array(args, &mut seen, "attachments", Some(command))?;
    reject_extra(args, &seen, Some(command))?;
    Ok(EmailCommand::Send {
        account: None,
        from,
        to,
        cc,
        bcc,
        subject,
        body_text,
        reply_to,
        in_reply_to,
    })
}

fn required_string(
    entries: &[(CborValue, CborValue)],
    seen: &mut BTreeSet<String>,
    name: &str,
    command: Option<&str>,
) -> Result<String, CborValue> {
    match field(entries, seen, name, command)? {
        Some(CborValue::Text(value)) if !value.trim().is_empty() => Ok(value.clone()),
        Some(CborValue::Text(_)) => Err(error_envelope(
            command,
            "invalid_input",
            &format!("`{name}` must not be empty"),
        )),
        Some(_) => Err(error_envelope(
            command,
            "invalid_input",
            &format!("`{name}` must be a string"),
        )),
        None => Err(error_envelope(
            command,
            "invalid_input",
            &format!("missing `{name}`"),
        )),
    }
}
fn required_string_allow_empty(
    entries: &[(CborValue, CborValue)],
    seen: &mut BTreeSet<String>,
    name: &str,
    command: Option<&str>,
) -> Result<String, CborValue> {
    match field(entries, seen, name, command)? {
        Some(CborValue::Text(value)) => Ok(value.clone()),
        Some(_) => Err(error_envelope(
            command,
            "invalid_input",
            &format!("`{name}` must be a string"),
        )),
        None => Err(error_envelope(
            command,
            "invalid_input",
            &format!("missing `{name}`"),
        )),
    }
}
fn optional_string(
    entries: &[(CborValue, CborValue)],
    seen: &mut BTreeSet<String>,
    name: &str,
    command: Option<&str>,
) -> Result<Option<String>, CborValue> {
    match field(entries, seen, name, command)? {
        Some(CborValue::Text(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
        Some(CborValue::Text(_)) => Err(error_envelope(
            command,
            "invalid_input",
            &format!("`{name}` must not be empty"),
        )),
        Some(CborValue::Null) | None => Ok(None),
        Some(_) => Err(error_envelope(
            command,
            "invalid_input",
            &format!("`{name}` must be a string"),
        )),
    }
}
fn optional_nullable_string(
    entries: &[(CborValue, CborValue)],
    seen: &mut BTreeSet<String>,
    name: &str,
    command: Option<&str>,
) -> Result<Option<String>, CborValue> {
    match field(entries, seen, name, command)? {
        Some(CborValue::Null) | None => Ok(None),
        Some(CborValue::Text(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
        Some(CborValue::Text(_)) => Err(error_envelope(
            command,
            "invalid_input",
            &format!("`{name}` must not be empty"),
        )),
        Some(_) => Err(error_envelope(
            command,
            "invalid_input",
            &format!("`{name}` must be a string or null"),
        )),
    }
}
fn optional_positive_u32(
    entries: &[(CborValue, CborValue)],
    seen: &mut BTreeSet<String>,
    name: &str,
    command: Option<&str>,
) -> Result<Option<u32>, CborValue> {
    match field(entries, seen, name, command)? {
        Some(CborValue::Integer(value)) => {
            let raw: i128 = (*value).into();
            if raw < 1 || i128::from(u32::MAX) < raw {
                return Err(error_envelope(
                    command,
                    "invalid_input",
                    &format!("`{name}` must be a positive integer"),
                ));
            }
            Ok(Some(raw as u32))
        }
        Some(CborValue::Null) | None => Ok(None),
        Some(_) => Err(error_envelope(
            command,
            "invalid_input",
            &format!("`{name}` must be an integer"),
        )),
    }
}
fn required_string_array(
    entries: &[(CborValue, CborValue)],
    seen: &mut BTreeSet<String>,
    name: &str,
    command: Option<&str>,
) -> Result<Vec<String>, CborValue> {
    let Some(value) = field(entries, seen, name, command)? else {
        return Err(error_envelope(
            command,
            "invalid_input",
            &format!("missing `{name}`"),
        ));
    };
    string_array_value(value, name, command, false)
}
fn optional_string_array(
    entries: &[(CborValue, CborValue)],
    seen: &mut BTreeSet<String>,
    name: &str,
    command: Option<&str>,
) -> Result<Vec<String>, CborValue> {
    match field(entries, seen, name, command)? {
        Some(value) => string_array_value(value, name, command, true),
        None => Ok(Vec::new()),
    }
}
fn string_array_value(
    value: &CborValue,
    name: &str,
    command: Option<&str>,
    allow_empty: bool,
) -> Result<Vec<String>, CborValue> {
    let CborValue::Array(values) = value else {
        return Err(error_envelope(
            command,
            "invalid_input",
            &format!("`{name}` must be an array"),
        ));
    };
    let mut out = Vec::new();
    for value in values {
        let CborValue::Text(text) = value else {
            return Err(error_envelope(
                command,
                "invalid_input",
                &format!("`{name}` entries must be strings"),
            ));
        };
        if text.trim().is_empty() {
            return Err(error_envelope(
                command,
                "invalid_input",
                &format!("`{name}` entries must not be empty"),
            ));
        }
        out.push(text.clone());
    }
    if out.is_empty() && !allow_empty {
        return Err(error_envelope(
            command,
            "invalid_input",
            &format!("`{name}` must not be empty"),
        ));
    }
    Ok(out)
}
fn reject_non_empty_array(
    entries: &[(CborValue, CborValue)],
    seen: &mut BTreeSet<String>,
    name: &str,
    command: Option<&str>,
) -> Result<(), CborValue> {
    match field(entries, seen, name, command)? {
        Some(CborValue::Array(values)) if values.is_empty() => Ok(()),
        Some(CborValue::Array(_)) => Err(error_envelope(
            command,
            "invalid_input",
            &format!("`{name}` are not supported yet"),
        )),
        Some(_) => Err(error_envelope(
            command,
            "invalid_input",
            &format!("`{name}` must be an array"),
        )),
        None => Ok(()),
    }
}
fn optional_object<'a>(
    entries: &'a [(CborValue, CborValue)],
    seen: &mut BTreeSet<String>,
    name: &str,
    command: Option<&str>,
) -> Result<Option<&'a [(CborValue, CborValue)]>, CborValue> {
    match field(entries, seen, name, command)? {
        Some(CborValue::Map(values)) => Ok(Some(values)),
        Some(CborValue::Null) | None => Ok(None),
        Some(_) => Err(error_envelope(
            command,
            "invalid_input",
            &format!("`{name}` must be an object"),
        )),
    }
}
fn field<'a>(
    entries: &'a [(CborValue, CborValue)],
    seen: &mut BTreeSet<String>,
    name: &str,
    command: Option<&str>,
) -> Result<Option<&'a CborValue>, CborValue> {
    let mut found = None;
    for (key, value) in entries {
        let CborValue::Text(key) = key else {
            return Err(error_envelope(
                command,
                "invalid_input",
                "argument object keys must be strings",
            ));
        };
        if key == name {
            if found.is_some() {
                return Err(error_envelope(
                    command,
                    "invalid_input",
                    &format!("duplicate `{name}`"),
                ));
            }
            found = Some(value);
            seen.insert(name.to_owned());
        }
    }
    Ok(found)
}
fn reject_extra(
    entries: &[(CborValue, CborValue)],
    seen: &BTreeSet<String>,
    command: Option<&str>,
) -> Result<(), CborValue> {
    for (key, _) in entries {
        let CborValue::Text(key) = key else {
            return Err(error_envelope(
                command,
                "invalid_input",
                "argument object keys must be strings",
            ));
        };
        if !seen.contains(key) {
            return Err(error_envelope(
                command,
                "invalid_input",
                &format!("unexpected argument `{key}`"),
            ));
        }
    }
    Ok(())
}

fn finish_tool_result(invoke: ToolStarted, result: CborValue) -> Event {
    if cbor_bool_field(&result, "ok") == Some(false) {
        return tool_error(invoke, result);
    }
    let display = success_display_for_tool(invoke.tool_name.as_str(), &result);
    Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        result,
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(display),
        originator: tau_proto::PromptOriginator::User,
    })
}

fn tool_error(invoke: ToolStarted, details: CborValue) -> Event {
    let message = email_error_message_for_tool(invoke.tool_name.as_str(), &details);
    let display = error_display_for_tool(
        invoke.tool_name.as_str(),
        &invoke.arguments,
        &details,
        &message,
    );
    Event::ToolError(ToolError {
        presentation: Default::default(),
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        message: message.clone(),
        details: Some(details),
        display: Some(display),
        originator: tau_proto::PromptOriginator::User,
    })
}
fn email_error_message_for_tool(tool_name: &str, details: &CborValue) -> String {
    let message =
        cbor_nested_text_field(details, "error", "message").unwrap_or("invalid email tool request");
    let Some(code) = cbor_nested_text_field(details, "error", "code") else {
        return message.to_owned();
    };
    if command_for_tool_name(tool_name).is_some() {
        return format!(
            "{} failed ({code}): {message}",
            safe_model_line(tool_name, MAX_HEADER_VALUE_CHARS)
        );
    }
    match cbor_text_field(details, "command") {
        Some(command) => format!(
            "email {} failed ({code}): {message}",
            safe_model_line(command, MAX_HEADER_VALUE_CHARS)
        ),
        None => format!("email failed ({code}): {message}"),
    }
}

#[cfg(test)]
fn email_error_message(details: &CborValue) -> String {
    email_error_message_for_tool(TOOL_NAME, details)
}
fn ok_envelope(command: &str, status: &str, data: CborValue) -> CborValue {
    cbor_map(vec![
        ("ok", CborValue::Bool(true)),
        ("command", CborValue::Text(command.to_owned())),
        ("status", CborValue::Text(status.to_owned())),
        ("data", data),
    ])
}
fn ok_line_envelope(
    command: &str,
    status: &str,
    headers: Vec<(&str, CborValue)>,
    data: CborValue,
    rows: &[CborValue],
) -> CborValue {
    let mut entries = vec![
        ("ok", CborValue::Bool(true)),
        ("command", CborValue::Text(command.to_owned())),
        ("status", CborValue::Text(status.to_owned())),
    ];
    entries.extend(headers);
    entries.push(("data", data));
    entries.push(("output", CborValue::Text(line_rows_output(rows))));
    cbor_map(entries)
}

fn line_rows_output(rows: &[CborValue]) -> String {
    let lines = rows
        .iter()
        .filter_map(|value| match value {
            CborValue::Text(line) => Some(line.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        "(no matches found)".to_owned()
    } else {
        lines.join("\n")
    }
}

fn error_envelope(command: Option<&str>, code: &str, message: &str) -> CborValue {
    error_envelope_with_details(command, code, message, CborValue::Map(Vec::new()))
}
fn error_envelope_with_details(
    command: Option<&str>,
    code: &str,
    message: &str,
    details: CborValue,
) -> CborValue {
    cbor_map(vec![
        ("ok", CborValue::Bool(false)),
        (
            "command",
            command
                .map(|c| CborValue::Text(c.to_owned()))
                .unwrap_or(CborValue::Null),
        ),
        (
            "error",
            structured_error_with_details(code, message, details),
        ),
    ])
}
fn backend_error_envelope(command: Option<&str>, default_code: &str, message: &str) -> CborValue {
    let code = backend_error_code(message, default_code);
    let text = backend_error_text(message);
    cbor_map(vec![
        ("ok", CborValue::Bool(false)),
        (
            "command",
            command
                .map(|c| CborValue::Text(c.to_owned()))
                .unwrap_or(CborValue::Null),
        ),
        (
            "error",
            structured_error_with_details(
                code,
                &text,
                cbor_map(vec![(
                    "backend_message",
                    CborValue::Text(safe_model_line(message, MAX_BACKEND_ERROR_CHARS)),
                )]),
            ),
        ),
    ])
}

const SMTP_OUTCOME_UNKNOWN_MESSAGE: &str = "SMTP may have accepted this email; do not retry \
automatically; reconcile with the account or provider before sending again.";

fn smtp_outcome_unknown_envelope() -> CborValue {
    error_envelope_with_details(
        Some("send"),
        "smtp_outcome_unknown",
        SMTP_OUTCOME_UNKNOWN_MESSAGE,
        CborValue::Map(Vec::new()),
    )
}
fn backend_error_code<'a>(message: &'a str, default_code: &'a str) -> &'a str {
    for code in [
        "auth_error",
        "network_error",
        "tls_error",
        "imap_error",
        "smtp_error",
        "message_not_found",
        "invalid_input",
        "internal_error",
    ] {
        if message
            .strip_prefix(code)
            .is_some_and(|rest| rest.starts_with(':'))
        {
            return code;
        }
    }
    default_code
}
fn backend_error_text(message: &str) -> String {
    let stripped = message
        .split_once(':')
        .filter(|(prefix, _)| backend_error_code(message, "") == *prefix)
        .map(|(_, rest)| rest.trim())
        .unwrap_or(message);
    let line = stripped.lines().next().unwrap_or("email backend error");
    safe_model_line(line, 200)
}
fn structured_error_with_details(code: &str, message: &str, details: CborValue) -> CborValue {
    cbor_map(vec![
        ("code", CborValue::Text(code.to_owned())),
        (
            "message",
            CborValue::Text(safe_model_line(message, MAX_BACKEND_ERROR_CHARS)),
        ),
        ("details", details),
    ])
}
fn format_attachment_line(index: usize, attachment: &BackendAttachment) -> String {
    format!(
        "{} {} {} {}",
        index,
        attachment
            .filename
            .as_deref()
            .map(|filename| list_token(filename, MAX_ATTACHMENT_NAME_CHARS))
            .unwrap_or_else(|| "-".to_owned()),
        attachment
            .content_type
            .as_deref()
            .map(|content_type| list_token(content_type, MAX_HEADER_VALUE_CHARS))
            .unwrap_or_else(|| "-".to_owned()),
        attachment
            .size_bytes
            .map(|size| size.to_string())
            .unwrap_or_else(|| "-".to_owned())
    )
}
fn policy_cbor(decision: &PolicyDecision) -> CborValue {
    cbor_map(vec![
        ("incoming_allowed", CborValue::Bool(decision.allowed)),
        ("allowed", CborValue::Bool(decision.allowed)),
        (
            "reason",
            CborValue::Text(safe_model_line(&decision.reason, MAX_HEADER_VALUE_CHARS)),
        ),
        (
            "matched_pattern",
            decision
                .matched_pattern
                .as_deref()
                .map(|pattern| CborValue::Text(safe_model_line(pattern, MAX_HEADER_VALUE_CHARS)))
                .unwrap_or(CborValue::Null),
        ),
    ])
}
fn cbor_map(entries: Vec<(&str, CborValue)>) -> CborValue {
    CborValue::Map(
        entries
            .into_iter()
            .map(|(key, value)| (CborValue::Text(key.to_owned()), value))
            .collect(),
    )
}
fn cbor_field<'a>(value: &'a CborValue, field: &str) -> Option<&'a CborValue> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries.iter().find_map(|(key, value)| match key {
        CborValue::Text(key) if key == field => Some(value),
        _ => None,
    })
}

fn cbor_text_field<'a>(value: &'a CborValue, field: &str) -> Option<&'a str> {
    match cbor_field(value, field) {
        Some(CborValue::Text(value)) => Some(value.as_str()),
        _ => None,
    }
}
fn cbor_bool_field(value: &CborValue, field: &str) -> Option<bool> {
    match cbor_field(value, field) {
        Some(CborValue::Bool(value)) => Some(*value),
        _ => None,
    }
}

fn cbor_integer_field_string(value: &CborValue, field: &str) -> Option<String> {
    match cbor_field(value, field) {
        Some(CborValue::Integer(value)) => {
            let raw: i128 = (*value).into();
            Some(raw.to_string())
        }
        _ => None,
    }
}

fn cbor_array_len(value: &CborValue, field: &str) -> Option<u64> {
    cbor_array_field(value, field).map(|values| values.len() as u64)
}

fn cbor_array_field<'a>(value: &'a CborValue, field: &str) -> Option<&'a [CborValue]> {
    match cbor_field(value, field) {
        Some(CborValue::Array(values)) => Some(values),
        _ => None,
    }
}

fn cbor_nested_text_field<'a>(value: &'a CborValue, outer: &str, inner: &str) -> Option<&'a str> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    let nested = entries.iter().find_map(|(key, value)| match key {
        CborValue::Text(key) if key == outer => Some(value),
        _ => None,
    })?;
    cbor_text_field(nested, inner)
}
fn success_display_for_tool(tool_name: &str, result: &CborValue) -> ToolUseState {
    let command = cbor_text_field(result, "command").unwrap_or("email");
    if command_for_tool_name(tool_name).is_some() {
        let data = cbor_field(result, "data");
        return ToolUseState {
            args: split_email_display_args(command, data).unwrap_or_default(),
            stats: email_display_stats(command, data),
            info_chips: email_display_info(command, data),
            status: ToolUseStatus::Success,
            status_text: success_status_text(result).to_owned(),
            ..Default::default()
        };
    }
    success_display(result)
}

fn success_display(result: &CborValue) -> ToolUseState {
    let command = cbor_text_field(result, "command").unwrap_or("email");
    let status = ToolUseStatus::Success;
    let data = cbor_field(result, "data");
    ToolUseState {
        args: email_display_args(command, data).unwrap_or_default(),
        stats: email_display_stats(command, data),
        info_chips: email_display_info(command, data),
        status,
        status_text: success_status_text(result).to_owned(),
        ..Default::default()
    }
}

fn success_status_text(result: &CborValue) -> &'static str {
    // `approval_required` describes pending rather than completed work, so it
    // keeps its lifecycle label instead of using `ok`.
    if cbor_text_field(result, "status") == Some("approval_required") {
        "approval_required"
    } else {
        "ok"
    }
}

fn error_display_for_tool(
    tool_name: &str,
    arguments: &CborValue,
    details: &CborValue,
    message: &str,
) -> ToolUseState {
    let command = cbor_text_field(details, "command").unwrap_or("email");
    if command_for_tool_name(tool_name).is_some() {
        let data = cbor_field(details, "data").or_else(|| {
            let details =
                cbor_field(details, "error").and_then(|error| cbor_field(error, "details"))?;
            match details {
                CborValue::Map(entries) if entries.is_empty() => None,
                _ => Some(details),
            }
        });
        return ToolUseState {
            args: split_invocation_display_args(command, cbor_field(arguments, "args"))
                .filter(|args| !args.is_empty())
                .or_else(|| split_email_display_args(command, data).filter(|args| !args.is_empty()))
                .unwrap_or_default(),
            status: ToolUseStatus::Error,
            status_text: message.to_owned(),
            ..Default::default()
        };
    }
    error_display(arguments, details, message)
}

fn error_display(arguments: &CborValue, details: &CborValue, message: &str) -> ToolUseState {
    let command = cbor_text_field(details, "command").unwrap_or("email");
    let data = cbor_field(details, "data").or_else(|| {
        let details =
            cbor_field(details, "error").and_then(|error| cbor_field(error, "details"))?;
        match details {
            CborValue::Map(entries) if entries.is_empty() => None,
            _ => Some(details),
        }
    });
    ToolUseState {
        args: email_display_args(command, data)
            .or_else(|| invocation_display_args(arguments))
            .unwrap_or_default(),
        status: ToolUseStatus::Error,
        status_text: message.to_owned(),
        ..Default::default()
    }
}

fn message_target_display(command: &str, args: Option<&CborValue>) -> Option<String> {
    let args = args?;
    let folder = cbor_text_field(args, "folder");
    let uid = cbor_text_field(args, "uid")
        .map(str::to_owned)
        .or_else(|| cbor_integer_field_string(args, "uid"));
    let mut display = folder
        .map(|folder| format!("{command} {}", safe_display_line(folder)))
        .unwrap_or_else(|| command.to_owned());
    if let Some(uid) = uid {
        display.push_str(&format!(" uid={}", safe_display_line(&uid)));
    }
    Some(display)
}

fn split_message_target_display(args: Option<&CborValue>) -> Option<String> {
    let args = args?;
    let folder = cbor_text_field(args, "folder");
    let email_id = cbor_text_field(args, "email_id")
        .map(str::to_owned)
        .or_else(|| cbor_integer_field_string(args, "email_id"))
        .or_else(|| cbor_text_field(args, "uid").map(str::to_owned))
        .or_else(|| cbor_integer_field_string(args, "uid"));
    let mut parts = Vec::new();
    if let Some(folder) = folder {
        parts.push(safe_display_line(folder));
    }
    if let Some(email_id) = email_id {
        parts.push(format!("email_id={}", safe_display_line(&email_id)));
    }
    Some(parts.join(" "))
}
pub(crate) fn initial_display(arguments: &CborValue) -> ToolUseState {
    ToolUseState {
        args: invocation_display_args(arguments).unwrap_or_default(),
        status: ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
        ..Default::default()
    }
}

fn initial_progress(invoke: &ToolStarted, local_tool_name: &tau_proto::ToolName) -> ToolProgress {
    ToolProgress {
        call_id: invoke.call_id.clone(),
        tool_name: invoke.tool_name.clone(),
        message: None,
        progress: None,
        display: Some(initial_display_for_tool(
            local_tool_name.as_str(),
            &invoke.arguments,
        )),
    }
}

fn invocation_display_args(arguments: &CborValue) -> Option<String> {
    let command = cbor_text_field(arguments, "command")?;
    let args = cbor_field(arguments, "args");
    match command {
        "list_folders" => Some("list_folders".to_owned()),
        "list" | "list_by_uid" | "list_recent" => list_display_args(command, args),
        "read" | "request_access" | "mark_read" | "mark_unread" | "star" | "unstar" | "trash" => {
            message_target_display(command, args)
        }
        "send" => {
            let recipients = args
                .and_then(|args| cbor_array_len(args, "to"))
                .map(|count| format!(" to={count}"))
                .unwrap_or_default();
            Some(format!("send{recipients}"))
        }
        other => Some(safe_display_line(other)),
    }
}

fn list_display_args(command: &str, args: Option<&CborValue>) -> Option<String> {
    let display_command = if command == "list" {
        "list_by_uid"
    } else {
        command
    };
    let folder = args.and_then(|args| cbor_text_field(args, "folder"));
    Some(match folder {
        Some(folder) => format!("{display_command} {}", safe_display_line(folder)),
        None => display_command.to_owned(),
    })
}

fn split_list_display_args(args: Option<&CborValue>) -> Option<String> {
    let folder = args.and_then(|args| cbor_text_field(args, "folder"));
    Some(folder.map(safe_display_line).unwrap_or_default())
}

fn split_invocation_display_args(command: &str, args: Option<&CborValue>) -> Option<String> {
    match command {
        "list_folders" => Some(String::new()),
        "list" | "list_by_uid" | "list_recent" => split_list_display_args(args),
        "read" | "request_access" | "mark_read" | "mark_unread" | "star" | "unstar" | "trash" => {
            split_message_target_display(args)
        }
        "send" => {
            let recipients = args
                .and_then(|args| cbor_array_len(args, "to"))
                .map(|count| format!("to={count}"))
                .unwrap_or_default();
            Some(recipients)
        }
        _ => Some(String::new()),
    }
}

fn split_email_display_args(command: &str, data: Option<&CborValue>) -> Option<String> {
    match command {
        "list_folders" => Some(String::new()),
        "list" | "list_by_uid" | "list_recent" => split_list_display_args(data),
        "read" | "request_access" | "mark_read" | "mark_unread" | "star" | "unstar" | "trash" => {
            split_message_target_display(data)
        }
        "send" => Some(String::new()),
        _ => Some(String::new()),
    }
}
fn email_display_args(command: &str, data: Option<&CborValue>) -> Option<String> {
    match command {
        "list_folders" => Some("list_folders".to_owned()),
        "list" | "list_by_uid" | "list_recent" => list_display_args(command, data),
        "read" | "request_access" | "mark_read" | "mark_unread" | "star" | "unstar" | "trash" => {
            message_target_display(command, data)
        }
        "send" => data.map(|_| "send".to_owned()),
        other => Some(safe_display_line(other)),
    }
}

fn email_display_stats(command: &str, data: Option<&CborValue>) -> ToolUseStats {
    let Some(data) = data else {
        return ToolUseStats::default();
    };
    match command {
        "list_folders" => line_array_stats(data, "folders"),
        "list" | "list_by_uid" | "list_recent" => line_array_stats(data, "messages"),
        "read" => cbor_text_field(data, "body_text")
            .or_else(|| cbor_text_field(data, "body_preview"))
            .map(ToolUseStats::for_text)
            .unwrap_or_default(),
        "send" => count_stats(
            cbor_array_len(data, "accepted_recipients")
                .or_else(|| cbor_array_len(data, "allowed_recipients"))
                .or_else(|| cbor_array_len(data, "blocked_recipients")),
        ),
        _ => ToolUseStats::default(),
    }
}

fn count_stats(count: Option<u64>) -> ToolUseStats {
    ToolUseStats {
        matches: count,
        ..Default::default()
    }
}

fn line_array_stats(data: &CborValue, field: &str) -> ToolUseStats {
    let Some(lines) = cbor_array_field(data, field) else {
        return ToolUseStats::default();
    };
    let line_count = lines.len() as u64;
    let byte_count = lines
        .iter()
        .filter_map(|line| match line {
            CborValue::Text(text) => Some(text.len() as u64),
            _ => None,
        })
        .sum();
    ToolUseStats {
        matches: Some(line_count),
        lines: (0 < line_count).then_some(line_count),
        bytes: (0 < line_count).then_some(byte_count),
    }
}

fn email_display_info(command: &str, data: Option<&CborValue>) -> Vec<String> {
    let Some(data) = data else {
        return Vec::new();
    };
    let mut chips = Vec::new();
    match command {
        "list_folders" => push_count_chip(&mut chips, cbor_array_len(data, "folders"), "folder"),
        "list" | "list_by_uid" | "list_recent" => {
            push_count_chip(&mut chips, cbor_array_len(data, "messages"), "message");
        }
        "read" => {
            push_count_chip(
                &mut chips,
                cbor_array_len(data, "attachments"),
                "attachment",
            );
            if cbor_bool_field(data, "body_truncated") == Some(true)
                || cbor_bool_field(data, "body_preview_truncated") == Some(true)
            {
                chips.push("truncated".to_owned());
            }
        }
        "send" => {
            push_count_chip(
                &mut chips,
                cbor_array_len(data, "accepted_recipients")
                    .or_else(|| cbor_array_len(data, "allowed_recipients")),
                "allowed recipient",
            );
            push_count_chip(
                &mut chips,
                cbor_array_len(data, "blocked_recipients"),
                "blocked recipient",
            );
        }
        _ => {}
    }
    chips
}

fn push_count_chip(chips: &mut Vec<String>, count: Option<u64>, singular: &str) {
    let Some(count) = count else {
        return;
    };
    let suffix = if count == 1 {
        singular.to_owned()
    } else {
        format!("{singular}s")
    };
    chips.push(format!("{count} {suffix}"));
}

#[cfg(test)]
mod tests;
