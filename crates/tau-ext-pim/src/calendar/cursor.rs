//! Opaque cursor state for calendar range queries.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::tool::CalendarRangeArgs;

const CURSOR_PREFIX: &str = "calendar:v1:";
const CURSOR_VERSION: u8 = 1;
const MAX_CURSOR_CHARS: usize = 8192;

/// The normalized semantic query bound into a continuation cursor.
pub(super) struct CalendarCursorQuery<'a> {
    /// Opaque model-visible calendar id.
    calendar: &'a str,
    /// Normalized absolute lower range bound.
    start: &'a str,
    /// Normalized absolute upper range bound.
    end: &'a str,
    /// Checked semantic page size.
    limit: u32,
    /// Command-specific query semantics.
    kind: CalendarCursorQueryKind<'a>,
}

/// Borrowed command-specific state accepted by the cursor encoder.
enum CalendarCursorQueryKind<'a> {
    /// Search filtering and lifecycle visibility.
    Search {
        /// Optional case-insensitive title filter.
        title: Option<&'a str>,
        /// Whether cancellation discovery is enabled.
        include_cancelled: bool,
    },
    /// Active blocking-time query.
    FreeBusy,
}

/// Typed command selector used when decoding a continuation.
#[derive(Clone, Copy)]
pub(super) enum CalendarCursorSelector {
    /// Decode only calendar-search cursors.
    Search,
    /// Decode only free/busy cursors.
    FreeBusy,
}

/// Decoded continuation state for one calendar query.
#[derive(Debug, Deserialize, Serialize)]
pub(super) struct CalendarCursor {
    /// Cursor format version.
    version: u8,
    /// Opaque calendar id selected for the first page.
    calendar: String,
    /// Normalized RFC3339 lower range bound.
    start: String,
    /// Normalized RFC3339 upper range bound.
    end: String,
    /// Page size selected for the first page.
    limit: u32,
    /// Command-specific semantic query state.
    #[serde(flatten)]
    kind: CalendarCursorKind,
    /// Backend-owned continuation token.
    backend_cursor: String,
}

/// Owned command-specific state stored in an opaque cursor.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum CalendarCursorKind {
    /// Calendar search with its immutable filters.
    Search {
        /// Search title filter, if any.
        title: Option<String>,
        /// Whether cancellation discovery was selected.
        include_cancelled: bool,
    },
    /// Active blocking-time query.
    FreeBusy,
}

impl<'a> CalendarCursorQuery<'a> {
    /// Build checked cursor state for a calendar search.
    pub(super) fn search(
        calendar: &'a str,
        start: &'a str,
        end: &'a str,
        limit: usize,
        title: Option<&'a str>,
        include_cancelled: bool,
    ) -> Result<Self, String> {
        Ok(Self {
            calendar,
            start,
            end,
            limit: checked_limit(limit)?,
            kind: CalendarCursorQueryKind::Search {
                title,
                include_cancelled,
            },
        })
    }

    /// Build checked cursor state for a free/busy query.
    pub(super) fn free_busy(
        calendar: &'a str,
        start: &'a str,
        end: &'a str,
        limit: usize,
    ) -> Result<Self, String> {
        Ok(Self {
            calendar,
            start,
            end,
            limit: checked_limit(limit)?,
            kind: CalendarCursorQueryKind::FreeBusy,
        })
    }
}

impl CalendarCursor {
    /// Decode cursor-only arguments and verify that the cursor belongs to the
    /// selected command.
    pub(super) fn from_args(
        args: &CalendarRangeArgs,
        selector: CalendarCursorSelector,
        maximum_limit: u32,
    ) -> Result<Option<Self>, String> {
        let Some(value) = args.cursor.as_deref() else {
            return Ok(None);
        };
        if args.calendar.is_some()
            || args.start.is_some()
            || args.end.is_some()
            || args.limit.is_some()
            || args.title.is_some()
            || args.include_cancelled.is_some()
        {
            return Err(
                "cursor already identifies the calendar, range, visibility, and limit; retry with cursor only"
                    .to_owned(),
            );
        }
        let cursor = Self::decode(value, maximum_limit)?;
        if !cursor.belongs_to(selector) {
            return Err(
                "cursor belongs to a different calendar query; start a new search without cursor"
                    .to_owned(),
            );
        }
        Ok(Some(cursor))
    }

    /// Recreate the exact provider-facing arguments captured on the first page.
    pub(super) fn continuation_args(&self) -> CalendarRangeArgs {
        let (title, include_cancelled) = match &self.kind {
            CalendarCursorKind::Search {
                title,
                include_cancelled,
            } => (title.clone(), Some(*include_cancelled)),
            CalendarCursorKind::FreeBusy => (None, None),
        };
        CalendarRangeArgs {
            calendar: Some(self.calendar.clone()),
            start: Some(self.start.clone()),
            end: Some(self.end.clone()),
            limit: Some(self.limit),
            cursor: Some(self.backend_cursor.clone()),
            title,
            include_cancelled,
        }
    }

    /// Encode a backend continuation token with its normalized semantic query.
    pub(super) fn encode_next(
        backend_cursor: Option<String>,
        query: &CalendarCursorQuery<'_>,
    ) -> Result<Option<String>, String> {
        let Some(backend_cursor) = backend_cursor else {
            return Ok(None);
        };
        let kind = match query.kind {
            CalendarCursorQueryKind::Search {
                title,
                include_cancelled,
            } => CalendarCursorKind::Search {
                title: title.map(str::to_owned),
                include_cancelled,
            },
            CalendarCursorQueryKind::FreeBusy => CalendarCursorKind::FreeBusy,
        };
        let cursor = Self {
            version: CURSOR_VERSION,
            calendar: query.calendar.to_owned(),
            start: query.start.to_owned(),
            end: query.end.to_owned(),
            limit: query.limit,
            kind,
            backend_cursor,
        };
        let json = serde_json::to_vec(&cursor)
            .map_err(|error| format!("serializing calendar cursor failed: {error}"))?;
        Ok(Some(format!(
            "{CURSOR_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(json)
        )))
    }

    fn belongs_to(&self, selector: CalendarCursorSelector) -> bool {
        matches!(
            (&self.kind, selector),
            (
                CalendarCursorKind::Search { .. },
                CalendarCursorSelector::Search
            ) | (
                CalendarCursorKind::FreeBusy,
                CalendarCursorSelector::FreeBusy
            )
        )
    }

    fn decode(value: &str, maximum_limit: u32) -> Result<Self, String> {
        let encoded = value.strip_prefix(CURSOR_PREFIX).ok_or_else(cursor_error)?;
        if MAX_CURSOR_CHARS < encoded.len() {
            return Err(cursor_error());
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| cursor_error())?;
        let cursor: Self = serde_json::from_slice(&bytes).map_err(|_| cursor_error())?;
        if cursor.version != CURSOR_VERSION
            || cursor.calendar.is_empty()
            || cursor.start.is_empty()
            || cursor.end.is_empty()
            || cursor.limit == 0
            || maximum_limit < cursor.limit
            || cursor.backend_cursor.is_empty()
        {
            return Err(cursor_error());
        }
        OffsetDateTime::parse(&cursor.start, &Rfc3339).map_err(|_| cursor_error())?;
        OffsetDateTime::parse(&cursor.end, &Rfc3339).map_err(|_| cursor_error())?;
        Ok(cursor)
    }
}

fn checked_limit(limit: usize) -> Result<u32, String> {
    u32::try_from(limit).map_err(|_| "calendar cursor limit exceeds supported range".to_owned())
}

fn cursor_error() -> String {
    "cursor belongs to a different calendar query; start a new search without cursor".to_owned()
}
