//! Reachable WSS target canary for no-direct-fallback assertions.

use std::net::{SocketAddr, TcpListener};

/// Reachable secure target whose listener detects forbidden direct fallback.
pub(super) struct DirectTargetCanary {
    /// Nonblocking listener that must remain untouched.
    listener: TcpListener,
    /// Bound target address.
    address: SocketAddr,
}

impl DirectTargetCanary {
    /// Binds one reachable target without accepting it.
    pub(super) fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind direct-target canary");
        listener
            .set_nonblocking(true)
            .expect("direct-target canary nonblocking");
        let address = listener.local_addr().expect("direct-target canary address");
        Self { listener, address }
    }

    /// Returns the HTTPS base URL that production lowering converts to WSS.
    pub(super) fn base_url(&self) -> String {
        format!("https://localhost:{}/backend-api", self.address.port())
    }

    /// Returns the target CONNECT authority.
    pub(super) fn authority(&self) -> String {
        format!("localhost:{}", self.address.port())
    }

    /// Asserts the selected proxy failure never reached the direct target.
    pub(super) fn assert_untouched(&self) {
        match self.listener.accept() {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Ok(_) => panic!("selected proxy failure silently reached direct WSS target"),
            Err(error) => panic!("direct-target canary accept failed: {error}"),
        }
    }
}
