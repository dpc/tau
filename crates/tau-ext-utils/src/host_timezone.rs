use jiff::tz::TimeZone;

/// Source of the running host's current local timezone and rules.
pub(crate) trait HostTimezoneProvider {
    /// Read the current timezone snapshot exposed by the provider.
    fn current_timezone(&self) -> Result<TimeZone, String>;
}

/// Production provider backed by Jiff's system-timezone discovery.
pub(crate) struct SystemHostTimezoneProvider;

impl HostTimezoneProvider for SystemHostTimezoneProvider {
    fn current_timezone(&self) -> Result<TimeZone, String> {
        TimeZone::try_system()
            .map_err(|error| format!("could not determine the host local timezone: {error}"))
    }
}
