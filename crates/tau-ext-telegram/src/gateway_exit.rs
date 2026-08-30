//! Stable process-exit policy for the standalone Telegram gateway.

use std::fmt;
use std::process::ExitCode;

use crate::TelegramApiFailure;

/// Typed gateway failure mapped to a stable `sysexits(3)` process status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum GatewayExitError {
    /// Malformed CLI input or a missing token environment value.
    Usage(String),
    /// Another owner or Telegram mode currently makes polling unavailable.
    Unavailable(String),
    /// An unexpected internal invariant or Bot API response-shape failure.
    Software(String),
    /// Local filesystem, lock, state, or durability failure.
    Io(String),
    /// Transient Bot API failure that a supervisor may retry.
    Temporary(String),
    /// Semantically invalid or permanently rejected configuration.
    Config(String),
}

impl GatewayExitError {
    /// Return the stable process status assigned to this failure class.
    pub(super) fn exit_code(&self) -> ExitCode {
        ExitCode::from(match self {
            Self::Usage(_) => 64,
            Self::Unavailable(_) => 69,
            Self::Software(_) => 70,
            Self::Io(_) => 74,
            Self::Temporary(_) => 75,
            Self::Config(_) => 78,
        })
    }

    /// Classify a Bot API failure during webhook startup preflight.
    pub(super) fn webhook_preflight(error: TelegramApiFailure) -> Self {
        match error {
            TelegramApiFailure::Transport => {
                Self::Temporary("Telegram webhook preflight transport failure".to_owned())
            }
            TelegramApiFailure::Http { status, message }
                if matches!(status, 408 | 425 | 429) || (500..=599).contains(&status) =>
            {
                Self::Temporary(format!(
                    "Telegram webhook preflight returned HTTP {status}: {message}"
                ))
            }
            TelegramApiFailure::Http { status, message } => Self::Config(format!(
                "Telegram webhook preflight returned HTTP {status}: {message}"
            )),
            TelegramApiFailure::Protocol(message) => Self::Software(message),
        }
    }

    /// Classify a runtime poll failure that requires process termination.
    pub(super) fn runtime_poll(error: &TelegramApiFailure) -> Option<Self> {
        match error {
            TelegramApiFailure::Http {
                status: 409,
                message,
            } => Some(Self::Temporary(format!(
                "Telegram getUpdates returned HTTP 409: {message}"
            ))),
            _ => None,
        }
    }
}

impl fmt::Display for GatewayExitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message)
            | Self::Unavailable(message)
            | Self::Software(message)
            | Self::Io(message)
            | Self::Temporary(message)
            | Self::Config(message) => message.fmt(formatter),
        }
    }
}

impl std::error::Error for GatewayExitError {}

#[cfg(test)]
mod tests;
