//! Shared environment-aware HTTP agent used by provider clients.

use std::sync::LazyLock;

/// A ureq agent configured to respect proxy-related environment variables.
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

#[cfg(test)]
mod tests;
