//! OpenAI OAuth auth-code + PKCE exchange and refresh.

mod error;

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use error::MAX_OAUTH_RESPONSE_BODY_BYTES;
pub use error::{OAuthError, OAuthErrorKind};
use rand::RngCore;
use rand::seq::SliceRandom;
use sha2::{Digest, Sha256};
use url::Url;

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
pub fn openai_codex_exchange(
    code: &str,
    verifier: &str,
    network: &tau_provider::OutboundNetworkPolicy,
) -> Result<OAuthTokens, OAuthError> {
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

    let json = post_form(OPENAI_TOKEN_URL, &body, network)?;
    parse_openai_token_response(&json)
}

/// Refreshes an OpenAI Codex access token using the refresh token.
///
/// # Errors
///
/// Returns a bounded [`OAuthError`] for transport failure, HTTP rejection, or
/// an oversized, malformed, incorrectly encoded, or incomplete successful
/// response.
pub fn openai_codex_refresh(
    refresh_token: &str,
    network: &tau_provider::OutboundNetworkPolicy,
) -> Result<OAuthTokens, OAuthError> {
    let body = format!(
        "grant_type=refresh_token&refresh_token={refresh_token}&client_id={client_id}",
        refresh_token = urlencoding(refresh_token),
        client_id = OPENAI_CLIENT_ID,
    );

    let json = post_form(OPENAI_TOKEN_URL, &body, network)?;
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
// HTTP helpers
// ---------------------------------------------------------------------------

/// POST a form-encoded body and parse JSON response.
fn post_form(
    url: &str,
    body: &str,
    network: &tau_provider::OutboundNetworkPolicy,
) -> Result<serde_json::Value, OAuthError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(OAuthError::transport)?;
    runtime.block_on(async {
        let client = network.client_for(url).map_err(OAuthError::from_outbound)?;
        let response = client
            .post(url)
            .header("content-type", "application/x-www-form-urlencoded")
            .timeout(Duration::from_secs(30))
            .body(body.to_owned())
            .send()
            .await
            .map_err(|error| {
                OAuthError::from_outbound(network.reqwest_error(
                    url,
                    tau_provider::OutboundPhase::Request,
                    &error,
                ))
            })?;
        read_success_json(response, network, url).await
    })
}

async fn map_status_error(
    mut resp: reqwest::Response,
    network: &tau_provider::OutboundNetworkPolicy,
    url: &str,
) -> Result<reqwest::Response, OAuthError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }

    if let Some(error) = network.proxy_response_error(url, status.as_u16()) {
        return Err(OAuthError::from_outbound(error));
    }
    let body = read_bounded_oauth_body(&mut resp).await.ok();
    Err(OAuthError::http(status.as_u16(), body.as_deref()))
}

/// Failure while reading one explicitly bounded OAuth response body.
enum OAuthBodyReadError {
    /// The response exceeded the configured byte cap.
    TooLarge,
    /// The complete bounded response was not valid UTF-8.
    InvalidEncoding,
    /// The transport failed before a complete body was available.
    Transport(reqwest::Error),
}

async fn read_bounded_oauth_body(
    response: &mut reqwest::Response,
) -> Result<String, OAuthBodyReadError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(OAuthBodyReadError::Transport)?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_OAUTH_RESPONSE_BODY_BYTES as usize {
            return Err(OAuthBodyReadError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.len() > MAX_OAUTH_RESPONSE_BODY_BYTES as usize {
        return Err(OAuthBodyReadError::TooLarge);
    }
    String::from_utf8(bytes).map_err(|_| OAuthBodyReadError::InvalidEncoding)
}

/// Read a bounded asynchronous response body as JSON.
async fn read_success_json(
    resp: reqwest::Response,
    network: &tau_provider::OutboundNetworkPolicy,
    url: &str,
) -> Result<serde_json::Value, OAuthError> {
    let mut resp = map_status_error(resp, network, url).await?;
    let text = read_bounded_oauth_body(&mut resp)
        .await
        .map_err(|error| match error {
            OAuthBodyReadError::TooLarge => {
                OAuthError::invalid_response("OAuth response exceeded size limit")
            }
            OAuthBodyReadError::InvalidEncoding => {
                OAuthError::invalid_response("OAuth response was not valid UTF-8")
            }
            OAuthBodyReadError::Transport(error) => OAuthError::from_outbound(
                network.reqwest_error(url, tau_provider::OutboundPhase::Body, &error),
            ),
        })?;
    serde_json::from_str(&text).map_err(OAuthError::invalid_response)
}

fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[cfg(test)]
mod tests;
