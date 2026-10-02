//! One-time authorization-code/PKCE transactions with account-bound callbacks.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore as _;
use sha2::{Digest as _, Sha256};
use url::Url;

use crate::{Error, ISSUER, RESOURCE};

/// Complete scope set, including identity and separate plan-use permission.
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// Initial registration entrypoint, never a saved or token-exchange client ID.
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";

/// Ephemeral values for exactly one sign-in attempt; deliberately not Debug.
pub struct Authorization {
    /// Fresh state bound to this attempt.
    state: String,
    /// Fresh OIDC nonce required in the signed ID token.
    pub(crate) nonce: String,
    /// Fresh PKCE secret used only at the token endpoint.
    pub(crate) verifier: String,
    /// Exact loopback URI retained through exchange.
    pub(crate) redirect_uri: String,
    /// Selected registration, absent on initial dynamic registration.
    client_id: Option<String>,
    /// Validated subject required on a returning sign-in.
    pub(crate) subject: Option<String>,
    /// Authorization deadline in Unix milliseconds.
    expires_at_ms: u64,
}

/// Verified callback material consumed by a single code exchange.
pub struct Callback {
    /// One-time authorization code; never rendered.
    pub(crate) code: String,
    /// Issued registration identifier, not the dynamic entrypoint.
    pub(crate) client_id: String,
    /// Transaction retains exact verifier, nonce and callback URI.
    pub(crate) authorization: Authorization,
}

impl Authorization {
    /// Create a transaction after the caller starts its loopback listener.
    ///
    /// `saved` binds returning sign-in to the exact issued client and verified
    /// subject. The host ID must already be durable before opening the browser.
    pub fn new(
        port: u16,
        host_id: &str,
        saved: Option<(&str, &str)>,
        request_consent: bool,
        now_ms: u64,
    ) -> Result<(Self, Url), Error> {
        if port == 0 || !valid_opaque(host_id) {
            return Err(Error::InvalidResponse);
        }
        if saved.is_some_and(|(client, subject)| !valid_client(client) || !valid_opaque(subject)) {
            return Err(Error::InvalidIdentity);
        }
        Self::bound(
            port,
            host_id,
            saved.map(|(client, _)| client),
            saved.map(|(_, subject)| subject),
            request_consent,
            now_ms,
        )
    }

    /// Retain issued registration authority independently of whether a first
    /// exchange has established a signed account subject yet.
    fn bound(
        port: u16,
        host_id: &str,
        client: Option<&str>,
        subject: Option<&str>,
        request_consent: bool,
        now_ms: u64,
    ) -> Result<(Self, Url), Error> {
        if port == 0 || !valid_opaque(host_id) {
            return Err(Error::InvalidResponse);
        }
        let this = Self {
            state: random_value(),
            nonce: random_value(),
            verifier: random_value(),
            redirect_uri: format!("http://127.0.0.1:{port}/auth/callback"),
            client_id: client.map(str::to_owned),
            subject: subject.map(str::to_owned),
            expires_at_ms: now_ms.saturating_add(10 * 60 * 1000),
        };
        let mut url = Url::parse(&format!("{ISSUER}/api/accounts/authorize"))
            .map_err(|_| Error::InvalidResponse)?;
        url.query_pairs_mut()
            .append_pair(
                "client_id",
                this.client_id.as_deref().unwrap_or(DYNAMIC_CLIENT),
            )
            .append_pair("ext_agent_host_id", host_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &this.redirect_uri)
            .append_pair("scope", SCOPES)
            .append_pair("resource", RESOURCE)
            .append_pair("state", &this.state)
            .append_pair("nonce", &this.nonce)
            .append_pair("code_challenge_method", "S256")
            .append_pair(
                "code_challenge",
                &URL_SAFE_NO_PAD.encode(Sha256::digest(this.verifier.as_bytes())),
            );
        if client.is_none() {
            url.query_pairs_mut().append_pair("agent_name_hint", "Tau");
        }
        if request_consent {
            url.query_pairs_mut().append_pair("prompt", "consent");
        }
        Ok((this, url))
    }

    /// Consume the transaction, rejecting duplicate parameters, denied consent,
    /// expired state, wrong callback addresses and changed returning clients.
    pub fn callback(self, callback: &Url, now_ms: u64) -> Result<Callback, Error> {
        let expected = Url::parse(&self.redirect_uri).map_err(|_| Error::InvalidCallback)?;
        if now_ms >= self.expires_at_ms
            || callback.scheme() != expected.scheme()
            || callback.host_str() != expected.host_str()
            || callback.port() != expected.port()
            || callback.path() != expected.path()
            || !callback.username().is_empty()
            || callback.password().is_some()
            || callback.fragment().is_some()
        {
            return Err(Error::InvalidCallback);
        }
        let mut params = BTreeMap::new();
        for (key, value) in callback.query_pairs() {
            if params
                .insert(key.into_owned(), value.into_owned())
                .is_some()
            {
                return Err(Error::InvalidCallback);
            }
        }
        if params.get("state") != Some(&self.state) {
            return Err(Error::InvalidCallback);
        }
        if let Some(error) = params.get("error") {
            return Err(if error == "access_denied" {
                Error::AccessDenied
            } else {
                Error::Rejected
            });
        }
        let client_id = match (&self.client_id, params.get("client_id")) {
            (Some(saved), Some(returned)) if saved != returned => {
                return Err(Error::AccountChanged);
            }
            (Some(saved), _) => saved.clone(),
            (None, Some(issued)) if valid_client(issued) => issued.clone(),
            _ => return Err(Error::InvalidCallback),
        };
        let code = params
            .remove("code")
            .filter(|code| valid_opaque(code))
            .ok_or(Error::InvalidCallback)?;
        Ok(Callback {
            code,
            client_id,
            authorization: self,
        })
    }
}

impl Callback {
    /// Prepare a fresh browser transaction for an invalid code grant without
    /// registering a second client or treating an unverified subject as saved.
    /// The caller must use it only after abandoning this callback's code.
    pub fn restart(
        &self,
        port: u16,
        host_id: &str,
        now_ms: u64,
    ) -> Result<(Authorization, Url), Error> {
        Authorization::bound(
            port,
            host_id,
            Some(&self.client_id),
            self.authorization.subject.as_deref(),
            false,
            now_ms,
        )
    }
}

/// Generate an opaque host identifier before first sign-in; callers persist it.
pub fn new_host_id() -> String {
    format!("tau-{}", random_value())
}

/// Generate 256 bits using the operating system seeded random generator.
fn random_value() -> String {
    let mut bytes = [0; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Issued OpenAI registrations use a distinct client namespace.
pub(crate) fn valid_client(value: &str) -> bool {
    value.starts_with("oaiapp_") && value.len() > 7 && valid_opaque(value)
}

/// Bound opaque protocol identifiers without interpreting their content.
pub(crate) fn valid_opaque(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 8192 && !value.chars().any(char::is_control)
}
