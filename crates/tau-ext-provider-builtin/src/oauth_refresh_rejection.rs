//! Process-local suppression for permanently rejected OAuth generations.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use tau_proto::ProviderName;
use tau_provider_codex::oauth::OAuthError;
use tau_provider_codex::responses::ResponsesMode;

use crate::OpenAiAuth;

/// Exact profile credential generation associated with one refresh attempt.
#[derive(Eq, PartialEq)]
struct OAuthCredentialGeneration {
    /// Complete credential values loaded for the attempted refresh.
    auth: OpenAiAuth,
    /// Startup-selected Responses mode associated with the profile.
    responses_mode: ResponsesMode,
}

impl OAuthCredentialGeneration {
    fn matches(&self, auth: &OpenAiAuth, responses_mode: ResponsesMode) -> bool {
        self.auth == *auth && self.responses_mode == responses_mode
    }
}

/// One permanently rejected credential generation and its bounded typed error.
struct OAuthRefreshRejection {
    /// Exact credential/profile generation that the provider rejected.
    generation: OAuthCredentialGeneration,
    /// Error whose default formatting is credential-safe.
    error: OAuthError,
}

/// Process-local suppression state for permanent OAuth refresh rejections.
#[derive(Default)]
pub(crate) struct OAuthRefreshRejectionCache {
    /// Last permanently rejected generation for each provider namespace.
    providers: BTreeMap<ProviderName, OAuthRefreshRejection>,
}

impl OAuthRefreshRejectionCache {
    /// Returns whether this provider has any remembered rejected generation.
    pub(crate) fn contains(&self, provider: &ProviderName) -> bool {
        self.providers.contains_key(provider)
    }

    /// Removes stale rejection state after an observed credential/profile
    /// change.
    fn reconcile(
        &mut self,
        provider: &ProviderName,
        auth: &OpenAiAuth,
        responses_mode: ResponsesMode,
    ) {
        let changed = self
            .providers
            .get(provider)
            .is_some_and(|rejection| !rejection.generation.matches(auth, responses_mode));
        if changed {
            self.providers.remove(provider);
        }
    }

    /// Returns the bounded typed rejection for an exact unchanged generation.
    pub(crate) fn rejection(
        &mut self,
        provider: &ProviderName,
        auth: &OpenAiAuth,
        responses_mode: ResponsesMode,
    ) -> Option<OAuthError> {
        self.reconcile(provider, auth, responses_mode);
        self.providers
            .get(provider)
            .map(|rejection| rejection.error.clone())
    }

    /// Records an error only when it permanently rejects one authoritative
    /// locked generation.
    pub(crate) fn record_if_permanent(
        &mut self,
        provider: &ProviderName,
        auth: &OpenAiAuth,
        responses_mode: ResponsesMode,
        error: &OAuthError,
    ) {
        if !error.is_permanent_refresh_rejection() {
            return;
        }
        self.providers.insert(
            provider.clone(),
            OAuthRefreshRejection {
                generation: OAuthCredentialGeneration {
                    auth: auth.clone(),
                    responses_mode,
                },
                error: error.clone(),
            },
        );
    }

    /// Clears any rejection associated with a removed or non-ChatGPT profile.
    pub(crate) fn clear(&mut self, provider: &ProviderName) {
        self.providers.remove(provider);
    }
}

/// Typed result of resolving a filesystem-backed ChatGPT credential refresh.
pub(crate) enum RefreshCredentialsError {
    /// Provider profile storage or locking failed before authoritative reload.
    Storage(std::io::Error),
    /// The OAuth endpoint rejected the authoritative locked credentials.
    OAuth {
        /// Credential generation loaded while holding the auth-file lock.
        credentials: Box<OpenAiAuth>,
        /// Typed bounded OAuth failure with credential-safe formatting.
        error: OAuthError,
    },
    /// Refresh failed or was suppressed for the locked generation, and lock
    /// release then also failed.
    OAuthWithUnlockFailure {
        /// Credential generation loaded while holding the auth-file lock.
        credentials: Box<OpenAiAuth>,
        /// Typed bounded OAuth failure with credential-safe formatting.
        error: OAuthError,
        /// Failure returned while releasing the sidecar lock.
        unlock_error: std::io::Error,
    },
    /// Authoritative current or newly saved credentials are available, but lock
    /// release failed afterwards.
    CredentialsWithUnlockFailure {
        /// Credential generation loaded or saved while holding the auth-file
        /// lock.
        credentials: Box<OpenAiAuth>,
        /// Failure returned while releasing the sidecar lock.
        unlock_error: std::io::Error,
    },
    /// The authoritative locked generation already had a permanent rejection.
    Suppressed {
        /// Credential generation loaded while holding the auth-file lock.
        credentials: Box<OpenAiAuth>,
        /// Previously recorded typed failure with credential-safe formatting.
        error: OAuthError,
    },
}

impl fmt::Display for RefreshCredentialsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "credential storage failed: {error}"),
            Self::OAuth {
                credentials: _,
                error,
            } => error.fmt(formatter),
            Self::OAuthWithUnlockFailure {
                credentials: _,
                error,
                unlock_error: _,
            } => {
                write!(formatter, "{error}; credential lock release also failed")
            }
            Self::CredentialsWithUnlockFailure {
                credentials: _,
                unlock_error: _,
            } => formatter.write_str("credential lock release failed"),
            Self::Suppressed {
                credentials: _,
                error,
            } => {
                write!(
                    formatter,
                    "OAuth refresh suppressed for unchanged credentials: {error}"
                )
            }
        }
    }
}

impl fmt::Debug for RefreshCredentialsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => formatter
                .debug_tuple("RefreshCredentialsError::Storage")
                .field(error)
                .finish(),
            Self::OAuth {
                credentials: _,
                error,
            } => formatter
                .debug_tuple("RefreshCredentialsError::OAuth")
                .field(error)
                .finish(),
            Self::OAuthWithUnlockFailure {
                credentials: _,
                error,
                unlock_error,
            } => formatter
                .debug_struct("RefreshCredentialsError::OAuthWithUnlockFailure")
                .field("error", error)
                .field("unlock_error", unlock_error)
                .finish(),
            Self::CredentialsWithUnlockFailure {
                credentials: _,
                unlock_error,
            } => formatter
                .debug_struct("RefreshCredentialsError::CredentialsWithUnlockFailure")
                .field("unlock_error", unlock_error)
                .finish(),
            Self::Suppressed {
                credentials: _,
                error,
            } => formatter
                .debug_tuple("RefreshCredentialsError::Suppressed")
                .field(error)
                .finish(),
        }
    }
}

impl Error for RefreshCredentialsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::OAuth {
                credentials: _,
                error,
            }
            | Self::OAuthWithUnlockFailure {
                credentials: _,
                error,
                unlock_error: _,
            }
            | Self::Suppressed {
                credentials: _,
                error,
            } => Some(error),
            Self::CredentialsWithUnlockFailure {
                credentials: _,
                unlock_error,
            } => Some(unlock_error),
        }
    }
}

/// Lock-scoped outcome used to update process-local rejection state afterwards.
pub(crate) enum LockedRefreshOutcome {
    /// Current or newly refreshed credentials are available.
    Credentials(OpenAiAuth),
    /// This authoritative generation was already permanently rejected.
    Suppressed {
        /// Credential generation loaded while holding the auth-file lock.
        credentials: OpenAiAuth,
        /// Previously recorded typed failure.
        error: OAuthError,
    },
    /// This authoritative generation's refresh attempt failed.
    Rejected {
        /// Credential generation loaded while holding the auth-file lock.
        credentials: OpenAiAuth,
        /// Typed OAuth failure returned by the endpoint.
        error: OAuthError,
    },
}
