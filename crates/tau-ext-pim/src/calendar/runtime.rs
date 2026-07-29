use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use tau_proto::{
    ActionError, ActionInvoke, ActionOutput, ActionResult, AgentId, CborValue, Event, SecretValue,
    ToolError, ToolProgress, ToolResult, ToolStarted, ToolUseRange, ToolUseState, ToolUseStats,
    ToolUseStatus,
};
use time_tz::{OffsetDateTimeExt, OffsetResult, PrimitiveDateTimeExt, TimeZone};

use super::config::{
    CalendarExtensionConfig, DescriptionPolicy, PrivateEventsPolicy, ValidatedAccount,
    ValidatedBackendConfig, ValidatedConfig, ValidatedPolicy,
};
use super::google::{GoogleBackend, GoogleEvent, GoogleEventWrite};
use super::ics_feed::{IcsEvent, IcsFeedBackend, TimeRange, normalize_feed_url};
use super::state::{CalendarChangeApproval, CalendarLogEntry, GooglePendingAuth, StateStore};
use super::tool::{
    CalendarCommand, CalendarRangeArgs, CreateEventArgs, DeleteEventArgs, ListCalendarsArgs,
    ReadEventArgs, RespondInviteArgs, ToolInvocation, UpdateEventArgs,
};
use crate::storage::SharedStorage;

const LIST_CALENDARS_FORMAT: &str = "calendar_id flags display_name";
const LIST_EVENTS_FORMAT: &str = "event_id start end flags status summary...";
const FREE_BUSY_FORMAT: &str = "event_id start end flags";
const EVENT_DETAIL_FORMAT: &str = "key value...";
const DEFAULT_EVENT_LIMIT: u32 = 50;
const MAX_EVENT_LIMIT: u32 = 100;
const DEFAULT_READ_LOOKBACK_DAYS: u8 = 2;
const DEFAULT_READ_WINDOW_DAYS: i64 = 7;
const CALENDAR_LOG_DEFAULT_LIMIT: usize = 20;
const CALENDAR_LOG_MAX_LIMIT: usize = 200;
const MAX_LOG_FIELD_CHARS: usize = 512;
const MAX_LOG_REASON_CHARS: usize = 512;
const MAX_DISPLAY_LINE_CHARS: usize = 256;
const MAX_EVENT_FIELD_CHARS: usize = 512;
const MAX_EVENT_DESCRIPTION_BYTES: usize = 64 * 1024;
const MAX_EVENT_DESCRIPTION_LINES: usize = 1000;
const MAX_ATTENDEE_CHARS: usize = 320;

const CALENDAR_TOOL_COMMANDS: &[(&str, &str)] = &[
    ("list_calendars", "list_calendars"),
    ("search", "list_events"),
    ("get", "read_event"),
    ("free_busy", "free_busy"),
    ("create", "create_event"),
    ("update", "update_event"),
    ("delete", "delete_event"),
    ("respond", "respond_invite"),
];

fn command_for_tool_name(tool_name: &str) -> Option<&'static str> {
    if tool_name == super::TOOL_NAME {
        return None;
    }
    let tool_suffix = tool_name.strip_prefix(super::TOOL_PREFIX)?;
    CALENDAR_TOOL_COMMANDS
        .iter()
        .find_map(|(tool_name, command)| (*tool_name == tool_suffix).then_some(*command))
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
    if command != "update_event" {
        return args;
    }
    let CborValue::Map(entries) = args else {
        return args;
    };
    let mut field_value = None;
    let mut new_value = None;
    let mut out = Vec::new();
    for (key, value) in entries {
        match (&key, &value) {
            (CborValue::Text(key), CborValue::Text(value)) if key == "field" => {
                field_value = Some(value.clone());
            }
            (CborValue::Text(key), CborValue::Text(value)) if key == "new_value" => {
                new_value = Some(value.clone());
            }
            _ => out.push((key, value)),
        }
    }
    if let (Some(field), Some(new_value)) = (field_value, new_value) {
        let value = if field == "attendees" {
            CborValue::Array(
                new_value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| CborValue::Text(value.to_owned()))
                    .collect(),
            )
        } else {
            CborValue::Text(new_value)
        };
        out.push((CborValue::Text(field), value));
    }
    CborValue::Map(out)
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

/// Runtime state for the calendar module.
pub struct RuntimeState {
    config_state: ConfigState,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            config_state: ConfigState::Unconfigured,
        }
    }
}

enum ConfigState {
    Unconfigured,
    Configured(Box<Engine>),
    Rejected { reason: String },
}

struct Engine {
    config: ValidatedConfig,
    state: StateStore,
    google: GoogleBackend,
    ics_feed: IcsFeedBackend,
    etags: RefCell<BTreeMap<EventEtagKey, String>>,
    last_events: RefCell<BTreeMap<String, Vec<RecentEventRef>>>,
}

#[derive(Clone, Debug)]
struct RecentEventRef {
    account: String,
    calendar: String,
    event_id: String,
    summary: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct EventEtagKey {
    account: String,
    calendar: String,
    event_id: String,
}

impl RuntimeState {
    /// Mark the calendar module configuration as rejected without keeping stale
    /// state.
    pub(crate) fn reject(&mut self, reason: String) {
        self.config_state = ConfigState::Rejected { reason };
    }

    /// Return effective accounts accepted by interactive Google Calendar OAuth.
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
                        &account.backend,
                        Some(ValidatedBackendConfig::Google {
                            refresh_token_secret: None,
                            ..
                        })
                    )
            })
            .map(|account| account.id.clone())
            .collect()
    }

    /// Configure the calendar module from an already-decoded calendar config.
    pub(crate) fn configure_with_config(
        &mut self,
        cfg: CalendarExtensionConfig,
        state_dir: Option<PathBuf>,
        secrets: BTreeMap<String, SecretValue>,
        storage: SharedStorage,
    ) -> Result<(), String> {
        let result = cfg.validate().and_then(|config| {
            validate_config_secrets(&config, &secrets)?;
            let state_dir = state_dir.unwrap_or_default();
            Ok(Engine {
                config,
                state: StateStore::open_with_storage(state_dir, storage)?,
                google: GoogleBackend::new(secrets.clone()),
                ics_feed: IcsFeedBackend::new(secrets),
                etags: RefCell::new(BTreeMap::new()),
                last_events: RefCell::new(BTreeMap::new()),
            })
        });
        match result {
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

    /// Dispatch a model-visible `calendar` tool invocation.
    pub fn dispatch(&mut self, invoke: ToolStarted) -> Event {
        let invoke = invoke_with_command(invoke);
        let result = match &self.config_state {
            ConfigState::Configured(engine) => engine.dispatch(&invoke.arguments, &invoke.agent_id),
            ConfigState::Unconfigured => error_envelope(
                None,
                "configuration_error",
                "calendar module has not been configured",
            ),
            ConfigState::Rejected { reason } => error_envelope(
                None,
                "configuration_error",
                &format!("calendar module configuration was rejected: {reason}"),
            ),
        };
        finish_tool_result(invoke, result)
    }

    /// Dispatch a user `:calendar` action invocation.
    pub fn dispatch_action(&mut self, invoke: ActionInvoke) -> Event {
        let result = match &self.config_state {
            ConfigState::Configured(engine) => {
                engine.dispatch_action(&invoke.action_id, &invoke.argv)
            }
            ConfigState::Unconfigured => Err("calendar module has not been configured".to_owned()),
            ConfigState::Rejected { reason } => Err(format!(
                "calendar module configuration was rejected: {reason}"
            )),
        };
        match result {
            Ok(text) => action_result(invoke, text),
            Err(message) => action_error(invoke, message),
        }
    }
}

fn validate_config_secrets(
    config: &ValidatedConfig,
    secrets: &BTreeMap<String, SecretValue>,
) -> Result<(), String> {
    if !config.enable {
        return Ok(());
    }
    for account in config.accounts.values() {
        if !(!account.enable) {
            match &account.backend {
                Some(ValidatedBackendConfig::IcsFeed {
                    url_secret: Some(secret),
                    allow_plain_http,
                    ..
                }) => {
                    let value =
                        required_config_secret(&account.id, "backend.url_secret", secret, secrets)?;
                    normalize_feed_url(&value, *allow_plain_http)?;
                }
                Some(ValidatedBackendConfig::Google {
                    client_id_secret,
                    client_secret_secret,
                    refresh_token_secret,
                    ..
                }) => {
                    required_config_secret(
                        &account.id,
                        "backend.client_id_secret",
                        client_id_secret,
                        secrets,
                    )?;
                    for (field, secret) in [
                        (
                            "backend.client_secret_secret",
                            client_secret_secret.as_deref(),
                        ),
                        (
                            "backend.refresh_token_secret",
                            refresh_token_secret.as_deref(),
                        ),
                    ] {
                        let Some(secret) = secret else { continue };
                        required_config_secret(&account.id, field, secret, secrets)?;
                    }
                }
                Some(ValidatedBackendConfig::Caldav { .. })
                | Some(ValidatedBackendConfig::IcsFeed {
                    url_secret: None, ..
                })
                | None => {}
            }
        }
    }
    Ok(())
}

fn required_config_secret(
    account_id: &str,
    field: &str,
    secret: &str,
    secrets: &BTreeMap<String, SecretValue>,
) -> Result<String, String> {
    secrets
        .get(secret)
        .map(|secret| secret.expose_secret().to_owned())
        .ok_or_else(|| {
            format!(
                "calendar account `{account_id}` {field} `{secret}` was not provided in Configure.secrets; declare it under the enabled extension's secrets"
            )
        })
}

enum BackendEvent {
    Ics(IcsEvent),
    Google(GoogleEvent),
}

struct BackendEventPage {
    events: Vec<BackendEvent>,
    next_cursor: Option<String>,
    truncated: bool,
}

enum CalendarMutationResult {
    Event(Box<GoogleEvent>),
    Deleted,
}

struct ChangeArgs {
    calendar: Option<String>,
    event_id: Option<String>,
    title: Option<String>,
    description: Option<String>,
    location: Option<String>,
    start: Option<String>,
    end: Option<String>,
    attendees: Option<Vec<String>>,
    response: Option<String>,
}

impl From<CreateEventArgs> for ChangeArgs {
    fn from(args: CreateEventArgs) -> Self {
        Self {
            calendar: args.calendar,
            event_id: None,
            title: args.title,
            description: args.description,
            location: args.location,
            start: args.start,
            end: args.end,
            attendees: args.attendees,
            response: None,
        }
    }
}

impl From<UpdateEventArgs> for ChangeArgs {
    fn from(args: UpdateEventArgs) -> Self {
        Self {
            calendar: args.calendar,
            event_id: args.event_id,
            title: args.title,
            description: args.description,
            location: args.location,
            start: args.start,
            end: args.end,
            attendees: args.attendees,
            response: None,
        }
    }
}

impl From<DeleteEventArgs> for ChangeArgs {
    fn from(args: DeleteEventArgs) -> Self {
        Self {
            calendar: args.calendar,
            event_id: args.event_id,
            title: None,
            description: None,
            location: None,
            start: None,
            end: None,
            attendees: None,
            response: None,
        }
    }
}

impl From<RespondInviteArgs> for ChangeArgs {
    fn from(args: RespondInviteArgs) -> Self {
        Self {
            calendar: args.calendar,
            event_id: args.event_id,
            title: None,
            description: None,
            location: None,
            start: None,
            end: None,
            attendees: None,
            response: args.response,
        }
    }
}

fn parse_invocation_args<T>(invocation: &ToolInvocation) -> Result<T, String>
where
    T: DeserializeOwned,
{
    let empty_args;
    let args = match invocation.args.as_ref() {
        Some(args) => args,
        None => {
            empty_args = CborValue::Map(Vec::new());
            &empty_args
        }
    };
    args.deserialized()
        .map_err(|error| calendar_arg_parse_error(command_name(invocation.command), &error))
}

fn calendar_arg_parse_error(command: &str, error: &dyn std::fmt::Display) -> String {
    let message = error.to_string();
    if let Some(field) = extract_unknown_field(&message) {
        return format!("{command} does not accept `{field}`");
    }
    format!("invalid {command} args: {message}")
}

fn extract_unknown_field(message: &str) -> Option<&str> {
    let (_, rest) = message.split_once("unknown field `")?;
    let (field, _) = rest.split_once('`')?;
    Some(field)
}

impl CalendarMutationResult {
    fn event_id(&self) -> Option<&str> {
        match self {
            Self::Event(event) => Some(&event.id),
            Self::Deleted => None,
        }
    }
}

impl Engine {
    fn dispatch(&self, arguments: &CborValue, agent_id: &AgentId) -> CborValue {
        let invocation: ToolInvocation = match arguments.deserialized() {
            Ok(invocation) => invocation,
            Err(error) => {
                return error_envelope(
                    None,
                    "invalid_input",
                    &format!("invalid calendar tool arguments: {error}"),
                );
            }
        };
        let command = invocation.command;
        let result = match command {
            CalendarCommand::ListCalendars => {
                parse_invocation_args::<ListCalendarsArgs>(&invocation)
                    .and_then(|_args| self.list_calendars())
            }
            CalendarCommand::ListEvents => parse_invocation_args::<CalendarRangeArgs>(&invocation)
                .and_then(|args| self.list_events(&args, agent_id)),
            CalendarCommand::ReadEvent => parse_invocation_args::<ReadEventArgs>(&invocation)
                .and_then(|args| self.read_event(&args, agent_id)),
            CalendarCommand::FreeBusy => parse_invocation_args::<CalendarRangeArgs>(&invocation)
                .and_then(|args| self.free_busy(&args, agent_id)),
            CalendarCommand::CreateEvent => parse_invocation_args::<CreateEventArgs>(&invocation)
                .and_then(|args| self.submit_change(command, ChangeArgs::from(args))),
            CalendarCommand::UpdateEvent => parse_invocation_args::<UpdateEventArgs>(&invocation)
                .and_then(|args| self.submit_change(command, ChangeArgs::from(args))),
            CalendarCommand::DeleteEvent => parse_invocation_args::<DeleteEventArgs>(&invocation)
                .and_then(|args| self.submit_change(command, ChangeArgs::from(args))),
            CalendarCommand::RespondInvite => {
                parse_invocation_args::<RespondInviteArgs>(&invocation)
                    .and_then(|args| self.submit_change(command, ChangeArgs::from(args)))
            }
        }
        .unwrap_or_else(|message| calendar_error_envelope(Some(command_name(command)), &message));
        self.append_calendar_log_for_invocation(&invocation, &result);
        result
    }

    fn dispatch_action(&self, action_id: &str, argv: &[String]) -> Result<String, String> {
        match action_id {
            "calendar.auth.google.start" => {
                require_one_arg(argv).and_then(|account| self.action_auth_google_start(account))
            }
            "calendar.auth.google.finish" => {
                require_one_arg(argv).and_then(|account| self.action_auth_google_finish(account))
            }
            "calendar.log.last" => {
                parse_log_limit(argv).and_then(|limit| self.action_log_last(limit))
            }
            "calendar.change.list" => {
                require_no_args(argv).and_then(|()| self.action_change_list())
            }
            "calendar.change.open" => {
                require_one_arg(argv).and_then(|id| self.action_change_open(id))
            }
            "calendar.change.approve" => self.action_change_approve_args(argv),
            "calendar.change.deny" => {
                require_change_ids(argv).and_then(|ids| self.action_change_deny_many(&ids))
            }
            _ => Err(format!("unknown calendar action `{action_id}`")),
        }
    }

    fn action_auth_google_start(&self, account_id: &str) -> Result<String, String> {
        let account = self.google_oauth_state_account(account_id)?;
        let started = self.google.start_device_auth(account)?;
        let pending = GooglePendingAuth::new(
            &account.id,
            &started.device_code,
            &started.user_code,
            &started.verification_uri,
            started.expires_in_secs,
            started.interval_secs,
        );
        self.state.save_pending_google_auth(&pending)?;
        Ok(format_google_auth_started(&account.id, &started))
    }

    fn action_auth_google_finish(&self, account_id: &str) -> Result<String, String> {
        let account = self.google_oauth_state_account(account_id)?;
        let pending = self.state.pending_google_auth(&account.id)?;
        if pending.expired() {
            self.state.clear_pending_google_auth(&account.id)?;
            return Err(format!(
                "Google authorization for account `{}` expired; run `:calendar auth google start {}` again",
                safe_display_line(&account.id),
                safe_display_line(&account.id)
            ));
        }
        let finished = self
            .google
            .finish_device_auth(account, &pending.device_code)?;
        self.state
            .save_google_refresh_token(&account.id, &finished.refresh_token)?;
        self.state.clear_pending_google_auth(&account.id)?;
        if let Some(access_token) = finished.access_token
            && let Err(message) = self.google.prime_access_token_cache(
                &account.id,
                access_token,
                finished.expires_in_secs,
            )
        {
            tracing::warn!(target: crate::LOG_TARGET, error = %message, "failed to prime Google Calendar access token cache");
        }
        Ok(format!(
            "Google Calendar authorization stored for account {}.",
            safe_display_line(&account.id)
        ))
    }

    fn action_log_last(&self, limit: usize) -> Result<String, String> {
        let entries = self.state.recent_calendar_log(limit)?;
        if entries.is_empty() {
            return Ok("No calendar log entries.".to_owned());
        }
        let mut lines = vec![format!("Last {} calendar log entry(s):", entries.len())];
        // Preserve this behavior; the structural alternative is not semantics-neutral
        // here. ast-grep-ignore: map-collect-loop
        for entry in entries.iter().rev() {
            lines.push(format_calendar_log_entry(entry));
        }
        Ok(lines.join("\n"))
    }

    fn action_change_list(&self) -> Result<String, String> {
        let changes = self.state.list_pending_changes()?;
        if changes.is_empty() {
            return Ok("No pending calendar changes.".to_owned());
        }
        let mut lines = vec![format!("{} pending calendar change(s):", changes.len())];
        // Preserve this behavior; the structural alternative is not semantics-neutral
        // here. ast-grep-ignore: map-collect-loop
        for change in changes {
            lines.push(format_change_summary(&change));
        }
        Ok(lines.join("\n"))
    }

    fn action_change_open(&self, id: &str) -> Result<String, String> {
        let change = self.state.pending_change_by_id(id)?;
        Ok(format_change_detail(&change))
    }

    fn action_change_approve_args(&self, argv: &[String]) -> Result<String, String> {
        if require_all_arg(argv)? {
            let ids = self
                .state
                .list_pending_changes()?
                .into_iter()
                .map(|change| change.id)
                .collect::<Vec<_>>();
            if ids.is_empty() {
                return Ok("No pending calendar changes to approve.".to_owned());
            }
            return self.action_change_approve_many(&ids);
        }
        require_change_ids(argv).and_then(|ids| self.action_change_approve_many(&ids))
    }

    fn action_change_approve_many(&self, ids: &[String]) -> Result<String, String> {
        if ids.len() == 1 {
            return self.action_change_approve(&ids[0]);
        }
        let mut lines = vec![format!("Approving {} calendar change(s):", ids.len())];
        let mut errors = Vec::new();
        for id in ids {
            match self.action_change_approve(id) {
                Ok(message) => lines.push(message),
                Err(error) => {
                    lines.push(format!(
                        "Failed calendar change {id}: {}",
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

    fn action_change_approve(&self, id: &str) -> Result<String, String> {
        if self.state.change_denied_exists(id)? {
            let denied = self.state.denied_change_by_id(id)?;
            return Err(format!(
                "Calendar change {id} was denied. command={} account={} calendar={}",
                safe_display_line(&denied.command),
                safe_display_line(&denied.account),
                safe_display_line(&denied.calendar)
            ));
        }
        if self.state.change_pending_exists(id)? {
            let pending = self.state.pending_change_by_id(id)?;
            self.validate_persisted_change(&pending)?;
            let change = self.state.claim_change(id)?;
            self.validate_persisted_change(&change)?;
            let result = match self.execute_change(&change) {
                Ok(result) => result,
                Err(error) => {
                    return match self.state.release_claimed_change(id) {
                        Ok(()) => Err(error),
                        Err(recovery_error) => Err(format!(
                            "{error}; additionally failed to restore approval to pending: {recovery_error}"
                        )),
                    };
                }
            };
            let result_event_id = result.event_id();
            return match self.state.complete_change(id, result_event_id) {
                Ok(()) => Ok(format_mutation_result("Applied", id, &change, &result)),
                Err(error) => Ok(format!(
                    "Applied calendar change {id}, but failed to record approval: {}",
                    safe_display_line(&error)
                )),
            };
        }
        if self.state.change_sending_exists(id)? {
            return Err(format!(
                "Calendar change {id} is already being applied or needs manual recovery."
            ));
        }
        let approved = self.state.approved_change_by_id(id)?;
        Ok(format!(
            "Calendar change {id} is already approved/applied. command={} account={} calendar={}",
            safe_display_line(&approved.command),
            safe_display_line(&approved.account),
            safe_display_line(&approved.calendar)
        ))
    }

    fn action_change_deny_many(&self, ids: &[String]) -> Result<String, String> {
        if ids.len() == 1 {
            return self.action_change_deny(&ids[0]);
        }
        let mut lines = vec![format!("Denying {} calendar change(s):", ids.len())];
        let mut errors = Vec::new();
        for id in ids {
            match self.action_change_deny(id) {
                Ok(message) => lines.push(message),
                Err(error) => {
                    lines.push(format!(
                        "Failed calendar change {id}: {}",
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

    fn action_change_deny(&self, id: &str) -> Result<String, String> {
        self.state.deny_change(id)?;
        Ok(format!("Denied calendar change {id}."))
    }

    fn append_calendar_log_for_invocation(&self, invocation: &ToolInvocation, result: &CborValue) {
        let Some(entry) = self.calendar_log_entry(invocation, result) else {
            return;
        };
        if let Err(message) = self.state.append_calendar_log(&entry) {
            tracing::warn!(target: crate::LOG_TARGET, error = %message, "failed to append calendar log");
        }
    }

    fn calendar_log_entry(
        &self,
        invocation: &ToolInvocation,
        result: &CborValue,
    ) -> Option<CalendarLogEntry> {
        let args = invocation.args.as_ref();
        let status = calendar_log_status(result);
        let result_data = cbor_field(result, "data");
        let account = self.log_account(args, result_data);
        let mut entry = CalendarLogEntry::tool(command_name(invocation.command), &status);
        entry.account = account
            .as_deref()
            .map(|value| safe_log_value(value, MAX_LOG_FIELD_CHARS));
        entry.calendar = self
            .log_calendar(args, result_data, account.as_deref())
            .map(|value| safe_log_value(&value, MAX_LOG_FIELD_CHARS));
        entry.event_id = args
            .and_then(|args| cbor_text_field(args, "event_id"))
            .map(|value| safe_log_value(value, MAX_LOG_FIELD_CHARS));
        entry.start = result_data
            .and_then(|data| cbor_text_field(data, "start"))
            .or_else(|| args.and_then(|args| cbor_nonblank_text_field(args, "start")))
            .map(|value| safe_log_value(value, MAX_LOG_FIELD_CHARS));
        entry.end = result_data
            .and_then(|data| cbor_text_field(data, "end"))
            .or_else(|| args.and_then(|args| cbor_nonblank_text_field(args, "end")))
            .map(|value| safe_log_value(value, MAX_LOG_FIELD_CHARS));
        entry.limit = args.and_then(|args| cbor_u32_field(args, "limit"));
        entry.item_count = calendar_log_item_count(invocation.command, result);
        entry.reason = calendar_log_error_message(result)
            .map(|reason| safe_log_value(reason, MAX_LOG_REASON_CHARS));
        Some(entry)
    }

    fn log_account(
        &self,
        args: Option<&CborValue>,
        result_data: Option<&CborValue>,
    ) -> Option<String> {
        result_data
            .and_then(|data| cbor_text_field(data, "calendar"))
            .or_else(|| args.and_then(|args| cbor_text_field(args, "calendar")))
            .and_then(|calendar| {
                split_flattened_calendar_id(calendar)
                    .ok()
                    .map(|(account, _)| account.to_owned())
            })
    }

    fn log_calendar(
        &self,
        args: Option<&CborValue>,
        result_data: Option<&CborValue>,
        _account_id: Option<&str>,
    ) -> Option<String> {
        result_data
            .and_then(|data| cbor_text_field(data, "calendar"))
            .or_else(|| args.and_then(|args| cbor_text_field(args, "calendar")))
            .and_then(|calendar| {
                split_flattened_calendar_id(calendar)
                    .ok()
                    .map(|(_, calendar)| calendar.to_owned())
            })
    }

    fn list_calendars(&self) -> Result<CborValue, String> {
        let mut rows = Vec::new();
        if self.config.enable {
            let accounts = self.accounts_for_read(None)?;
            for account in accounts {
                match &account.backend {
                    Some(ValidatedBackendConfig::IcsFeed { .. }) => {
                        for calendar in self.ics_feed.list_calendars(account) {
                            let flags = if calendar.read_only {
                                "read_only"
                            } else {
                                "writable"
                            };
                            rows.push(format!(
                                "{} {} {}",
                                flatten_calendar_id(&account.id, &calendar.id),
                                flags,
                                quoted_display_field(&calendar.display_name)
                            ));
                        }
                    }
                    Some(ValidatedBackendConfig::Google { .. }) => {
                        let stored_refresh_token = self.google_refresh_token(account)?;
                        for calendar in self
                            .google
                            .list_calendars(account, stored_refresh_token.as_deref())?
                        {
                            let flags = if calendar.read_only {
                                "read_only"
                            } else {
                                "writable"
                            };
                            rows.push(format!(
                                "{} {} {}",
                                flatten_calendar_id(&account.id, &calendar.id),
                                flags,
                                quoted_display_field(&calendar.summary)
                            ));
                        }
                    }
                    Some(ValidatedBackendConfig::Caldav { .. }) | None => {}
                }
            }
        }
        Ok(ok_line_envelope(
            "list_calendars",
            "ok",
            vec![("format", CborValue::Text(LIST_CALENDARS_FORMAT.to_owned()))],
            cbor_map(vec![
                ("format", CborValue::Text(LIST_CALENDARS_FORMAT.to_owned())),
                ("calendars", line_array(rows.clone())),
            ]),
            &rows,
        ))
    }

    fn list_events(
        &self,
        args: &CalendarRangeArgs,
        agent_id: &AgentId,
    ) -> Result<CborValue, String> {
        let limit = normalized_limit(args.limit)?;
        let title_filter = optional_trimmed_line(args.title.as_deref(), "title")?;
        let (account, calendar) = self.resolve_calendar_arg(args.calendar.as_deref())?;
        let calendar = calendar.as_str();
        let range = parse_range(args, account)?;
        let output_timezone = timezone_for_read("event output", account.timezone.as_deref())?;
        let range_start = format_optional_read_bound(range.min, "start")?;
        let range_end = format_optional_read_bound(range.max, "end")?;
        let page =
            self.events_for_account(account, calendar, range, limit, args.cursor.as_deref())?;
        let scanned_events = page.events.len();
        let events = filtered_events(&self.config.policy, &page.events, title_filter.as_deref());
        let returned_events = events.len();
        self.remember_visible_events(agent_id, account, calendar, &events);
        let mut rows = Vec::new();
        for event in events {
            self.remember_event_etag(account, calendar, event);
            rows.push(format_event_line(
                &self.config.policy,
                output_timezone,
                event,
            ));
        }
        let data = cbor_map(vec![
            (
                "calendar",
                CborValue::Text(safe_display_line(&flatten_calendar_id(
                    &account.id,
                    calendar,
                ))),
            ),
            ("format", CborValue::Text(LIST_EVENTS_FORMAT.to_owned())),
            ("start", optional_text(range_start.clone())),
            ("end", optional_text(range_end.clone())),
            ("events", line_array(rows.clone())),
            ("title_filter", optional_text(title_filter.clone())),
            (
                "returned_events",
                CborValue::Integer(returned_events.into()),
            ),
            ("scanned_events", CborValue::Integer(scanned_events.into())),
            (
                "total_events",
                if page.truncated {
                    CborValue::Null
                } else {
                    CborValue::Integer(scanned_events.into())
                },
            ),
            ("next_cursor", optional_text(page.next_cursor.clone())),
            ("truncated", CborValue::Bool(page.truncated)),
        ]);
        Ok(ok_line_envelope(
            "list_events",
            "ok",
            vec![
                (
                    "calendar",
                    CborValue::Text(safe_display_line(&flatten_calendar_id(
                        &account.id,
                        calendar,
                    ))),
                ),
                ("format", CborValue::Text(LIST_EVENTS_FORMAT.to_owned())),
                ("start", optional_text(range_start)),
                ("end", optional_text(range_end)),
                ("title_filter", optional_text(title_filter)),
                (
                    "returned_events",
                    CborValue::Integer(returned_events.into()),
                ),
                ("scanned_events", CborValue::Integer(scanned_events.into())),
                (
                    "total_events",
                    if page.truncated {
                        CborValue::Null
                    } else {
                        CborValue::Integer(scanned_events.into())
                    },
                ),
                ("next_cursor", optional_text(page.next_cursor)),
                ("truncated", CborValue::Bool(page.truncated)),
            ],
            data,
            &rows,
        ))
    }

    fn read_event(&self, args: &ReadEventArgs, agent_id: &AgentId) -> Result<CborValue, String> {
        let (account, calendar) = self.resolve_calendar_arg(args.calendar.as_deref())?;
        let calendar = calendar.as_str();
        let output_timezone = timezone_for_read("event output", account.timezone.as_deref())?;
        let implicit_event_id = args.event_id.is_none();
        let event_id =
            self.resolve_read_event_id(agent_id, account, calendar, args.event_id.as_deref())?;
        let event = match &account.backend {
            Some(ValidatedBackendConfig::IcsFeed { .. }) => {
                BackendEvent::Ics(self.ics_feed.read_event(account, calendar, &event_id)?)
            }
            Some(ValidatedBackendConfig::Google { .. }) => {
                let stored_refresh_token = self.google_refresh_token(account)?;
                BackendEvent::Google(self.google.read_event(
                    account,
                    stored_refresh_token.as_deref(),
                    calendar,
                    &event_id,
                )?)
            }
            Some(ValidatedBackendConfig::Caldav { .. }) | None => {
                return Err(format!(
                    "calendar account `{}` backend `{}` does not support read_event yet",
                    account.id,
                    account.backend_kind()
                ));
            }
        };
        self.remember_event_etag(account, calendar, &event);
        let mut data = vec![
            (
                "calendar",
                CborValue::Text(safe_display_line(&flatten_calendar_id(
                    &account.id,
                    calendar,
                ))),
            ),
            ("event_id", CborValue::Text(safe_display_line(&event_id))),
            ("format", CborValue::Text(EVENT_DETAIL_FORMAT.to_owned())),
            (
                "event",
                line_array(format_event_detail(
                    &self.config.policy,
                    account,
                    calendar,
                    output_timezone,
                    &event,
                )),
            ),
        ];
        if implicit_event_id {
            data.push((
                "event_id_source",
                CborValue::Text("implicit_recent".to_owned()),
            ));
        }
        Ok(ok_envelope("read_event", "ok", cbor_map(data)))
    }

    fn free_busy(&self, args: &CalendarRangeArgs, agent_id: &AgentId) -> Result<CborValue, String> {
        let limit = normalized_limit(args.limit)?;
        if args.title.is_some() {
            return Err(
                "calendar_free_busy does not accept `title`; use calendar_search for title filtering".to_owned(),
            );
        }
        let (account, calendar) = self.resolve_calendar_arg(args.calendar.as_deref())?;
        let calendar = calendar.as_str();
        let range = parse_range(args, account)?;
        let output_timezone = timezone_for_read("event output", account.timezone.as_deref())?;
        let range_start = format_optional_read_bound(range.min, "start")?;
        let range_end = format_optional_read_bound(range.max, "end")?;
        let page =
            self.events_for_account(account, calendar, range, limit, args.cursor.as_deref())?;
        let events = filtered_events(&self.config.policy, &page.events, None);
        self.remember_visible_events(agent_id, account, calendar, &events);
        let mut rows = Vec::new();
        for event in events {
            self.remember_event_etag(account, calendar, event);
            rows.push(format_free_busy_line(
                &self.config.policy,
                output_timezone,
                event,
            ));
        }
        let data = cbor_map(vec![
            (
                "calendar",
                CborValue::Text(safe_display_line(&flatten_calendar_id(
                    &account.id,
                    calendar,
                ))),
            ),
            ("format", CborValue::Text(FREE_BUSY_FORMAT.to_owned())),
            ("start", optional_text(range_start.clone())),
            ("end", optional_text(range_end.clone())),
            ("busy", line_array(rows.clone())),
            ("next_cursor", optional_text(page.next_cursor.clone())),
            ("truncated", CborValue::Bool(page.truncated)),
        ]);
        Ok(ok_line_envelope(
            "free_busy",
            "ok",
            vec![
                (
                    "calendar",
                    CborValue::Text(safe_display_line(&flatten_calendar_id(
                        &account.id,
                        calendar,
                    ))),
                ),
                ("format", CborValue::Text(FREE_BUSY_FORMAT.to_owned())),
                ("start", optional_text(range_start)),
                ("end", optional_text(range_end)),
                ("next_cursor", optional_text(page.next_cursor)),
                ("truncated", CborValue::Bool(page.truncated)),
            ],
            data,
            &rows,
        ))
    }

    fn submit_change(
        &self,
        command: CalendarCommand,
        args: ChangeArgs,
    ) -> Result<CborValue, String> {
        let change = self.build_change(command, args)?;
        if self.config.policy.write.require_approval {
            let id = self.state.pending_change(&change)?;
            return Ok(format_change_queued(&id, &change));
        }
        let result = self.execute_change(&change)?;
        Ok(format_mutation_result_envelope("direct", &change, &result))
    }

    fn build_change(
        &self,
        command: CalendarCommand,
        args: ChangeArgs,
    ) -> Result<CalendarChangeApproval, String> {
        let (account, calendar) = self.resolve_calendar_arg(args.calendar.as_deref())?;
        let calendar = calendar.as_str();
        self.ensure_calendar_allowed(account, calendar)?;
        ensure_google_write_backend(account)?;
        self.build_calendar_change(command, &args, account, calendar)
    }

    fn build_calendar_change(
        &self,
        command: CalendarCommand,
        args: &ChangeArgs,
        account: &ValidatedAccount,
        calendar: &str,
    ) -> Result<CalendarChangeApproval, String> {
        let mut change =
            CalendarChangeApproval::pending(command_name(command), &account.id, calendar);
        match command {
            CalendarCommand::CreateEvent => {
                self.fill_create_event_change(&mut change, args, account)?
            }
            CalendarCommand::UpdateEvent => {
                self.fill_update_event_change(&mut change, args, account)?
            }
            CalendarCommand::DeleteEvent => self.fill_delete_event_change(&mut change, args)?,
            CalendarCommand::RespondInvite => self.fill_respond_invite_change(&mut change, args)?,
            CalendarCommand::ListCalendars
            | CalendarCommand::ListEvents
            | CalendarCommand::ReadEvent
            | CalendarCommand::FreeBusy => unreachable!("read commands are not calendar changes"),
        }
        Ok(change)
    }

    fn fill_create_event_change(
        &self,
        change: &mut CalendarChangeApproval,
        args: &ChangeArgs,
        account: &ValidatedAccount,
    ) -> Result<(), String> {
        change.title = Some(required_text(args.title.as_deref(), "title")?);
        let (start, end) = create_event_time_pair(
            args.start.as_deref(),
            args.end.as_deref(),
            account.timezone.as_deref(),
        )?;
        change.start = Some(start);
        change.end = Some(end);
        change.description = optional_description(args.description.as_deref())?;
        change.location = optional_line(args.location.as_deref(), "location", true)?;
        change.attendees = optional_attendees(
            args.attendees.as_deref(),
            self.config.policy.write.max_attendees,
        )?;
        Ok(())
    }

    fn fill_update_event_change(
        &self,
        change: &mut CalendarChangeApproval,
        args: &ChangeArgs,
        account: &ValidatedAccount,
    ) -> Result<(), String> {
        change.event_id = Some(required_text(args.event_id.as_deref(), "event_id")?);
        change.etag = Some(self.cached_etag_for_change(change)?);
        change.title = optional_line(args.title.as_deref(), "title", false)?;
        change.description = optional_description(args.description.as_deref())?;
        change.location = optional_line(args.location.as_deref(), "location", true)?;
        fill_update_time_change(change, args, account.timezone.as_deref())?;
        change.attendees = optional_attendees(
            args.attendees.as_deref(),
            self.config.policy.write.max_attendees,
        )?;
        ensure_update_change_has_payload(change)?;
        ensure_response_only_for_invites(args)?;
        Ok(())
    }

    fn fill_delete_event_change(
        &self,
        change: &mut CalendarChangeApproval,
        args: &ChangeArgs,
    ) -> Result<(), String> {
        change.event_id = Some(required_text(args.event_id.as_deref(), "event_id")?);
        change.etag = Some(self.cached_etag_for_change(change)?);
        Ok(())
    }

    fn fill_respond_invite_change(
        &self,
        change: &mut CalendarChangeApproval,
        args: &ChangeArgs,
    ) -> Result<(), String> {
        change.event_id = Some(required_text(args.event_id.as_deref(), "event_id")?);
        change.etag = Some(self.cached_etag_for_change(change)?);
        change.response = Some(required_response(args.response.as_deref())?);
        Ok(())
    }

    fn remember_event_etag(
        &self,
        account: &ValidatedAccount,
        calendar: &str,
        event: &BackendEvent,
    ) {
        let key = EventEtagKey {
            account: account.id.clone(),
            calendar: calendar.to_owned(),
            event_id: event_id(event).to_owned(),
        };
        let Some(etag) = event_etag(event) else {
            self.etags.borrow_mut().remove(&key);
            return;
        };
        self.etags.borrow_mut().insert(key, etag.to_owned());
    }

    fn cached_etag_for_change(&self, change: &CalendarChangeApproval) -> Result<String, String> {
        let event_id = required_change_field(change.event_id.as_deref(), "event_id")?;
        self.etags
            .borrow()
            .get(&EventEtagKey {
                account: change.account.clone(),
                calendar: change.calendar.clone(),
                event_id: event_id.to_owned(),
            })
            .cloned()
            .ok_or_else(|| {
                format!(
                    "calendar event `{}` has not been read in this session or changed since it was cached; re-read the event and retry",
                    safe_display_line(event_id)
                )
            })
    }

    fn remember_mutation_result(
        &self,
        change: &CalendarChangeApproval,
        result: &CalendarMutationResult,
    ) {
        match result {
            CalendarMutationResult::Event(event) => {
                if let Some(etag) = &event.etag {
                    self.etags.borrow_mut().insert(
                        EventEtagKey {
                            account: change.account.clone(),
                            calendar: change.calendar.clone(),
                            event_id: event.id.clone(),
                        },
                        etag.clone(),
                    );
                }
            }
            CalendarMutationResult::Deleted => {
                if let Some(event_id) = &change.event_id {
                    self.etags.borrow_mut().remove(&EventEtagKey {
                        account: change.account.clone(),
                        calendar: change.calendar.clone(),
                        event_id: event_id.clone(),
                    });
                }
            }
        }
    }

    fn remember_visible_events(
        &self,
        agent_id: &AgentId,
        account: &ValidatedAccount,
        calendar: &str,
        events: &[&BackendEvent],
    ) {
        let recent = events
            .iter()
            .map(|event| RecentEventRef {
                account: account.id.clone(),
                calendar: calendar.to_owned(),
                event_id: event_id(event).to_owned(),
                summary: event_summary_for_policy(&self.config.policy, event).to_owned(),
            })
            .collect();
        self.last_events
            .borrow_mut()
            .insert(agent_id.as_ref().to_owned(), recent);
    }

    fn resolve_read_event_id(
        &self,
        agent_id: &AgentId,
        account: &ValidatedAccount,
        calendar: &str,
        event_id: Option<&str>,
    ) -> Result<String, String> {
        if let Some(event_id) = event_id {
            return required_arg(Some(event_id), "event_id").map(str::to_owned);
        }
        let recent = self.last_events.borrow();
        let candidates = recent
            .get(agent_id.as_ref())
            .map(|events| {
                events
                    .iter()
                    .filter(|event| event.account == account.id && event.calendar == calendar)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        match candidates.as_slice() {
            [event] => Ok(event.event_id.clone()),
            [] => Err(
                "event_id is required; retry calendar_get with event_id set to the event id from calendar_search".to_owned(),
            ),
            events => Err(format!(
                "event_id is required; choose one of: {}",
                events
                    .iter()
                    .map(|event| format!(
                        "{} ({})",
                        safe_display_line(&event.event_id),
                        safe_display_line(&event.summary)
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    fn validate_persisted_change(&self, change: &CalendarChangeApproval) -> Result<(), String> {
        let account = self.account_by_id(&change.account)?;
        self.ensure_calendar_allowed(account, &change.calendar)?;
        if !matches!(
            &account.backend,
            Some(ValidatedBackendConfig::Google { .. })
        ) {
            return Err(format!(
                "calendar account `{}` backend `{}` does not support calendar writes",
                account.id,
                account.backend_kind()
            ));
        }
        if let Some(attendees) = &change.attendees
            && self.config.policy.write.max_attendees < attendees.len()
        {
            return Err("calendar change has too many attendees for current policy".to_owned());
        }
        validate_change_shape(change)?;
        if change.etag.as_deref() == Some("*") {
            return Err("calendar change contains unsafe wildcard etag".to_owned());
        }
        Ok(())
    }

    fn execute_change(
        &self,
        change: &CalendarChangeApproval,
    ) -> Result<CalendarMutationResult, String> {
        let account = self.account_by_id(&change.account)?;
        self.ensure_calendar_allowed(account, &change.calendar)?;
        let stored_refresh_token = self.google_refresh_token(account)?;
        let result = match change.command.as_str() {
            "create_event" => {
                let event = self
                    .google
                    .create_event(
                        account,
                        stored_refresh_token.as_deref(),
                        &change.calendar,
                        &google_write_from_change(change),
                    )
                    .map_err(calendar_write_error)?;
                Ok(CalendarMutationResult::Event(Box::new(event)))
            }
            "update_event" => {
                let event_id = required_change_field(change.event_id.as_deref(), "event_id")?;
                let etag = required_change_field(change.etag.as_deref(), "etag")?;
                let event = self
                    .google
                    .update_event(
                        account,
                        stored_refresh_token.as_deref(),
                        &change.calendar,
                        event_id,
                        etag,
                        &google_write_from_change(change),
                    )
                    .map_err(calendar_write_error)?;
                Ok(CalendarMutationResult::Event(Box::new(event)))
            }
            "delete_event" => {
                let event_id = required_change_field(change.event_id.as_deref(), "event_id")?;
                let etag = required_change_field(change.etag.as_deref(), "etag")?;
                self.google
                    .delete_event(
                        account,
                        stored_refresh_token.as_deref(),
                        &change.calendar,
                        event_id,
                        etag,
                    )
                    .map_err(calendar_write_error)?;
                Ok(CalendarMutationResult::Deleted)
            }
            "respond_invite" => {
                let event_id = required_change_field(change.event_id.as_deref(), "event_id")?;
                let etag = required_change_field(change.etag.as_deref(), "etag")?;
                let response = required_change_field(change.response.as_deref(), "response")?;
                let event = self
                    .google
                    .respond_invite(
                        account,
                        stored_refresh_token.as_deref(),
                        &change.calendar,
                        event_id,
                        etag,
                        response,
                    )
                    .map_err(calendar_write_error)?;
                Ok(CalendarMutationResult::Event(Box::new(event)))
            }
            other => Err(format!("unsupported calendar change command `{other}`")),
        }?;
        self.remember_mutation_result(change, &result);
        Ok(result)
    }

    fn events_for_account(
        &self,
        account: &ValidatedAccount,
        calendar: &str,
        range: TimeRange,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<BackendEventPage, String> {
        match &account.backend {
            Some(ValidatedBackendConfig::IcsFeed { .. }) => {
                let page = self
                    .ics_feed
                    .list_events_page(account, calendar, range, limit, cursor)?;
                Ok(BackendEventPage {
                    events: page.events.into_iter().map(BackendEvent::Ics).collect(),
                    next_cursor: page.next_cursor,
                    truncated: page.truncated,
                })
            }
            Some(ValidatedBackendConfig::Google { .. }) => {
                let stored_refresh_token = self.google_refresh_token(account)?;
                let page = self.google.list_events_page(
                    account,
                    stored_refresh_token.as_deref(),
                    calendar,
                    range,
                    limit,
                    cursor,
                )?;
                let truncated = page.next_cursor.is_some();
                Ok(BackendEventPage {
                    events: page.events.into_iter().map(BackendEvent::Google).collect(),
                    next_cursor: page.next_cursor,
                    truncated,
                })
            }
            Some(ValidatedBackendConfig::Caldav { .. }) | None => Err(format!(
                "calendar account `{}` backend `{}` does not support event reads yet",
                account.id,
                account.backend_kind()
            )),
        }
    }

    fn accounts_for_read(&self, account: Option<&str>) -> Result<Vec<&ValidatedAccount>, String> {
        if let Some(account_id) = account {
            return Ok(vec![self.account_by_id(account_id)?]);
        }
        Ok(self
            .config
            .account_order
            .iter()
            .filter_map(|id| self.config.accounts.get(id))
            .filter(|account| account.enable)
            .collect())
    }

    fn single_account(&self, account: Option<&str>) -> Result<&ValidatedAccount, String> {
        if let Some(account_id) = account {
            return self.account_by_id(account_id);
        }
        let mut accounts = self.accounts_for_read(None)?.into_iter();
        let Some(first) = accounts.next() else {
            return Err("no enabled calendar accounts are configured".to_owned());
        };
        Ok(first)
    }

    fn account_by_id(&self, account_id: &str) -> Result<&ValidatedAccount, String> {
        let account = self
            .config
            .accounts
            .get(account_id)
            .ok_or_else(|| format!("unknown calendar account `{account_id}`"))?;
        if !self.config.enable {
            return Err("calendar module is disabled".to_owned());
        }
        if !account.enable {
            return Err(format!("calendar account `{account_id}` is disabled"));
        }
        Ok(account)
    }

    fn resolve_calendar_arg(
        &self,
        calendar: Option<&str>,
    ) -> Result<(&ValidatedAccount, String), String> {
        if let Some(calendar_id) = calendar {
            let (account_id, calendar) = split_flattened_calendar_id(calendar_id)?;
            let account = self.account_by_id(&account_id)?;
            return Ok((account, calendar));
        }
        let account = self.single_account(None)?;
        let Some(calendar) = default_calendar_id_for_account(account) else {
            return Err(format!(
                "calendar is required for account `{}` because no default calendar is configured",
                account.id
            ));
        };
        Ok((account, calendar.to_owned()))
    }

    fn ensure_calendar_allowed(
        &self,
        account: &ValidatedAccount,
        calendar: &str,
    ) -> Result<(), String> {
        if account
            .allowed_calendars
            .iter()
            .any(|allowed| allowed == calendar)
        {
            return Ok(());
        }
        Err(format!(
            "calendar `{calendar}` is not allowed for account `{}`",
            account.id
        ))
    }

    fn google_oauth_state_account(&self, account_id: &str) -> Result<&ValidatedAccount, String> {
        let account = self.account_by_id(account_id)?;
        match &account.backend {
            Some(ValidatedBackendConfig::Google {
                refresh_token_secret: None,
                ..
            }) => Ok(account),
            Some(ValidatedBackendConfig::Google { .. }) => Err(format!(
                "calendar account `{}` already uses refresh_token_secret; remove it before using `:calendar auth google`",
                account.id
            )),
            _ => Err(format!(
                "calendar account `{}` backend `{}` is not google",
                account.id,
                account.backend_kind()
            )),
        }
    }

    fn google_refresh_token(&self, account: &ValidatedAccount) -> Result<Option<String>, String> {
        match &account.backend {
            Some(ValidatedBackendConfig::Google {
                refresh_token_secret: Some(_),
                ..
            }) => Ok(None),
            Some(ValidatedBackendConfig::Google {
                refresh_token_secret: None,
                ..
            }) => self
                .state
                .google_refresh_token(&account.id)?
                .map(Some)
                .ok_or_else(|| {
                    format!(
                        "Google calendar account `{}` is not authorized; run `:calendar auth google start {}` and then `:calendar auth google finish {}`",
                        account.id, account.id, account.id
                    )
                }),
            _ => Err(format!(
                "calendar account `{}` backend `{}` is not google",
                account.id,
                account.backend_kind()
            )),
        }
    }
}

fn format_google_auth_started(
    account_id: &str,
    started: &crate::google_oauth::GoogleDeviceAuthStart,
) -> String {
    format!(
        "Google Calendar authorization started for account {}.\nOpen this URL:\n{}\nEnter this code:\n{}\nThen run:\n:calendar auth google finish {}\nExpires in {} second(s). If authorization is still pending, wait at least {} second(s) before retrying finish.",
        safe_display_line(account_id),
        safe_display_line(&started.verification_uri),
        safe_display_line(&started.user_code),
        safe_display_line(account_id),
        started.expires_in_secs,
        started.interval_secs
    )
}

fn default_calendar_id_for_account(account: &ValidatedAccount) -> Option<&str> {
    account.default_calendar.as_deref().or_else(|| {
        account
            .allowed_calendars
            .first()
            .map(String::as_str)
            .or(match &account.backend {
                Some(ValidatedBackendConfig::IcsFeed { .. }) => Some("main"),
                Some(ValidatedBackendConfig::Google { .. })
                | Some(ValidatedBackendConfig::Caldav { .. })
                | None => None,
            })
    })
}

fn parse_range(args: &CalendarRangeArgs, account: &ValidatedAccount) -> Result<TimeRange, String> {
    let start_value = optional_trimmed_line(args.start.as_deref(), "start")?;
    let (start, default_end) = start_value.as_deref().map_or_else(
        || {
            let (start, end) = default_read_range(account.timezone.as_deref())?;
            Ok::<_, String>((start, Some(end)))
        },
        |start_value| {
            Ok((
                parse_read_bound(start_value, "start", account.timezone.as_deref())?,
                None,
            ))
        },
    )?;
    let end = match args.end.as_deref() {
        Some(end) if !end.trim().is_empty() => {
            parse_read_bound(end, "end", account.timezone.as_deref())?
        }
        _ => start_value.as_deref().map_or_else(
            || Ok(default_end.expect("default end present when start is absent")),
            |start_value| default_read_end_bound(start_value, start, account.timezone.as_deref()),
        )?,
    };
    if !is_datetime_before(start, end) {
        return Err("start must be before end".to_owned());
    }
    Ok(TimeRange {
        min: Some(start),
        max: Some(end),
    })
}

fn parse_read_bound(
    value: &str,
    field: &str,
    account_timezone: Option<&str>,
) -> Result<time::OffsetDateTime, String> {
    if let Ok(value) =
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
    {
        return Ok(value);
    }
    match parse_local_read_bound(value, field, account_timezone) {
        Ok(local) => local_read_bound_to_utc(local, field, account_timezone),
        Err(local_error) => {
            parse_natural_datetime_bound(value, field, account_timezone).map_err(|natural_error| {
                format!("{local_error}; natural date parser also rejected it: {natural_error}")
            })
        }
    }
}

fn format_optional_read_bound(
    value: Option<time::OffsetDateTime>,
    field: &str,
) -> Result<Option<String>, String> {
    value
        .map(|value| {
            value
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(|error| format!("{field} could not be formatted: {error}"))
        })
        .transpose()
}

fn default_read_range(
    account_timezone: Option<&str>,
) -> Result<(time::OffsetDateTime, time::OffsetDateTime), String> {
    let timezone = timezone_for_read("start", account_timezone)?;
    let today = time::OffsetDateTime::now_utc().to_timezone(timezone).date();
    let start_date = date_days_before(today, DEFAULT_READ_LOOKBACK_DAYS)?;
    let start_local = start_date.with_time(time::Time::MIDNIGHT);
    let end_local = start_local
        .checked_add(time::Duration::days(DEFAULT_READ_WINDOW_DAYS))
        .ok_or_else(|| "default end is out of range".to_owned())?;
    let start = local_read_bound_to_utc(start_local, "start", account_timezone)?;
    let end = local_read_bound_to_utc(end_local, "end", account_timezone)?;
    Ok((start, end))
}

fn date_days_before(mut date: time::Date, days: u8) -> Result<time::Date, String> {
    for _ in 0..days {
        date = date
            .previous_day()
            .ok_or_else(|| "default start is out of range".to_owned())?;
    }
    Ok(date)
}

fn default_read_end_bound(
    start_value: &str,
    start_utc: time::OffsetDateTime,
    account_timezone: Option<&str>,
) -> Result<time::OffsetDateTime, String> {
    if time::OffsetDateTime::parse(start_value, &time::format_description::well_known::Rfc3339)
        .is_ok()
    {
        return start_utc
            .checked_add(time::Duration::days(DEFAULT_READ_WINDOW_DAYS))
            .ok_or_else(|| "default end is out of range".to_owned());
    }
    if let Ok(local_start) = parse_local_read_bound(start_value, "start", account_timezone) {
        let local = local_start
            .checked_add(time::Duration::days(DEFAULT_READ_WINDOW_DAYS))
            .ok_or_else(|| "default end is out of range".to_owned())?;
        return local_read_bound_to_utc(local, "end", account_timezone);
    }
    start_utc
        .checked_add(time::Duration::days(DEFAULT_READ_WINDOW_DAYS))
        .ok_or_else(|| "default end is out of range".to_owned())
}

fn parse_local_read_bound(
    value: &str,
    field: &str,
    account_timezone: Option<&str>,
) -> Result<time::PrimitiveDateTime, String> {
    let value = value.trim();
    if let Some(date) = parse_natural_date(value, field, account_timezone)? {
        return Ok(date.with_time(time::Time::MIDNIGHT));
    }
    if let Some(date) = parse_tool_date(value) {
        return Ok(date.with_time(time::Time::MIDNIGHT));
    }
    time::PrimitiveDateTime::parse(
        value,
        time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]"),
    )
    .map_err(|error| {
        format!(
            "{field} must be RFC3339 with offset, YYYY-MM-DD, today, yesterday, tomorrow, or local YYYY-MM-DDTHH:MM:SS: {error}"
        )
    })
}

fn parse_natural_date(
    value: &str,
    field: &str,
    account_timezone: Option<&str>,
) -> Result<Option<time::Date>, String> {
    let timezone = timezone_for_read(field, account_timezone)?;
    let today = time::OffsetDateTime::now_utc().to_timezone(timezone).date();
    Ok(match value.trim().to_ascii_lowercase().as_str() {
        "today" => Some(today),
        "yesterday" => today.previous_day(),
        "tomorrow" => today.next_day(),
        _ => None,
    })
}

fn parse_natural_datetime_bound(
    value: &str,
    field: &str,
    _account_timezone: Option<&str>,
) -> Result<time::OffsetDateTime, String> {
    let parsed = parse_datetime::parse_datetime(value)
        .map_err(|error| format!("{field} natural date/time parse failed: {error}"))?;
    time::OffsetDateTime::from_unix_timestamp_nanos(parsed.timestamp().as_nanosecond())
        .map_err(|error| format!("{field} natural date/time is out of range: {error}"))
}

fn local_read_bound_to_utc(
    local: time::PrimitiveDateTime,
    field: &str,
    account_timezone: Option<&str>,
) -> Result<time::OffsetDateTime, String> {
    let timezone = timezone_for_read(field, account_timezone)?;
    match local.assume_timezone(timezone) {
        OffsetResult::Some(value) => Ok(value),
        OffsetResult::Ambiguous(_, _) => Err(format!(
            "{field} is ambiguous in timezone `{}`",
            timezone.name()
        )),
        OffsetResult::None => Err(format!(
            "{field} is invalid in timezone `{}`",
            timezone.name()
        )),
    }
}

fn timezone_for_read(
    field: &str,
    account_timezone: Option<&str>,
) -> Result<&'static time_tz::Tz, String> {
    if let Some(timezone_name) = account_timezone {
        return time_tz::timezones::get_by_name(timezone_name).ok_or_else(|| {
            format!("account timezone `{timezone_name}` is not recognized for {field}")
        });
    }
    if let Some(timezone_name) = system_timezone_name()
        && let Some(timezone) = time_tz::timezones::get_by_name(&timezone_name)
    {
        return Ok(timezone);
    }
    time_tz::timezones::get_by_name("UTC")
        .ok_or_else(|| "UTC timezone is not recognized".to_owned())
}

fn system_timezone_name() -> Option<String> {
    if let Ok(value) = std::env::var("TZ") {
        let value = value.trim().trim_start_matches(':');
        if !value.is_empty() {
            return Some(value.to_owned());
        }
    }
    let path = std::fs::read_link("/etc/localtime").ok()?;
    let text = path.to_string_lossy();
    text.split("zoneinfo/").nth(1).map(str::to_owned)
}

fn normalized_limit(limit: Option<u32>) -> Result<usize, String> {
    let limit = limit.unwrap_or(DEFAULT_EVENT_LIMIT);
    if limit == 0 {
        return Err("limit must be a positive integer".to_owned());
    }
    let capped = if MAX_EVENT_LIMIT < limit {
        MAX_EVENT_LIMIT
    } else {
        limit
    };
    Ok(capped as usize)
}

fn required_arg<'a>(value: Option<&'a str>, name: &str) -> Result<&'a str, String> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(format!("{name} is required")),
    }
}

fn required_text(value: Option<&str>, name: &str) -> Result<String, String> {
    let value = required_arg(value, name)?;
    validate_line(value, name, false)?;
    Ok(value.to_owned())
}

fn calendar_write_error(error: String) -> String {
    if error.contains("HTTP 412") || error.contains("Precondition Failed") {
        "calendar event changed since it was last read; re-read the event and retry".to_owned()
    } else {
        error
    }
}

fn required_change_field<'a>(value: Option<&'a str>, name: &str) -> Result<&'a str, String> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(format!("calendar change is missing {name}")),
    }
}

fn optional_line(
    value: Option<&str>,
    name: &str,
    allow_empty: bool,
) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    validate_line(value, name, allow_empty)?;
    Ok(Some(value.to_owned()))
}

fn optional_trimmed_line(value: Option<&str>, name: &str) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    validate_line(value, name, false)?;
    Ok(Some(value.to_owned()))
}

fn optional_description(value: Option<&str>) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    if MAX_EVENT_DESCRIPTION_BYTES < value.len()
        || MAX_EVENT_DESCRIPTION_LINES < value.lines().count()
        || value.chars().any(|ch| {
            (ch.is_control() && ch != '\n' && ch != '\r' && ch != '\t')
                || is_unsafe_format_control(ch)
        })
    {
        return Err("description is too large or contains unsafe characters".to_owned());
    }
    Ok(Some(value.to_owned()))
}

fn validate_line(value: &str, name: &str, allow_empty: bool) -> Result<(), String> {
    if (!allow_empty && value.trim().is_empty())
        || MAX_EVENT_FIELD_CHARS < value.chars().count()
        || value
            .chars()
            .any(|ch| ch.is_control() || is_unsafe_format_control(ch))
    {
        return Err(format!("{name} is too large or contains unsafe characters"));
    }
    Ok(())
}

fn optional_attendees(
    value: Option<&[String]>,
    max_attendees: usize,
) -> Result<Option<Vec<String>>, String> {
    let Some(attendees) = value else {
        return Ok(None);
    };
    if max_attendees < attendees.len() {
        return Err(format!("too many attendees; maximum is {max_attendees}"));
    }
    for attendee in attendees {
        validate_attendee(attendee)?;
    }
    Ok(Some(attendees.to_vec()))
}

fn validate_attendee(value: &str) -> Result<(), String> {
    if value.trim().is_empty()
        || MAX_ATTENDEE_CHARS < value.chars().count()
        || value.matches('@').count() != 1
        || value
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace() || is_unsafe_format_control(ch))
    {
        return Err("attendee email address is invalid".to_owned());
    }
    Ok(())
}

fn create_event_time_pair(
    start: Option<&str>,
    end: Option<&str>,
    account_timezone: Option<&str>,
) -> Result<(String, String), String> {
    let start = required_text(start, "start")?;
    let start = normalize_write_time_value(&start, "start", account_timezone)?;
    let end = match end {
        Some(end) if !end.trim().is_empty() => {
            let end = required_text(Some(end), "end")?;
            normalize_write_time_value(&end, "end", account_timezone)?
        }
        _ => default_create_event_end(&start)?,
    };
    validate_time_pair(&start, &end)?;
    Ok((start, end))
}

fn default_create_event_end(start: &str) -> Result<String, String> {
    if let Some(date) = parse_tool_date(start) {
        let end = date
            .next_day()
            .ok_or_else(|| "default event end is out of range".to_owned())?;
        return Ok(end.to_string());
    }
    let start = time::OffsetDateTime::parse(start, &time::format_description::well_known::Rfc3339)
        .map_err(|error| format!("start must be RFC3339 or YYYY-MM-DD: {error}"))?;
    let end = start
        .checked_add(time::Duration::hours(1))
        .ok_or_else(|| "default event end is out of range".to_owned())?;
    end.format(&time::format_description::well_known::Rfc3339)
        .map_err(|error| format!("default event end could not be formatted: {error}"))
}

fn required_time_pair(
    start: Option<&str>,
    end: Option<&str>,
    account_timezone: Option<&str>,
) -> Result<(String, String), String> {
    let start = required_text(start, "start")?;
    let end = required_text(end, "end")?;
    let start = normalize_write_time_value(&start, "start", account_timezone)?;
    let end = normalize_write_time_value(&end, "end", account_timezone)?;
    validate_time_pair(&start, &end)?;
    Ok((start, end))
}

fn normalize_write_time_value(
    value: &str,
    field: &str,
    account_timezone: Option<&str>,
) -> Result<String, String> {
    if parse_tool_date(value).is_some()
        || time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
            .is_ok()
    {
        return Ok(value.to_owned());
    }
    match parse_local_read_bound(value, field, account_timezone) {
        Ok(local) => local_read_bound_to_utc(local, field, account_timezone)?
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|error| format!("{field} could not be formatted: {error}")),
        Err(local_error) => parse_natural_datetime_bound(value, field, account_timezone)
            .and_then(|datetime| {
                datetime
                    .format(&time::format_description::well_known::Rfc3339)
                    .map_err(|error| format!("{field} could not be formatted: {error}"))
            })
            .map_err(|natural_error| {
                format!("{local_error}; natural date parser also rejected it: {natural_error}")
            }),
    }
}

fn validate_time_pair(start: &str, end: &str) -> Result<(), String> {
    let start_date = parse_tool_date(start);
    let end_date = parse_tool_date(end);
    match (start_date, end_date) {
        (Some(start), Some(end)) => {
            if !is_date_before(start, end) {
                return Err("event start must be before event end".to_owned());
            }
            Ok(())
        }
        (None, None) => {
            let start =
                time::OffsetDateTime::parse(start, &time::format_description::well_known::Rfc3339)
                    .map_err(|error| format!("start must be RFC3339 or YYYY-MM-DD: {error}"))?;
            let end =
                time::OffsetDateTime::parse(end, &time::format_description::well_known::Rfc3339)
                    .map_err(|error| format!("end must be RFC3339 or YYYY-MM-DD: {error}"))?;
            if !is_datetime_before(start, end) {
                return Err("event start must be before event end".to_owned());
            }
            Ok(())
        }
        _ => Err(
            "event start and end must both be all-day dates or both be RFC3339 date-times"
                .to_owned(),
        ),
    }
}

fn parse_tool_date(value: &str) -> Option<time::Date> {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !bytes[..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..7].iter().all(u8::is_ascii_digit)
        || !bytes[8..].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let year = value[0..4].parse::<i32>().ok()?;
    let month = time::Month::try_from(value[5..7].parse::<u8>().ok()?).ok()?;
    let day = value[8..10].parse::<u8>().ok()?;
    time::Date::from_calendar_date(year, month, day).ok()
}

fn is_date_before(left: time::Date, right: time::Date) -> bool {
    left < right
}

fn is_datetime_before(left: time::OffsetDateTime, right: time::OffsetDateTime) -> bool {
    left < right
}

fn required_response(value: Option<&str>) -> Result<String, String> {
    let response = required_arg(value, "response")?;
    if !matches!(response, "accepted" | "tentative" | "declined") {
        return Err("response must be accepted, tentative, or declined".to_owned());
    }
    Ok(response.to_owned())
}

fn ensure_google_write_backend(account: &ValidatedAccount) -> Result<(), String> {
    if matches!(
        &account.backend,
        Some(ValidatedBackendConfig::Google { .. })
    ) {
        return Ok(());
    }
    Err(format!(
        "calendar account `{}` backend `{}` does not support calendar writes",
        account.id,
        account.backend_kind()
    ))
}

fn fill_update_time_change(
    change: &mut CalendarChangeApproval,
    args: &ChangeArgs,
    timezone: Option<&str>,
) -> Result<(), String> {
    match (args.start.as_deref(), args.end.as_deref()) {
        (Some(_), Some(_)) => {
            let (start, end) =
                required_time_pair(args.start.as_deref(), args.end.as_deref(), timezone)?;
            change.start = Some(start);
            change.end = Some(end);
            Ok(())
        }
        (Some(_), None) => {
            let (start, end) = create_event_time_pair(args.start.as_deref(), None, timezone)?;
            change.start = Some(start);
            change.end = Some(end);
            Ok(())
        }
        (None, None) => Ok(()),
        (None, Some(_)) => {
            Err("end without start is ambiguous; pass start too, or omit both".to_owned())
        }
    }
}

fn change_has_update_payload(change: &CalendarChangeApproval) -> bool {
    change.title.is_some()
        || change.description.is_some()
        || change.location.is_some()
        || change.start.is_some()
        || change.end.is_some()
        || change.attendees.is_some()
}

fn ensure_update_change_has_payload(change: &CalendarChangeApproval) -> Result<(), String> {
    if change_has_update_payload(change) {
        return Ok(());
    }
    Err("update_event requires at least one field to update".to_owned())
}

fn ensure_response_only_for_invites(args: &ChangeArgs) -> Result<(), String> {
    if args.response.is_none() {
        return Ok(());
    }
    Err("response is only valid for respond_invite".to_owned())
}

fn validate_change_shape(change: &CalendarChangeApproval) -> Result<(), String> {
    match change.command.as_str() {
        "create_event" => {
            required_change_field(change.title.as_deref(), "title")?;
            let start = required_change_field(change.start.as_deref(), "start")?;
            let end = required_change_field(change.end.as_deref(), "end")?;
            validate_time_pair(start, end)
        }
        "update_event" => {
            required_change_field(change.event_id.as_deref(), "event_id")?;
            required_change_field(change.etag.as_deref(), "etag")?;
            if !change_has_update_payload(change) {
                return Err("update_event requires at least one field to update".to_owned());
            }
            if let (Some(start), Some(end)) = (change.start.as_deref(), change.end.as_deref()) {
                validate_time_pair(start, end)?;
            } else if change.start.is_some() || change.end.is_some() || change.timezone.is_some() {
                return Err("start and end must be provided together for time updates".to_owned());
            }
            Ok(())
        }
        "delete_event" => {
            required_change_field(change.event_id.as_deref(), "event_id")?;
            required_change_field(change.etag.as_deref(), "etag")?;
            Ok(())
        }
        "respond_invite" => {
            required_change_field(change.event_id.as_deref(), "event_id")?;
            required_change_field(change.etag.as_deref(), "etag")?;
            required_response(change.response.as_deref()).map(|_| ())
        }
        other => Err(format!("unsupported calendar change command `{other}`")),
    }
}

fn google_write_from_change(change: &CalendarChangeApproval) -> GoogleEventWrite<'_> {
    GoogleEventWrite {
        title: change.title.as_deref(),
        description: change.description.as_deref(),
        location: change.location.as_deref(),
        start: change.start.as_deref(),
        end: change.end.as_deref(),
        timezone: change.timezone.as_deref(),
        clear_opposite_time_kind: change.command == "update_event" && change.start.is_some(),
        attendees: change.attendees.as_deref(),
    }
}

fn parse_log_limit(argv: &[String]) -> Result<usize, String> {
    let limit = match argv {
        [] => CALENDAR_LOG_DEFAULT_LIMIT,
        [value] if !value.trim().is_empty() => value
            .parse::<usize>()
            .map_err(|_| "log limit must be a positive integer".to_owned())?,
        [_] => return Err("log limit must not be empty".to_owned()),
        _ => return Err("too many action arguments".to_owned()),
    };
    if limit == 0 {
        return Err("log limit must be a positive integer".to_owned());
    }
    Ok(if CALENDAR_LOG_MAX_LIMIT < limit {
        CALENDAR_LOG_MAX_LIMIT
    } else {
        limit
    })
}

fn require_no_args(argv: &[String]) -> Result<(), String> {
    if argv.is_empty() {
        Ok(())
    } else {
        Err("this calendar action does not accept arguments".to_owned())
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

fn require_change_ids(argv: &[String]) -> Result<Vec<String>, String> {
    let raw = require_one_arg(argv)?;
    let ids = raw
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return Err("missing required action argument".to_owned());
    }
    for id in &ids {
        if id.parse::<u64>().ok().filter(|value| 0 < *value).is_none()
            || !id.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(format!("invalid calendar change id `{id}`"));
        }
    }
    Ok(ids)
}

fn calendar_log_item_count(command: CalendarCommand, result: &CborValue) -> Option<u64> {
    if cbor_bool_field(result, "ok") != Some(true) {
        return None;
    }
    let data = cbor_field(result, "data")?;
    match command {
        CalendarCommand::ListCalendars => cbor_array_len(data, "calendars"),
        CalendarCommand::ListEvents => cbor_array_len(data, "events"),
        CalendarCommand::FreeBusy => cbor_array_len(data, "busy"),
        CalendarCommand::ReadEvent => Some(1),
        CalendarCommand::CreateEvent
        | CalendarCommand::UpdateEvent
        | CalendarCommand::DeleteEvent
        | CalendarCommand::RespondInvite => None,
    }
}

fn format_calendar_log_entry(entry: &CalendarLogEntry) -> String {
    let mut fields = vec![
        format!("ts={}", entry.ts_unix_ms),
        format!("kind={}", safe_display_line(&entry.kind)),
        format!("command={}", safe_display_line(&entry.command)),
        format!("status={}", safe_display_line(&entry.status)),
    ];
    push_log_field(&mut fields, "account", entry.account.as_deref());
    push_log_field(&mut fields, "calendar", entry.calendar.as_deref());
    push_log_field(&mut fields, "event_id", entry.event_id.as_deref());
    push_log_field(&mut fields, "start", entry.start.as_deref());
    push_log_field(&mut fields, "end", entry.end.as_deref());
    if let Some(limit) = entry.limit {
        fields.push(format!("limit={limit}"));
    }
    if let Some(item_count) = entry.item_count {
        fields.push(format!("items={item_count}"));
    }
    push_log_field(&mut fields, "reason", entry.reason.as_deref());
    fields.join(" ")
}

fn push_log_field(fields: &mut Vec<String>, name: &str, value: Option<&str>) {
    if let Some(value) = value {
        fields.push(format!("{name}={}", safe_display_line(value)));
    }
}

fn format_event_line(
    policy: &ValidatedPolicy,
    output_timezone: &time_tz::Tz,
    event: &BackendEvent,
) -> String {
    let status = event_status(event).unwrap_or("-");
    format!(
        "{} {} {} {} {} {}",
        safe_field(event_id(event)),
        safe_field(&event_start_for_output(event, output_timezone)),
        safe_field(&event_end_for_output(event, output_timezone)),
        event_flags(policy, event),
        safe_field(status),
        safe_field(event_summary_for_policy(policy, event))
    )
}

fn format_free_busy_line(
    policy: &ValidatedPolicy,
    output_timezone: &time_tz::Tz,
    event: &BackendEvent,
) -> String {
    format!(
        "{} {} {} {}",
        safe_field(event_id(event)),
        safe_field(&event_start_for_output(event, output_timezone)),
        safe_field(&event_end_for_output(event, output_timezone)),
        event_flags(policy, event)
    )
}

fn format_event_detail(
    policy: &ValidatedPolicy,
    account: &ValidatedAccount,
    calendar: &str,
    output_timezone: &time_tz::Tz,
    event: &BackendEvent,
) -> Vec<String> {
    let mut lines = vec![
        format!(
            "calendar {}",
            safe_field(&flatten_calendar_id(&account.id, calendar))
        ),
        format!("event_id {}", safe_field(event_id(event))),
        format!(
            "start {}",
            safe_field(&event_start_for_output(event, output_timezone))
        ),
        format!(
            "end {}",
            safe_field(&event_end_for_output(event, output_timezone))
        ),
        format!("flags {}", event_flags(policy, event)),
        format!(
            "summary {}",
            safe_field(event_summary_for_policy(policy, event))
        ),
    ];
    if let Some(uid) = event_uid(event) {
        lines.push(format!("uid {}", safe_field(uid)));
    }
    if let Some(status) = event_status(event) {
        lines.push(format!("status {}", safe_field(status)));
    }
    if !event_private_busy_only(policy, event) {
        if let Some(location) = event_location(event) {
            lines.push(format!("location {}", safe_field(location)));
        }
        if let Some(organizer) = event_organizer(event) {
            lines.push(format!("organizer {}", safe_field(organizer)));
        }
        let attendees = event_attendees(event);
        if !attendees.is_empty() {
            lines.push(format!("attendees {}", safe_field(&attendees.join(","))));
        }
        if let Some(description) = event_description_for_policy(policy, event) {
            lines.push(format!("description {}", safe_multiline(description)));
        }
    }
    lines
}

fn filtered_events<'a>(
    policy: &ValidatedPolicy,
    events: &'a [BackendEvent],
    title_filter: Option<&str>,
) -> Vec<&'a BackendEvent> {
    let Some(title_filter) = title_filter else {
        return events.iter().collect();
    };
    let title_filter = title_filter.to_lowercase();
    events
        .iter()
        .filter(|event| {
            event_summary_for_policy(policy, event)
                .to_lowercase()
                .contains(&title_filter)
        })
        .collect()
}

fn event_id(event: &BackendEvent) -> &str {
    match event {
        BackendEvent::Ics(event) => &event.id,
        BackendEvent::Google(event) => &event.id,
    }
}

fn event_uid(event: &BackendEvent) -> Option<&str> {
    match event {
        BackendEvent::Ics(event) => Some(&event.uid),
        BackendEvent::Google(event) => event.i_cal_uid.as_deref(),
    }
}

fn event_etag(event: &BackendEvent) -> Option<&str> {
    match event {
        BackendEvent::Ics(_) => None,
        BackendEvent::Google(event) => event.etag.as_deref(),
    }
}

fn event_summary(event: &BackendEvent) -> &str {
    match event {
        BackendEvent::Ics(event) => &event.summary,
        BackendEvent::Google(event) => &event.summary,
    }
}

fn event_summary_for_policy<'a>(policy: &ValidatedPolicy, event: &'a BackendEvent) -> &'a str {
    if event_private_busy_only(policy, event) {
        "(private)"
    } else {
        event_summary(event)
    }
}

fn event_description_for_policy<'a>(
    policy: &ValidatedPolicy,
    event: &'a BackendEvent,
) -> Option<&'a str> {
    if event_private_busy_only(policy, event) {
        return None;
    }
    match policy.read.descriptions {
        DescriptionPolicy::Always => match event {
            BackendEvent::Ics(event) => event.description.as_deref(),
            BackendEvent::Google(event) => event.description.as_deref(),
        },
        DescriptionPolicy::ApprovedOnly | DescriptionPolicy::Omit => None,
    }
}

fn event_location(event: &BackendEvent) -> Option<&str> {
    match event {
        BackendEvent::Ics(event) => event.location.as_deref(),
        BackendEvent::Google(event) => event.location.as_deref(),
    }
}

fn event_start(event: &BackendEvent) -> &str {
    match event {
        BackendEvent::Ics(event) => &event.start,
        BackendEvent::Google(event) => &event.start,
    }
}

fn event_end(event: &BackendEvent) -> &str {
    match event {
        BackendEvent::Ics(event) => &event.end,
        BackendEvent::Google(event) => &event.end,
    }
}

fn event_start_for_output(event: &BackendEvent, timezone: &time_tz::Tz) -> String {
    event_time_for_output(event_start(event), timezone)
}

fn event_end_for_output(event: &BackendEvent, timezone: &time_tz::Tz) -> String {
    event_time_for_output(event_end(event), timezone)
}

fn event_time_for_output(value: &str, timezone: &time_tz::Tz) -> String {
    let Ok(time) =
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
    else {
        return value.to_owned();
    };
    time.to_timezone(timezone)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| value.to_owned())
}

fn event_status(event: &BackendEvent) -> Option<&str> {
    match event {
        BackendEvent::Ics(event) => event.status.as_deref(),
        BackendEvent::Google(event) => event.status.as_deref(),
    }
}

fn event_organizer(event: &BackendEvent) -> Option<&str> {
    match event {
        BackendEvent::Ics(event) => event.organizer.as_deref(),
        BackendEvent::Google(event) => event.organizer.as_deref(),
    }
}

fn event_attendees(event: &BackendEvent) -> &[String] {
    match event {
        BackendEvent::Ics(event) => &event.attendees,
        BackendEvent::Google(event) => &event.attendees,
    }
}

fn event_is_private(event: &BackendEvent) -> bool {
    match event {
        BackendEvent::Ics(event) => event.private,
        BackendEvent::Google(event) => event.visibility.as_deref() == Some("private"),
    }
}

fn event_private_busy_only(policy: &ValidatedPolicy, event: &BackendEvent) -> bool {
    policy.read.private_events == PrivateEventsPolicy::BusyOnly && event_is_private(event)
}

fn event_flags(policy: &ValidatedPolicy, event: &BackendEvent) -> String {
    let mut flags = vec!["read_only"];
    if event_private_busy_only(policy, event) {
        flags.push("private_busy_only");
    } else if event_is_private(event) {
        flags.push("private");
    }
    match event {
        BackendEvent::Ics(event) => {
            if event.recurring {
                flags.push("recurring");
            }
            if event.time_unparsed {
                flags.push("time_unparsed");
            }
        }
        BackendEvent::Google(event) => {
            if event.recurring {
                flags.push("recurring");
            }
            if event.transparency.as_deref() == Some("transparent") {
                flags.push("transparent");
            }
            if let Some(response) = &event.self_response_status
                && response == "declined"
            {
                flags.push("self_declined");
            }
        }
    }
    flags.join(",")
}

fn format_change_queued(id: &str, change: &CalendarChangeApproval) -> CborValue {
    let mut entries = vec![
        (
            "message",
            CborValue::Text("Calendar change pending approval.".to_owned()),
        ),
        ("approval_id", CborValue::Text(safe_display_line(id))),
        (
            "calendar",
            CborValue::Text(safe_display_line(&flatten_calendar_id(
                &change.account,
                &change.calendar,
            ))),
        ),
    ];
    if let Some(event_id) = change.event_id.as_deref() {
        entries.push(("event_id", CborValue::Text(safe_display_line(event_id))));
    }
    if let Some(start) = change.start.as_deref() {
        entries.push(("start", CborValue::Text(safe_display_line(start))));
    }
    if let Some(end) = change.end.as_deref() {
        entries.push(("end", CborValue::Text(safe_display_line(end))));
    }
    ok_envelope(&change.command, "approval_required", cbor_map(entries))
}

fn format_change_summary(change: &CalendarChangeApproval) -> String {
    let title = change.title.as_deref().unwrap_or("-");
    let event_id = change.event_id.as_deref().unwrap_or("-");
    let start = change.start.as_deref().unwrap_or("-");
    format!(
        "{} command={} account={} calendar={} event_id={} start={} title={}",
        safe_display_line(&change.id),
        safe_display_line(&change.command),
        safe_display_line(&change.account),
        safe_display_line(&change.calendar),
        safe_display_line(event_id),
        safe_display_line(start),
        safe_display_line(title)
    )
}

fn format_change_detail(change: &CalendarChangeApproval) -> String {
    let mut lines = vec![
        format!("Calendar change {}", safe_display_line(&change.id)),
        format!("status: {}", safe_display_line(&change.status)),
        format!("command: {}", safe_display_line(&change.command)),
        format!("account: {}", safe_display_line(&change.account)),
        format!("calendar: {}", safe_display_line(&change.calendar)),
        format!("reason: {}", safe_display_line(&change.reason)),
    ];
    push_change_detail(&mut lines, "event_id", change.event_id.as_deref());
    push_change_detail(&mut lines, "title", change.title.as_deref());
    push_change_detail(&mut lines, "location", change.location.as_deref());
    push_change_detail(&mut lines, "start", change.start.as_deref());
    push_change_detail(&mut lines, "end", change.end.as_deref());
    push_change_detail(&mut lines, "timezone", change.timezone.as_deref());
    if let Some(attendees) = &change.attendees {
        lines.push(format!(
            "attendees: {}",
            safe_display_line(&attendees.join(", "))
        ));
    }
    push_change_detail(&mut lines, "response", change.response.as_deref());
    if let Some(description) = &change.description {
        lines.push(format!("description:\n{}", safe_display_text(description)));
    }
    lines.join("\n")
}

fn push_change_detail(lines: &mut Vec<String>, name: &str, value: Option<&str>) {
    if let Some(value) = value {
        lines.push(format!("{name}: {}", safe_display_line(value)));
    }
}

fn format_mutation_result(
    verb: &str,
    id: &str,
    change: &CalendarChangeApproval,
    result: &CalendarMutationResult,
) -> String {
    match result {
        CalendarMutationResult::Event(event) => format!(
            "{verb} calendar change {id}. command={} account={} calendar={} event_id={}",
            safe_display_line(&change.command),
            safe_display_line(&change.account),
            safe_display_line(&change.calendar),
            safe_display_line(&event.id)
        ),
        CalendarMutationResult::Deleted => format!(
            "{verb} calendar change {id}. command={} account={} calendar={} event_id={}",
            safe_display_line(&change.command),
            safe_display_line(&change.account),
            safe_display_line(&change.calendar),
            safe_display_line(change.event_id.as_deref().unwrap_or("-"))
        ),
    }
}

fn format_mutation_result_envelope(
    id: &str,
    change: &CalendarChangeApproval,
    result: &CalendarMutationResult,
) -> CborValue {
    let mut entries = vec![
        (
            "message",
            CborValue::Text(format!(
                "Calendar change {}.",
                mutation_result_status(&change.command, result)
            )),
        ),
        ("change_id", CborValue::Text(safe_display_line(id))),
        (
            "calendar",
            CborValue::Text(safe_display_line(&flatten_calendar_id(
                &change.account,
                &change.calendar,
            ))),
        ),
    ];
    match result {
        CalendarMutationResult::Event(event) => {
            entries.push(("event_id", CborValue::Text(safe_display_line(&event.id))));
        }
        CalendarMutationResult::Deleted => {
            entries.push((
                "event_id",
                CborValue::Text(safe_display_line(change.event_id.as_deref().unwrap_or("-"))),
            ));
        }
    }
    ok_envelope(
        &change.command,
        mutation_result_status(&change.command, result),
        cbor_map(entries),
    )
}

fn mutation_result_status(command: &str, result: &CalendarMutationResult) -> &'static str {
    match (command, result) {
        (_, CalendarMutationResult::Deleted) => "deleted",
        ("create_event", CalendarMutationResult::Event(_)) => "created",
        ("update_event", CalendarMutationResult::Event(_)) => "updated",
        ("respond_invite", CalendarMutationResult::Event(_)) => "responded",
        _ => "applied",
    }
}

fn flatten_calendar_id(account: &str, calendar: &str) -> String {
    format!(
        "{}/{}",
        crate::opaque_id::encode_component(account),
        crate::opaque_id::encode_component(calendar)
    )
}

fn split_flattened_calendar_id(calendar_id: &str) -> Result<(String, String), String> {
    let Some((account, calendar)) = calendar_id.split_once('/') else {
        return Err("calendar must be a calendar id from calendar_list_calendars".to_owned());
    };
    let account = crate::opaque_id::decode_component(account)
        .map_err(|_| "calendar must be a calendar id from calendar_list_calendars".to_owned())?;
    let calendar = crate::opaque_id::decode_component(calendar)
        .map_err(|_| "calendar must be a calendar id from calendar_list_calendars".to_owned())?;
    if account.trim().is_empty() || calendar.trim().is_empty() {
        return Err("calendar must be a calendar id from calendar_list_calendars".to_owned());
    }
    Ok((account, calendar))
}

fn command_name(command: CalendarCommand) -> &'static str {
    match command {
        CalendarCommand::ListCalendars => "list_calendars",
        CalendarCommand::ListEvents => "list_events",
        CalendarCommand::ReadEvent => "read_event",
        CalendarCommand::FreeBusy => "free_busy",
        CalendarCommand::CreateEvent => "create_event",
        CalendarCommand::UpdateEvent => "update_event",
        CalendarCommand::DeleteEvent => "delete_event",
        CalendarCommand::RespondInvite => "respond_invite",
    }
}

fn finish_tool_result(invoke: ToolStarted, result: CborValue) -> Event {
    if cbor_bool_field(&result, "ok") == Some(false) {
        return tool_error(invoke, result);
    }
    let display = success_display_for_tool(invoke.tool_name.as_str(), &result);
    Event::ToolResult(ToolResult {
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
    let message = calendar_error_message_for_tool(invoke.tool_name.as_str(), &details);
    let display = error_display_for_tool(
        invoke.tool_name.as_str(),
        &invoke.arguments,
        &details,
        &message,
    );
    Event::ToolError(ToolError {
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        message: message.clone(),
        details: Some(details),
        display: Some(display),
        originator: tau_proto::PromptOriginator::User,
    })
}

fn action_result(invoke: ActionInvoke, text: String) -> Event {
    Event::ActionResult(ActionResult {
        invocation_id: invoke.invocation_id,
        action_id: invoke.action_id,
        output: ActionOutput::Text { text },
    })
}

fn action_error(invoke: ActionInvoke, message: String) -> Event {
    Event::ActionError(ActionError {
        invocation_id: invoke.invocation_id,
        action_id: invoke.action_id,
        message,
        details: None,
    })
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
    rows: &[String],
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

fn line_rows_output(rows: &[String]) -> String {
    if rows.is_empty() {
        "(no matches found)".to_owned()
    } else {
        rows.join("\n")
    }
}

fn error_envelope(command: Option<&str>, code: &str, message: &str) -> CborValue {
    cbor_map(vec![
        ("ok", CborValue::Bool(false)),
        (
            "command",
            command
                .map(|command| CborValue::Text(command.to_owned()))
                .unwrap_or(CborValue::Null),
        ),
        (
            "error",
            cbor_map(vec![
                ("code", CborValue::Text(code.to_owned())),
                ("message", CborValue::Text(safe_display_line(message))),
                ("details", cbor_map(Vec::new())),
            ]),
        ),
    ])
}

fn calendar_error_envelope(command: Option<&str>, message: &str) -> CborValue {
    error_envelope(command, calendar_error_code(message), message)
}

fn calendar_error_code(message: &str) -> &'static str {
    if (message.starts_with("Google calendar account") && message.contains("not authorized"))
        || (message.starts_with("refreshing Google access token")
            && (message.contains("invalid_grant")
                || message.contains("invalid_client")
                || message.contains("unauthorized_client")))
    {
        "auth_error"
    } else if message.starts_with("Google Calendar API")
        || message.starts_with("Google token response")
        || message.starts_with("Google create event response")
        || message.starts_with("Google update event response")
        || message.starts_with("Google invite response")
        || message.starts_with("refreshing Google access token")
        || message.starts_with("fetching iCalendar feed")
        || message.starts_with("reading iCalendar feed")
        || message.starts_with("iCalendar feed returned HTTP")
    {
        "network_error"
    } else if message.starts_with("Google calendar secret")
        || message.contains("missing url_secret")
        || message.contains("has not been configured")
    {
        "configuration_error"
    } else if message.starts_with("serializing Google Calendar request") {
        "internal_error"
    } else {
        "invalid_input"
    }
}

fn calendar_error_message_for_tool(tool_name: &str, details: &CborValue) -> String {
    let message = cbor_nested_text_field(details, "error", "message")
        .unwrap_or("invalid calendar tool request");
    let Some(code) = cbor_nested_text_field(details, "error", "code") else {
        return message.to_owned();
    };
    if command_for_tool_name(tool_name).is_some() {
        return format!(
            "{} failed ({code}): {message}",
            safe_display_line(tool_name)
        );
    }
    match cbor_text_field(details, "command") {
        Some(command) => format!(
            "calendar {} failed ({code}): {message}",
            safe_display_line(command)
        ),
        None => format!("calendar failed ({code}): {message}"),
    }
}

#[cfg(test)]
fn calendar_error_message(details: &CborValue) -> String {
    calendar_error_message_for_tool(super::TOOL_NAME, details)
}

fn calendar_log_status(result: &CborValue) -> String {
    cbor_text_field(result, "status")
        .or_else(|| cbor_nested_text_field(result, "error", "code"))
        .unwrap_or("unknown")
        .to_owned()
}

fn calendar_log_error_message(result: &CborValue) -> Option<&str> {
    if cbor_bool_field(result, "ok") == Some(false) {
        cbor_nested_text_field(result, "error", "message")
    } else {
        None
    }
}

fn success_display_for_tool(tool_name: &str, result: &CborValue) -> ToolUseState {
    let command = cbor_text_field(result, "command").unwrap_or("calendar");
    if command_for_tool_name(tool_name).is_some() {
        let data = cbor_field(result, "data");
        return ToolUseState {
            args: split_calendar_display_args(command, data).unwrap_or_default(),
            range: calendar_display_range(command, data),
            stats: calendar_display_stats(command, data),
            info_chips: calendar_display_info(command, data),
            status: ToolUseStatus::Success,
            status_text: success_status_text(result).to_owned(),
            ..Default::default()
        };
    }
    success_display(result)
}

fn success_display(result: &CborValue) -> ToolUseState {
    let command = cbor_text_field(result, "command").unwrap_or("calendar");
    let data = cbor_field(result, "data");
    ToolUseState {
        args: calendar_display_args(command, data).unwrap_or_default(),
        range: calendar_display_range(command, data),
        stats: calendar_display_stats(command, data),
        info_chips: calendar_display_info(command, data),
        status: ToolUseStatus::Success,
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
    if let Some(command) = command_for_tool_name(tool_name) {
        let args = cbor_field(arguments, "args");
        return ToolUseState {
            args: split_invocation_display_args(command, args).unwrap_or_default(),
            range: calendar_range_display(command, args),
            status: ToolUseStatus::Error,
            status_text: message.to_owned(),
            ..Default::default()
        };
    }
    error_display(arguments, details, message)
}

fn error_display(arguments: &CborValue, details: &CborValue, message: &str) -> ToolUseState {
    let command = cbor_text_field(details, "command").unwrap_or("calendar");
    ToolUseState {
        args: invocation_display_args(arguments).unwrap_or_else(|| safe_display_line(command)),
        range: invocation_display_range(arguments),
        status: ToolUseStatus::Error,
        status_text: message.to_owned(),
        ..Default::default()
    }
}

fn calendar_display_args(command: &str, data: Option<&CborValue>) -> Option<String> {
    Some(calendar_args_text(command, data))
}

fn calendar_display_stats(command: &str, data: Option<&CborValue>) -> ToolUseStats {
    let Some(data) = data else {
        return ToolUseStats::default();
    };
    match command {
        "list_calendars" => line_array_count_stats(data, "calendars"),
        "list_events" => line_array_count_stats(data, "events"),
        "read_event" => line_array_count_stats(data, "event"),
        "free_busy" => line_array_count_stats(data, "busy"),
        _ => ToolUseStats::default(),
    }
}

fn calendar_display_info(_command: &str, _data: Option<&CborValue>) -> Vec<String> {
    Vec::new()
}

fn calendar_display_range(command: &str, data: Option<&CborValue>) -> Option<ToolUseRange> {
    calendar_range_display(command, data)
}

pub(crate) fn initial_display(arguments: &CborValue) -> ToolUseState {
    ToolUseState {
        args: invocation_display_args(arguments).unwrap_or_default(),
        range: invocation_display_range(arguments),
        status: ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
        ..Default::default()
    }
}

pub(crate) fn initial_display_for_tool(tool_name: &str, arguments: &CborValue) -> ToolUseState {
    if let Some(command) = command_for_tool_name(tool_name) {
        return ToolUseState {
            args: split_invocation_display_args(command, Some(arguments)).unwrap_or_default(),
            range: calendar_range_display(command, Some(arguments)),
            status: ToolUseStatus::InProgress,
            status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
            ..Default::default()
        };
    }
    initial_display(arguments)
}

pub(crate) fn initial_progress(invoke: &ToolStarted) -> ToolProgress {
    ToolProgress {
        call_id: invoke.call_id.clone(),
        tool_name: invoke.tool_name.clone(),
        message: None,
        progress: None,
        display: Some(initial_display_for_tool(
            invoke.tool_name.as_str(),
            &invoke.arguments,
        )),
    }
}

fn invocation_display_args(arguments: &CborValue) -> Option<String> {
    let command = cbor_text_field(arguments, "command")?;
    let args = cbor_field(arguments, "args");
    Some(calendar_args_text(command, args))
}

fn invocation_display_range(arguments: &CborValue) -> Option<ToolUseRange> {
    let command = cbor_text_field(arguments, "command")?;
    let args = cbor_field(arguments, "args");
    calendar_range_display(command, args)
}
fn calendar_args_text(command: &str, args: Option<&CborValue>) -> String {
    let mut text = safe_display_line(command);
    append_calendar_args_text(&mut text, command, args, false);
    text
}

fn split_invocation_display_args(command: &str, args: Option<&CborValue>) -> Option<String> {
    Some(split_calendar_args_text(command, args))
}

fn split_calendar_display_args(command: &str, data: Option<&CborValue>) -> Option<String> {
    Some(split_calendar_args_text(command, data))
}

fn split_calendar_args_text(command: &str, args: Option<&CborValue>) -> String {
    let mut text = String::new();
    append_calendar_args_text(&mut text, command, args, true);
    text
}

fn append_calendar_args_text(
    text: &mut String,
    command: &str,
    args: Option<&CborValue>,
    split: bool,
) {
    if let Some(scope) = calendar_scope_text(args) {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&scope);
    }
    if matches!(
        command,
        "read_event" | "update_event" | "delete_event" | "respond_invite"
    ) && let Some(event_id) = args.and_then(|args| cbor_text_field(args, "event_id"))
    {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(if split { "event_id=" } else { "event=" });
        text.push_str(&safe_display_line(event_id));
    }
    if command == "list_events"
        && let Some(title_filter) = args
            .and_then(|args| cbor_text_field(args, "title_filter"))
            .or_else(|| args.and_then(|args| cbor_text_field(args, "title")))
    {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str("title=");
        text.push_str(&safe_field(title_filter));
    }
}
fn calendar_scope_text(args: Option<&CborValue>) -> Option<String> {
    args.and_then(|args| cbor_text_field(args, "calendar"))
        .map(safe_display_line)
}

fn calendar_range_display(command: &str, args: Option<&CborValue>) -> Option<ToolUseRange> {
    if !matches!(
        command,
        "list_events" | "free_busy" | "create_event" | "update_event"
    ) {
        return None;
    }
    let args = args?;
    let range = ToolUseRange {
        start: cbor_text_field(args, "start").map(display_date_bound),
        end: cbor_text_field(args, "end").map(display_date_bound),
    };
    (!range.is_empty()).then_some(range)
}

fn display_date_bound(value: &str) -> String {
    let value = safe_display_line(value);
    if !looks_like_iso_date_prefix(&value) {
        return value;
    }
    if is_date_only_or_midnight_bound(&value) {
        return value[..10].to_owned();
    }
    if !matches!(value.as_bytes().get(10), Some(b'T' | b' ')) {
        return value;
    }
    if !looks_like_iso_minute_prefix(&value) {
        return value;
    }
    value.get(..16).map(ToOwned::to_owned).unwrap_or(value)
}

fn is_date_only_or_midnight_bound(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        || (19 <= bytes.len() && matches!(bytes[10], b'T' | b' ') && &bytes[11..19] == b"00:00:00")
}

fn looks_like_iso_minute_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    16 <= bytes.len()
        && matches!(bytes[10], b'T' | b' ')
        && bytes[11].is_ascii_digit()
        && bytes[12].is_ascii_digit()
        && bytes[13] == b':'
        && bytes[14].is_ascii_digit()
        && bytes[15].is_ascii_digit()
}

fn looks_like_iso_date_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    10 <= bytes.len()
        && bytes[0].is_ascii_digit()
        && bytes[1].is_ascii_digit()
        && bytes[2].is_ascii_digit()
        && bytes[3].is_ascii_digit()
        && bytes[4] == b'-'
        && bytes[5].is_ascii_digit()
        && bytes[6].is_ascii_digit()
        && bytes[7] == b'-'
        && bytes[8].is_ascii_digit()
        && bytes[9].is_ascii_digit()
}

fn line_array(rows: Vec<String>) -> CborValue {
    CborValue::Array(rows.into_iter().map(CborValue::Text).collect())
}

fn optional_text(value: Option<String>) -> CborValue {
    value.map(CborValue::Text).unwrap_or(CborValue::Null)
}

fn line_array_count_stats(data: &CborValue, field: &str) -> ToolUseStats {
    let Some(lines) = cbor_array_field(data, field) else {
        return ToolUseStats::default();
    };
    let line_count = lines.len() as u64;
    ToolUseStats {
        matches: Some(line_count),
        lines: None,
        bytes: None,
    }
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

fn cbor_nonblank_text_field<'a>(value: &'a CborValue, field: &str) -> Option<&'a str> {
    cbor_text_field(value, field).filter(|value| !value.trim().is_empty())
}

fn cbor_bool_field(value: &CborValue, field: &str) -> Option<bool> {
    match cbor_field(value, field) {
        Some(CborValue::Bool(value)) => Some(*value),
        _ => None,
    }
}

fn cbor_u32_field(value: &CborValue, field: &str) -> Option<u32> {
    match cbor_field(value, field) {
        Some(CborValue::Integer(value)) => u32::try_from(i128::from(*value)).ok(),
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
    let nested = cbor_field(value, outer)?;
    cbor_text_field(nested, inner)
}

fn safe_field(value: &str) -> String {
    let field = value
        .chars()
        .map(|c| {
            if c.is_control() || is_unsafe_format_control(c) {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("_");
    let field = truncate_chars(&field, MAX_DISPLAY_LINE_CHARS);
    if field.is_empty() {
        "-".to_owned()
    } else {
        field
    }
}

fn quoted_display_field(value: &str) -> String {
    format!("\"{}\"", safe_field(value).replace('"', "'"))
}

fn safe_multiline(value: &str) -> String {
    let collapsed = value
        .chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c.is_control() || is_unsafe_format_control(c) {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let collapsed = truncate_chars(&collapsed, MAX_EVENT_FIELD_CHARS);
    if collapsed.is_empty() {
        "-".to_owned()
    } else {
        collapsed
    }
}

fn safe_display_line(value: &str) -> String {
    safe_log_value(value, MAX_DISPLAY_LINE_CHARS)
}

fn safe_display_text(value: &str) -> String {
    let mut out = String::new();
    for (index, ch) in value.chars().enumerate() {
        if MAX_EVENT_FIELD_CHARS < index + 1 {
            out.push('…');
            break;
        }
        push_escaped_char(&mut out, ch, true);
    }
    out
}

fn safe_log_value(value: &str, max_chars: usize) -> String {
    let collapsed = value
        .chars()
        .map(|c| {
            if c.is_control() || is_unsafe_format_control(c) {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    truncate_chars(&collapsed, max_chars)
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

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (index, c) in value.chars().enumerate() {
        if max_chars < index + 1 {
            out.push_str("...");
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests;
