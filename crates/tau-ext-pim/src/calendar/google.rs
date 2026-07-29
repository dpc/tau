use std::collections::BTreeMap;
use std::io::Read;

use serde_json::{Map, Value, json};
use tau_proto::SecretValue;
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime};
use url::form_urlencoded;

use super::config::{ValidatedAccount, ValidatedBackendConfig};
use super::ics_feed::TimeRange;
use crate::google_oauth::{
    GoogleDeviceAuthFinish, GoogleDeviceAuthStart, GoogleOauthClient, GoogleOauthSecretConfig,
    google_http_agent,
};
#[cfg(test)]
use crate::google_oauth::{
    google_oauth_error_message, parse_access_token_response, parse_device_auth_start,
};

const GOOGLE_OAUTH_SCOPE: &str = "https://www.googleapis.com/auth/calendar";
const GOOGLE_CALENDAR_API_BASE: &str = "https://www.googleapis.com/calendar/v3";
const GOOGLE_SEND_UPDATES: &str = "all";
const MAX_ERROR_BODY_BYTES: usize = 4096;
const MAX_JSON_BODY_BYTES: usize = 1024 * 1024;
const MAX_PAGE_TOKEN_CHARS: usize = 4096;
const GOOGLE_CURSOR_PREFIX: &str = "google:";

/// Read/write-capable Google Calendar API backend.
pub struct GoogleBackend {
    agent: ureq::Agent,
    oauth: GoogleOauthClient,
}

/// One Google calendar visible to the account.
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

/// One Google Calendar event.
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

/// One page of Google Calendar events.
pub struct GoogleEventPage {
    /// Events in this page.
    pub events: Vec<GoogleEvent>,
    /// Cursor for the next page, when Google returns another page token.
    pub next_cursor: Option<String>,
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

enum GoogleBoundary {
    Date {
        raw: String,
        date: Date,
    },
    DateTime {
        raw: String,
        datetime: OffsetDateTime,
    },
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
    pub fn prime_access_token_cache(
        &self,
        account_id: &str,
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
        let token = self.access_token(account, stored_refresh_token)?;
        let api_base = api_base(account)?;
        let url = format!("{api_base}/users/me/calendarList");
        let json = self.get_json(&url, &token)?;
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
        Ok(self
            .list_events_page(
                account,
                stored_refresh_token,
                calendar_id,
                range,
                limit,
                None,
            )?
            .events)
    }

    /// List one cursor page of Google events in a calendar.
    pub fn list_events_page(
        &self,
        account: &ValidatedAccount,
        stored_refresh_token: Option<&str>,
        calendar_id: &str,
        range: TimeRange,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<GoogleEventPage, String> {
        ensure_google_calendar_allowed(account, calendar_id)?;
        let page_token = parse_google_cursor(cursor)?;
        let token = self.access_token(account, stored_refresh_token)?;
        let api_base = api_base(account)?;
        let mut query = form_urlencoded::Serializer::new(String::new());
        query.append_pair("singleEvents", "true");
        query.append_pair("orderBy", "startTime");
        query.append_pair("maxResults", &limit.to_string());
        if let Some(page_token) = page_token {
            query.append_pair("pageToken", page_token);
        }
        if let Some(min) = range.min {
            query.append_pair(
                "timeMin",
                &min.format(&Rfc3339)
                    .map_err(|error| format!("formatting start failed: {error}"))?,
            );
        }
        if let Some(max) = range.max {
            query.append_pair(
                "timeMax",
                &max.format(&Rfc3339)
                    .map_err(|error| format!("formatting end failed: {error}"))?,
            );
        }
        let url = format!(
            "{api_base}/calendars/{}/events?{}",
            encode_path_segment(calendar_id),
            query.finish()
        );
        let json = self.get_json(&url, &token)?;
        let next_cursor = google_next_cursor(&json)?;
        let events = json
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_event)
            .collect();
        Ok(GoogleEventPage {
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
        ensure_google_calendar_allowed(account, calendar_id)?;
        let token = self.access_token(account, stored_refresh_token)?;
        let api_base = api_base(account)?;
        let url = format!(
            "{api_base}/calendars/{}/events/{}",
            encode_path_segment(calendar_id),
            encode_path_segment(event_id)
        );
        parse_event(&self.get_json(&url, &token)?).ok_or_else(|| {
            format!("Google event `{event_id}` response was missing required fields")
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
        ensure_google_calendar_allowed(account, calendar_id)?;
        let token = self.access_token(account, stored_refresh_token)?;
        let api_base = api_base(account)?;
        let mut query = form_urlencoded::Serializer::new(String::new());
        query.append_pair("sendUpdates", GOOGLE_SEND_UPDATES);
        let url = format!(
            "{api_base}/calendars/{}/events?{}",
            encode_path_segment(calendar_id),
            query.finish()
        );
        parse_event(&self.post_json(&url, &token, &google_event_body(event)?)?)
            .ok_or_else(|| "Google create event response was missing required fields".to_owned())
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
        ensure_google_calendar_allowed(account, calendar_id)?;
        let token = self.access_token(account, stored_refresh_token)?;
        let api_base = api_base(account)?;
        let mut query = form_urlencoded::Serializer::new(String::new());
        query.append_pair("sendUpdates", GOOGLE_SEND_UPDATES);
        let url = format!(
            "{api_base}/calendars/{}/events/{}?{}",
            encode_path_segment(calendar_id),
            encode_path_segment(event_id),
            query.finish()
        );
        parse_event(&self.patch_json(&url, &token, Some(etag), &google_event_body(event)?)?)
            .ok_or_else(|| "Google update event response was missing required fields".to_owned())
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
        ensure_google_calendar_allowed(account, calendar_id)?;
        let token = self.access_token(account, stored_refresh_token)?;
        let api_base = api_base(account)?;
        let mut query = form_urlencoded::Serializer::new(String::new());
        query.append_pair("sendUpdates", GOOGLE_SEND_UPDATES);
        let url = format!(
            "{api_base}/calendars/{}/events/{}?{}",
            encode_path_segment(calendar_id),
            encode_path_segment(event_id),
            query.finish()
        );
        let mut response = self
            .agent
            .delete(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("If-Match", google_if_match_header(etag))
            .call()
            .map_err(|error| format!("Google Calendar API delete failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "Google Calendar API returned HTTP {}: {}",
                response.status().as_u16(),
                read_error_body(&mut response)
            ));
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
        ensure_google_calendar_allowed(account, calendar_id)?;
        let token = self.access_token(account, stored_refresh_token)?;
        let api_base = api_base(account)?;
        let event_url = format!(
            "{api_base}/calendars/{}/events/{}",
            encode_path_segment(calendar_id),
            encode_path_segment(event_id)
        );
        let current = self.get_json(&event_url, &token)?;
        let patch = attendee_response_patch(&current, response_status)?;
        let mut query = form_urlencoded::Serializer::new(String::new());
        query.append_pair("sendUpdates", GOOGLE_SEND_UPDATES);
        let patch_url = format!("{event_url}?{}", query.finish());
        parse_event(&self.patch_json(&patch_url, &token, Some(etag), &patch)?)
            .ok_or_else(|| "Google invite response response was missing required fields".to_owned())
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

    fn get_json(&self, url: &str, access_token: &str) -> Result<Value, String> {
        let mut response = self
            .agent
            .get(url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Accept", "application/json")
            .call()
            .map_err(|error| format!("Google Calendar API request failed: {error}"))?;
        self.parse_json_response(&mut response)
    }

    fn post_json(&self, url: &str, access_token: &str, body: &Value) -> Result<Value, String> {
        let json_body = serde_json::to_string(body)
            .map_err(|error| format!("serializing Google Calendar request failed: {error}"))?;
        let mut response = self
            .agent
            .post(url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Accept", "application/json")
            .content_type("application/json")
            .send(json_body)
            .map_err(|error| format!("Google Calendar API request failed: {error}"))?;
        self.parse_json_response(&mut response)
    }

    fn patch_json(
        &self,
        url: &str,
        access_token: &str,
        if_match: Option<&str>,
        body: &Value,
    ) -> Result<Value, String> {
        let mut request = self
            .agent
            .patch(url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Accept", "application/json");
        if let Some(etag) = if_match {
            request = request.header("If-Match", google_if_match_header(etag));
        }
        let json_body = serde_json::to_string(body)
            .map_err(|error| format!("serializing Google Calendar request failed: {error}"))?;
        let mut response = request
            .content_type("application/json")
            .send(json_body)
            .map_err(|error| format!("Google Calendar API request failed: {error}"))?;
        self.parse_json_response(&mut response)
    }

    fn parse_json_response(
        &self,
        response: &mut ureq::http::Response<ureq::Body>,
    ) -> Result<Value, String> {
        if !response.status().is_success() {
            return Err(format!(
                "Google Calendar API returned HTTP {}: {}",
                response.status().as_u16(),
                read_error_body(response)
            ));
        }
        let text = read_limited_body(response, "Google Calendar API response")?;
        serde_json::from_str(&text)
            .map_err(|error| format!("Google Calendar API response was not JSON: {error}"))
    }
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

fn api_base(account: &ValidatedAccount) -> Result<&str, String> {
    let Some(ValidatedBackendConfig::Google { api_base, .. }) = &account.backend else {
        return Err(format!(
            "calendar account `{}` is not a google account",
            account.id
        ));
    };
    Ok(api_base.as_deref().unwrap_or(GOOGLE_CALENDAR_API_BASE))
}

fn allowed_google_calendar(
    account: &ValidatedAccount,
    mut calendar: GoogleCalendar,
) -> Option<GoogleCalendar> {
    if google_calendar_id_allowed(account, &calendar.id) {
        return Some(calendar);
    }
    if calendar.primary && google_calendar_id_allowed(account, "primary") {
        calendar.id = "primary".to_owned();
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
    calendar_id: &str,
) -> Result<(), String> {
    if google_calendar_id_allowed(account, calendar_id) {
        return Ok(());
    }
    Err(format!(
        "calendar `{calendar_id}` is not allowed for account `{}`",
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

fn parse_calendar(value: &Value) -> Option<GoogleCalendar> {
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
    Some(GoogleCalendar {
        id,
        summary,
        primary,
        read_only: matches!(access_role, "freeBusyReader" | "reader"),
    })
}

fn parse_event(value: &Value) -> Option<GoogleEvent> {
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
    Some(GoogleEvent {
        id,
        etag: value.get("etag").and_then(Value::as_str).map(str::to_owned),
        i_cal_uid: value
            .get("iCalUID")
            .and_then(Value::as_str)
            .map(str::to_owned),
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

fn google_event_body(event: &GoogleEventWrite<'_>) -> Result<Value, String> {
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
    if event.start.is_some() || event.end.is_some() {
        let (Some(start), Some(end)) = (event.start, event.end) else {
            return Err("Google event writes require both start and end".to_owned());
        };
        let pair = google_time_pair(start, end, event.timezone, event.clear_opposite_time_kind)?;
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
    start: &str,
    end: &str,
    timezone: Option<&str>,
    clear_opposite_time_kind: bool,
) -> Result<GoogleTimePair, String> {
    match (
        parse_google_boundary(start, "start")?,
        parse_google_boundary(end, "end")?,
    ) {
        (
            GoogleBoundary::Date {
                raw: start,
                date: start_date,
            },
            GoogleBoundary::Date {
                raw: end,
                date: end_date,
            },
        ) => {
            if !is_date_before(start_date, end_date) {
                return Err("event start must be before event end".to_owned());
            }
            let mut start_value = Map::new();
            start_value.insert("date".to_owned(), Value::String(start));
            let mut end_value = Map::new();
            end_value.insert("date".to_owned(), Value::String(end));
            if clear_opposite_time_kind {
                start_value.insert("dateTime".to_owned(), Value::Null);
                end_value.insert("dateTime".to_owned(), Value::Null);
            }
            Ok(GoogleTimePair {
                start: Value::Object(start_value),
                end: Value::Object(end_value),
            })
        }
        (
            GoogleBoundary::DateTime {
                raw: start,
                datetime: start_datetime,
            },
            GoogleBoundary::DateTime {
                raw: end,
                datetime: end_datetime,
            },
        ) => {
            if !is_datetime_before(start_datetime, end_datetime) {
                return Err("event start must be before event end".to_owned());
            }
            let mut start_value = Map::new();
            if clear_opposite_time_kind {
                start_value.insert("date".to_owned(), Value::Null);
            }
            start_value.insert("dateTime".to_owned(), Value::String(start));
            let mut end_value = Map::new();
            if clear_opposite_time_kind {
                end_value.insert("date".to_owned(), Value::Null);
            }
            end_value.insert("dateTime".to_owned(), Value::String(end));
            if let Some(timezone) = timezone {
                start_value.insert("timeZone".to_owned(), Value::String(timezone.to_owned()));
                end_value.insert("timeZone".to_owned(), Value::String(timezone.to_owned()));
            }
            Ok(GoogleTimePair {
                start: Value::Object(start_value),
                end: Value::Object(end_value),
            })
        }
        _ => Err(
            "event start and end must both be all-day dates or both be RFC3339 date-times"
                .to_owned(),
        ),
    }
}

fn parse_google_boundary(value: &str, field: &str) -> Result<GoogleBoundary, String> {
    if let Some(date) = parse_google_date(value) {
        return Ok(GoogleBoundary::Date {
            raw: value.to_owned(),
            date,
        });
    }
    let datetime = OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|error| format!("{field} must be RFC3339 or YYYY-MM-DD: {error}"))?;
    Ok(GoogleBoundary::DateTime {
        raw: value.to_owned(),
        datetime,
    })
}

fn parse_google_date(value: &str) -> Option<Date> {
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
    let month = Month::try_from(value[5..7].parse::<u8>().ok()?).ok()?;
    let day = value[8..10].parse::<u8>().ok()?;
    Date::from_calendar_date(year, month, day).ok()
}

fn is_date_before(left: Date, right: Date) -> bool {
    left < right
}

fn is_datetime_before(left: OffsetDateTime, right: OffsetDateTime) -> bool {
    left < right
}

fn attendee_response_patch(event: &Value, response_status: &str) -> Result<Value, String> {
    if !matches!(response_status, "accepted" | "tentative" | "declined") {
        return Err("response must be accepted, tentative, or declined".to_owned());
    }
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
                    Value::String(response_status.to_owned()),
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
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: silent-map-err
    String::from_utf8(bytes).map_err(|_| format!("{context} was not valid UTF-8"))
}

fn read_error_body(response: &mut ureq::http::Response<ureq::Body>) -> String {
    let mut bytes = Vec::new();
    let _ = response
        .body_mut()
        .as_reader()
        .take(MAX_ERROR_BODY_BYTES as u64 + 1)
        .read_to_end(&mut bytes);
    if MAX_ERROR_BODY_BYTES < bytes.len() {
        bytes.truncate(MAX_ERROR_BODY_BYTES);
    }
    sanitize_error_text(&String::from_utf8_lossy(&bytes))
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
