//! OAuth flows: auth-code + PKCE (manual paste) and device-code (polling).

mod error;

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use error::MAX_OAUTH_RESPONSE_BODY_BYTES;
pub use error::{OAuthError, OAuthErrorKind};
use rand::RngCore;
use rand::seq::SliceRandom;
use sha2::{Digest, Sha256};
use url::Url;

/// A ureq::Agent configured to respect proxy-related environment variables.
///
/// `ureq::Proxy::try_from_env` owns the environment parsing, including
/// `NO_PROXY` / `no_proxy` bypass rules.
pub fn proxy_agent() -> &'static ureq::Agent {
    static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
        let tls_config = ureq::tls::TlsConfig::builder()
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build();
        let mut builder = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .tls_config(tls_config);

        if let Some(proxy) = proxy_from_env() {
            builder = builder.proxy(Some(proxy));
        }

        ureq::Agent::new_with_config(builder.build())
    });
    &AGENT
}

fn proxy_from_env() -> Option<ureq::Proxy> {
    ureq::Proxy::try_from_env()
}

// ---------------------------------------------------------------------------
// PKCE helpers
// ---------------------------------------------------------------------------

/// Generate a random code verifier (64 unreserved characters).
fn generate_code_verifier() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::thread_rng();
    (0..64)
        .map(|_| {
            // CHARSET is non-empty (66 chars); `choose` only returns None
            // for empty slices.
            *CHARSET.choose(&mut rng).expect("non-empty CHARSET") as char
        })
        .collect()
}

/// Generate a random state parameter (32 hex chars).
fn generate_state() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Compute S256 code challenge from verifier.
fn code_challenge(verifier: &str) -> String {
    let hash = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hash)
}

// ---------------------------------------------------------------------------
// OpenAI Codex (Auth Code + PKCE, manual paste)
// ---------------------------------------------------------------------------

const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
/// Result of a successful OAuth token exchange.
pub struct OAuthTokens {
    /// Bearer token used for authenticated provider requests.
    pub access_token: String,
    /// Credential used to obtain a replacement access token.
    pub refresh_token: String,
    /// Milliseconds since the Unix epoch when the access token expires.
    pub expires_at_ms: u64,
    /// Provider account identifier associated with the token, when available.
    pub account_id: Option<String>,
}

/// Build the authorization URL for OpenAI Codex. Returns (url, state,
/// code_verifier) — the caller must present the URL to the user.
pub fn openai_codex_auth_url() -> (String, String, String) {
    let verifier = generate_code_verifier();
    let challenge = code_challenge(&verifier);
    let state = generate_state();

    let url = format!(
        "{OPENAI_AUTH_URL}?client_id={client_id}&redirect_uri={redirect}&response_type=code&scope={scope}&code_challenge={challenge}&code_challenge_method=S256&state={state}&codex_cli_simplified_flow=true&id_token_add_organizations=true",
        client_id = OPENAI_CLIENT_ID,
        redirect = urlencoding(OPENAI_REDIRECT_URI),
        scope = urlencoding("openid profile email offline_access"),
    );

    (url, state, verifier)
}

/// Parse the redirect URL pasted by the user. Extracts `code` and
/// `state` query parameters.
pub fn parse_redirect_url(input: &str) -> Result<(String, String), String> {
    // User might paste the full URL, just the path+query, or just the
    // query string. Require an explicit `?`/`/` prefix on the latter
    // forms so a stray `code=x&state=y` doesn't silently parse against
    // a dummy host (yielding a URL like `http://localhostcode=x...`
    // with neither parameter set).
    let trimmed = input.trim();
    let url = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        Url::parse(trimmed).map_err(|e| format!("invalid URL: {e}"))?
    } else if trimmed.starts_with('/') || trimmed.starts_with('?') {
        Url::parse(&format!("http://localhost{trimmed}"))
            .map_err(|e| format!("invalid URL fragment: {e}"))?
    } else {
        return Err("expected full URL, or path/query string starting with '/' or '?'".to_string());
    };

    let params: HashMap<_, _> = url.query_pairs().collect();
    let code = params
        .get("code")
        .ok_or("no 'code' parameter in URL")?
        .to_string();
    let state = params
        .get("state")
        .ok_or("no 'state' parameter in URL")?
        .to_string();

    Ok((code, state))
}

/// Exchanges an authorization code for OpenAI Codex tokens.
///
/// # Errors
///
/// Returns a bounded [`OAuthError`] for transport failure, HTTP rejection, or
/// an oversized, malformed, incorrectly encoded, or incomplete successful
/// response.
pub fn openai_codex_exchange(code: &str, verifier: &str) -> Result<OAuthTokens, OAuthError> {
    // `code` and `verifier` must be form-encoded: the code is an opaque
    // server-issued token that can legally contain `+`, `=`, `&`, etc.,
    // and a raw `+` in a form body would be decoded as a space on the
    // server, producing a spurious 400 from /oauth/token. (Our generated
    // verifier only uses unreserved chars, but encoding is harmless.)
    let body = format!(
        "grant_type=authorization_code&code={code}&code_verifier={verifier}&redirect_uri={redirect}&client_id={client_id}",
        code = urlencoding(code),
        verifier = urlencoding(verifier),
        redirect = urlencoding(OPENAI_REDIRECT_URI),
        client_id = OPENAI_CLIENT_ID,
    );

    let json = post_form(OPENAI_TOKEN_URL, &body)?;
    parse_openai_token_response(&json)
}

/// Refreshes an OpenAI Codex access token using the refresh token.
///
/// # Errors
///
/// Returns a bounded [`OAuthError`] for transport failure, HTTP rejection, or
/// an oversized, malformed, incorrectly encoded, or incomplete successful
/// response.
pub fn openai_codex_refresh(refresh_token: &str) -> Result<OAuthTokens, OAuthError> {
    let body = format!(
        "grant_type=refresh_token&refresh_token={refresh_token}&client_id={client_id}",
        refresh_token = urlencoding(refresh_token),
        client_id = OPENAI_CLIENT_ID,
    );

    let json = post_form(OPENAI_TOKEN_URL, &body)?;
    parse_openai_token_response(&json)
}

fn parse_openai_token_response(json: &serde_json::Value) -> Result<OAuthTokens, OAuthError> {
    let access_token = json["access_token"]
        .as_str()
        .ok_or_else(|| OAuthError::invalid_response("missing access_token"))?
        .to_string();
    let refresh_token = json["refresh_token"]
        .as_str()
        .ok_or_else(|| OAuthError::invalid_response("missing refresh_token"))?
        .to_string();
    let expires_in = json["expires_in"]
        .as_u64()
        .ok_or_else(|| OAuthError::invalid_response("missing expires_in"))?;

    let now_ms: u64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    let expires_at_ms = now_ms.saturating_add(expires_in.saturating_mul(1000));

    // Try to extract account_id from JWT claims.
    let account_id = extract_openai_account_id(&access_token);

    Ok(OAuthTokens {
        access_token,
        refresh_token,
        expires_at_ms,
        account_id,
    })
}

/// Decode URL-safe base64 without padding.
pub fn base64_url_safe_no_pad_decode(input: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(input).ok()
}

/// Decode JWT payload (no verification) to extract OpenAI account ID.
fn extract_openai_account_id(jwt: &str) -> Option<String> {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    claims
        .get("https://api.openai.com/auth")
        .and_then(|v| v.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

// ---------------------------------------------------------------------------
// GitHub Copilot (Device Code Flow)
// ---------------------------------------------------------------------------

const GITHUB_CLIENT_ID: &str = "Iv1.b507a08c887ecfe98";
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GITHUB_COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";

/// Device code flow step 1 response.
pub struct DeviceCodeResponse {
    /// Opaque code sent while polling the authorization endpoint.
    pub device_code: String,
    /// Short code displayed for the user to enter.
    pub user_code: String,
    /// Provider page where the user enters the short code.
    pub verification_uri: String,
    /// Initial number of seconds between polling attempts.
    pub interval: u64,
    /// Seconds from now until the device code expires.
    pub expires_in: u64,
}

/// Starts the GitHub device code flow.
///
/// # Errors
///
/// Returns a bounded [`OAuthError`] when the request fails or GitHub returns an
/// invalid or rejected device-code response.
pub fn github_device_code_start() -> Result<DeviceCodeResponse, OAuthError> {
    let body = format!("client_id={GITHUB_CLIENT_ID}&scope=read:user");

    let json = post_form_with_accept(GITHUB_DEVICE_CODE_URL, &body, "application/json")?;

    if let Some(err) = json["error"].as_str() {
        return Err(OAuthError::authorization(
            err,
            json["error_description"].as_str(),
        ));
    }

    let device_code = json["device_code"]
        .as_str()
        .ok_or_else(|| OAuthError::invalid_response("missing device_code"))?
        .to_string();
    let user_code = json["user_code"]
        .as_str()
        .ok_or_else(|| OAuthError::invalid_response("missing user_code"))?
        .to_string();
    let verification_uri = json["verification_uri"]
        .as_str()
        .ok_or_else(|| OAuthError::invalid_response("missing verification_uri"))?
        .to_string();
    let interval = json["interval"].as_u64().unwrap_or(5);
    // RFC 8628 requires `expires_in` but the GitHub flow has historically
    // returned ~15 minutes; fall back to that if the field is absent.
    let expires_in = json["expires_in"].as_u64().unwrap_or(900);

    Ok(DeviceCodeResponse {
        device_code,
        user_code,
        verification_uri,
        interval,
        expires_in,
    })
}

/// Poll for device code flow completion. Returns the access token on success,
/// or an error if the user does not authorize within `expires_in` seconds.
///
/// # Errors
///
/// Returns a bounded [`OAuthError`] when polling fails, GitHub rejects the
/// device code, or the flow expires.
pub fn github_device_code_poll(
    device_code: &str,
    interval: u64,
    expires_in: u64,
) -> Result<String, OAuthError> {
    let mut wait = Duration::from_secs(interval);
    let deadline = std::time::Instant::now() + Duration::from_secs(expires_in);

    loop {
        if std::time::Instant::now() >= deadline {
            return Err(OAuthError::timed_out(
                "device code expired before authorization completed",
            ));
        }
        std::thread::sleep(wait);

        let body = format!(
            "client_id={GITHUB_CLIENT_ID}&device_code={device_code}&grant_type=urn:ietf:params:oauth:grant-type:device_code"
        );

        let json = post_form_with_accept(GITHUB_TOKEN_URL, &body, "application/json")?;

        if let Some(token) = json["access_token"].as_str() {
            return Ok(token.to_string());
        }

        match json["error"].as_str() {
            Some("authorization_pending") => {} // keep polling
            Some("slow_down") => {
                wait = wait.mul_f32(1.4);
            }
            Some(err) => {
                return Err(OAuthError::authorization(err, None));
            }
            None => {
                return Err(OAuthError::invalid_response(
                    "unexpected response from GitHub",
                ));
            }
        }
    }
}

/// Exchanges a GitHub access token for a Copilot token.
///
/// # Errors
///
/// Returns a bounded [`OAuthError`] when the request fails or the response is
/// rejected, oversized, malformed, or missing required token fields.
pub fn github_copilot_token(github_token: &str) -> Result<OAuthTokens, OAuthError> {
    let resp = proxy_agent()
        .get(GITHUB_COPILOT_TOKEN_URL)
        .header("Authorization", format!("Bearer {github_token}"))
        .header("Accept", "application/json")
        .call()
        .map_err(OAuthError::transport)?;

    let json = read_success_json(resp)?;

    let token = json["token"]
        .as_str()
        .ok_or_else(|| OAuthError::invalid_response("missing token"))?
        .to_string();
    let expires_at = json["expires_at"]
        .as_u64()
        .ok_or_else(|| OAuthError::invalid_response("missing expires_at"))?;

    Ok(OAuthTokens {
        access_token: token,
        refresh_token: github_token.to_string(), // GitHub token is the "refresh" token
        expires_at_ms: expires_at * 1000,
        account_id: None,
    })
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

/// POST a form-encoded body and parse JSON response.
fn post_form(url: &str, body: &str) -> Result<serde_json::Value, OAuthError> {
    let resp = proxy_agent()
        .post(url)
        .content_type("application/x-www-form-urlencoded")
        .send(body)
        .map_err(OAuthError::transport)?;
    read_success_json(resp)
}

/// POST a form-encoded body with custom Accept header and parse JSON
/// response.
fn post_form_with_accept(
    url: &str,
    body: &str,
    accept: &str,
) -> Result<serde_json::Value, OAuthError> {
    let resp = proxy_agent()
        .post(url)
        .content_type("application/x-www-form-urlencoded")
        .header("Accept", accept)
        .send(body)
        .map_err(OAuthError::transport)?;
    read_success_json(resp)
}

fn map_status_error(
    mut resp: ureq::http::Response<ureq::Body>,
) -> Result<ureq::http::Response<ureq::Body>, OAuthError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }

    let body = read_bounded_oauth_body(resp.body_mut()).ok();
    Err(OAuthError::http(status.as_u16(), body.as_deref()))
}

/// Failure while reading one explicitly bounded OAuth response body.
enum OAuthBodyReadError {
    /// The response exceeded the configured byte cap.
    TooLarge,
    /// The complete bounded response was not valid UTF-8.
    InvalidEncoding,
    /// The transport failed before a complete body was available.
    Transport(ureq::Error),
}

fn read_bounded_oauth_body(body: &mut ureq::Body) -> Result<String, OAuthBodyReadError> {
    let bytes = body
        .with_config()
        .limit(MAX_OAUTH_RESPONSE_BODY_BYTES.saturating_add(1))
        .read_to_vec()
        .map_err(|error| match error {
            ureq::Error::BodyExceedsLimit(_) => OAuthBodyReadError::TooLarge,
            other => OAuthBodyReadError::Transport(other),
        })?;
    if bytes.len() > MAX_OAUTH_RESPONSE_BODY_BYTES as usize {
        return Err(OAuthBodyReadError::TooLarge);
    }
    String::from_utf8(bytes).map_err(|_| OAuthBodyReadError::InvalidEncoding)
}

/// Read a ureq response body as JSON.
fn read_success_json(
    resp: ureq::http::Response<ureq::Body>,
) -> Result<serde_json::Value, OAuthError> {
    let mut resp = map_status_error(resp)?;
    let text = read_bounded_oauth_body(resp.body_mut()).map_err(|error| match error {
        OAuthBodyReadError::TooLarge => {
            OAuthError::invalid_response("OAuth response exceeded size limit")
        }
        OAuthBodyReadError::InvalidEncoding => {
            OAuthError::invalid_response("OAuth response was not valid UTF-8")
        }
        OAuthBodyReadError::Transport(error) => OAuthError::transport(error),
    })?;
    serde_json::from_str(&text).map_err(OAuthError::invalid_response)
}

fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[cfg(test)]
mod tests;
