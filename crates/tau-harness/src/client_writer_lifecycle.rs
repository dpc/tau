//! Explicit transport ownership for one harness client writer.

use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::event::LiveConsumerHandle;

/// Grace allowed for a responsive socket to consume a fatal-startup Disconnect.
pub(crate) const STARTUP_DISCONNECT_GRACE: Duration = Duration::from_millis(100);
/// Grace allowed for attached UIs to consume their final shutdown terminal.
pub(crate) const FINAL_UI_DISCONNECT_GRACE: Duration = Duration::from_millis(100);

/// Owns the live cursor and any transport handle capable of canceling its I/O.
pub(crate) struct ClientWriterLifecycle {
    /// Cursor followed by the connection's writer thread.
    consumer: LiveConsumerHandle,
    /// Independently owned socket handle used to wake blocked read and write
    /// I/O.
    socket_shutdown: Option<UnixStream>,
}

impl ClientWriterLifecycle {
    /// Creates lifecycle ownership for a Unix-socket client.
    pub(crate) fn socket(consumer: LiveConsumerHandle, socket_shutdown: UnixStream) -> Self {
        Self {
            consumer,
            socket_shutdown: Some(socket_shutdown),
        }
    }

    /// Creates lifecycle ownership for generic stdio or pipe-backed client I/O.
    pub(crate) fn generic(consumer: LiveConsumerHandle) -> Self {
        Self {
            consumer,
            socket_shutdown: None,
        }
    }

    /// Waits until the writer processes every frame admitted through the
    /// current tail.
    pub(crate) fn flush(&self) {
        self.consumer.flush();
    }

    /// Starts bounded best-effort delivery through the current tail.
    ///
    /// The returned worker waits at most `grace`, then cancels an owned socket
    /// so a stalled writer cannot block harness shutdown. Generic transports
    /// lack cancellation but the worker still returns at the same deadline.
    pub(crate) fn start_bounded_close(self, grace: Duration) -> Option<thread::JoinHandle<()>> {
        self.consumer.close_after_current();
        let consumer = self.consumer;
        let socket_shutdown = self.socket_shutdown;
        let fallback_shutdown = socket_shutdown
            .as_ref()
            .and_then(|stream| stream.try_clone().ok());
        match thread::Builder::new()
            .name("tau-client-final-close".to_owned())
            .spawn(move || {
                let _retired = consumer.wait_for_retirement(grace);
                if let Some(stream) = socket_shutdown {
                    let _ = stream.shutdown(Shutdown::Both);
                }
            }) {
            Ok(handle) => Some(handle),
            Err(_) => {
                if let Some(stream) = fallback_shutdown {
                    let _ = stream.shutdown(Shutdown::Both);
                }
                None
            }
        }
    }

    /// Requests terminal delivery, then closes or cancels the owned transport.
    ///
    /// Unix sockets get a bounded best-effort delivery window followed by
    /// `shutdown`, which wakes blocked writer and reader syscalls. Generic
    /// writers have no equivalent cancellation primitive, so they retain
    /// their synchronous drain behavior.
    pub(crate) fn close_after_current_for_startup(self, grace: Duration) {
        self.consumer.close_after_current();
        let Some(socket_shutdown) = self.socket_shutdown else {
            self.consumer.flush();
            return;
        };

        let socket_shutdown = Arc::new(socket_shutdown);
        let worker_stream = Arc::clone(&socket_shutdown);
        let consumer = self.consumer;
        if thread::Builder::new()
            .name("tau-client-startup-close".to_owned())
            .spawn(move || {
                let _ = consumer.wait_for_retirement(grace);
                let _ = worker_stream.shutdown(Shutdown::Both);
            })
            .is_err()
        {
            // A failed watchdog spawn must fail closed rather than restore the
            // unbounded blocked-writer lifetime this owner exists to prevent.
            let _ = socket_shutdown.shutdown(Shutdown::Both);
        }
    }
}
