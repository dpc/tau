//! Process-local suppression for permanently rejected OAuth generations.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use tau_proto::ProviderName;
use tau_provider_codex::CodexMode;
use tau_provider_codex::oauth::OAuthError;

use crate::OpenAiAuth;

/// Exact profile credential generation associated with one refresh attempt.
#[derive(Eq, PartialEq)]
struct OAuthCredentialGeneration {
    /// Complete credential values loaded for the attempted refresh.
    auth: OpenAiAuth,
    /// Startup-selected Responses mode associated with the profile.
    responses_mode: CodexMode,
}

impl OAuthCredentialGeneration {
    fn matches(&self, auth: &OpenAiAuth, responses_mode: CodexMode) -> bool {
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
    fn reconcile(&mut self, provider: &ProviderName, auth: &OpenAiAuth, responses_mode: CodexMode) {
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
        responses_mode: CodexMode,
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
        responses_mode: CodexMode,
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

/// Typed result of resolving a Secret-RPC-backed ChatGPT credential refresh.
pub(crate) enum RefreshCredentialsError {
    /// Credential RPC or serialization failed.
    Storage(std::io::Error),
    /// The OAuth endpoint rejected the authoritative credential generation.
    OAuth {
        /// Credential generation read through Secret RPC.
        credentials: Box<OpenAiAuth>,
        /// Typed bounded OAuth failure with credential-safe formatting.
        error: OAuthError,
    },
    /// The authoritative generation already had a permanent rejection.
    Suppressed {
        /// Credential generation read through Secret RPC.
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
            | Self::Suppressed {
                credentials: _,
                error,
            } => Some(error),
        }
    }
}
