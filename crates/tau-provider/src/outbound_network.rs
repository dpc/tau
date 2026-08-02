//! Immutable proxy and TLS policy shared by built-in provider transports.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::{fmt, fs as path_std_fs};

use reqwest::redirect as path_reqwest_redirect;
use rustls::crypto as path_rustls_crypto;
use url::{Host, Url};

const CUSTOM_CA_ENV: &str = "TAU_PROVIDER_CA_BUNDLE";
const MAX_CA_BUNDLE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CA_CERTIFICATES: usize = 256;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether an outbound request was configured for a direct or proxy route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboundRouteKind {
    /// Connect directly because no proxy applies or `NO_PROXY` matched.
    Direct,
    /// Connect only through the selected proxy.
    Proxy,
}

/// The bounded network phase which failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboundPhase {
    /// Startup environment, URL, proxy, or certificate configuration.
    Configure,
    /// DNS and socket connection establishment.
    Connect,
    /// Proxy TLS, authentication, or tunneling.
    Proxy,
    /// Target TLS establishment.
    Tls,
    /// HTTP request or WebSocket upgrade.
    Request,
    /// Bounded response body acquisition.
    Body,
}

/// Closed failure category for provider outbound routing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboundErrorKind {
    /// Startup proxy, bypass, target, or trust configuration was invalid.
    InvalidConfiguration,
    /// DNS, socket, proxy tunnel, TLS, or request I/O failed.
    Transport,
    /// The selected proxy returned HTTP 407.
    ProxyAuthentication,
    /// The absolute operation deadline elapsed.
    Deadline,
    /// A peer violated the required transport protocol.
    Protocol,
}

/// A route-scoped provider transport error safe for logs and user status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboundError {
    /// Direct or selected-proxy route; never an endpoint.
    route: OutboundRouteKind,
    /// Bounded phase in which the error occurred.
    phase: OutboundPhase,
    /// Closed programmatic category.
    kind: OutboundErrorKind,
    /// Fixed credential- and endpoint-free diagnostic.
    reason: &'static str,
}

impl OutboundError {
    fn new(
        route: OutboundRouteKind,
        phase: OutboundPhase,
        kind: OutboundErrorKind,
        reason: &'static str,
    ) -> Self {
        Self {
            route,
            phase,
            kind,
            reason,
        }
    }

    /// Return whether the failing request had selected a proxy route.
    #[must_use]
    pub fn route(&self) -> OutboundRouteKind {
        self.route
    }

    /// Return the bounded phase that failed.
    #[must_use]
    pub fn phase(&self) -> OutboundPhase {
        self.phase
    }

    /// Return the closed failure category.
    #[must_use]
    pub fn kind(&self) -> OutboundErrorKind {
        self.kind
    }
}

impl fmt::Display for OutboundError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let route = match self.route {
            OutboundRouteKind::Direct => "direct",
            OutboundRouteKind::Proxy => "proxy",
        };
        write!(
            formatter,
            "provider {route} transport failed during {:?}: {}",
            self.phase, self.reason
        )
    }
}

impl std::error::Error for OutboundError {}

/// Immutable startup snapshot of provider proxy routing and TLS trust.
pub struct OutboundNetworkPolicy {
    /// Prepared startup state, or the immutable configuration error observed
    /// while preparing it.
    snapshot: Result<PreparedPolicy, OutboundError>,
}

impl fmt::Debug for OutboundNetworkPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutboundNetworkPolicy")
            .field("configured", &self.snapshot.is_ok())
            .finish()
    }
}

/// Successfully prepared routing and trust state shared by constructed clients.
struct PreparedPolicy {
    /// Selected proxy for cleartext HTTP and WS targets.
    http_proxy: Option<ProxyEndpoint>,
    /// Selected proxy for TLS-protected HTTPS and WSS targets.
    https_proxy: Option<ProxyEndpoint>,
    /// Ordered, DNS-free bypass matchers.
    no_proxy: Vec<NoProxyEntry>,
    /// Platform verifier with any strictly parsed additive roots.
    tls: rustls::ClientConfig,
}

/// Parsed credential-free proxy endpoint and separately retained
/// authentication.
struct ProxyEndpoint {
    /// Credential-free HTTP or HTTPS proxy URL.
    endpoint: Url,
    /// Once-decoded basic credentials, when configured.
    credentials: Option<ProxyCredentials>,
}

/// Once-decoded HTTP Basic credentials for a selected proxy.
struct ProxyCredentials {
    /// Once-decoded basic-auth username.
    username: String,
    /// Once-decoded basic-auth password.
    password: String,
}

/// One syntactically matched `NO_PROXY` entry.
struct NoProxyEntry {
    /// Host, domain suffix, address, network, or wildcard matcher.
    host: NoProxyHost,
    /// Optional exact target-port constraint.
    port: Option<u16>,
}

/// DNS-free host component of a `NO_PROXY` matcher.
enum NoProxyHost {
    /// Match every target.
    Any,
    /// Match one DNS label boundary or any of its subdomains.
    Domain(String),
    /// Match one exact IP address.
    Ip(IpAddr),
    /// Match one IP network.
    Network(ipnet::IpNet),
}

impl OutboundNetworkPolicy {
    /// Capture proxy variables and the optional custom CA bundle once.
    #[must_use]
    pub fn from_env() -> Self {
        let mut environment = BTreeMap::new();
        for key in [
            "http_proxy",
            "HTTP_PROXY",
            "https_proxy",
            "HTTPS_PROXY",
            "all_proxy",
            "ALL_PROXY",
            "no_proxy",
            "NO_PROXY",
        ] {
            if let Some(value) = std::env::var_os(key) {
                let Ok(value) = value.into_string() else {
                    return Self {
                        snapshot: Err(config_error("proxy environment was not valid UTF-8")),
                    };
                };
                environment.insert(key.to_owned(), value);
            }
        }
        Self::from_environment(
            environment,
            std::env::var_os(CUSTOM_CA_ENV).map(PathBuf::from),
        )
    }

    /// Build an immutable policy from an already-captured environment map.
    ///
    /// Embedders and deterministic tests can use this constructor to avoid
    /// process-global environment mutation. Production provider startup uses
    /// [`Self::from_env`].
    #[must_use]
    pub fn from_environment(
        environment: BTreeMap<String, String>,
        ca_path: Option<PathBuf>,
    ) -> Self {
        Self {
            snapshot: prepare_policy(&environment, ca_path),
        }
    }

    /// Build a reqwest client whose route is fixed for `target`.
    ///
    /// The client disables reqwest's environment discovery and redirects. A
    /// selected proxy is the client's only route; failures never construct or
    /// retry a direct client. HTTP responses negotiate and decode gzip and
    /// zstd; callers observe decoded response chunks. An explicit caller
    /// `Accept-Encoding` header remains authoritative and suppresses the
    /// automatic advertisement.
    ///
    /// # Errors
    ///
    /// Returns a redacted configuration error when the startup snapshot, target
    /// URL, selected proxy, or client construction is invalid.
    pub fn client_for(&self, target: &str) -> Result<reqwest::Client, OutboundError> {
        let (builder, route_kind) = self.client_builder_for(target)?;
        Self::build_client(builder, route_kind)
    }

    /// Builds a client with an explicit resolver for deterministic
    /// route-failure acceptance without changing the production resolver
    /// boundary.
    #[cfg(test)]
    fn client_for_with_resolver<R>(
        &self,
        target: &str,
        resolver: Arc<R>,
    ) -> Result<reqwest::Client, OutboundError>
    where
        R: reqwest::dns::Resolve + 'static,
    {
        let (builder, route_kind) = self.client_builder_for(target)?;
        Self::build_client(builder.dns_resolver(resolver), route_kind)
    }

    /// Prepares the route-fixed reqwest builder shared by production and
    /// deterministic resolver-injection tests.
    fn client_builder_for(
        &self,
        target: &str,
    ) -> Result<(reqwest::ClientBuilder, OutboundRouteKind), OutboundError> {
        let prepared = self.snapshot.as_ref().map_err(Clone::clone)?;
        let target = Url::parse(target).map_err(|_| {
            OutboundError::new(
                OutboundRouteKind::Direct,
                OutboundPhase::Configure,
                OutboundErrorKind::InvalidConfiguration,
                "invalid target URL",
            )
        })?;
        let route = prepared.route_for(&target)?;
        let route_kind = route.map_or(OutboundRouteKind::Direct, |_| OutboundRouteKind::Proxy);
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(path_reqwest_redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .use_preconfigured_tls(prepared.tls.clone());
        if let Some(endpoint) = route {
            let mut proxy = reqwest::Proxy::all(endpoint.endpoint.as_str()).map_err(|_| {
                OutboundError::new(
                    route_kind,
                    OutboundPhase::Configure,
                    OutboundErrorKind::InvalidConfiguration,
                    "invalid proxy configuration",
                )
            })?;
            if let Some(credentials) = &endpoint.credentials {
                proxy = proxy.basic_auth(&credentials.username, &credentials.password);
            }
            builder = builder.proxy(proxy);
        }
        Ok((builder, route_kind))
    }

    /// Finalizes a route-fixed client without retaining reqwest diagnostics.
    fn build_client(
        builder: reqwest::ClientBuilder,
        route_kind: OutboundRouteKind,
    ) -> Result<reqwest::Client, OutboundError> {
        builder.build().map_err(|_| {
            OutboundError::new(
                route_kind,
                OutboundPhase::Configure,
                OutboundErrorKind::InvalidConfiguration,
                "unable to construct HTTP client",
            )
        })
    }

    /// Classify a client transport failure without exposing library
    /// diagnostics.
    #[must_use]
    pub fn request_error(&self, target: &str, phase: OutboundPhase) -> OutboundError {
        let route = self.route_kind(target).unwrap_or(OutboundRouteKind::Direct);
        OutboundError::new(
            route,
            phase,
            OutboundErrorKind::Transport,
            "network request failed",
        )
    }

    /// Classify a reqwest failure without retaining its URL or source chain.
    #[must_use]
    pub fn reqwest_error(
        &self,
        target: &str,
        phase: OutboundPhase,
        error: &reqwest::Error,
    ) -> OutboundError {
        let route = self.route_kind(target).unwrap_or(OutboundRouteKind::Direct);
        let phase = if route == OutboundRouteKind::Proxy && error.is_connect() {
            OutboundPhase::Proxy
        } else {
            phase
        };
        let (kind, reason) = if error.is_timeout() {
            (
                OutboundErrorKind::Deadline,
                "network request deadline elapsed",
            )
        } else {
            (OutboundErrorKind::Transport, "network request failed")
        };
        OutboundError::new(route, phase, kind, reason)
    }

    /// Classify an unambiguous status authored by the selected proxy.
    #[must_use]
    pub fn proxy_response_error(&self, target: &str, status: u16) -> Option<OutboundError> {
        if self.route_kind(target).ok()? != OutboundRouteKind::Proxy {
            return None;
        }
        let target = Url::parse(target).ok()?;
        if !matches!(target.scheme(), "http" | "ws") {
            // A visible HTTPS/WSS response is target-authored because CONNECT
            // already succeeded. Tunnel rejections are hidden reqwest connect
            // errors and intentionally remain Proxy/Transport.
            return None;
        }
        match status {
            407 => Some(OutboundError::new(
                OutboundRouteKind::Proxy,
                OutboundPhase::Proxy,
                OutboundErrorKind::ProxyAuthentication,
                "proxy authentication failed",
            )),
            _ => None,
        }
    }

    /// Build a redacted deadline error for the selected route.
    #[must_use]
    pub fn deadline_error(&self, target: &str, phase: OutboundPhase) -> OutboundError {
        let route = self.route_kind(target).unwrap_or(OutboundRouteKind::Direct);
        OutboundError::new(
            route,
            phase,
            OutboundErrorKind::Deadline,
            "network request deadline elapsed",
        )
    }

    /// Build a redacted protocol error for the selected route.
    #[must_use]
    pub fn protocol_error(&self, target: &str, phase: OutboundPhase) -> OutboundError {
        let route = self.route_kind(target).unwrap_or(OutboundRouteKind::Direct);
        OutboundError::new(
            route,
            phase,
            OutboundErrorKind::Protocol,
            "peer violated the transport protocol",
        )
    }

    /// Return the startup-selected route kind for a target URL.
    ///
    /// # Errors
    ///
    /// Returns the immutable startup configuration error or a redacted
    /// configuration error when `target` is invalid or unsupported.
    pub fn route_kind(&self, target: &str) -> Result<OutboundRouteKind, OutboundError> {
        let prepared = self.snapshot.as_ref().map_err(Clone::clone)?;
        let target = Url::parse(target).map_err(|_| {
            OutboundError::new(
                OutboundRouteKind::Direct,
                OutboundPhase::Configure,
                OutboundErrorKind::InvalidConfiguration,
                "invalid target URL",
            )
        })?;
        Ok(if prepared.route_for(&target)?.is_some() {
            OutboundRouteKind::Proxy
        } else {
            OutboundRouteKind::Direct
        })
    }
}

impl PreparedPolicy {
    fn route_for(&self, target: &Url) -> Result<Option<&ProxyEndpoint>, OutboundError> {
        if self.no_proxy.iter().any(|entry| entry.matches(target)) {
            return Ok(None);
        }
        match target.scheme() {
            "http" | "ws" => Ok(self.http_proxy.as_ref()),
            "https" | "wss" => Ok(self.https_proxy.as_ref()),
            _ => Err(OutboundError::new(
                OutboundRouteKind::Direct,
                OutboundPhase::Configure,
                OutboundErrorKind::InvalidConfiguration,
                "unsupported target URL scheme",
            )),
        }
    }
}

fn prepare_policy(
    environment: &BTreeMap<String, String>,
    ca_path: Option<PathBuf>,
) -> Result<PreparedPolicy, OutboundError> {
    let all_proxy = selected(environment, "all_proxy", "ALL_PROXY");
    let http_proxy = parse_proxy(selected(environment, "http_proxy", "HTTP_PROXY").or(all_proxy))?;
    let https_proxy =
        parse_proxy(selected(environment, "https_proxy", "HTTPS_PROXY").or(all_proxy))?;
    let no_proxy = parse_no_proxy(selected(environment, "no_proxy", "NO_PROXY"))?;
    let roots = load_custom_roots(ca_path)?;
    let provider = Arc::new(path_rustls_crypto::ring::default_provider());
    let verifier =
        rustls_platform_verifier::Verifier::new_with_extra_roots(roots, Arc::clone(&provider))
            .map_err(|_| {
                OutboundError::new(
                    OutboundRouteKind::Direct,
                    OutboundPhase::Configure,
                    OutboundErrorKind::InvalidConfiguration,
                    "unable to initialize platform certificate verifier",
                )
            })?;
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|_| {
            OutboundError::new(
                OutboundRouteKind::Direct,
                OutboundPhase::Configure,
                OutboundErrorKind::InvalidConfiguration,
                "unable to initialize TLS versions",
            )
        })?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(PreparedPolicy {
        http_proxy,
        https_proxy,
        no_proxy,
        tls,
    })
}

fn selected<'a>(
    environment: &'a BTreeMap<String, String>,
    lower: &str,
    upper: &str,
) -> Option<&'a str> {
    environment
        .get(lower)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            environment
                .get(upper)
                .filter(|value| !value.trim().is_empty())
        })
        .map(String::as_str)
}

fn parse_proxy(value: Option<&str>) -> Result<Option<ProxyEndpoint>, OutboundError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let mut endpoint = Url::parse(value).map_err(|_| proxy_config_error("invalid proxy URL"))?;
    if !matches!(endpoint.scheme(), "http" | "https")
        || endpoint.host().is_none()
        || endpoint.port_or_known_default().is_none()
        || !matches!(endpoint.path(), "" | "/")
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(proxy_config_error("invalid proxy URL"));
    }
    let credentials = if endpoint.username().is_empty() && endpoint.password().is_none() {
        None
    } else {
        let username = percent_decode(endpoint.username())
            .map_err(|_| proxy_config_error("invalid proxy credentials"))?;
        let password = percent_decode(endpoint.password().unwrap_or_default())
            .map_err(|_| proxy_config_error("invalid proxy credentials"))?;
        if username.contains(':')
            || username
                .chars()
                .chain(password.chars())
                .any(char::is_control)
        {
            return Err(proxy_config_error("invalid proxy credentials"));
        }
        Some(ProxyCredentials { username, password })
    };
    endpoint
        .set_username("")
        .map_err(|_| proxy_config_error("invalid proxy URL"))?;
    endpoint
        .set_password(None)
        .map_err(|_| proxy_config_error("invalid proxy URL"))?;
    Ok(Some(ProxyEndpoint {
        endpoint,
        credentials,
    }))
}

fn percent_decode(value: &str) -> Result<String, OutboundError> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(config_error("invalid proxy credentials"));
            }
            let high = hex(bytes[index + 1])?;
            let low = hex(bytes[index + 2])?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| config_error("invalid proxy credentials"))
}

fn hex(value: u8) -> Result<u8, OutboundError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(config_error("invalid proxy credentials")),
    }
}

fn parse_no_proxy(value: Option<&str>) -> Result<Vec<NoProxyEntry>, OutboundError> {
    value
        .unwrap_or_default()
        .split(',')
        .filter(|entry| !entry.trim().is_empty())
        .map(parse_no_proxy_entry)
        .collect()
}

fn parse_no_proxy_entry(value: &str) -> Result<NoProxyEntry, OutboundError> {
    let value = value.trim();
    if value == "*" {
        return Ok(NoProxyEntry {
            host: NoProxyHost::Any,
            port: None,
        });
    }
    let (host, port) = split_no_proxy_host_port(value)?;
    let host = if let Ok(network) = host.parse::<ipnet::IpNet>() {
        NoProxyHost::Network(network)
    } else if let Ok(ip) = host.parse::<IpAddr>() {
        NoProxyHost::Ip(ip)
    } else {
        let domain = host
            .strip_prefix("*.")
            .or_else(|| host.strip_prefix('.'))
            .unwrap_or(host)
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if domain.is_empty()
            || domain
                .split('.')
                .any(|label| label.is_empty() || label.starts_with('-') || label.ends_with('-'))
            || domain
                .bytes()
                .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')))
        {
            return Err(config_error("invalid NO_PROXY entry"));
        }
        NoProxyHost::Domain(domain)
    };
    Ok(NoProxyEntry { host, port })
}

fn split_no_proxy_host_port(value: &str) -> Result<(&str, Option<u16>), OutboundError> {
    if let Some(rest) = value.strip_prefix('[') {
        let (host, suffix) = rest
            .split_once(']')
            .ok_or_else(|| config_error("invalid NO_PROXY entry"))?;
        let port = if suffix.is_empty() {
            None
        } else {
            Some(
                suffix
                    .strip_prefix(':')
                    .ok_or_else(|| config_error("invalid NO_PROXY entry"))?
                    .parse()
                    .map_err(|_| config_error("invalid NO_PROXY entry"))?,
            )
        };
        return Ok((host, port));
    }
    if value.parse::<IpAddr>().is_ok() || value.contains('/') {
        return Ok((value, None));
    }
    match value.rsplit_once(':') {
        Some((host, port)) => Ok((
            host,
            Some(
                port.parse()
                    .map_err(|_| config_error("invalid NO_PROXY entry"))?,
            ),
        )),
        None => Ok((value, None)),
    }
}

impl NoProxyEntry {
    fn matches(&self, target: &Url) -> bool {
        if self
            .port
            .is_some_and(|port| target.port_or_known_default() != Some(port))
        {
            return false;
        }
        match (&self.host, target.host()) {
            (NoProxyHost::Any, Some(_)) => true,
            (NoProxyHost::Ip(expected), Some(Host::Ipv4(actual))) => {
                *expected == IpAddr::V4(actual)
            }
            (NoProxyHost::Ip(expected), Some(Host::Ipv6(actual))) => {
                *expected == IpAddr::V6(actual)
            }
            (NoProxyHost::Network(network), Some(Host::Ipv4(actual))) => {
                network.contains(&IpAddr::V4(actual))
            }
            (NoProxyHost::Network(network), Some(Host::Ipv6(actual))) => {
                network.contains(&IpAddr::V6(actual))
            }
            (NoProxyHost::Domain(expected), Some(Host::Domain(actual))) => {
                let actual = actual.trim_end_matches('.').to_ascii_lowercase();
                actual == *expected
                    || actual
                        .strip_suffix(expected)
                        .is_some_and(|prefix| prefix.ends_with('.'))
            }
            _ => false,
        }
    }
}

fn load_custom_roots(
    path: Option<PathBuf>,
) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, OutboundError> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    use std::io::Read;

    let file =
        path_std_fs::File::open(path).map_err(|_| config_error("unable to read CA bundle"))?;
    let mut bytes = Vec::new();
    file.take(MAX_CA_BUNDLE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| config_error("unable to read CA bundle"))?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_CA_BUNDLE_BYTES {
        return Err(config_error("invalid CA bundle"));
    }
    validate_pem_text(&bytes)?;
    let mut certificates = Vec::new();
    for item in rustls_pemfile::read_all(&mut bytes.as_slice()) {
        let item = item.map_err(|_| config_error("invalid CA bundle"))?;
        let rustls_pemfile::Item::X509Certificate(certificate) = item else {
            return Err(config_error("invalid CA bundle"));
        };
        certificates.push(certificate);
        if certificates.len() > MAX_CA_CERTIFICATES {
            return Err(config_error("invalid CA bundle"));
        }
    }
    if certificates.is_empty() {
        return Err(config_error("invalid CA bundle"));
    }
    let mut unique = BTreeSet::new();
    certificates.retain(|certificate| unique.insert(certificate.as_ref().to_vec()));
    Ok(certificates)
}

fn validate_pem_text(bytes: &[u8]) -> Result<(), OutboundError> {
    let text = std::str::from_utf8(bytes).map_err(|_| config_error("invalid CA bundle"))?;
    let mut inside = false;
    for line in text.lines() {
        match line.trim() {
            "-----BEGIN CERTIFICATE-----" if !inside => inside = true,
            "-----END CERTIFICATE-----" if inside => inside = false,
            "" if !inside => {}
            value
                if inside
                    && !value.is_empty()
                    && value.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=')
                    }) => {}
            _ => return Err(config_error("invalid CA bundle")),
        }
    }
    if inside {
        return Err(config_error("invalid CA bundle"));
    }
    Ok(())
}

fn config_error(reason: &'static str) -> OutboundError {
    OutboundError::new(
        OutboundRouteKind::Direct,
        OutboundPhase::Configure,
        OutboundErrorKind::InvalidConfiguration,
        reason,
    )
}

fn proxy_config_error(reason: &'static str) -> OutboundError {
    OutboundError::new(
        OutboundRouteKind::Proxy,
        OutboundPhase::Configure,
        OutboundErrorKind::InvalidConfiguration,
        reason,
    )
}

#[cfg(test)]
mod tests;
