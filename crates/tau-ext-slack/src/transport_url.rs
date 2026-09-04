//! Exact validated endpoint identities for Slack network transports.

/// Validated Slack Web API base retaining its exact request-building bytes.
#[derive(Clone)]
pub(super) struct SlackApiBaseUrl {
    /// Exact post-default, post-trailing-slash-removal endpoint bytes.
    raw: String,
    /// Parsed URL retained as the validation proof.
    _parsed: url::Url,
}

impl SlackApiBaseUrl {
    /// Validate one normalized API base without changing its retained bytes.
    pub(super) fn parse_exact(raw: String) -> Result<Self, String> {
        if raw.is_empty() {
            return Err("slack `api_base` must not be empty".to_owned());
        }
        let parsed = url::Url::parse(&raw)
            .map_err(|error| format!("slack `api_base` must be a valid URL: {error}"))?;
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err("slack `api_base` must not include userinfo".to_owned());
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err("slack `api_base` must not include query or fragment".to_owned());
        }
        match parsed.scheme() {
            "https" => {}
            "http" if parsed.host().is_some_and(is_loopback_host) => {}
            "http" => {
                return Err("slack `api_base` may use http only for loopback hosts".to_owned());
            }
            _ => {
                return Err(
                    "slack `api_base` must use https, or http for loopback tests".to_owned(),
                );
            }
        }
        Ok(Self {
            raw,
            _parsed: parsed,
        })
    }

    /// Build one Slack method URL with the existing byte-exact concatenation.
    pub(super) fn method_url(&self, method: &str) -> String {
        format!("{}/{method}", self.raw)
    }

    /// Borrow the exact validated API base bytes.
    #[cfg(test)]
    pub(super) fn raw(&self) -> &str {
        &self.raw
    }
}

/// Validated provider-issued one-use Slack Socket Mode URL.
pub(super) struct SlackSocketUrl {
    /// Exact provider-issued URL bytes, including ticket path and suffix data.
    raw: String,
    /// Parsed URL retained as the validation proof.
    _parsed: url::Url,
}

impl SlackSocketUrl {
    /// Validate one provider-issued Socket Mode URL without changing its bytes.
    pub(super) fn parse_exact(raw: String) -> Result<Self, String> {
        let parsed = url::Url::parse(&raw)
            .map_err(|error| format!("Slack Socket Mode URL is invalid: {error}"))?;
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err("Slack Socket Mode URL must not include userinfo".to_owned());
        }
        match parsed.scheme() {
            "wss" => {}
            "ws" if parsed.host().is_some_and(is_loopback_host) => {}
            "ws" => {
                return Err("Slack Socket Mode URL may use ws only for loopback hosts".to_owned());
            }
            _ => {
                return Err(
                    "Slack Socket Mode URL must use wss, or ws for loopback tests".to_owned(),
                );
            }
        }
        Ok(Self {
            raw,
            _parsed: parsed,
        })
    }

    /// Borrow the exact provider-issued bytes for the websocket handshake.
    pub(super) fn raw(&self) -> &str {
        &self.raw
    }

    /// Return whether this validated socket URL requires TLS.
    pub(super) fn uses_tls(&self) -> bool {
        self._parsed.scheme() == "wss"
    }
}

/// Return whether one parsed URL host is local loopback.
fn is_loopback_host(host: url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(addr) => addr.is_loopback(),
        url::Host::Ipv6(addr) => addr.is_loopback(),
    }
}
