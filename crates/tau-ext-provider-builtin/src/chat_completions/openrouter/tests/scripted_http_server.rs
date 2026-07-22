//! Bounded, joined OpenRouter HTTP fixture.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

/// One finite localhost HTTP response with an exact captured request.
pub(super) struct ScriptedHttpServer {
    /// Bound loopback address.
    address: SocketAddr,
    /// Signals teardown connections that must not execute the script.
    shutdown: Arc<AtomicBool>,
    /// Bounded captured-request receiver.
    request_rx: mpsc::Receiver<String>,
    /// Joinable fixture worker.
    worker: Option<JoinHandle<()>>,
}

impl ScriptedHttpServer {
    /// Serves one fixed status and body, then closes the connection.
    pub(super) fn spawn(status: u16, body: impl Into<String>) -> Self {
        let body = body.into();
        Self::spawn_with_response(move |stream| {
            write!(
                stream,
                "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("fixture response");
        })
    }

    /// Sends a successful head and truncated body, then closes the connection.
    pub(super) fn spawn_truncated_success() -> Self {
        Self::spawn_with_response(|stream| {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{\"data\":[",
                )
                .expect("truncated fixture response");
        })
    }

    /// Starts one bounded request-capture worker with a response script.
    fn spawn_with_response(script: impl FnOnce(&mut TcpStream) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind OpenRouter fixture");
        let address = listener.local_addr().expect("OpenRouter fixture address");
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let (request_tx, request_rx) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("OpenRouter fixture request");
            if worker_shutdown.load(Ordering::SeqCst) {
                return;
            }
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("bounded fixture read");
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .expect("bounded fixture write");
            let request = read_http_head(&mut stream);
            script(&mut stream);
            request_tx.send(request).expect("request receiver");
        });
        Self {
            address,
            shutdown,
            request_rx,
            worker: Some(worker),
        }
    }

    /// Returns the bound address for redaction assertions.
    pub(super) fn address(&self) -> SocketAddr {
        self.address
    }

    /// Returns the explicit deterministic discovery endpoint.
    pub(super) fn url(&self) -> String {
        format!("http://{}/models", self.address)
    }

    /// Receives the bounded capture and joins the worker.
    pub(super) fn finish(mut self) -> String {
        let request = self
            .request_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("OpenRouter fixture request capture");
        self.join_worker();
        request
    }

    /// Joins the worker once.
    fn join_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !std::thread::panicking() {
                result.expect("OpenRouter fixture worker");
            }
        }
    }
}

impl Drop for ScriptedHttpServer {
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.shutdown.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(self.address);
        }
        self.join_worker();
    }
}

/// Reads one bounded HTTP request head.
fn read_http_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("request head");
        head.push(byte[0]);
        assert!(head.len() < 32 * 1024, "request head bound");
    }
    String::from_utf8(head).expect("ASCII request head")
}
