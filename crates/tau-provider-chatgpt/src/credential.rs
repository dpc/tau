//! Separate issued registrations survive logout without retaining usable
//! tokens.

use serde::{Deserialize, Serialize};

use crate::authorization::{valid_client, valid_opaque};
use crate::identity::Identity;
use crate::token_response::TokenResponse;
use crate::{Error, PLAN_SCOPE};

/// One account/workspace registration and its optional renewable session.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    /// Closed record version independent of legacy Codex records.
    version: u8,
    /// Issued OpenAI registration, never `dynamic_agent_client`.
    client_id: String,
    /// Signature-validated account subject; email cannot replace it.
    subject: String,
    /// Optional recognizable account label, not an authority.
    email: Option<String>,
    /// Latest granted scope set; empty for a signed-out registration.
    scopes: Vec<String>,
    /// Latest bearer token; absent after local logout.
    access_token: Option<String>,
    /// Latest renewable-session secret; absent after local logout.
    refresh_token: Option<String>,
    /// Last verified ID token, cleared on logout.
    id_token: Option<String>,
    /// Absolute bearer expiration in Unix milliseconds.
    expires_at_ms: u64,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatGptCredential(<redacted>)")
    }
}

impl Credential {
    /// Construct only after identity verification; identity-only grants remain
    /// saved but are never eligible for inference.
    pub(crate) fn from_login(
        client_id: String,
        identity: Identity,
        tokens: TokenResponse,
        now_ms: u64,
    ) -> Self {
        let mut credential = Self {
            version: 0,
            client_id,
            subject: identity.sub,
            email: identity.email,
            scopes: Vec::new(),
            access_token: None,
            refresh_token: None,
            id_token: None,
            expires_at_ms: 0,
        };
        credential.replace(tokens, now_ms);
        credential
    }

    /// Install a fully checked rotation without changing registration identity.
    pub(crate) fn replace(&mut self, tokens: TokenResponse, now_ms: u64) {
        self.scopes = tokens
            .scope
            .split_ascii_whitespace()
            .map(str::to_owned)
            .collect();
        self.access_token = Some(tokens.access_token);
        self.refresh_token = tokens.refresh_token;
        if tokens.id_token.is_some() {
            self.id_token = tokens.id_token;
        }
        self.expires_at_ms = now_ms.saturating_add(tokens.expires_in.saturating_mul(1000));
    }

    /// Parse a protected record without granting authority to malformed state.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let credential: Self = serde_json::from_slice(bytes).map_err(|_| Error::InvalidResponse)?;
        if credential.version != 0
            || !valid_client(&credential.client_id)
            || !valid_opaque(&credential.subject)
            || (credential.access_token.is_none() && credential.refresh_token.is_some())
            || credential
                .access_token
                .as_deref()
                .is_some_and(|token| !valid_opaque(token))
            || credential
                .refresh_token
                .as_deref()
                .is_some_and(|token| !valid_opaque(token))
        {
            return Err(Error::InvalidResponse);
        }
        Ok(credential)
    }

    /// Serialize only for a protected atomic credential publication.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        serde_json::to_vec(self).map_err(|_| Error::InvalidResponse)
    }

    /// Issued client retained across expiry, revocation and local logout.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Signature-validated subject used to pin returning login and retries.
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// Optional human-recognizable label, never a workspace identity.
    pub fn email(&self) -> Option<&str> {
        self.email.as_deref()
    }

    /// Require the granted direct-plan permission before exposing a bearer.
    pub fn access_token(&self) -> Result<&str, Error> {
        if !self.scopes.iter().any(|scope| scope == PLAN_SCOPE) {
            return Err(Error::PlanDisabled);
        }
        self.access_token.as_deref().ok_or(Error::InvalidGrant)
    }

    /// Renewable session token for one serialized refresh or revocation.
    pub(crate) fn refresh_token(&self) -> Result<&str, Error> {
        self.refresh_token.as_deref().ok_or(Error::InvalidGrant)
    }

    /// Whether renewal is needed before starting new inference.
    pub fn needs_refresh(&self, now_ms: u64) -> bool {
        self.expires_at_ms <= now_ms.saturating_add(30_000)
    }

    /// Erase all local tokens while preserving the account/client mapping.
    pub fn sign_out(&mut self) {
        self.access_token = None;
        self.refresh_token = None;
        self.id_token = None;
        self.scopes.clear();
        self.expires_at_ms = 0;
    }
}
