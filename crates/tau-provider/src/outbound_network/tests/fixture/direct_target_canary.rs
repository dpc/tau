//! Reachable target canary for no-direct-fallback assertions.

use std::net::{SocketAddr, TcpListener};

/// Reachable direct target whose listener proves a selected proxy never fell
/// back to direct routing.
pub(in crate::outbound_network::tests) struct DirectTargetCanary {
    /// Nonblocking target listener.
    listener: TcpListener,
    /// Target address embedded in the request URL.
    address: SocketAddr,
}

impl DirectTargetCanary {
    /// Binds one reachable direct target without accepting it.
    pub(in crate::outbound_network::tests) fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind direct-target canary");
        listener
            .set_nonblocking(true)
            .expect("direct-target canary nonblocking");
        let address = listener.local_addr().expect("direct-target canary address");
        Self { listener, address }
    }

    /// Returns an HTTP target URL using the canary's port and supplied host.
    pub(in crate::outbound_network::tests) fn http_url(&self, host: &str) -> String {
        format!("http://{host}:{}/must-not-connect", self.address.port())
    }

    /// Returns the address a deterministic resolver may use for this target.
    pub(in crate::outbound_network::tests) fn address(&self) -> SocketAddr {
        self.address
    }

    /// Asserts no direct connection reached the target listener.
    pub(in crate::outbound_network::tests) fn assert_untouched(&self) {
        match self.listener.accept() {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Ok(_) => panic!("selected proxy failure silently reached the direct target"),
            Err(error) => panic!("direct-target canary accept failed: {error}"),
        }
    }
}
