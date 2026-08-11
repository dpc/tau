use std::collections::BTreeMap;
use std::net as path_std_net;
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tungstenite::protocol::frame::Frame;
use tungstenite::protocol::frame::coding::{Data, OpCode};
use tungstenite::{Message, handshake as path_tungstenite_handshake};

use super::super::MAX_WS_EVENT_BYTES;

/// Finite behavior for one localhost WebSocket connection.
pub(super) enum ServerScript {
    /// Send each provider text frame after receiving one client request.
    Frames(Vec<String>),
    /// Send one text message split across exactly two wire frames.
    FragmentedText {
        /// Initial non-final text-frame payload.
        first: String,
        /// Final continuation-frame payload.
        second: String,
    },
    /// Keep the upgraded connection quiet until the client disconnects.
    Silent,
}

/// Captured production upgrade and request facts from one localhost peer.
#[derive(Default)]
pub(super) struct ServerCapture {
    /// Upgrade headers accepted from the provider client.
    pub(super) headers: BTreeMap<String, String>,
    /// Exact client text frames received by the peer.
    pub(super) requests: Vec<String>,
}

/// Bounded localhost WebSocket peer serving exactly one connection and request.
pub(super) struct TestWsServer {
    /// Loopback listener address.
    addr: SocketAddr,
    /// Facts captured by the server worker.
    capture: Arc<Mutex<ServerCapture>>,
    /// Notification emitted after the request frame is captured.
    request_rx: mpsc::Receiver<()>,
    /// Signals teardown before an upgrade reaches the peer.
    shutdown: Arc<AtomicBool>,
    /// Finite server worker joined before fixture teardown completes.
    worker: Option<JoinHandle<()>>,
}

impl TestWsServer {
    /// Starts a loopback-only peer with one finite scripted response.
    pub(super) fn spawn(script: ServerScript) -> Self {
        if let ServerScript::Frames(frames) = &script {
            assert!(frames.len() <= 65, "localhost provider frame bound");
            assert!(
                frames
                    .iter()
                    .all(|frame| frame.len() <= MAX_WS_EVENT_BYTES + 1),
                "localhost provider frame byte bound"
            );
        }
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind localhost WebSocket peer");
        let addr = listener.local_addr().expect("localhost WebSocket address");
        let capture = Arc::new(Mutex::new(ServerCapture::default()));
        let worker_capture = Arc::clone(&capture);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let (request_tx, request_rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            serve_one(
                listener,
                script,
                &worker_capture,
                &request_tx,
                &worker_shutdown,
            );
        });
        Self {
            addr,
            capture,
            request_rx,
            shutdown,
            worker: Some(worker),
        }
    }

    /// Returns an HTTP base URL that production lowering converts to `ws://`.
    pub(super) fn base_url(&self) -> String {
        format!("http://{}/backend-api", self.addr)
    }

    /// Waits for the single bounded client request to reach the peer.
    pub(super) fn wait_for_request(&self) {
        self.request_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("localhost WebSocket request");
    }

    /// Returns the shared capture for exact assertions.
    pub(super) fn capture(&self) -> Arc<Mutex<ServerCapture>> {
        Arc::clone(&self.capture)
    }

    /// Joins the finite peer and surfaces worker panics.
    pub(super) fn join(mut self) {
        self.join_worker();
    }

    /// Joins the worker once.
    fn join_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result.expect("localhost WebSocket peer");
            }
        }
    }
}

impl Drop for TestWsServer {
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.shutdown.store(true, Ordering::SeqCst);
            let _ = path_std_net::TcpStream::connect(self.addr);
        }
        self.join_worker();
    }
}

/// Accepts one upgrade, captures one text request, and executes the script.
fn serve_one(
    listener: TcpListener,
    script: ServerScript,
    capture: &Arc<Mutex<ServerCapture>>,
    request_tx: &mpsc::SyncSender<()>,
    shutdown: &AtomicBool,
) {
    let (stream, _) = listener.accept().expect("accept localhost WebSocket");
    if shutdown.load(Ordering::SeqCst) {
        return;
    }
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("bound localhost WebSocket reads");
    let mut headers = BTreeMap::new();
    let mut socket = tungstenite::accept_hdr(
        stream,
        #[allow(clippy::result_large_err)]
        |request: &path_tungstenite_handshake::server::Request, response| {
            headers = capture_headers(request.headers());
            Ok(response)
        },
    )
    .expect("upgrade localhost WebSocket");
    let request = loop {
        match socket.read().expect("read localhost WebSocket request") {
            Message::Text(text) => break text.to_string(),
            Message::Ping(payload) => socket
                .send(Message::Pong(payload))
                .expect("reply to client ping"),
            other => panic!("unexpected pre-request WebSocket frame: {other:?}"),
        }
    };
    {
        let mut capture = capture.lock().expect("localhost WebSocket capture");
        capture.headers = headers;
        capture.requests.push(request);
    }
    request_tx.send(()).expect("request observer");

    match script {
        ServerScript::Frames(frames) => {
            for frame in frames {
                if socket.send(Message::Text(frame.into())).is_err() {
                    break;
                }
            }
        }
        ServerScript::FragmentedText { first, second } => {
            if socket
                .send(Message::Frame(Frame::message(
                    first.into_bytes(),
                    OpCode::Data(Data::Text),
                    false,
                )))
                .is_ok()
            {
                let _ = socket.send(Message::Frame(Frame::message(
                    second.into_bytes(),
                    OpCode::Data(Data::Continue),
                    true,
                )));
            }
        }
        ServerScript::Silent => {
            while let Ok(message) = socket.read() {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        }
    }
}

/// Copies UTF-8 upgrade headers into a stable assertion map.
fn capture_headers(headers: &tungstenite::http::HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect()
}
