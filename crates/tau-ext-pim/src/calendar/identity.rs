//! Internal calendar provider identity types.

use super::config::CalendarAccountId;

/// Opaque calendar identifier supplied by a calendar provider.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct ProviderCalendarId(String);

/// Opaque event identifier supplied or synthesized by a calendar provider.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct EventId(String);

/// iCalendar UID shared by instances of one logical event.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct ICalUid(String);

/// Provider concurrency token used to protect a calendar mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct EventEtag(String);

/// Fully scoped key for one provider event.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct EventKey {
    /// Configured account that owns the event.
    account: CalendarAccountId,
    /// Provider calendar that contains the event.
    calendar: ProviderCalendarId,
    /// Provider event within the calendar.
    event: EventId,
}

macro_rules! string_identity {
    ($type:ident) => {
        impl $type {
            /// Preserve one raw boundary value without changing its accepted shape.
            pub(super) fn new(raw: impl Into<String>) -> Self {
                Self(raw.into())
            }

            /// Borrow the exact underlying provider value.
            pub(super) fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

string_identity!(ProviderCalendarId);
string_identity!(EventId);
string_identity!(ICalUid);
string_identity!(EventEtag);

impl ProviderCalendarId {
    /// Return the exact underlying provider value.
    pub(super) fn into_string(self) -> String {
        self.0
    }
}

impl EventId {
    /// Return the exact underlying provider value.
    pub(super) fn into_string(self) -> String {
        self.0
    }
}

impl ICalUid {
    /// Return the exact underlying provider value.
    pub(super) fn into_string(self) -> String {
        self.0
    }
}

impl EventEtag {
    /// Return the exact underlying provider value.
    pub(super) fn into_string(self) -> String {
        self.0
    }
}

impl EventKey {
    /// Build an event key from its already-separated namespaces.
    pub(super) fn new(
        account: &CalendarAccountId,
        calendar: ProviderCalendarId,
        event: EventId,
    ) -> Self {
        Self {
            account: account.clone(),
            calendar,
            event,
        }
    }

    /// Return whether this key belongs to the selected account and calendar.
    pub(super) fn belongs_to(
        &self,
        account: &CalendarAccountId,
        calendar: &ProviderCalendarId,
    ) -> bool {
        &self.account == account && self.calendar == *calendar
    }

    /// Borrow the event component.
    pub(super) fn event(&self) -> &EventId {
        &self.event
    }
}
