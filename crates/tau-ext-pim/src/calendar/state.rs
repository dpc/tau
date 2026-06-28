use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::storage::{SharedStorage, StorageCreateError, file_name};

const LOG_SCHEMA: u32 = 1;
const CHANGE_SCHEMA: u32 = 1;
const CHANGE_KIND: &str = "calendar_change";
const CHANGE_APPROVAL_KIND: &str = "calendar-change";
const MAX_CHANGE_FIELD_CHARS: usize = 512;
const MAX_DESCRIPTION_BYTES: usize = 64 * 1024;
const MAX_DESCRIPTION_LINES: usize = 1000;
const MAX_ATTENDEES_HARD: usize = 200;
const GOOGLE_AUTH_SCHEMA: u32 = 1;
const GOOGLE_AUTH_PENDING_SCHEMA: u32 = 1;
const MAX_GOOGLE_AUTH_FIELD_CHARS: usize = 4096;

/// One persisted calendar audit log entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct CalendarLogEntry {
    /// Log schema version.
    pub(crate) schema: u32,
    /// Wall-clock timestamp in milliseconds since the Unix epoch.
    pub(crate) ts_unix_ms: u64,
    /// Log entry kind, currently `tool`.
    pub(crate) kind: String,
    /// Calendar tool command name.
    pub(crate) command: String,
    /// Command outcome status.
    pub(crate) status: String,
    /// Configured account id when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) account: Option<String>,
    /// Calendar id when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) calendar: Option<String>,
    /// Event id when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) event_id: Option<String>,
    /// Inclusive lower time bound from the tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) start: Option<String>,
    /// Exclusive upper time bound from the tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) end: Option<String>,
    /// Requested row limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) limit: Option<u32>,
    /// Number of rows or records returned when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) item_count: Option<u64>,
    /// Sanitized error reason for failed commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

impl CalendarLogEntry {
    /// Build a schema-v1 tool audit entry.
    pub(crate) fn tool(command: &str, status: &str) -> Self {
        Self {
            schema: LOG_SCHEMA,
            ts_unix_ms: current_unix_millis(),
            kind: "tool".to_owned(),
            command: command.to_owned(),
            status: status.to_owned(),
            account: None,
            calendar: None,
            event_id: None,
            start: None,
            end: None,
            limit: None,
            item_count: None,
            reason: None,
        }
    }
}

/// One persisted calendar mutation approval.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct CalendarChangeApproval {
    /// Approval schema version.
    pub(crate) schema: u32,
    /// Opaque stable approval ID.
    pub(crate) id: String,
    /// Approval kind.
    pub(crate) kind: String,
    /// Approval status: pending, sending, approved, or denied.
    pub(crate) status: String,
    /// Calendar tool command name.
    pub(crate) command: String,
    /// Configured account id.
    pub(crate) account: String,
    /// Calendar id.
    pub(crate) calendar: String,
    /// Target event id for update, delete, and invite response commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) event_id: Option<String>,
    /// Provider ETag used for stale-write protection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) etag: Option<String>,
    /// Event title for create/update commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    /// Event description for create/update commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    /// Event location for create/update commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) location: Option<String>,
    /// Event start as RFC3339 date-time or all-day date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) start: Option<String>,
    /// Event end as RFC3339 date-time or all-day exclusive date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) end: Option<String>,
    /// IANA timezone for date-time values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) timezone: Option<String>,
    /// Attendee email addresses. `None` means leave existing attendees
    /// unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) attendees: Option<Vec<String>>,
    /// Invitation response for respond_invite commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) response: Option<String>,
    /// Reason this change requires approval.
    pub(crate) reason: String,
    /// Provider event id after a successful approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) result_event_id: Option<String>,
}

impl CalendarChangeApproval {
    /// Build a pending schema-v1 calendar change approval.
    pub(crate) fn pending(command: &str, account: &str, calendar: &str) -> Self {
        Self {
            schema: CHANGE_SCHEMA,
            id: String::new(),
            kind: CHANGE_KIND.to_owned(),
            status: "pending".to_owned(),
            command: command.to_owned(),
            account: account.to_owned(),
            calendar: calendar.to_owned(),
            event_id: None,
            etag: None,
            title: None,
            description: None,
            location: None,
            start: None,
            end: None,
            timezone: None,
            attendees: None,
            response: None,
            reason: "user_approval_required".to_owned(),
            result_event_id: None,
        }
    }
}

/// Stored OAuth authorization for one Google calendar account.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct GoogleStoredAuth {
    /// Stored auth schema version.
    pub(crate) schema: u32,
    /// Configured account id.
    pub(crate) account: String,
    /// Google OAuth refresh token.
    pub(crate) refresh_token: String,
}

impl GoogleStoredAuth {
    /// Build a stored OAuth auth record.
    pub(crate) fn new(account: &str, refresh_token: &str) -> Self {
        Self {
            schema: GOOGLE_AUTH_SCHEMA,
            account: account.to_owned(),
            refresh_token: refresh_token.to_owned(),
        }
    }
}

/// Pending Google device authorization request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct GooglePendingAuth {
    /// Pending auth schema version.
    pub(crate) schema: u32,
    /// Configured account id.
    pub(crate) account: String,
    /// Google OAuth device code. This is never shown back to the user.
    pub(crate) device_code: String,
    /// User-facing code entered on Google's verification page.
    pub(crate) user_code: String,
    /// Google verification URL for the user to open.
    pub(crate) verification_uri: String,
    /// Unix timestamp in milliseconds when this request expires.
    pub(crate) expires_at_unix_ms: u64,
    /// Suggested polling interval from Google.
    pub(crate) interval_secs: u64,
}

impl GooglePendingAuth {
    /// Build a pending Google device authorization record.
    pub(crate) fn new(
        account: &str,
        device_code: &str,
        user_code: &str,
        verification_uri: &str,
        expires_in_secs: u64,
        interval_secs: u64,
    ) -> Self {
        Self {
            schema: GOOGLE_AUTH_PENDING_SCHEMA,
            account: account.to_owned(),
            device_code: device_code.to_owned(),
            user_code: user_code.to_owned(),
            verification_uri: verification_uri.to_owned(),
            expires_at_unix_ms: current_unix_millis()
                .saturating_add(expires_in_secs.saturating_mul(1000)),
            interval_secs,
        }
    }

    /// Return true when this pending authorization has expired.
    pub(crate) fn expired(&self) -> bool {
        self.expires_at_unix_ms <= current_unix_millis()
    }
}

/// Calendar module persistent state under the extension storage root.
pub(crate) struct StateStore {
    storage: SharedStorage,
}

impl StateStore {
    /// Open the calendar state area.
    #[cfg(test)]
    pub(crate) fn open(state_dir: PathBuf) -> Result<Self, String> {
        Self::open_with_storage(
            state_dir.clone(),
            std::rc::Rc::new(crate::storage::FsStorage::new(state_dir)),
        )
    }

    /// Open the calendar state area using a supplied storage backend.
    pub(crate) fn open_with_storage(
        state_dir: PathBuf,
        storage: SharedStorage,
    ) -> Result<Self, String> {
        let _ = state_dir;
        Ok(Self { storage })
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

    /// Append one calendar audit log entry.
    pub(crate) fn append_calendar_log(&self, entry: &CalendarLogEntry) -> Result<(), String> {
        let path = self.calendar_log_path();
        let mut bytes = serde_json::to_vec(entry).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        self.storage.append_file(&path, bytes)
    }

    /// Load recent calendar audit log entries in chronological order.
    pub(crate) fn recent_calendar_log(
        &self,
        limit: usize,
    ) -> Result<Vec<CalendarLogEntry>, String> {
        let path = self.calendar_log_path();
        let Some(bytes) = self.read_file(&path)? else {
            return Ok(Vec::new());
        };
        let mut entries = VecDeque::new();
        for line in BufReader::new(bytes.as_slice()).lines() {
            let line = line.map_err(|error| format!("failed to read {path}: {error}"))?;
            if line.trim().is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<CalendarLogEntry>(&line) else {
                continue;
            };
            if entry.schema != LOG_SCHEMA {
                continue;
            }
            entries.push_back(entry);
            while limit < entries.len() {
                entries.pop_front();
            }
        }
        Ok(entries.into_iter().collect())
    }

    /// Store a Google OAuth refresh token for one account.
    pub(crate) fn save_google_refresh_token(
        &self,
        account: &str,
        refresh_token: &str,
    ) -> Result<(), String> {
        let auth = GoogleStoredAuth::new(account, refresh_token);
        validate_google_stored_auth(&auth, Some(account))?;
        self.write_json(&self.google_auth_path(account), &auth)
    }

    /// Load a stored Google OAuth refresh token for one account.
    pub(crate) fn google_refresh_token(&self, account: &str) -> Result<Option<String>, String> {
        let path = self.google_auth_path(account);
        let Some(bytes) = self.read_file(&path)? else {
            return Ok(None);
        };
        let auth: GoogleStoredAuth = serde_json::from_slice(&bytes)
            .map_err(|error| format!("failed to parse {path}: {error}"))?;
        validate_google_stored_auth(&auth, Some(account))?;
        Ok(Some(auth.refresh_token))
    }

    /// Store a pending Google device authorization request.
    pub(crate) fn save_pending_google_auth(
        &self,
        pending: &GooglePendingAuth,
    ) -> Result<(), String> {
        validate_google_pending_auth(pending, Some(&pending.account))?;
        self.write_json(&self.google_pending_auth_path(&pending.account), pending)
    }

    /// Load a pending Google device authorization request.
    pub(crate) fn pending_google_auth(&self, account: &str) -> Result<GooglePendingAuth, String> {
        let path = self.google_pending_auth_path(account);
        let Some(bytes) = self.read_file(&path)? else {
            return Err(format!(
                "no pending Google authorization for account `{account}`"
            ));
        };
        let pending: GooglePendingAuth = serde_json::from_slice(&bytes)
            .map_err(|error| format!("failed to parse {path}: {error}"))?;
        validate_google_pending_auth(&pending, Some(account))?;
        Ok(pending)
    }

    /// Clear a pending Google device authorization request.
    pub(crate) fn clear_pending_google_auth(&self, account: &str) -> Result<(), String> {
        self.storage
            .delete_file(&self.google_pending_auth_path(account))
    }

    /// Load pending calendar changes in deterministic order.
    pub(crate) fn list_pending_changes(&self) -> Result<Vec<CalendarChangeApproval>, String> {
        self.list_change_approvals("pending")
    }

    /// Load one pending calendar change by id.
    pub(crate) fn pending_change_by_id(&self, id: &str) -> Result<CalendarChangeApproval, String> {
        self.load_change_approval("pending", id)
    }

    /// Load one approved calendar change by id.
    pub(crate) fn approved_change_by_id(&self, id: &str) -> Result<CalendarChangeApproval, String> {
        self.load_change_approval("approved", id)
    }

    /// Load one denied calendar change by id.
    pub(crate) fn denied_change_by_id(&self, id: &str) -> Result<CalendarChangeApproval, String> {
        self.load_change_approval("denied", id)
    }

    /// Return an existing pending calendar change approval or create it.
    pub(crate) fn pending_change(
        &self,
        request: &CalendarChangeApproval,
    ) -> Result<String, String> {
        let mut validated_request = request.clone();
        if validated_request.id.is_empty() {
            validated_request.id = "1".to_owned();
        }
        validate_calendar_change_approval(&validated_request, "pending", None)?;
        loop {
            for approval in self.list_pending_changes()? {
                if calendar_change_matches(&approval, request) {
                    return Ok(approval.id);
                }
            }
            let mut request = request.clone();
            request.id = self.next_change_id()?;
            let path = self.change_path("pending", &request.id)?;
            match self.create_json(&path, &request) {
                Ok(()) => return Ok(request.id),
                Err(CreateNewJsonError::AlreadyExists) => continue,
                Err(CreateNewJsonError::Other(message)) => return Err(message),
            }
        }
    }

    /// Return true if a pending calendar change exists.
    pub(crate) fn change_pending_exists(&self, id: &str) -> Result<bool, String> {
        self.file_exists(&self.change_path("pending", id)?)
    }

    /// Return true if a sending calendar change exists.
    pub(crate) fn change_sending_exists(&self, id: &str) -> Result<bool, String> {
        self.file_exists(&self.change_path("sending", id)?)
    }

    /// Return true if a denied calendar change tombstone exists.
    pub(crate) fn change_denied_exists(&self, id: &str) -> Result<bool, String> {
        self.file_exists(&self.change_path("denied", id)?)
    }

    /// Claim one pending calendar change for execution.
    pub(crate) fn claim_change(&self, id: &str) -> Result<CalendarChangeApproval, String> {
        if self.change_denied_exists(id)? {
            return Err(format!("calendar change `{id}` was denied"));
        }
        let approval = self.pending_change_by_id(id)?;
        let mut sending = approval.clone();
        sending.status = "sending".to_owned();
        let sending_path = self.change_path("sending", id)?;
        match self.create_json(&sending_path, &sending) {
            Ok(()) => {}
            Err(CreateNewJsonError::AlreadyExists) => {
                return Err(format!("calendar change `{id}` is already being applied"));
            }
            Err(CreateNewJsonError::Other(message)) => return Err(message),
        }
        let pending_path = self.change_path("pending", id)?;
        if let Err(error) = self.storage.delete_file(&pending_path) {
            let _ = self.storage.delete_file(&sending_path);
            return Err(error);
        }
        Ok(approval)
    }

    /// Restore a claimed calendar change to pending after execution failed.
    pub(crate) fn release_claimed_change(&self, id: &str) -> Result<(), String> {
        let mut approval = self.load_change_approval("sending", id)?;
        if self.change_denied_exists(id)? {
            return self.storage.delete_file(&self.change_path("sending", id)?);
        }
        approval.status = "pending".to_owned();
        let pending_path = self.change_path("pending", id)?;
        match self.create_json(&pending_path, &approval) {
            Ok(()) | Err(CreateNewJsonError::AlreadyExists) => {}
            Err(CreateNewJsonError::Other(message)) => return Err(message),
        }
        self.storage.delete_file(&self.change_path("sending", id)?)
    }

    /// Mark a claimed calendar change as approved after successful execution.
    pub(crate) fn complete_change(
        &self,
        id: &str,
        result_event_id: Option<&str>,
    ) -> Result<(), String> {
        let mut approval = self.load_change_approval("sending", id)?;
        approval.status = "approved".to_owned();
        approval.result_event_id = result_event_id.map(safe_persisted_line);
        let approved_path = self.change_path("approved", id)?;
        match self.create_json(&approved_path, &approval) {
            Ok(()) | Err(CreateNewJsonError::AlreadyExists) => {}
            Err(CreateNewJsonError::Other(message)) => return Err(message),
        }
        self.storage.delete_file(&self.change_path("sending", id)?)
    }

    /// Deny a pending calendar change.
    pub(crate) fn deny_change(&self, id: &str) -> Result<(), String> {
        if self.change_denied_exists(id)? {
            let _ = self.storage.delete_file(&self.change_path("pending", id)?);
            return Ok(());
        }
        self.move_pending_change(id, "denied")
    }

    fn list_change_approvals(&self, status: &str) -> Result<Vec<CalendarChangeApproval>, String> {
        let dir = self.change_dir(status);
        self.storage
            .list_files(&dir)?
            .into_iter()
            .filter(|entry| !entry.is_dir && entry.path.ends_with(".json"))
            .map(|entry| {
                let bytes = self
                    .read_file(&entry.path)?
                    .ok_or_else(|| format!("calendar change file `{}` not found", entry.path))?;
                let approval: CalendarChangeApproval = serde_json::from_slice(&bytes)
                    .map_err(|error| format!("failed to parse {}: {error}", entry.path))?;
                validate_calendar_change_approval(&approval, status, None)?;
                Ok(approval)
            })
            .collect()
    }

    fn load_change_approval(
        &self,
        status: &str,
        id: &str,
    ) -> Result<CalendarChangeApproval, String> {
        let path = self.change_path(status, id)?;
        let Some(bytes) = self.read_file(&path)? else {
            return Err(format!("calendar change `{id}` not found"));
        };
        let approval: CalendarChangeApproval = serde_json::from_slice(&bytes)
            .map_err(|error| format!("failed to parse {path}: {error}"))?;
        validate_calendar_change_approval(&approval, status, Some(id))?;
        Ok(approval)
    }

    fn move_pending_change(&self, id: &str, new_status: &str) -> Result<(), String> {
        validate_change_id(id)?;
        let from = self.change_path("pending", id)?;
        let to = self.change_path(new_status, id)?;
        if let Some(bytes) = self.read_file(&from)? {
            let mut record: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|error| format!("failed to parse {from}: {error}"))?;
            validate_change_record(&record, "pending", id)?;
            record["status"] = serde_json::Value::String(new_status.to_owned());
            match self.create_json(&to, &record) {
                Ok(()) | Err(CreateNewJsonError::AlreadyExists) => {}
                Err(CreateNewJsonError::Other(message)) => return Err(message),
            }
            self.storage.delete_file(&from)
        } else if self.file_exists(&to)? {
            Ok(())
        } else {
            Err(format!("calendar change `{id}` not found"))
        }
    }

    fn calendar_log_path(&self) -> String {
        "logs/calendar.jsonl".to_owned()
    }

    fn google_auth_path(&self, account: &str) -> String {
        format!("auth/google/{}.json", account_file_stem(account))
    }

    fn google_pending_auth_path(&self, account: &str) -> String {
        format!("auth/google-pending/{}.json", account_file_stem(account))
    }

    fn change_dir(&self, status: &str) -> String {
        format!("approvals/{CHANGE_APPROVAL_KIND}/{status}")
    }

    fn change_path(&self, status: &str, id: &str) -> Result<String, String> {
        validate_change_id(id)?;
        Ok(format!("{}/{id}.json", self.change_dir(status)))
    }

    fn next_change_id(&self) -> Result<String, String> {
        let mut max_id = 0_u64;
        for status in ["pending", "sending", "approved", "denied"] {
            let dir = self.change_dir(status);
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

fn validate_calendar_change_approval(
    approval: &CalendarChangeApproval,
    expected_status: &str,
    expected_id: Option<&str>,
) -> Result<(), String> {
    validate_change_approval_identity(approval, expected_status, expected_id)?;
    validate_change_approval_metadata(approval)?;
    validate_change_approval_fields(approval)?;
    validate_change_approval_attendees(approval)?;
    Ok(())
}

fn validate_change_approval_identity(
    approval: &CalendarChangeApproval,
    expected_status: &str,
    expected_id: Option<&str>,
) -> Result<(), String> {
    if approval.schema != CHANGE_SCHEMA {
        return Err(format!(
            "calendar change `{}` has unsupported schema",
            approval.id
        ));
    }
    validate_change_id(&approval.id)?;
    if let Some(expected_id) = expected_id
        && approval.id != expected_id
    {
        return Err(format!(
            "calendar change `{expected_id}` has mismatched embedded id"
        ));
    }
    if approval.kind != CHANGE_KIND {
        return Err(format!(
            "calendar change `{}` has mismatched embedded kind",
            approval.id
        ));
    }
    if approval.status != expected_status {
        return Err(format!(
            "calendar change `{}` has mismatched embedded status",
            approval.id
        ));
    }
    Ok(())
}

fn validate_change_approval_metadata(approval: &CalendarChangeApproval) -> Result<(), String> {
    if !is_allowed_calendar_change_command(&approval.command) {
        return Err(format!(
            "calendar change `{}` contains unsafe metadata",
            approval.id
        ));
    }

    for value in [
        &approval.command,
        &approval.account,
        &approval.calendar,
        &approval.reason,
    ] {
        if !is_safe_persisted_line(value, MAX_CHANGE_FIELD_CHARS) {
            return Err(format!(
                "calendar change `{}` contains unsafe metadata",
                approval.id
            ));
        }
    }
    Ok(())
}

fn is_allowed_calendar_change_command(command: &str) -> bool {
    matches!(
        command,
        "create_event" | "update_event" | "delete_event" | "respond_invite"
    )
}

fn validate_change_approval_fields(approval: &CalendarChangeApproval) -> Result<(), String> {
    for (field, value) in [
        ("event_id", approval.event_id.as_ref()),
        ("etag", approval.etag.as_ref()),
        ("title", approval.title.as_ref()),
        ("location", approval.location.as_ref()),
        ("start", approval.start.as_ref()),
        ("end", approval.end.as_ref()),
        ("timezone", approval.timezone.as_ref()),
        ("response", approval.response.as_ref()),
        ("result_event_id", approval.result_event_id.as_ref()),
    ] {
        validate_optional_line(value, field)?;
    }
    validate_optional_multiline(approval.description.as_ref(), "description")?;
    Ok(())
}

fn validate_change_approval_attendees(approval: &CalendarChangeApproval) -> Result<(), String> {
    if let Some(attendees) = &approval.attendees {
        if MAX_ATTENDEES_HARD < attendees.len() {
            return Err("calendar change contains too many attendees".to_owned());
        }
        for attendee in attendees {
            if !is_safe_persisted_line(attendee, MAX_CHANGE_FIELD_CHARS) {
                return Err("calendar change contains an unsafe attendee".to_owned());
            }
        }
    }
    Ok(())
}

fn validate_optional_line(value: Option<&String>, field: &str) -> Result<(), String> {
    if let Some(value) = value
        && !is_safe_persisted_line(value, MAX_CHANGE_FIELD_CHARS)
    {
        return Err(format!(
            "calendar change field `{field}` contains unsafe text"
        ));
    }
    Ok(())
}

fn validate_optional_multiline(value: Option<&String>, field: &str) -> Result<(), String> {
    if let Some(value) = value
        && !is_safe_persisted_multiline(value)
    {
        return Err(format!(
            "calendar change field `{field}` contains unsafe text"
        ));
    }
    Ok(())
}

fn calendar_change_matches(left: &CalendarChangeApproval, right: &CalendarChangeApproval) -> bool {
    left.command == right.command
        && left.account == right.account
        && left.calendar == right.calendar
        && left.event_id == right.event_id
        && left.etag == right.etag
        && left.title == right.title
        && left.description == right.description
        && left.location == right.location
        && left.start == right.start
        && left.end == right.end
        && left.timezone == right.timezone
        && left.attendees == right.attendees
        && left.response == right.response
}

fn validate_change_record(
    record: &serde_json::Value,
    expected_status: &str,
    id: &str,
) -> Result<(), String> {
    if record.get("schema").and_then(serde_json::Value::as_u64) != Some(u64::from(CHANGE_SCHEMA)) {
        return Err(format!("calendar change `{id}` has unsupported schema"));
    }
    let field = |name: &str| record.get(name).and_then(serde_json::Value::as_str);
    if field("id") != Some(id) {
        return Err(format!("calendar change `{id}` has mismatched embedded id"));
    }
    if field("kind") != Some(CHANGE_KIND) {
        return Err(format!(
            "calendar change `{id}` has mismatched embedded kind"
        ));
    }
    if field("status") != Some(expected_status) {
        return Err(format!(
            "calendar change `{id}` has mismatched embedded status"
        ));
    }
    Ok(())
}

fn validate_google_stored_auth(
    auth: &GoogleStoredAuth,
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
    pending: &GooglePendingAuth,
    expected_account: Option<&str>,
) -> Result<(), String> {
    if pending.schema != GOOGLE_AUTH_PENDING_SCHEMA {
        return Err("pending Google auth has unsupported schema".to_owned());
    }
    validate_google_auth_account(&pending.account, expected_account)?;
    for (field, value) in [
        ("device_code", pending.device_code.as_str()),
        ("user_code", pending.user_code.as_str()),
        ("verification_uri", pending.verification_uri.as_str()),
    ] {
        if !is_safe_persisted_line(value, MAX_GOOGLE_AUTH_FIELD_CHARS) {
            return Err(format!(
                "pending Google auth field `{field}` contains unsafe text"
            ));
        }
    }
    if pending.expires_at_unix_ms == 0 || pending.interval_secs == 0 {
        return Err("pending Google auth timing is invalid".to_owned());
    }
    Ok(())
}

fn validate_google_auth_account(
    account: &str,
    expected_account: Option<&str>,
) -> Result<(), String> {
    if account.trim().is_empty() || !is_safe_persisted_line(account, MAX_CHANGE_FIELD_CHARS) {
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

fn validate_change_id(id: &str) -> Result<(), String> {
    let Ok(value) = id.parse::<u64>() else {
        return Err(format!("invalid calendar change id `{id}`"));
    };
    if value == 0 || id.contains(['/', '\\', '\0']) || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("invalid calendar change id `{id}`"));
    }
    Ok(())
}

fn safe_persisted_line(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_control() || is_unsafe_format_control(ch) {
                ' '
            } else {
                ch
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_safe_persisted_line(value: &str, max_chars: usize) -> bool {
    value.chars().count() <= max_chars
        && !value
            .chars()
            .any(|ch| ch.is_control() || is_unsafe_format_control(ch))
}

fn is_safe_persisted_multiline(value: &str) -> bool {
    value.len() <= MAX_DESCRIPTION_BYTES
        && value.lines().count() <= MAX_DESCRIPTION_LINES
        && !value.chars().any(|ch| {
            (ch.is_control() && ch != '\n' && ch != '\r' && ch != '\t')
                || is_unsafe_format_control(ch)
        })
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

#[cfg(test)]
mod tests;
