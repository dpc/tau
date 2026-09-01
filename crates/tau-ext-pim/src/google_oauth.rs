//! Shared Google OAuth2 device-flow, installed-app PKCE, and refresh-token
//! helpers.
//!
//! The intentional Gmail/Calendar flow split is recorded in
//! `SPEC-tau-ext-pim-google-oauth`.

use ureq::tls as path_ureq_tls;

#[cfg(test)]
mod tests;
use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::seq::SliceRandom;
use rand::{Rng, RngCore};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tau_proto::SecretValue;
use url::{Url, form_urlencoded};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_DEVICE_CODE_URL: &str = "https://oauth2.googleapis.com/device/code";
const GOOGLE_AUTHORIZATION_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const MAX_JSON_BODY_BYTES: usize = 1024 * 1024;
const MAX_OAUTH_FIELD_CHARS: usize = 4096;
const MAX_REDIRECT_URL_CHARS: usize = 8192;
const PKCE_VERIFIER_LEN: usize = 64;
const PKCE_VERIFIER_MIN_CHARS: usize = 43;
const PKCE_VERIFIER_MAX_CHARS: usize = 128;
const GOOGLE_MAIL_SCOPE: &str = "https://mail.google.com/";
const TOKEN_CACHE_SKEW: Duration = Duration::from_secs(60);
const PKCE_VERIFIER_ALPHABET: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

/// Google OAuth secret references for one configured account.
#[derive(Clone, Copy)]
pub(crate) struct GoogleOauthSecretConfig<'a> {
    /// Secret containing the Google OAuth client id.
    pub(crate) client_id_secret: &'a str,
    /// Optional secret containing the Google OAuth client secret.
    pub(crate) client_secret_secret: Option<&'a str>,
    /// Optional secret containing a pre-provisioned refresh token.
    pub(crate) refresh_token_secret: Option<&'a str>,
}

/// User-facing information returned by Google device authorization start.
pub struct GoogleDeviceAuthStart {
    /// Provider device code used only by the extension to finish auth.
    pub device_code: String,
    /// User code to enter on Google's verification page.
    pub user_code: String,
    /// Verification URL to open manually.
    pub verification_uri: String,
    /// Number of seconds before the device authorization expires.
    pub expires_in_secs: u64,
    /// Suggested seconds to wait before retrying the finish action.
    pub interval_secs: u64,
}

/// Tokens returned by Google after device authorization completes.
pub struct GoogleDeviceAuthFinish {
    /// Long-lived refresh token to store in private extension state.
    pub refresh_token: String,
    /// Short-lived access token that can be primed into the in-memory cache.
    pub access_token: Option<String>,
    /// Seconds until the optional access token expires.
    pub expires_in_secs: Option<u64>,
}

/// User-facing information returned by Google installed-app authorization
/// start.
pub struct GoogleInstalledAppAuthStart {
    /// Authorization URL to open in a browser.
    pub authorization_url: String,
    /// OAuth state parameter stored privately until finish.
    pub state: String,
    /// RFC 7636 PKCE verifier stored privately until finish.
    pub pkce_verifier: String,
    /// Exact loopback redirect URI to send during token exchange.
    pub redirect_uri: String,
    /// Time before the pending authorization should expire.
    pub pending_lifetime: Duration,
}

/// Validated data extracted from a pasted installed-app redirect URL.
pub struct GoogleInstalledAppRedirect {
    /// One-time authorization code returned by Google.
    pub code: String,
}

impl std::fmt::Debug for GoogleInstalledAppRedirect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleInstalledAppRedirect")
            .field("code", &"<redacted>")
            .finish()
    }
}

/// Tokens returned by Google after installed-app authorization completes.
pub struct GoogleInstalledAppAuthFinish {
    /// Long-lived refresh token to store in private extension state.
    pub refresh_token: String,
    /// Short-lived access token that can be primed into the in-memory cache.
    pub access_token: Option<String>,
    /// Lifetime of the optional access token.
    pub access_token_lifetime: Option<Duration>,
}

#[derive(Debug)]
pub(crate) struct GoogleAccessToken {
    pub(crate) access_token: String,
    pub(crate) expires_in_secs: Option<u64>,
}

struct CachedAccessToken {
    access_token: String,
    expires_at: Instant,
}

/// Shared Google OAuth client with a per-account access-token cache.
pub(crate) struct GoogleOauthClient<AccountId> {
    secrets: BTreeMap<String, SecretValue>,
    agent: ureq::Agent,
    access_token_cache: Mutex<BTreeMap<AccountId, CachedAccessToken>>,
}

impl<AccountId> GoogleOauthClient<AccountId>
where
    AccountId: Clone + Ord,
{
    /// Build a Google OAuth client using the extension-authorized secrets.
    pub(crate) fn new(secrets: BTreeMap<String, SecretValue>) -> Self {
        Self {
            secrets,
            agent: google_http_agent(),
            access_token_cache: Mutex::new(BTreeMap::new()),
        }
    }

    /// Start Gmail installed-app authorization with PKCE.
    pub(crate) fn start_gmail_installed_app_auth(
        &self,
        config: GoogleOauthSecretConfig<'_>,
    ) -> Result<GoogleInstalledAppAuthStart, String> {
        let client_id = self.secret(config.client_id_secret)?;
        let redirect_uri = random_loopback_redirect_uri();
        let state = generate_oauth_state();
        let pkce_verifier = generate_pkce_verifier();
        let authorization_url = build_installed_app_authorization_url(
            &client_id,
            &redirect_uri,
            GOOGLE_MAIL_SCOPE,
            &state,
            &pkce_s256_challenge(&pkce_verifier),
        )?;
        Ok(GoogleInstalledAppAuthStart {
            authorization_url,
            state,
            pkce_verifier,
            redirect_uri,
            pending_lifetime: Duration::from_secs(10 * 60),
        })
    }

    /// Finish Gmail installed-app authorization by exchanging an auth code.
    pub(crate) fn finish_installed_app_auth(
        &self,
        config: GoogleOauthSecretConfig<'_>,
        code: &str,
        pkce_verifier: &str,
        redirect_uri: &str,
    ) -> Result<GoogleInstalledAppAuthFinish, String> {
        let client_id = self.secret(config.client_id_secret)?;
        let client_secret = config
            .client_secret_secret
            .map(|secret_name| self.secret(secret_name))
            .transpose()?;
        let body = build_installed_app_token_request_body(
            &client_id,
            client_secret.as_deref(),
            code,
            pkce_verifier,
            redirect_uri,
        );
        let mut response = self
            .agent
            .post(GOOGLE_TOKEN_URL)
            .content_type("application/x-www-form-urlencoded")
            .send(body)
            .map_err(|error| format!("finishing Google authorization failed: {error}"))?;
        if !response.status().is_success() {
            return Err(google_oauth_http_error(
                "finishing Google authorization",
                &mut response,
                &[
                    code,
                    pkce_verifier,
                    client_secret.as_deref().unwrap_or_default(),
                ],
            ));
        }
        let text = read_limited_body(&mut response, "Google authorization-code token response")?;
        parse_installed_app_token_response(&text)
    }

    /// Start Google device authorization for the requested OAuth scope.
    pub(crate) fn start_device_auth(
        &self,
        config: GoogleOauthSecretConfig<'_>,
        scope: &str,
    ) -> Result<GoogleDeviceAuthStart, String> {
        let client_id = self.secret(config.client_id_secret)?;
        let mut body = form_urlencoded::Serializer::new(String::new());
        body.append_pair("client_id", &client_id);
        body.append_pair("scope", scope);
        let mut response = self
            .agent
            .post(GOOGLE_DEVICE_CODE_URL)
            .content_type("application/x-www-form-urlencoded")
            .send(body.finish())
            .map_err(|error| format!("starting Google authorization failed: {error}"))?;
        if !response.status().is_success() {
            return Err(google_oauth_http_error(
                "starting Google authorization",
                &mut response,
                &[],
            ));
        }
        let text = read_limited_body(&mut response, "Google device authorization response")?;
        parse_device_auth_start(&text)
    }

    /// Finish Google device authorization after the user approves it.
    pub(crate) fn finish_device_auth(
        &self,
        config: GoogleOauthSecretConfig<'_>,
        device_code: &str,
    ) -> Result<GoogleDeviceAuthFinish, String> {
        let client_id = self.secret(config.client_id_secret)?;
        let mut body = form_urlencoded::Serializer::new(String::new());
        body.append_pair("client_id", &client_id);
        body.append_pair("device_code", device_code);
        body.append_pair("grant_type", "urn:ietf:params:oauth:grant-type:device_code");
        let client_secret = config
            .client_secret_secret
            .map(|secret_name| self.secret(secret_name))
            .transpose()?;
        if client_secret.is_some() {
            body.append_pair(
                "client_secret",
                client_secret.as_deref().unwrap_or_default(),
            );
        }
        let mut response = self
            .agent
            .post(GOOGLE_TOKEN_URL)
            .content_type("application/x-www-form-urlencoded")
            .send(body.finish())
            .map_err(|error| format!("finishing Google authorization failed: {error}"))?;
        if !response.status().is_success() {
            return Err(google_oauth_http_error(
                "finishing Google authorization",
                &mut response,
                &[device_code, client_secret.as_deref().unwrap_or_default()],
            ));
        }
        let text = read_limited_body(&mut response, "Google device token response")?;
        let json: Value = serde_json::from_str(&text)
            .map_err(|error| format!("Google device token response was not JSON: {error}"))?;
        let refresh_token =
            required_oauth_string(&json, "refresh_token", "Google device token response")?
                .to_owned();
        let access_token = optional_oauth_string(&json, "access_token")?.map(str::to_owned);
        let expires_in_secs = optional_oauth_u64(&json, "expires_in")?;
        Ok(GoogleDeviceAuthFinish {
            refresh_token,
            access_token,
            expires_in_secs,
        })
    }

    /// Return a cached or freshly refreshed Google access token.
    pub(crate) fn access_token(
        &self,
        account_id: &AccountId,
        config: GoogleOauthSecretConfig<'_>,
        stored_refresh_token: Option<&str>,
        not_authorized_message: &str,
    ) -> Result<String, String> {
        if let Some(access_token) = self.cached_access_token(account_id)? {
            return Ok(access_token);
        }
        let client_id = self.secret(config.client_id_secret)?;
        let refresh_token = self.refresh_token(
            config.refresh_token_secret,
            stored_refresh_token,
            not_authorized_message,
        )?;
        let access_token =
            self.exchange_refresh_token(&client_id, config.client_secret_secret, &refresh_token)?;
        self.cache_access_token(
            account_id,
            access_token.access_token.clone(),
            access_token.expires_in_secs,
        )?;
        Ok(access_token.access_token)
    }

    /// Prime the access token cache from a freshly completed OAuth flow.
    pub(crate) fn prime_access_token_cache(
        &self,
        account_id: &AccountId,
        access_token: String,
        expires_in_secs: Option<u64>,
    ) -> Result<(), String> {
        self.cache_access_token(account_id, access_token, expires_in_secs)
    }

    /// Prime the access-token cache with an already validated token lifetime.
    pub(crate) fn prime_access_token_cache_with_lifetime(
        &self,
        account_id: &AccountId,
        access_token: String,
        access_token_lifetime: Option<Duration>,
    ) -> Result<(), String> {
        self.cache_access_token_with_lifetime(account_id, access_token, access_token_lifetime)
    }

    /// Invalidate any cached access token for an account.
    pub(crate) fn invalidate_access_token(&self, account_id: &AccountId) -> Result<(), String> {
        let mut cache = self
            .access_token_cache
            .lock()
            .map_err(|_| "Google access token cache lock was poisoned".to_owned())?;
        cache.remove(account_id);
        Ok(())
    }

    fn refresh_token(
        &self,
        secret_name: Option<&str>,
        stored_refresh_token: Option<&str>,
        not_authorized_message: &str,
    ) -> Result<String, String> {
        if let Some(refresh_token) = stored_refresh_token {
            return Ok(refresh_token.to_owned());
        }
        let Some(secret_name) = secret_name else {
            return Err(not_authorized_message.to_owned());
        };
        self.secret(secret_name)
    }

    fn exchange_refresh_token(
        &self,
        client_id: &str,
        client_secret_secret: Option<&str>,
        refresh_token: &str,
    ) -> Result<GoogleAccessToken, String> {
        let mut body = form_urlencoded::Serializer::new(String::new());
        body.append_pair("client_id", client_id);
        body.append_pair("refresh_token", refresh_token);
        body.append_pair("grant_type", "refresh_token");
        let client_secret = client_secret_secret
            .map(|secret_name| self.secret(secret_name))
            .transpose()?;
        if client_secret.is_some() {
            body.append_pair(
                "client_secret",
                client_secret.as_deref().unwrap_or_default(),
            );
        }
        let mut response = self
            .agent
            .post(GOOGLE_TOKEN_URL)
            .content_type("application/x-www-form-urlencoded")
            .send(body.finish())
            .map_err(|error| format!("refreshing Google access token failed: {error}"))?;
        if !response.status().is_success() {
            return Err(google_oauth_http_error(
                "refreshing Google access token",
                &mut response,
                &[refresh_token, client_secret.as_deref().unwrap_or_default()],
            ));
        }
        let text = read_limited_body(&mut response, "Google token response")?;
        parse_access_token_response(&text, "Google token response")
    }

    fn cached_access_token(&self, account_id: &AccountId) -> Result<Option<String>, String> {
        let now = Instant::now();
        let mut cache = self
            .access_token_cache
            .lock()
            .map_err(|_| "Google access token cache lock was poisoned".to_owned())?;
        if let Some(cached) = cache.get(account_id)
            && now + TOKEN_CACHE_SKEW < cached.expires_at
        {
            return Ok(Some(cached.access_token.clone()));
        }
        cache.remove(account_id);
        Ok(None)
    }

    fn cache_access_token(
        &self,
        account_id: &AccountId,
        access_token: String,
        expires_in_secs: Option<u64>,
    ) -> Result<(), String> {
        self.cache_access_token_with_lifetime(
            account_id,
            access_token,
            expires_in_secs.map(Duration::from_secs),
        )
    }

    fn cache_access_token_with_lifetime(
        &self,
        account_id: &AccountId,
        access_token: String,
        access_token_lifetime: Option<Duration>,
    ) -> Result<(), String> {
        let access_token_lifetime =
            access_token_lifetime.unwrap_or_else(|| Duration::from_secs(3600));
        if access_token_lifetime <= TOKEN_CACHE_SKEW {
            return Ok(());
        }
        let Some(expires_at) = Instant::now().checked_add(access_token_lifetime) else {
            return Ok(());
        };
        let mut cache = self
            .access_token_cache
            .lock()
            .map_err(|_| "Google access token cache lock was poisoned".to_owned())?;
        cache.insert(
            account_id.clone(),
            CachedAccessToken {
                access_token,
                expires_at,
            },
        );
        Ok(())
    }

    fn secret(&self, name: &str) -> Result<String, String> {
        self.secrets
            .get(name)
            .map(|secret| secret.expose_secret().to_owned())
            .ok_or_else(|| format!("Google OAuth secret `{name}` was not provided"))
    }
}

/// Build a Google installed-app authorization URL.
pub(crate) fn build_installed_app_authorization_url(
    client_id: &str,
    redirect_uri: &str,
    scope: &str,
    state: &str,
    code_challenge: &str,
) -> Result<String, String> {
    validate_loopback_redirect_uri(redirect_uri)?;
    if !is_safe_oauth_parameter(client_id)
        || !is_safe_oauth_parameter(scope)
        || !is_safe_oauth_parameter(state)
        || !is_safe_oauth_parameter(code_challenge)
    {
        return Err("Google authorization URL input contained unsafe text".to_owned());
    }
    let mut url = Url::parse(GOOGLE_AUTHORIZATION_URL)
        .map_err(|error| format!("Google authorization endpoint was invalid: {error}"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", scope)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("state", state)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url.into())
}

/// Build the authorization-code token exchange body.
pub(crate) fn build_installed_app_token_request_body(
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    pkce_verifier: &str,
    redirect_uri: &str,
) -> String {
    let mut body = form_urlencoded::Serializer::new(String::new());
    body.append_pair("grant_type", "authorization_code");
    body.append_pair("client_id", client_id);
    if let Some(client_secret) = client_secret {
        body.append_pair("client_secret", client_secret);
    }
    body.append_pair("code", code);
    body.append_pair("code_verifier", pkce_verifier);
    body.append_pair("redirect_uri", redirect_uri);
    body.finish()
}

/// Parse and validate a pasted Google installed-app redirect URL.
pub(crate) fn parse_installed_app_redirect_url(
    pasted_url: &str,
    stored_redirect_uri: &str,
    expected_state: &str,
) -> Result<GoogleInstalledAppRedirect, String> {
    if pasted_url.trim().is_empty() || MAX_REDIRECT_URL_CHARS < pasted_url.chars().count() {
        return Err("Google redirect URL was empty or too long".to_owned());
    }
    let stored = validate_loopback_redirect_uri(stored_redirect_uri)?;
    let parsed =
        Url::parse(pasted_url).map_err(|_| "Google redirect URL was not a valid URL".to_owned())?;
    validate_installed_app_redirect_target(&parsed, &stored)?;

    let query = parse_installed_app_redirect_query(&parsed)?;
    let state = query
        .state
        .ok_or_else(|| "Google redirect URL was missing state".to_owned())?;
    if state != expected_state {
        return Err("Google redirect URL state did not match pending authorization".to_owned());
    }
    if let Some(error) = query.provider_error {
        let message = match error.as_str() {
            "access_denied" => "Google authorization was denied".to_owned(),
            _ => format!(
                "Google authorization failed: {}",
                sanitize_error_text(&error)
            ),
        };
        return Err(message);
    }
    let code = query
        .code
        .ok_or_else(|| "Google redirect URL was missing authorization code".to_owned())?;
    if !is_safe_oauth_parameter(&code) {
        return Err("Google redirect URL authorization code was invalid".to_owned());
    }
    Ok(GoogleInstalledAppRedirect { code })
}

#[derive(Default)]
struct InstalledAppRedirectQuery {
    /// State parameter supplied by Google and matched against pending auth
    /// state.
    state: Option<String>,
    /// One-time authorization code supplied by Google after user consent.
    code: Option<String>,
    /// Provider error code supplied by Google when user consent failed.
    provider_error: Option<String>,
}

fn validate_installed_app_redirect_target(parsed: &Url, stored: &Url) -> Result<(), String> {
    if parsed.scheme() != "http" || parsed.host_str() != Some("127.0.0.1") {
        return Err(
            "Google redirect URL must use the stored 127.0.0.1 loopback address".to_owned(),
        );
    }
    if parsed.port_or_known_default() != stored.port_or_known_default()
        || parsed.path() != stored.path()
        || parsed.fragment().is_some()
    {
        return Err("Google redirect URL did not match the stored loopback redirect".to_owned());
    }
    Ok(())
}

fn parse_installed_app_redirect_query(parsed: &Url) -> Result<InstalledAppRedirectQuery, String> {
    let mut query = InstalledAppRedirectQuery::default();
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "state" => set_unique_redirect_query_value(
                &mut query.state,
                value.into_owned(),
                "Google redirect URL contained duplicate state",
            )?,
            "code" => set_unique_redirect_query_value(
                &mut query.code,
                value.into_owned(),
                "Google redirect URL contained duplicate code",
            )?,
            "error" => set_unique_redirect_query_value(
                &mut query.provider_error,
                value.into_owned(),
                "Google redirect URL contained duplicate error",
            )?,
            _ => {}
        }
    }
    Ok(query)
}

fn set_unique_redirect_query_value(
    target: &mut Option<String>,
    value: String,
    duplicate_error: &str,
) -> Result<(), String> {
    if target.is_some() {
        return Err(duplicate_error.to_owned());
    }
    *target = Some(value);
    Ok(())
}

/// Validate a stored loopback redirect URI and return its parsed URL.
pub(crate) fn validate_loopback_redirect_uri(redirect_uri: &str) -> Result<Url, String> {
    let parsed =
        Url::parse(redirect_uri).map_err(|_| "Google redirect URI was invalid".to_owned())?;
    if parsed.scheme() != "http"
        || parsed.host_str() != Some("127.0.0.1")
        || parsed.port().is_none()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("Google redirect URI must be http://127.0.0.1:<port>/".to_owned());
    }
    Ok(parsed)
}

/// Return true when a PKCE verifier satisfies RFC 7636 verifier syntax.
pub(crate) fn is_valid_pkce_verifier(value: &str) -> bool {
    let len = value.chars().count();
    (PKCE_VERIFIER_MIN_CHARS..=PKCE_VERIFIER_MAX_CHARS).contains(&len)
        && value
            .bytes()
            .all(|byte| PKCE_VERIFIER_ALPHABET.contains(&byte))
}

/// Compute the RFC 7636 S256 challenge for a PKCE verifier.
pub(crate) fn pkce_s256_challenge(verifier: &str) -> String {
    let hash = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hash)
}

/// Build a bounded, TLS-verifying HTTP agent for Google API calls.
pub(crate) fn google_http_agent() -> ureq::Agent {
    let tls_config = path_ureq_tls::TlsConfig::builder()
        .root_certs(path_ureq_tls::RootCerts::PlatformVerifier)
        .build();
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .http_status_as_error(false)
        .tls_config(tls_config)
        .build();
    ureq::Agent::new_with_config(config)
}

fn parse_installed_app_token_response(text: &str) -> Result<GoogleInstalledAppAuthFinish, String> {
    let json: Value = serde_json::from_str(text).map_err(|error| {
        format!("Google authorization-code token response was not JSON: {error}")
    })?;
    let refresh_token = required_oauth_string(
        &json,
        "refresh_token",
        "Google authorization-code token response",
    )
    .map_err(|error| {
        if error.contains("missing refresh_token") {
            "Google did not return a refresh token; run `:email auth google start <account>` again and approve the consent prompt".to_owned()
        } else {
            error
        }
    })?
    .to_owned();
    let access_token = optional_oauth_string(&json, "access_token")?.map(str::to_owned);
    let access_token_lifetime = optional_oauth_u64(&json, "expires_in")?.map(Duration::from_secs);
    Ok(GoogleInstalledAppAuthFinish {
        refresh_token,
        access_token,
        access_token_lifetime,
    })
}

pub(crate) fn parse_device_auth_start(text: &str) -> Result<GoogleDeviceAuthStart, String> {
    let json: Value = serde_json::from_str(text)
        .map_err(|error| format!("Google device authorization response was not JSON: {error}"))?;
    let verification_uri = json
        .get("verification_uri")
        .or_else(|| json.get("verification_url"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            "Google device authorization response missing verification_uri".to_owned()
        })?;
    let expires_in_secs =
        required_oauth_u64(&json, "expires_in", "Google device authorization response")?;
    let interval_secs = optional_oauth_u64(&json, "interval")?.unwrap_or(5);
    if expires_in_secs == 0 || interval_secs == 0 {
        return Err("Google device authorization response had invalid timing".to_owned());
    }
    Ok(GoogleDeviceAuthStart {
        device_code: required_oauth_string(
            &json,
            "device_code",
            "Google device authorization response",
        )?
        .to_owned(),
        user_code: required_oauth_string(
            &json,
            "user_code",
            "Google device authorization response",
        )?
        .to_owned(),
        verification_uri: validated_oauth_string(verification_uri, "verification_uri")?.to_owned(),
        expires_in_secs,
        interval_secs,
    })
}

pub(crate) fn parse_access_token_response(
    text: &str,
    context: &str,
) -> Result<GoogleAccessToken, String> {
    let json: Value =
        serde_json::from_str(text).map_err(|error| format!("{context} was not JSON: {error}"))?;
    Ok(GoogleAccessToken {
        access_token: required_oauth_string(&json, "access_token", context)?.to_owned(),
        expires_in_secs: optional_oauth_u64(&json, "expires_in")?,
    })
}

fn required_oauth_string<'a>(
    json: &'a Value,
    field: &str,
    context: &str,
) -> Result<&'a str, String> {
    let value = json
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{context} missing {field}"))?;
    validated_oauth_string(value, field)
}

fn optional_oauth_string<'a>(json: &'a Value, field: &str) -> Result<Option<&'a str>, String> {
    let Some(value) = json.get(field) else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .ok_or_else(|| format!("Google OAuth field `{field}` was not a string"))?;
    validated_oauth_string(value, field).map(Some)
}

fn validated_oauth_string<'a>(value: &'a str, field: &str) -> Result<&'a str, String> {
    if !is_safe_oauth_parameter(value) {
        return Err(format!("Google OAuth field `{field}` was invalid"));
    }
    Ok(value)
}

fn is_safe_oauth_parameter(value: &str) -> bool {
    !value.trim().is_empty()
        && value.chars().count() <= MAX_OAUTH_FIELD_CHARS
        && !value.chars().any(char::is_control)
}

fn required_oauth_u64(json: &Value, field: &str, context: &str) -> Result<u64, String> {
    optional_oauth_u64(json, field)?.ok_or_else(|| format!("{context} missing {field}"))
}

fn optional_oauth_u64(json: &Value, field: &str) -> Result<Option<u64>, String> {
    let Some(value) = json.get(field) else {
        return Ok(None);
    };
    value
        .as_u64()
        .map(Some)
        .ok_or_else(|| format!("Google OAuth field `{field}` was not an integer"))
}

fn google_oauth_http_error(
    context: &str,
    response: &mut ureq::http::Response<ureq::Body>,
    sensitive_values: &[&str],
) -> String {
    let status = response.status().as_u16();
    let text = read_limited_body(response, context)
        .unwrap_or_else(|error| format!("failed to read error response: {error}"));
    format_google_oauth_http_error(context, status, &text, sensitive_values)
}

fn format_google_oauth_http_error(
    context: &str,
    status: u16,
    text: &str,
    sensitive_values: &[&str],
) -> String {
    let text = redact_exact_sensitive_values(text, sensitive_values);
    let message = google_oauth_error_message(&text).unwrap_or_else(|| sanitize_error_text(&text));
    format!("{context} returned HTTP {status}: {message}")
}

fn redact_exact_sensitive_values(text: &str, sensitive_values: &[&str]) -> String {
    let mut redacted = text.to_owned();
    for value in sensitive_values {
        if value.is_empty() {
            continue;
        }
        redacted = redacted.replace(value, "<redacted>");
    }
    redacted
}

pub(crate) fn google_oauth_error_message(text: &str) -> Option<String> {
    let json: Value = serde_json::from_str(text).ok()?;
    let error = json.get("error").and_then(Value::as_str)?;
    let safe_error = sanitize_error_text(error);
    let message = match error {
        "authorization_pending" => "Google authorization is still pending; approve it in the browser, then run the finish action again".to_owned(),
        "slow_down" => "Google asked to slow down; wait before running the finish action again".to_owned(),
        "expired_token" => "Google authorization expired; run the start action again".to_owned(),
        "access_denied" => "Google authorization was denied".to_owned(),
        _ => {
            let description = json
                .get("error_description")
                .and_then(Value::as_str)
                .map(sanitize_error_text)
                .filter(|value| !value.is_empty());
            if let Some(description) = description {
                format!("{safe_error}: {description}")
            } else {
                safe_error
            }
        }
    };
    Some(message)
}

fn read_limited_body(
    response: &mut ureq::http::Response<ureq::Body>,
    context: &str,
) -> Result<String, String> {
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_JSON_BODY_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("reading {context} failed: {error}"))?;
    if MAX_JSON_BODY_BYTES < bytes.len() {
        return Err(format!("{context} was too large"));
    }
    String::from_utf8(bytes).map_err(|_| format!("{context} was not valid UTF-8"))
}

fn sanitize_error_text(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn random_loopback_redirect_uri() -> String {
    let port = rand::thread_rng().gen_range(49152..=65535);
    format!("http://127.0.0.1:{port}/")
}

fn generate_pkce_verifier() -> String {
    let mut rng = rand::thread_rng();
    (0..PKCE_VERIFIER_LEN)
        .map(|_| {
            *PKCE_VERIFIER_ALPHABET
                .choose(&mut rng)
                .expect("PKCE alphabet is non-empty") as char
        })
        .collect()
}

fn generate_oauth_state() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
