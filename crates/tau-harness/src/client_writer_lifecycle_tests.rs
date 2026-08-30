//! Focused tests for bounded client-writer shutdown.

use std::io::Read as _;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crate::client_writer_lifecycle::ClientWriterLifecycle;
use crate::event::LiveConsumerHandle;

/// A consumer that never advances must hit the bounded close deadline rather
/// than hanging canonical harness shutdown.
#[test]
fn bounded_close_cancels_a_stalled_socket_consumer() {
    let consumer = LiveConsumerHandle::stalled_for_test();
    let (server, mut client) = UnixStream::pair().expect("socket pair");
    let lifecycle = ClientWriterLifecycle::socket(consumer, server);
    let started = Instant::now();

    let close = lifecycle
        .start_bounded_close(Duration::from_millis(20))
        .expect("spawn bounded close");
    close.join().expect("bounded close worker");

    assert!(started.elapsed() < Duration::from_secs(1));
    let mut byte = [0_u8; 1];
    assert_eq!(client.read(&mut byte).expect("observe socket shutdown"), 0);
}
