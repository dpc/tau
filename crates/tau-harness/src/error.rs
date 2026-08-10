//! [`HarnessError`]: the unified error type returned by every fallible
//! harness operation. Aggregates I/O, protocol, routing, and timeout
//! failures behind one `From`-rich enum.

use std::path::Path;
use std::{fmt, io};

use tau_core::{AgentStoreError, RouteError, SessionStoreError, ToolRouteError};
use tau_proto::DecodeError;
use tau_socket::SocketTransportError;

const SPAWN_DIAGNOSTIC_FIELD_CHARS: usize = 160;

/// Secret-safe context for a configured extension process that failed to spawn.
///
/// Only the configured instance name, executable from the `command` field, and
/// optional working directory are retained. Command arguments and all other
/// configuration are intentionally excluded.
#[derive(Debug)]
pub struct ExtensionSpawnError {
    /// Bounded, escaped extension instance name.
    extension: String,
    /// Bounded, escaped executable selected from the `command` field.
    command: String,
    /// Bounded, escaped explicitly configured working directory, when present.
    cwd: Option<String>,
    /// Underlying operating-system spawn error.
    source: io::Error,
}

impl ExtensionSpawnError {
    /// Builds spawn context without retaining arguments or unrelated extension
    /// configuration.
    #[must_use]
    pub(crate) fn new(
        extension: &str,
        command: &str,
        cwd: Option<&Path>,
        source: io::Error,
    ) -> Self {
        Self {
            extension: bounded_debug_value(extension),
            command: bounded_debug_value(command),
            cwd: cwd.map(|path| bounded_debug_value(&path.to_string_lossy())),
            source,
        }
    }
}

impl fmt::Display for ExtensionSpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "failed to spawn configured extension instance {} from its `command` executable {}",
            self.extension, self.command
        )?;
        if let Some(cwd) = &self.cwd {
            write!(f, " with configured cwd {cwd}")?;
        }
        write!(f, ": {}", self.source)
    }
}

impl std::error::Error for ExtensionSpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn bounded_debug_value(value: &str) -> String {
    let mut bounded = value
        .chars()
        .take(SPAWN_DIAGNOSTIC_FIELD_CHARS)
        .collect::<String>();
    if value.chars().nth(SPAWN_DIAGNOSTIC_FIELD_CHARS).is_some() {
        bounded.push('…');
    }
    format!("{bounded:?}")
}

/// Errors returned by the harness.
#[derive(Debug)]
pub enum HarnessError {
    Io(io::Error),
    /// A configured extension process could not be spawned.
    ///
    /// Its source chain retains [`ExtensionSpawnError`] and the underlying
    /// operating-system [`io::Error`].
    ExtensionSpawn(ExtensionSpawnError),
    ProtocolDecode(DecodeError),
    ProtocolEncode(tau_proto::EncodeError),
    SessionStore(SessionStoreError),
    AgentStore(AgentStoreError),
    SocketTransport(SocketTransportError),
    Route(RouteError),
    ToolRoute(ToolRouteError),
    StartupTimeout,
    /// Registered context providers did not finish session initialization.
    SessionInitTimeout,
    ResponseTimeout,
    ThreadJoin(String),
    Participant(String),
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(f, "I/O error: {source}"),
            Self::ExtensionSpawn(source) => source.fmt(f),
            Self::ProtocolDecode(source) => write!(f, "protocol decode error: {source}"),
            Self::ProtocolEncode(source) => write!(f, "protocol encode error: {source}"),
            Self::SessionStore(source) => write!(f, "session store error: {source}"),
            Self::AgentStore(source) => write!(f, "agent store error: {source}"),
            Self::SocketTransport(source) => write!(f, "socket transport error: {source}"),
            Self::Route(source) => write!(f, "routing error: {source}"),
            Self::ToolRoute(source) => write!(f, "tool routing error: {source}"),
            Self::StartupTimeout => f.write_str("timed out waiting for extensions to start"),
            Self::SessionInitTimeout => {
                f.write_str("timed out waiting for session context providers to initialize")
            }
            Self::ResponseTimeout => f.write_str("timed out waiting for agent response"),
            Self::ThreadJoin(name) => write!(f, "failed to join {name} thread cleanly"),
            Self::Participant(message) => write!(f, "participant error: {message}"),
        }
    }
}

impl std::error::Error for HarnessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::ExtensionSpawn(source) => Some(source),
            Self::ProtocolDecode(source) => Some(source),
            Self::ProtocolEncode(source) => Some(source),
            Self::SessionStore(source) => Some(source),
            Self::AgentStore(source) => Some(source),
            Self::SocketTransport(source) => Some(source),
            Self::Route(source) => Some(source),
            Self::ToolRoute(source) => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for HarnessError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}
impl From<DecodeError> for HarnessError {
    fn from(source: DecodeError) -> Self {
        Self::ProtocolDecode(source)
    }
}
impl From<SessionStoreError> for HarnessError {
    fn from(source: SessionStoreError) -> Self {
        Self::SessionStore(source)
    }
}
impl From<AgentStoreError> for HarnessError {
    fn from(source: AgentStoreError) -> Self {
        Self::AgentStore(source)
    }
}
impl From<SocketTransportError> for HarnessError {
    fn from(source: SocketTransportError) -> Self {
        Self::SocketTransport(source)
    }
}
impl From<RouteError> for HarnessError {
    fn from(source: RouteError) -> Self {
        Self::Route(source)
    }
}
impl From<ToolRouteError> for HarnessError {
    fn from(source: ToolRouteError) -> Self {
        Self::ToolRoute(source)
    }
}

#[cfg(test)]
mod tests;
