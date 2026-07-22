//! Deterministic selected-proxy DNS failure fixture.

use std::net::SocketAddr;
use std::sync::Mutex;

/// Resolver that fails the selected proxy name while making the direct target
/// reachable and recording every name reqwest attempts to resolve.
pub(in crate::outbound_network::tests) struct FailingProxyResolver {
    /// Reachable address returned only for the target canary name.
    target: SocketAddr,
    /// Exact names requested by reqwest.
    queries: Mutex<Vec<String>>,
}

impl FailingProxyResolver {
    /// Creates a resolver whose proxy lookup deterministically fails.
    pub(in crate::outbound_network::tests) fn new(target: SocketAddr) -> Self {
        Self {
            target,
            queries: Mutex::new(Vec::new()),
        }
    }

    /// Returns a stable snapshot of queried names.
    pub(in crate::outbound_network::tests) fn queries(&self) -> Vec<String> {
        self.queries.lock().expect("resolver query lock").clone()
    }
}

impl reqwest::dns::Resolve for FailingProxyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let name = name.as_str().to_owned();
        self.queries
            .lock()
            .expect("resolver query lock")
            .push(name.clone());
        if name == "target.invalid" {
            let target = self.target;
            return Box::pin(async move {
                Ok(Box::new(std::iter::once(target)) as reqwest::dns::Addrs)
            });
        }
        Box::pin(async move {
            Err(
                std::io::Error::new(std::io::ErrorKind::NotFound, "scripted proxy DNS failure")
                    .into(),
            )
        })
    }
}
