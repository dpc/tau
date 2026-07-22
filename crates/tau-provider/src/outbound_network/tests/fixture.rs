//! Reusable loopback TLS fixtures for the outbound acceptance matrix.

mod direct_target_canary;
mod failing_proxy_resolver;
mod scripted_tcp_server;
mod test_ca;

use std::io::Read;

pub(super) use direct_target_canary::DirectTargetCanary;
pub(super) use failing_proxy_resolver::FailingProxyResolver;
pub(super) use scripted_tcp_server::ScriptedTcpServer;
pub(super) use test_ca::TestCa;

/// Reads one bounded HTTP/1 head from a scripted stream.
pub(super) fn read_http_head(stream: &mut impl Read) -> String {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("scripted HTTP head");
        head.push(byte[0]);
        assert!(head.len() < 32 * 1024, "scripted HTTP head is bounded");
    }
    String::from_utf8(head).expect("scripted HTTP head is ASCII")
}
