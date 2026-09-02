use std::collections::BTreeMap;
use std::io::Read;

use serde_json::{Map, Value, json};
use tau_proto::SecretValue;
use time::format_description::well_known::Rfc3339;
use url::form_urlencoded;

use super::config::{CalendarAccountId, ValidatedAccount, ValidatedBackendConfig};
use super::event_time::EventTimeRange;
use super::ics_feed::TimeRange;
use super::identity::{EventEtag, EventId, ICalUid, ProviderCalendarId};
use super::runtime::InviteResponse;
use crate::google_oauth::{
    GoogleDeviceAuthFinish, GoogleDeviceAuthStart, GoogleOauthClient, GoogleOauthSecretConfig,
    google_http_agent,
};
const GOOGLE_OAUTH_SCOPE: &str = "https://www.googleapis.com/auth/calendar";
const GOOGLE_CALENDAR_API_BASE: &str = "https://www.googleapis.com/calendar/v3";
const GOOGLE_SEND_UPDATES: &str = "all";
const MAX_ERROR_BODY_BYTES: usize = 4096;
pub(super) const MAX_JSON_BODY_BYTES: usize = 1024 * 1024;
const MAX_PAGE_TOKEN_CHARS: usize = 4096;
const GOOGLE_CURSOR_PREFIX: &str = "google:";
const OUTCOME_UNKNOWN_MESSAGE: &str =
    "Calendar change may have applied; do not retry. Reconcile manually with the provider.";

/// Read/write-capable Google Calendar API backend.
pub struct GoogleBackend {
    agent: ureq::Agent,
    oauth: GoogleOauthClient<CalendarAccountId>,
}

/// Failure phase for one side-effecting Google Calendar request.
pub(crate) enum GoogleWriteError {
    /// The backend proved that mutation dispatch did not begin.
    NotDispatched(String),
    /// Mutation dispatch began, but no complete trusted success result arrived.
    OutcomeUnknown,
}

/// Effective Calendar API base and its optional diagnostic-secret role.
struct GoogleApiBase<'a> {
    /// Base URL used to build Calendar API requests.
    effective: &'a str,
    /// Configured custom base that diagnostics must redact.
    custom: Option<&'a str>,
}

/// Exact request credentials and their bounded provider-error redaction rules.
struct ErrorBodyRedaction<'a> {
    /// Active bearer token bytes, when nonempty.
    token: Option<&'a [u8]>,
    /// Configured custom Calendar API base bytes, when nonempty.
    endpoint: Option<&'a [u8]>,
}

/// One Google calendar visible through the public backend API.
pub struct GoogleCalendar {
    /// Calendar id used in tool and API calls.
    pub id: String,
    /// Calendar display name.
    pub summary: String,
    /// Whether this is the authenticated user's primary calendar.
    pub primary: bool,
    /// Whether the calendar is read-only for this authenticated user.
    pub read_only: bool,
}

/// One Google calendar retained by the calendar runtime.
pub(super) struct GoogleCalendarRecord {
    /// Calendar id used in tool and API calls.
    pub(crate) id: ProviderCalendarId,
    /// Calendar display name.
    pub summary: String,
    /// Whether this is the authenticated user's primary calendar.
    pub primary: bool,
    /// Whether the calendar is read-only for this authenticated user.
    pub read_only: bool,
}

/// One Google Calendar event exposed through the public backend API.
pub struct GoogleEvent {
    /// Backend event id.
    pub id: String,
    /// Event ETag.
    pub etag: Option<String>,
    /// iCalendar UID, when Google exposes it.
    pub i_cal_uid: Option<String>,
    /// Event summary.
    pub summary: String,
    /// Event description.
    pub description: Option<String>,
    /// Event location.
    pub location: Option<String>,
    /// Event start date or date-time.
    pub start: String,
    /// Event end date or date-time.
    pub end: String,
    /// Event status.
    pub status: Option<String>,
    /// Event visibility, such as `private`.
    pub visibility: Option<String>,
    /// Event transparency, such as `transparent` for non-busy events.
    pub transparency: Option<String>,
    /// Organizer email or display name.
    pub organizer: Option<String>,
    /// Attendee emails.
    pub attendees: Vec<String>,
    /// Current authenticated attendee response, when Google marks an attendee
    /// as `self`.
    pub self_response_status: Option<String>,
    /// Whether the event is part of a recurring series.
    pub recurring: bool,
}

/// One Google Calendar event retained by the calendar runtime.
pub(super) struct GoogleEventRecord {
    /// Backend event id.
    pub(crate) id: EventId,
    /// Event ETag.
    pub(crate) etag: Option<EventEtag>,
    /// iCalendar UID, when Google exposes it.
    pub(crate) i_cal_uid: Option<ICalUid>,
    /// Event summary.
    pub summary: String,
    /// Event description.
    pub description: Option<String>,
    /// Event location.
    pub location: Option<String>,
    /// Event start date or date-time.
    pub start: String,
    /// Event end date or date-time.
    pub end: String,
    /// Event status.
    pub status: Option<String>,
    /// Event visibility, such as `private`.
    pub visibility: Option<String>,
    /// Event transparency, such as `transparent` for non-busy events.
    pub transparency: Option<String>,
    /// Organizer email or display name.
    pub organizer: Option<String>,
    /// Attendee emails.
    pub attendees: Vec<String>,
    /// Current authenticated attendee response, when Google marks an attendee
    /// as `self`.
    pub self_response_status: Option<String>,
    /// Whether the event is part of a recurring series.
    pub recurring: bool,
}

/// One page of Google Calendar events.
pub(super) struct GoogleEventRecordPage {
    /// Events in this page.
    pub events: Vec<GoogleEventRecord>,
    /// Cursor for the next page, when Google returns another page token.
    pub next_cursor: Option<String>,
}

impl From<GoogleCalendarRecord> for GoogleCalendar {
    fn from(calendar: GoogleCalendarRecord) -> Self {
        Self {
            id: calendar.id.into_string(),
            summary: calendar.summary,
            primary: calendar.primary,
            read_only: calendar.read_only,
        }
    }
}

impl From<GoogleEventRecord> for GoogleEvent {
    fn from(event: GoogleEventRecord) -> Self {
        Self {
            id: event.id.into_string(),
            etag: event.etag.map(EventEtag::into_string),
            i_cal_uid: event.i_cal_uid.map(ICalUid::into_string),
            summary: event.summary,
            description: event.description,
            location: event.location,
            start: event.start,
            end: event.end,
            status: event.status,
            visibility: event.visibility,
            transparency: event.transparency,
            organizer: event.organizer,
            attendees: event.attendees,
            self_response_status: event.self_response_status,
            recurring: event.recurring,
        }
    }
}

/// Parameters for one Google event-list page.
pub(crate) struct GoogleEventListQuery<'a> {
    /// Inclusive/exclusive time range to request.
    pub range: TimeRange,
    /// Maximum provider rows to request.
    pub limit: usize,
    /// Backend continuation token from a prior page.
    pub cursor: Option<&'a str>,
    /// Whether to request Google deleted-event records.
    pub include_cancelled: bool,
}

/// Event fields used by Google create/update requests.
#[derive(Default)]
pub struct GoogleEventWrite<'a> {
    /// Event title/summary.
    pub title: Option<&'a str>,
    /// Event description.
    pub description: Option<&'a str>,
    /// Event location.
    pub location: Option<&'a str>,
    /// Event start as RFC3339 date-time or all-day date.
    pub start: Option<&'a str>,
    /// Event end as RFC3339 date-time or all-day exclusive date.
    pub end: Option<&'a str>,
    /// IANA timezone for date-time values.
    pub timezone: Option<&'a str>,
    /// Clear the opposite Google time representation (`date` vs `dateTime`).
    /// Required by Google when PATCH converts all-day events to timed events
    /// or the reverse.
    pub clear_opposite_time_kind: bool,
    /// Attendee email addresses. `None` leaves attendees unchanged for updates.
    pub attendees: Option<&'a [String]>,
}

struct GoogleTimePair {
    start: Value,
    end: Value,
}

impl<'a> ErrorBodyRedaction<'a> {
    fn new(access_token: &'a str, custom_api_base: Option<&'a str>) -> Self {
        Self {
            token: (!access_token.is_empty()).then_some(access_token.as_bytes()),
            endpoint: custom_api_base
                .filter(|api_base| !api_base.is_empty())
                .map(str::as_bytes),
        }
    }

    fn read_and_format(&self, reader: impl Read) -> String {
        let mut bytes = Vec::new();
        let _ = reader
            .take(self.read_limit() as u64)
            .read_to_end(&mut bytes);
        self.format(&bytes)
    }

    fn read_limit(&self) -> usize {
        let longest_secret = self
            .token
            .into_iter()
            .chain(self.endpoint)
            .map(<[u8]>::len)
            .max()
            .unwrap_or(0);
        MAX_ERROR_BODY_BYTES.saturating_add(longest_secret.saturating_sub(1))
    }

    fn format(&self, bytes: &[u8]) -> String {
        let mut text = String::from_utf8_lossy(&self.redact_prefix(bytes)).into_owned();
        truncate_utf8_bytes(&mut text, MAX_ERROR_BODY_BYTES);
        sanitize_error_text(&text)
    }

    fn redact_prefix(&self, bytes: &[u8]) -> Vec<u8> {
        let source_limit = bytes.len().min(MAX_ERROR_BODY_BYTES);
        let mut redacted = Vec::with_capacity(source_limit);
        let mut index = 0;
        while index < source_limit {
            let Some((mut end, mut includes_token)) = self.match_end(bytes, index) else {
                redacted.push(bytes[index]);
                index += 1;
                continue;
            };
            let mut probe = index + 1;
            while probe < end.min(source_limit) {
                if let Some((next_end, next_includes_token)) = self.match_end(bytes, probe) {
                    end = end.max(next_end);
                    includes_token |= next_includes_token;
                }
                probe += 1;
            }
            redacted.extend_from_slice(if includes_token {
                b"<redacted>"
            } else {
                b"<redacted-endpoint>"
            });
            index = end;
        }
        redacted
    }

    fn match_end(&self, bytes: &[u8], index: usize) -> Option<(usize, bool)> {
        let token_end = self
            .token
            .filter(|secret| bytes[index..].starts_with(secret))
            .map(|secret| index + secret.len());
        let endpoint_end = self
            .endpoint
            .filter(|secret| bytes[index..].starts_with(secret))
            .map(|secret| index + secret.len());
        match (token_end, endpoint_end) {
            (Some(token_end), Some(endpoint_end)) => Some((token_end.max(endpoint_end), true)),
            (Some(token_end), None) => Some((token_end, true)),
            (None, Some(endpoint_end)) => Some((endpoint_end, false)),
            (None, None) => None,
        }
    }
}

impl GoogleBackend {
    /// Build a backend using the extension-authorized secret set.
    pub fn new(secrets: BTreeMap<String, SecretValue>) -> Self {
        Self {
            agent: google_http_agent(),
            oauth: GoogleOauthClient::new(secrets),
        }
    }

    /// Start Google device authorization for this account.
    pub fn start_device_auth(
        &self,
        account: &ValidatedAccount,
    ) -> Result<GoogleDeviceAuthStart, String> {
        let config = google_oauth_config(account)?;
        self.oauth.start_device_auth(config, GOOGLE_OAUTH_SCOPE)
    }

    /// Finish Google device authorization after the user approves it in the
    /// browser.
    pub fn finish_device_auth(
        &self,
        account: &ValidatedAccount,
        device_code: &str,
    ) -> Result<GoogleDeviceAuthFinish, String> {
        let config = google_oauth_config(account)?;
        self.oauth.finish_device_auth(config, device_code)
    }

    /// Prime the access token cache from a freshly completed OAuth flow.
    pub(crate) fn prime_access_token_cache(
        &self,
        account_id: &CalendarAccountId,
        access_token: String,
        expires_in_secs: Option<u64>,
    ) -> Result<(), String> {
        self.oauth
            .prime_access_token_cache(account_id, access_token, expires_in_secs)
    }

    /// List Google calendars allowed by account policy.
    pub fn list_calendars(
        &self,
        account: &ValidatedAccount,
        stored_refresh_token: Option<&str>,
    ) -> Result<Vec<GoogleCalendar>, String> {
        self.list_calendar_records(account, stored_refresh_token)
            .map(|calendars| calendars.into_iter().map(GoogleCalendar::from).collect())
    }

    /// List typed Google calendar records for the calendar runtime.
    pub(super) fn list_calendar_records(
        &self,
        account: &ValidatedAccount,
        stored_refresh_token: Option<&str>,
    ) -> Result<Vec<GoogleCalendarRecord>, String> {
        let token = self.access_token(account, stored_refresh_token)?;
        let api_base = api_base(account)?;
        let url = format!("{}/users/me/calendarList", api_base.effective);
        let json = self.get_json(&url, &token, api_base.custom)?;
        let calendars = json
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_calendar)
            .filter_map(|calendar| allowed_google_calendar(account, calendar))
            .collect();
        Ok(calendars)
    }

    /// List Google events in a calendar.
    pub fn list_events(
        &self,
        account: &ValidatedAccount,
        stored_refresh_token: Option<&str>,
        calendar_id: &str,
        range: TimeRange,
        limit: usize,
    ) -> Result<Vec<GoogleEvent>, String> {
        let calendar_id = ProviderCalendarId::new(calendar_id);
        Ok(self
            .list_events_page(
                account,
                stored_refresh_token,
                &calendar_id,
                GoogleEventListQuery {
                    range,
                    limit,
                    cursor: None,
                    include_cancelled: false,
                },
            )?
            .events
            .into_iter()
            .map(GoogleEvent::from)
            .collect())
    }

    /// List one cursor page of Google events in a calendar.
    pub(super) fn list_events_page(
        &self,
        account: &ValidatedAccount,
        stored_refresh_token: Option<&str>,
        calendar_id: &ProviderCalendarId,
        query: GoogleEventListQuery<'_>,
    ) -> Result<GoogleEventRecordPage, String> {
        ensure_google_calendar_allowed(account, calendar_id)?;
        let token = self.access_token(account, stored_refresh_token)?;
        let api_base = api_base(account)?;
        let query = event_list_query(&query)?;
        let url = format!(
            "{}/calendars/{}/events?{}",
            api_base.effective,
            encode_path_segment(calendar_id.as_str()),
            query
        );
        let json = self.get_json(&url, &token, api_base.custom)?;
        let next_cursor = google_next_cursor(&json)?;
        let events = json
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_event)
            .collect();
        Ok(GoogleEventRecordPage {
            events,
            next_cursor,
        })
    }

    /// Read one Google event.
    pub fn read_event(
        &self,
        account: &ValidatedAccount,
        stored_refresh_token: Option<&str>,
        calendar_id: &str,
        event_id: &str,
    ) -> Result<GoogleEvent, String> {
        self.read_event_record(
            account,
            stored_refresh_token,
            &ProviderCalendarId::new(calendar_id),
            &EventId::new(event_id),
        )
        .map(GoogleEvent::from)
    }

    /// Read one typed Google event for the calendar runtime.
    pub(super) fn read_event_record(
        &self,
        account: &ValidatedAccount,
        stored_refresh_token: Option<&str>,
        calendar_id: &ProviderCalendarId,
        event_id: &EventId,
    ) -> Result<GoogleEventRecord, String> {
        ensure_google_calendar_allowed(account, calendar_id)?;
        let token = self.access_token(account, stored_refresh_token)?;
        let api_base = api_base(account)?;
        let url = format!(
            "{}/calendars/{}/events/{}",
            api_base.effective,
            encode_path_segment(calendar_id.as_str()),
            encode_path_segment(event_id.as_str())
        );
        parse_event(&self.get_json(&url, &token, api_base.custom)?).ok_or_else(|| {
            format!(
                "Google event `{}` response was missing required fields",
                event_id.as_str()
            )
        })
    }

    /// Create one Google event.
    pub fn create_event(
        &self,
        account: &ValidatedAccount,
        stored_refresh_token: Option<&str>,
        calendar_id: &str,
        event: &GoogleEventWrite<'_>,
    ) -> Result<GoogleEvent, String> {
        self.create_event_classified(
            account,
            stored_refresh_token,
            &ProviderCalendarId::new(calendar_id),
            (event, None),
        )
        .map(GoogleEvent::from)
        .map_err(GoogleWriteError::into_string)
    }

    /// Create one Google event while preserving mutation-dispatch authority.
    pub(super) fn create_event_classified(
        &self,
        account: &ValidatedAccount,
        stored_refresh_token: Option<&str>,
        calendar_id: &ProviderCalendarId,
        event: (&GoogleEventWrite<'_>, Option<&EventTimeRange>),
    ) -> Result<GoogleEventRecord, GoogleWriteError> {
        let (event, event_time) = event;
        ensure_google_calendar_allowed(account, calendar_id)
            .map_err(GoogleWriteError::NotDispatched)?;
        let token = self
            .access_token(account, stored_refresh_token)
            .map_err(GoogleWriteError::NotDispatched)?;
        let api_base = api_base(account).map_err(GoogleWriteError::NotDispatched)?;
        let mut query = form_urlencoded::Serializer::new(String::new());
        query.append_pair("sendUpdates", GOOGLE_SEND_UPDATES);
        let url = format!(
            "{}/calendars/{}/events?{}",
            api_base.effective,
            encode_path_segment(calendar_id.as_str()),
            query.finish()
        );
        let body = google_event_body(event, event_time).map_err(GoogleWriteError::NotDispatched)?;
        parse_event(&self.post_json_write(&url, &token, &body)?)
            .ok_or(GoogleWriteError::OutcomeUnknown)
    }

    /// Patch one Google event using an ETag precondition.
    pub fn update_event(
        &self,
        account: &ValidatedAccount,
        stored_refresh_token: Option<&str>,
        calendar_id: &str,
        event_id: &str,
        etag: &str,
        event: &GoogleEventWrite<'_>,
    ) -> Result<GoogleEvent, String> {
        self.update_event_classified(
            account,
            stored_refresh_token,
            &ProviderCalendarId::new(calendar_id),
            &EventId::new(event_id),
            &EventEtag::new(etag),
            (event, None),
        )
        .map(GoogleEvent::from)
        .map_err(GoogleWriteError::into_string)
    }

    /// Patch one Google event while preserving mutation-dispatch authority.
    pub(super) fn update_event_classified(
        &self,
        account: &ValidatedAccount,
        stored_refresh_token: Option<&str>,
        calendar_id: &ProviderCalendarId,
        event_id: &EventId,
        etag: &EventEtag,
        event: (&GoogleEventWrite<'_>, Option<&EventTimeRange>),
    ) -> Result<GoogleEventRecord, GoogleWriteError> {
        let (event, event_time) = event;
        ensure_google_calendar_allowed(account, calendar_id)
            .map_err(GoogleWriteError::NotDispatched)?;
        let token = self
            .access_token(account, stored_refresh_token)
            .map_err(GoogleWriteError::NotDispatched)?;
        let api_base = api_base(account).map_err(GoogleWriteError::NotDispatched)?;
        let mut query = form_urlencoded::Serializer::new(String::new());
        query.append_pair("sendUpdates", GOOGLE_SEND_UPDATES);
        let url = format!(
            "{}/calendars/{}/events/{}?{}",
            api_base.effective,
            encode_path_segment(calendar_id.as_str()),
            encode_path_segment(event_id.as_str()),
            query.finish()
        );
        let body = google_event_body(event, event_time).map_err(GoogleWriteError::NotDispatched)?;
        parse_event(&self.patch_json_write(&url, &token, Some(etag.as_str()), &body)?)
            .ok_or(GoogleWriteError::OutcomeUnknown)
    }

    /// Delete one Google event using an ETag precondition.
    pub fn delete_event(
        &self,
        account: &ValidatedAccount,
        stored_refresh_token: Option<&str>,
        calendar_id: &str,
        event_id: &str,
        etag: &str,
    ) -> Result<(), String> {
        self.delete_event_classified(
            account,
            stored_refresh_token,
            &ProviderCalendarId::new(calendar_id),
            &EventId::new(event_id),
            &EventEtag::new(etag),
        )
        .map_err(GoogleWriteError::into_string)
    }

    /// Delete one Google event while preserving mutation-dispatch authority.
    pub(super) fn delete_event_classified(
        &self,
        account: &ValidatedAccount,
        stored_refresh_token: Option<&str>,
        calendar_id: &ProviderCalendarId,
        event_id: &EventId,
        etag: &EventEtag,
    ) -> Result<(), GoogleWriteError> {
        ensure_google_calendar_allowed(account, calendar_id)
            .map_err(GoogleWriteError::NotDispatched)?;
        let token = self
            .access_token(account, stored_refresh_token)
            .map_err(GoogleWriteError::NotDispatched)?;
        let api_base = api_base(account).map_err(GoogleWriteError::NotDispatched)?;
        let mut query = form_urlencoded::Serializer::new(String::new());
        query.append_pair("sendUpdates", GOOGLE_SEND_UPDATES);
        let url = format!(
            "{}/calendars/{}/events/{}?{}",
            api_base.effective,
            encode_path_segment(calendar_id.as_str()),
            encode_path_segment(event_id.as_str()),
            query.finish()
        );
        let response = self
            .agent
            .delete(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("If-Match", google_if_match_header(etag.as_str()))
            .call()
            .map_err(|_| GoogleWriteError::OutcomeUnknown)?;
        if !response.status().is_success() {
            return Err(GoogleWriteError::OutcomeUnknown);
        }
        Ok(())
    }

    /// Respond to an invitation by updating the authenticated attendee's
    /// response status with an ETag precondition.
    pub fn respond_invite(
        &self,
        account: &ValidatedAccount,
        stored_refresh_token: Option<&str>,
        calendar_id: &str,
        event_id: &str,
        etag: &str,
        response_status: &str,
    ) -> Result<GoogleEvent, String> {
        self.respond_invite_classified_with(
            account,
            stored_refresh_token,
            &ProviderCalendarId::new(calendar_id),
            &EventId::new(event_id),
            &EventEtag::new(etag),
            || InviteResponse::parse(response_status),
        )
        .map(GoogleEvent::from)
        .map_err(GoogleWriteError::into_string)
    }

    /// Respond to an invitation while preserving mutation-dispatch authority.
    pub(super) fn respond_invite_classified(
        &self,
        account: &ValidatedAccount,
        stored_refresh_token: Option<&str>,
        calendar_id: &ProviderCalendarId,
        event_id: &EventId,
        etag: &EventEtag,
        response: InviteResponse,
    ) -> Result<GoogleEventRecord, GoogleWriteError> {
        self.respond_invite_classified_with(
            account,
            stored_refresh_token,
            calendar_id,
            event_id,
            etag,
            || Ok(response),
        )
    }

    fn respond_invite_classified_with(
        &self,
        account: &ValidatedAccount,
        stored_refresh_token: Option<&str>,
        calendar_id: &ProviderCalendarId,
        event_id: &EventId,
        etag: &EventEtag,
        response: impl FnOnce() -> Result<InviteResponse, String>,
    ) -> Result<GoogleEventRecord, GoogleWriteError> {
        ensure_google_calendar_allowed(account, calendar_id)
            .map_err(GoogleWriteError::NotDispatched)?;
        let token = self
            .access_token(account, stored_refresh_token)
            .map_err(GoogleWriteError::NotDispatched)?;
        let api_base = api_base(account).map_err(GoogleWriteError::NotDispatched)?;
        let event_url = format!(
            "{}/calendars/{}/events/{}",
            api_base.effective,
            encode_path_segment(calendar_id.as_str()),
            encode_path_segment(event_id.as_str())
        );
        let current = self
            .get_json(&event_url, &token, api_base.custom)
            .map_err(GoogleWriteError::NotDispatched)?;
        let response = response().map_err(GoogleWriteError::NotDispatched)?;
        let patch =
            attendee_response_patch(&current, response).map_err(GoogleWriteError::NotDispatched)?;
        let mut query = form_urlencoded::Serializer::new(String::new());
        query.append_pair("sendUpdates", GOOGLE_SEND_UPDATES);
        let patch_url = format!("{event_url}?{}", query.finish());
        parse_event(&self.patch_json_write(&patch_url, &token, Some(etag.as_str()), &patch)?)
            .ok_or(GoogleWriteError::OutcomeUnknown)
    }

    fn access_token(
        &self,
        account: &ValidatedAccount,
        stored_refresh_token: Option<&str>,
    ) -> Result<String, String> {
        let config = google_oauth_config(account)?;
        let message = "Google calendar account is not authorized; run `:calendar auth google start <account>` and then `:calendar auth google finish <account>`";
        self.oauth
            .access_token(&account.id, config, stored_refresh_token, message)
    }

    fn get_json(
        &self,
        url: &str,
        access_token: &str,
        custom_api_base: Option<&str>,
    ) -> Result<Value, String> {
        let redaction = ErrorBodyRedaction::new(access_token, custom_api_base);
        let mut response = self
            .agent
            .get(url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Accept", "application/json")
            .call()
            .map_err(|error| format!("Google Calendar API request failed: {error}"))?;
        self.parse_json_response(&mut response, &redaction)
    }

    fn post_json_write(
        &self,
        url: &str,
        access_token: &str,
        body: &Value,
    ) -> Result<Value, GoogleWriteError> {
        let json_body = serde_json::to_string(body).map_err(|error| {
            GoogleWriteError::NotDispatched(format!(
                "serializing Google Calendar request failed: {error}"
            ))
        })?;
        let mut response = self
            .agent
            .post(url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Accept", "application/json")
            .content_type("application/json")
            .send(json_body)
            .map_err(|_| GoogleWriteError::OutcomeUnknown)?;
        Self::parse_json_write_response(&mut response)
    }

    fn patch_json_write(
        &self,
        url: &str,
        access_token: &str,
        if_match: Option<&str>,
        body: &Value,
    ) -> Result<Value, GoogleWriteError> {
        let mut request = self
            .agent
            .patch(url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Accept", "application/json");
        if let Some(etag) = if_match {
            request = request.header("If-Match", google_if_match_header(etag));
        }
        let json_body = serde_json::to_string(body).map_err(|error| {
            GoogleWriteError::NotDispatched(format!(
                "serializing Google Calendar request failed: {error}"
            ))
        })?;
        let mut response = request
            .content_type("application/json")
            .send(json_body)
            .map_err(|_| GoogleWriteError::OutcomeUnknown)?;
        Self::parse_json_write_response(&mut response)
    }

    fn parse_json_write_response(
        response: &mut ureq::http::Response<ureq::Body>,
    ) -> Result<Value, GoogleWriteError> {
        if !response.status().is_success() {
            return Err(GoogleWriteError::OutcomeUnknown);
        }
        let text = read_limited_body(response, "Google Calendar API response")
            .map_err(|_| GoogleWriteError::OutcomeUnknown)?;
        serde_json::from_str(&text).map_err(|_| GoogleWriteError::OutcomeUnknown)
    }

    fn parse_json_response(
        &self,
        response: &mut ureq::http::Response<ureq::Body>,
        redaction: &ErrorBodyRedaction<'_>,
    ) -> Result<Value, String> {
        if !response.status().is_success() {
            return Err(format!(
                "Google Calendar API returned HTTP {}: {}",
                response.status().as_u16(),
                read_error_body(response, redaction)
            ));
        }
        let text = read_limited_body(response, "Google Calendar API response")?;
        serde_json::from_str(&text)
            .map_err(|error| format!("Google Calendar API response was not JSON: {error}"))
    }
}

impl GoogleWriteError {
    fn into_string(self) -> String {
        match self {
            Self::NotDispatched(error) => error,
            Self::OutcomeUnknown => outcome_unknown_message().to_owned(),
        }
    }
}

/// Return the fixed diagnostic for a dispatched mutation without trusted
/// success.
pub(super) fn outcome_unknown_message() -> &'static str {
    OUTCOME_UNKNOWN_MESSAGE
}

/// Build the Google event-list query with explicit cancellation visibility.
fn event_list_query(event_query: &GoogleEventListQuery<'_>) -> Result<String, String> {
    let mut query = form_urlencoded::Serializer::new(String::new());
    query.append_pair("singleEvents", "true");
    query.append_pair("orderBy", "startTime");
    query.append_pair("maxResults", &event_query.limit.to_string());
    query.append_pair(
        "showDeleted",
        if event_query.include_cancelled {
            "true"
        } else {
            "false"
        },
    );
    let page_token = parse_google_cursor(event_query.cursor)?;
    if let Some(page_token) = page_token {
        query.append_pair("pageToken", page_token);
    }
    if let Some(min) = event_query.range.min {
        query.append_pair(
            "timeMin",
            &min.format(&Rfc3339)
                .map_err(|error| format!("formatting start failed: {error}"))?,
        );
    }
    if let Some(max) = event_query.range.max {
        query.append_pair(
            "timeMax",
            &max.format(&Rfc3339)
                .map_err(|error| format!("formatting end failed: {error}"))?,
        );
    }
    Ok(query.finish())
}

fn google_oauth_config(account: &ValidatedAccount) -> Result<GoogleOauthSecretConfig<'_>, String> {
    let Some(ValidatedBackendConfig::Google {
        client_id_secret,
        client_secret_secret,
        refresh_token_secret,
        ..
    }) = &account.backend
    else {
        return Err(format!(
            "calendar account `{}` is not a google account",
            account.id
        ));
    };
    Ok(GoogleOauthSecretConfig {
        client_id_secret,
        client_secret_secret: client_secret_secret.as_deref(),
        refresh_token_secret: refresh_token_secret.as_deref(),
    })
}

fn api_base(account: &ValidatedAccount) -> Result<GoogleApiBase<'_>, String> {
    let Some(ValidatedBackendConfig::Google { api_base, .. }) = &account.backend else {
        return Err(format!(
            "calendar account `{}` is not a google account",
            account.id
        ));
    };
    Ok(GoogleApiBase {
        effective: api_base.as_deref().unwrap_or(GOOGLE_CALENDAR_API_BASE),
        custom: api_base.as_deref(),
    })
}

fn allowed_google_calendar(
    account: &ValidatedAccount,
    mut calendar: GoogleCalendarRecord,
) -> Option<GoogleCalendarRecord> {
    if google_calendar_id_allowed(account, calendar.id.as_str()) {
        return Some(calendar);
    }
    if calendar.primary && google_calendar_id_allowed(account, "primary") {
        calendar.id = ProviderCalendarId::new("primary");
        return Some(calendar);
    }
    None
}

fn google_calendar_id_allowed(account: &ValidatedAccount, calendar_id: &str) -> bool {
    account
        .allowed_calendars
        .iter()
        .any(|allowed| allowed == calendar_id)
}

fn ensure_google_calendar_allowed(
    account: &ValidatedAccount,
    calendar_id: &ProviderCalendarId,
) -> Result<(), String> {
    if google_calendar_id_allowed(account, calendar_id.as_str()) {
        return Ok(());
    }
    Err(format!(
        "calendar `{}` is not allowed for account `{}`",
        calendar_id.as_str(),
        account.id
    ))
}

fn parse_google_cursor(cursor: Option<&str>) -> Result<Option<&str>, String> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    let Some(token) = cursor.strip_prefix(GOOGLE_CURSOR_PREFIX) else {
        return Err("cursor is not a Google Calendar cursor returned by this tool".to_owned());
    };
    if !is_safe_google_page_token(token) {
        return Err("Google Calendar cursor is invalid".to_owned());
    }
    Ok(Some(token))
}

fn google_next_cursor(json: &Value) -> Result<Option<String>, String> {
    let Some(token) = json.get("nextPageToken").and_then(Value::as_str) else {
        return Ok(None);
    };
    if !is_safe_google_page_token(token) {
        return Err("Google Calendar API returned an unsafe nextPageToken".to_owned());
    }
    Ok(Some(format!("{GOOGLE_CURSOR_PREFIX}{token}")))
}

fn is_safe_google_page_token(token: &str) -> bool {
    !token.is_empty()
        && token.chars().count() <= MAX_PAGE_TOKEN_CHARS
        && !token.chars().any(char::is_control)
}

fn parse_calendar(value: &Value) -> Option<GoogleCalendarRecord> {
    let id = value.get("id")?.as_str()?.to_owned();
    let summary = value
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or(&id)
        .to_owned();
    let access_role = value
        .get("accessRole")
        .and_then(Value::as_str)
        .unwrap_or("reader");
    let primary = value
        .get("primary")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(GoogleCalendarRecord {
        id: ProviderCalendarId::new(id),
        summary,
        primary,
        read_only: matches!(access_role, "freeBusyReader" | "reader"),
    })
}

fn parse_event(value: &Value) -> Option<GoogleEventRecord> {
    let id = value.get("id")?.as_str()?.to_owned();
    let start = google_event_time(value.get("start")?)?;
    let end = google_event_time(value.get("end")?)?;
    let attendee_values = value
        .get("attendees")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let attendees = attendee_values
        .iter()
        .filter_map(|attendee| attendee.get("email").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let self_response_status = attendee_values.iter().find_map(|attendee| {
        if attendee
            .get("self")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            attendee
                .get("responseStatus")
                .and_then(Value::as_str)
                .map(str::to_owned)
        } else {
            None
        }
    });
    let organizer = value.get("organizer").and_then(|organizer| {
        organizer
            .get("email")
            .or_else(|| organizer.get("displayName"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    Some(GoogleEventRecord {
        id: EventId::new(id),
        etag: value
            .get("etag")
            .and_then(Value::as_str)
            .map(EventEtag::new),
        i_cal_uid: value
            .get("iCalUID")
            .and_then(Value::as_str)
            .map(ICalUid::new),
        summary: value
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("(untitled)")
            .to_owned(),
        description: value
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        location: value
            .get("location")
            .and_then(Value::as_str)
            .map(str::to_owned),
        start,
        end,
        status: value
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_owned),
        visibility: value
            .get("visibility")
            .and_then(Value::as_str)
            .map(str::to_owned),
        transparency: value
            .get("transparency")
            .and_then(Value::as_str)
            .map(str::to_owned),
        organizer,
        attendees,
        self_response_status,
        recurring: value.get("recurringEventId").is_some() || value.get("recurrence").is_some(),
    })
}

fn google_event_time(value: &Value) -> Option<String> {
    value
        .get("dateTime")
        .or_else(|| value.get("date"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn google_event_body(
    event: &GoogleEventWrite<'_>,
    event_time: Option<&EventTimeRange>,
) -> Result<Value, String> {
    let mut object = Map::new();
    if let Some(title) = event.title {
        object.insert("summary".to_owned(), Value::String(title.to_owned()));
    }
    if let Some(description) = event.description {
        object.insert(
            "description".to_owned(),
            Value::String(description.to_owned()),
        );
    }
    if let Some(location) = event.location {
        object.insert("location".to_owned(), Value::String(location.to_owned()));
    }
    if let Some(event_time) = event_time {
        let pair = google_time_pair(event_time, event.timezone, event.clear_opposite_time_kind);
        object.insert("start".to_owned(), pair.start);
        object.insert("end".to_owned(), pair.end);
    } else if event.start.is_some() || event.end.is_some() {
        let (Some(start), Some(end)) = (event.start, event.end) else {
            return Err("Google event writes require both start and end".to_owned());
        };
        let event_time = EventTimeRange::from_google_exact(start.to_owned(), end.to_owned())?;
        let pair = google_time_pair(&event_time, event.timezone, event.clear_opposite_time_kind);
        object.insert("start".to_owned(), pair.start);
        object.insert("end".to_owned(), pair.end);
    }
    if let Some(attendees) = event.attendees {
        object.insert(
            "attendees".to_owned(),
            Value::Array(
                attendees
                    .iter()
                    .map(|email| json!({ "email": email }))
                    .collect(),
            ),
        );
    }
    Ok(Value::Object(object))
}

fn google_time_pair(
    event_time: &EventTimeRange,
    timezone: Option<&str>,
    clear_opposite_time_kind: bool,
) -> GoogleTimePair {
    let start = event_time.start_raw();
    let end = event_time.end_raw();
    if event_time.is_all_day() {
        let mut start_value = Map::new();
        start_value.insert("date".to_owned(), Value::String(start.to_owned()));
        let mut end_value = Map::new();
        end_value.insert("date".to_owned(), Value::String(end.to_owned()));
        if clear_opposite_time_kind {
            start_value.insert("dateTime".to_owned(), Value::Null);
            end_value.insert("dateTime".to_owned(), Value::Null);
        }
        GoogleTimePair {
            start: Value::Object(start_value),
            end: Value::Object(end_value),
        }
    } else {
        let mut start_value = Map::new();
        if clear_opposite_time_kind {
            start_value.insert("date".to_owned(), Value::Null);
        }
        start_value.insert("dateTime".to_owned(), Value::String(start.to_owned()));
        let mut end_value = Map::new();
        if clear_opposite_time_kind {
            end_value.insert("date".to_owned(), Value::Null);
        }
        end_value.insert("dateTime".to_owned(), Value::String(end.to_owned()));
        if let Some(timezone) = timezone {
            start_value.insert("timeZone".to_owned(), Value::String(timezone.to_owned()));
            end_value.insert("timeZone".to_owned(), Value::String(timezone.to_owned()));
        }
        GoogleTimePair {
            start: Value::Object(start_value),
            end: Value::Object(end_value),
        }
    }
}

fn attendee_response_patch(event: &Value, response: InviteResponse) -> Result<Value, String> {
    let attendees = event
        .get("attendees")
        .and_then(Value::as_array)
        .ok_or_else(|| "Google event has no attendees to respond to".to_owned())?;
    let mut found_self = false;
    let updated = attendees
        .iter()
        .map(|attendee| {
            let mut attendee = attendee.clone();
            let is_self = attendee
                .get("self")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if is_self && let Some(object) = attendee.as_object_mut() {
                object.insert(
                    "responseStatus".to_owned(),
                    Value::String(response.as_str().to_owned()),
                );
                found_self = true;
            }
            attendee
        })
        .collect::<Vec<_>>();
    if !found_self {
        return Err("Google event does not identify the authenticated attendee".to_owned());
    }
    Ok(json!({ "attendees": updated }))
}

fn encode_path_segment(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if is_path_segment_unreserved(byte) {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0f));
        }
    }
    out
}

fn google_if_match_header(etag: &str) -> String {
    if etag == "*" || etag.starts_with('"') || etag.starts_with("W/\"") {
        etag.to_owned()
    } else {
        format!("\"{etag}\"")
    }
}

fn is_path_segment_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        _ => (b'A' + (value - 10)) as char,
    }
}

fn read_limited_body(
    response: &mut ureq::http::Response<ureq::Body>,
    context: &str,
) -> Result<String, String> {
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_JSON_BODY_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("reading {context} failed: {error}"))?;
    if MAX_JSON_BODY_BYTES < bytes.len() {
        return Err(format!("{context} was too large"));
    }
    String::from_utf8(bytes).map_err(|_| format!("{context} was not valid UTF-8"))
}

fn read_error_body(
    response: &mut ureq::http::Response<ureq::Body>,
    redaction: &ErrorBodyRedaction<'_>,
) -> String {
    redaction.read_and_format(response.body_mut().as_reader())
}

fn truncate_utf8_bytes(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

fn sanitize_error_text(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests;
