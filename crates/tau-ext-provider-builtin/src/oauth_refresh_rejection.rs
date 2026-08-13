//! Process-local suppression for permanently rejected OAuth generations.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use tau_proto::ProviderName;
use tau_provider_codex::CodexMode;
use tau_provider_codex::oauth::OAuthError;

use crate::OpenAiAuth;

const MAX_UNAUTHORIZED_GENERATIONS_PER_PROVIDER: usize = 64;

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
    /// Bounded exact-generation unauthorized recovery state per provider.
    unauthorized: BTreeMap<ProviderName, UnauthorizedGenerations>,
}

/// Exact generations plus fail-closed overflow state for one provider.
#[derive(Default)]
struct UnauthorizedGenerations {
    /// Whether each observed generation spent its forced recovery.
    generations: BTreeMap<u64, bool>,
    /// Whether capacity exhaustion disables unknown-generation recovery.
    saturated: bool,
}

impl OAuthRefreshRejectionCache {
    /// Returns whether this provider has any remembered rejected generation.
    pub(crate) fn contains(&self, provider: &ProviderName) -> bool {
        self.providers.contains_key(provider)
    }

    /// Records one canonical provider 401 for an exact resolved credential
    /// identity.
    pub(crate) fn record_unauthorized(&mut self, provider: ProviderName, identity: u64) {
        let state = self.unauthorized.entry(provider).or_default();
        if state.generations.contains_key(&identity) || state.saturated {
            return;
        }
        if MAX_UNAUTHORIZED_GENERATIONS_PER_PROVIDER <= state.generations.len() {
            state.saturated = true;
            return;
        }
        state.generations.insert(identity, false);
    }

    /// Consumes forced-refresh authority only for the exact rejected identity.
    pub(crate) fn take_unauthorized(&mut self, provider: &ProviderName, identity: u64) -> bool {
        self.unauthorized.get_mut(provider).is_some_and(|state| {
            let Some(consumed) = state.generations.get_mut(&identity) else {
                return false;
            };
            if *consumed {
                return false;
            }
            *consumed = true;
            true
        })
    }

    /// Returns whether this exact rejected generation already spent recovery.
    pub(crate) fn unauthorized_exhausted(&self, provider: &ProviderName, identity: u64) -> bool {
        self.unauthorized.get(provider).is_some_and(|state| {
            state.saturated || state.generations.get(&identity).copied() == Some(true)
        })
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

    /// Clears only refresh-endpoint rejection state after successful rotation.
    pub(crate) fn clear_refresh_rejection(&mut self, provider: &ProviderName) {
        self.providers.remove(provider);
    }
}

/// Typed result of resolving a Secret-RPC-backed ChatGPT credential refresh.
pub(crate) enum RefreshCredentialsError {
    /// Credential RPC or serialization failed.
    Storage(std::io::Error),
    /// Refreshed or concurrently published credentials crossed identity.
    IdentityMismatch,
    /// Forced recovery retained the provider-rejected access token.
    RejectedGeneration,
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
            Self::IdentityMismatch => formatter.write_str("ChatGPT identity changed"),
            Self::RejectedGeneration => {
                formatter.write_str("ChatGPT access token was not replaced")
            }
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
            Self::IdentityMismatch => {
                formatter.write_str("RefreshCredentialsError::IdentityMismatch")
            }
            Self::RejectedGeneration => {
                formatter.write_str("RefreshCredentialsError::RejectedGeneration")
            }
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
            Self::IdentityMismatch | Self::RejectedGeneration => None,
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
