//! Generated certificate authority for scripted TLS layers.

/// A generated test-only certificate authority with reusable server issuance.
pub(in crate::outbound_network::tests) struct TestCa {
    /// Self-signed CA certificate.
    certificate: rcgen::Certificate,
    /// CA signing key.
    key: rcgen::KeyPair,
}

impl TestCa {
    /// Generates one isolated certificate authority.
    pub(in crate::outbound_network::tests) fn new() -> Self {
        let mut params =
            rcgen::CertificateParams::new(Vec::<String>::new()).expect("test CA params");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let key = rcgen::KeyPair::generate().expect("test CA key");
        let certificate = params.self_signed(&key).expect("test CA certificate");
        Self { certificate, key }
    }

    /// Returns the CA in PEM form for a provider bundle.
    pub(in crate::outbound_network::tests) fn pem(&self) -> String {
        self.certificate.pem()
    }

    /// Issues a TLS server certificate for `dns_name`.
    pub(in crate::outbound_network::tests) fn server_config(
        &self,
        dns_name: &str,
    ) -> rustls::ServerConfig {
        let leaf_key = rcgen::KeyPair::generate().expect("test leaf key");
        let leaf = rcgen::CertificateParams::new(vec![dns_name.to_owned()])
            .expect("test leaf params")
            .signed_by(&leaf_key, &self.certificate, &self.key)
            .expect("test leaf certificate");
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(
                    rustls::pki_types::PrivatePkcs8KeyDer::from(leaf_key.serialize_der()),
                ),
            )
            .expect("test TLS server")
    }
}
