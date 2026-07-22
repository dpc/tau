//! Bounded, joined loopback TCP script fixture.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

/// One loopback TCP script with teardown-owned accept wake and worker join.
pub(in crate::outbound_network::tests) struct ScriptedTcpServer<T> {
    /// Bound loopback address.
    address: SocketAddr,
    /// Signals teardown connections that must not execute the script.
    shutdown: Arc<AtomicBool>,
    /// Bounded script-result receiver.
    result_rx: mpsc::Receiver<T>,
    /// Joinable script worker.
    worker: Option<JoinHandle<()>>,
}

impl<T: Send + 'static> ScriptedTcpServer<T> {
    /// Starts one finite script after accepting a loopback connection.
    pub(in crate::outbound_network::tests) fn spawn(
        script: impl FnOnce(TcpStream) -> T + Send + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind scripted TCP server");
        let address = listener.local_addr().expect("scripted TCP address");
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("scripted TCP connection");
            if worker_shutdown.load(Ordering::SeqCst) {
                return;
            }
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("scripted TCP read timeout");
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .expect("scripted TCP write timeout");
            result_tx
                .send(script(stream))
                .expect("script result receiver");
        });
        Self {
            address,
            shutdown,
            result_rx,
            worker: Some(worker),
        }
    }

    /// Returns the bound loopback address.
    pub(in crate::outbound_network::tests) fn address(&self) -> SocketAddr {
        self.address
    }

    /// Receives the bounded script result and joins its worker.
    pub(in crate::outbound_network::tests) fn finish(mut self) -> T {
        let result = self
            .result_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("scripted TCP result");
        self.join_worker();
        result
    }
}

impl<T> ScriptedTcpServer<T> {
    /// Joins the worker once.
    fn join_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !std::thread::panicking() {
                result.expect("scripted TCP worker");
            }
        }
    }
}

impl<T> Drop for ScriptedTcpServer<T> {
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.shutdown.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(self.address);
        }
        self.join_worker();
    }
}
