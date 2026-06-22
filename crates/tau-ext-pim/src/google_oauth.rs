//! Shared Google OAuth2 device-flow and refresh-token helpers.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use tau_proto::SecretValue;
use url::form_urlencoded;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_DEVICE_CODE_URL: &str = "https://oauth2.googleapis.com/device/code";
const MAX_JSON_BODY_BYTES: usize = 1024 * 1024;
const MAX_OAUTH_FIELD_CHARS: usize = 4096;
const TOKEN_CACHE_SKEW: Duration = Duration::from_secs(60);

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
pub(crate) struct GoogleOauthClient {
    secrets: BTreeMap<String, SecretValue>,
    agent: ureq::Agent,
    access_token_cache: Mutex<BTreeMap<String, CachedAccessToken>>,
}

impl GoogleOauthClient {
    /// Build a Google OAuth client using the extension-authorized secrets.
    pub(crate) fn new(secrets: BTreeMap<String, SecretValue>) -> Self {
        Self {
            secrets,
            agent: google_http_agent(),
            access_token_cache: Mutex::new(BTreeMap::new()),
        }
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
        if let Some(secret_name) = config.client_secret_secret {
            body.append_pair("client_secret", &self.secret(secret_name)?);
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
        account_id: &str,
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
        account_id: &str,
        access_token: String,
        expires_in_secs: Option<u64>,
    ) -> Result<(), String> {
        self.cache_access_token(account_id, access_token, expires_in_secs)
    }

    /// Invalidate any cached access token for an account.
    pub(crate) fn invalidate_access_token(&self, account_id: &str) -> Result<(), String> {
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
        if let Some(secret_name) = client_secret_secret {
            body.append_pair("client_secret", &self.secret(secret_name)?);
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
            ));
        }
        let text = read_limited_body(&mut response, "Google token response")?;
        parse_access_token_response(&text, "Google token response")
    }

    fn cached_access_token(&self, account_id: &str) -> Result<Option<String>, String> {
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
        account_id: &str,
        access_token: String,
        expires_in_secs: Option<u64>,
    ) -> Result<(), String> {
        let expires_in_secs = expires_in_secs.unwrap_or(3600);
        if expires_in_secs <= TOKEN_CACHE_SKEW.as_secs() {
            return Ok(());
        }
        let Some(expires_at) = Instant::now().checked_add(Duration::from_secs(expires_in_secs))
        else {
            return Ok(());
        };
        let mut cache = self
            .access_token_cache
            .lock()
            .map_err(|_| "Google access token cache lock was poisoned".to_owned())?;
        cache.insert(
            account_id.to_owned(),
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

/// Build a bounded, TLS-verifying HTTP agent for Google API calls.
pub(crate) fn google_http_agent() -> ureq::Agent {
    let tls_config = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .http_status_as_error(false)
        .tls_config(tls_config)
        .build();
    ureq::Agent::new_with_config(config)
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
    if value.trim().is_empty()
        || MAX_OAUTH_FIELD_CHARS < value.chars().count()
        || value.chars().any(char::is_control)
    {
        return Err(format!("Google OAuth field `{field}` was invalid"));
    }
    Ok(value)
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
) -> String {
    let status = response.status().as_u16();
    let text = read_limited_body(response, context)
        .unwrap_or_else(|error| format!("failed to read error response: {error}"));
    let message = google_oauth_error_message(&text).unwrap_or_else(|| sanitize_error_text(&text));
    format!("{context} returned HTTP {status}: {message}")
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensures Google OAuth JSON parsing accepts Google's documented
    /// verification_uri spelling and keeps token values out of errors.
    #[test]
    fn parses_device_authorization_response() {
        let start = parse_device_auth_start(
            r#"{"device_code":"device","user_code":"ABCD-EFGH","verification_uri":"https://example.test","expires_in":900}"#,
        )
        .expect("device authorization response parses");
        assert_eq!(start.device_code, "device");
        assert_eq!(start.interval_secs, 5);
    }

    /// Ensures malformed token fields produce generic field names instead of
    /// echoing the unsafe token value into diagnostics.
    #[test]
    fn rejects_unsafe_oauth_field_without_echoing_value() {
        let err = parse_access_token_response(
            "{\"access_token\":\"secret\\u0001value\",\"expires_in\":3600}",
            "Google token response",
        )
        .expect_err("unsafe access token is rejected");
        assert_eq!(err, "Google OAuth field `access_token` was invalid");
        assert!(!err.contains("secret"));
    }

    /// Ensures malicious or malformed provider expiry values cannot panic the
    /// access-token cache by overflowing `Instant`.
    #[test]
    fn huge_expires_in_skips_cache_without_panicking() {
        let client = GoogleOauthClient::new(BTreeMap::new());
        client
            .prime_access_token_cache("work", "access-token".to_owned(), Some(u64::MAX))
            .expect("huge expiry is ignored");
        assert_eq!(
            client
                .cached_access_token("work")
                .expect("cache remains readable"),
            None
        );
    }
}
