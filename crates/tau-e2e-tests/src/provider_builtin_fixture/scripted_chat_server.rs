//! Bounded, joined loopback Chat Completions server for production
//! provider-builtin acceptance.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use nix::poll::{PollFd, PollFlags, poll};

/// Outer deadlock guard for one causally signaled upstream request.
const REQUEST_WATCHDOG: Duration = Duration::from_secs(30);

/// One bounded Chat Completions request captured from the exact provider
/// binary.
#[derive(Debug)]
pub struct CapturedChatRequest {
    /// HTTP request method.
    pub method: String,
    /// Exact request target.
    pub path: String,
    /// Parsed JSON request body.
    pub body: serde_json::Value,
}

/// Closed three-step response script selected by the fixture.
#[derive(Clone, Copy, Debug)]
pub(super) enum Script {
    /// One throttle followed by two ordinary successful prompts.
    Retry,
    /// One single-tool round, one parallel-tool round, then visible output.
    Qwen,
}

/// Three-step bounded loopback Chat Completions response script.
#[derive(Debug)]
pub(super) struct ScriptedChatServer {
    /// Bound IPv4 loopback address.
    address: SocketAddr,
    /// Signals a teardown wake connection.
    shutdown: Arc<AtomicBool>,
    /// Number of complete upstream requests the script accepted.
    request_count: Arc<AtomicUsize>,
    /// Ordered wake source paired with the listener in the server worker.
    retry_release: Mutex<UnixStream>,
    /// Captured requests in upstream arrival order.
    request_rx: mpsc::Receiver<CapturedChatRequest>,
    /// Joined server worker.
    worker: Option<JoinHandle<Result<(), String>>>,
}

/// One immutable response in the closed three-request fixture script.
#[derive(Clone, Copy)]
enum ScriptStep {
    /// Initial one-day throttling response.
    Throttle,
    /// Successful completion of the manually retried first prompt.
    P1Success,
    /// Successful completion proving the shared cooldown is released.
    P2Success,
    /// Qwen reasoning followed by one function call.
    QwenSingleTool,
    /// Qwen continuation followed by two parallel function calls.
    QwenParallelTools,
    /// Qwen continuation followed by visible output and usage-only terminal
    /// data.
    QwenFinal,
}

impl ScriptedChatServer {
    /// Starts the selected closed three-response script on a private loopback
    /// port.
    pub(super) fn spawn(script: Script) -> Result<Self, Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let request_count = Arc::new(AtomicUsize::new(0));
        let worker_request_count = Arc::clone(&request_count);
        let (retry_release, mut worker_retry_release) = UnixStream::pair()?;
        let (request_tx, request_rx) = mpsc::sync_channel(4);
        let worker = std::thread::spawn(move || {
            let steps = match script {
                Script::Retry => [
                    ScriptStep::Throttle,
                    ScriptStep::P1Success,
                    ScriptStep::P2Success,
                ],
                Script::Qwen => [
                    ScriptStep::QwenSingleTool,
                    ScriptStep::QwenParallelTools,
                    ScriptStep::QwenFinal,
                ],
            };
            for step in steps {
                if matches!(step, ScriptStep::P1Success) {
                    require_accepted_retry_release(&mut worker_retry_release)?;
                }
                let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
                if worker_shutdown.load(Ordering::SeqCst) {
                    return Ok(());
                }
                configure_stream(&stream)?;
                let request = read_request(&mut stream)?;
                worker_request_count.fetch_add(1, Ordering::SeqCst);
                request_tx
                    .send(request)
                    .map_err(|_| "test stopped receiving captured requests".to_owned())?;
                write_scripted_response(&mut stream, step)?;
            }
            let (stream, _) = listener.accept().map_err(|error| error.to_string())?;
            if !worker_shutdown.load(Ordering::SeqCst) {
                drop(stream);
                return Err("provider issued more than three upstream requests".to_owned());
            }
            Ok(())
        });
        Ok(Self {
            address,
            shutdown,
            request_count,
            retry_release: Mutex::new(retry_release),
            request_rx,
            worker: Some(worker),
        })
    }

    /// Opens the P1-success server phase after accepted UI retry is observable.
    pub(super) fn release_accepted_retry(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.retry_release
            .lock()
            .map_err(|_| "retry release gate was poisoned")?
            .write_all(&[1])?;
        Ok(())
    }

    /// Returns the profile base URL.
    pub(super) fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    /// Receives one request through the single server watchdog.
    pub(super) fn recv_request(&self) -> Result<CapturedChatRequest, Box<dyn std::error::Error>> {
        self.request_rx
            .recv_timeout(REQUEST_WATCHDOG)
            .map_err(|error| {
                format!(
                    "timed out waiting for loopback Chat Completions request: {error}; \
                     accepted_requests={}; expected_profile=local/retry-model; \
                     credential=none; route=/v1/chat/completions",
                    self.request_count.load(Ordering::SeqCst)
                )
                .into()
            })
    }

    /// Requires that no request is already queued before a causal release.
    pub(super) fn require_no_ready_request(&self) -> Result<(), Box<dyn std::error::Error>> {
        match self.request_rx.try_recv() {
            Ok(request) => {
                Err(format!("unexpected upstream request before retry: {request:?}").into())
            }
            Err(mpsc::TryRecvError::Empty) => Ok(()),
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("Chat Completions server disconnected before retry".into())
            }
        }
    }

    /// Completes the exact script and joins its worker.
    pub(super) fn finish(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let request_count = self.request_count.load(Ordering::SeqCst);
        if request_count != 3 {
            return Err(format!(
                "loopback Chat Completions script consumed {request_count} requests, expected 3"
            )
            .into());
        }
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        let worker = self.worker.take().expect("server worker is available");
        worker
            .join()
            .map_err(|_| "Chat Completions server panicked")?
            .map_err(Into::into)
    }
}

/// Withholds request-2 processing and SSE until accepted retry is observable.
fn require_accepted_retry_release(release: &mut UnixStream) -> Result<(), String> {
    let mut descriptors = [PollFd::new(release.as_fd(), PollFlags::POLLIN)];
    let timeout = u16::try_from(REQUEST_WATCHDOG.as_millis())
        .map_err(|error| format!("invalid retry gate timeout: {error}"))?;
    if poll(&mut descriptors, timeout).map_err(|error| error.to_string())? == 0 {
        return Err("timed out waiting for accepted UI retry release".to_owned());
    }
    if !descriptors[0]
        .revents()
        .is_some_and(|events| events.contains(PollFlags::POLLIN))
    {
        return Err("retry release gate woke without a release".to_owned());
    }
    let mut byte = [0_u8; 1];
    release
        .read_exact(&mut byte)
        .map_err(|error| error.to_string())
}

impl Drop for ScriptedChatServer {
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.shutdown.store(true, Ordering::SeqCst);
            let _ = self.release_accepted_retry();
            let _ = TcpStream::connect(self.address);
            let _ = self
                .worker
                .take()
                .expect("server worker is available")
                .join();
        }
    }
}

/// Applies bounded I/O deadlines to one accepted upstream connection.
fn configure_stream(stream: &TcpStream) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| error.to_string())
}

/// Parses one bounded JSON HTTP request.
fn read_request(stream: &mut TcpStream) -> Result<CapturedChatRequest, String> {
    const MAX_HEAD_BYTES: usize = 32 * 1024;
    const MAX_BODY_BYTES: usize = 1024 * 1024;

    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("upstream closed before HTTP request headers".to_owned());
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > MAX_HEAD_BYTES {
            return Err("upstream HTTP request headers exceeded bound".to_owned());
        }
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let head = std::str::from_utf8(&bytes[..header_end]).map_err(|error| error.to_string())?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or("upstream omitted HTTP request line")?;
    let mut request_line = request_line.split_ascii_whitespace();
    let method = request_line
        .next()
        .ok_or("upstream omitted HTTP method")?
        .to_owned();
    let path = request_line
        .next()
        .ok_or("upstream omitted HTTP target")?
        .to_owned();
    if request_line.next().is_none() {
        return Err("upstream omitted HTTP version".to_owned());
    }
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .ok_or("upstream omitted Content-Length")?
        .1
        .trim()
        .parse::<usize>()
        .map_err(|error| format!("invalid Content-Length: {error}"))?;
    if MAX_BODY_BYTES < content_length {
        return Err("upstream HTTP request body exceeded bound".to_owned());
    }
    while bytes.len() < header_end + content_length {
        let read = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("upstream closed before complete HTTP request body".to_owned());
        }
        bytes.extend_from_slice(&chunk[..read]);
        if MAX_HEAD_BYTES + MAX_BODY_BYTES < bytes.len() {
            return Err("upstream HTTP request exceeded total bound".to_owned());
        }
    }
    let body = serde_json::from_slice(&bytes[header_end..header_end + content_length])
        .map_err(|error| format!("upstream request body is not JSON: {error}"))?;
    Ok(CapturedChatRequest { method, path, body })
}

/// Writes one bounded HTTP response from the selected fixed script.
fn write_scripted_response(stream: &mut TcpStream, step: ScriptStep) -> Result<(), String> {
    let (status, content_type, body, retry_after) = match step {
        ScriptStep::Throttle => (
            "429 Too Many Requests",
            "application/json",
            r#"{"error":{"code":"rate_limit_exceeded","type":"rate_limit_exceeded","message":"fixture throttle"}}"#,
            Some("86400"),
        ),
        ScriptStep::P1Success => (
            "200 OK",
            "text/event-stream",
            "data: {\"choices\":[{\"delta\":{\"content\":\"P1 complete\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n",
            None,
        ),
        ScriptStep::P2Success => (
            "200 OK",
            "text/event-stream",
            "data: {\"choices\":[{\"delta\":{\"content\":\"P2 complete\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n",
            None,
        ),
        ScriptStep::QwenSingleTool => (
            "200 OK",
            "text/event-stream",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"plan one\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"calling one\\n\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"qwen-call-1\",\"type\":\"function\",\"function\":{\"name\":\"restart_test_dummy\",\"arguments\":\" { } \"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
             data: [DONE]\n\n",
            None,
        ),
        ScriptStep::QwenParallelTools => (
            "200 OK",
            "text/event-stream",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"plan parallel\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"qwen-call-2\",\"type\":\"function\",\"function\":{\"name\":\"restart_test_dummy\",\"arguments\":\"{}\"}},{\"index\":1,\"id\":\"qwen-call-3\",\"type\":\"function\",\"function\":{\"name\":\"restart_test_dummy\",\"arguments\":\" {  } \"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
             data: [DONE]\n\n",
            None,
        ),
        ScriptStep::QwenFinal => (
            "200 OK",
            "text/event-stream",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"final plan\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"Qwen complete ✓\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
             data: {\"choices\":[],\"usage\":{\"prompt_tokens\":101,\"completion_tokens\":17,\"total_tokens\":118}}\n\n\
             data: [DONE]\n\n",
            None,
        ),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    )
    .map_err(|error| error.to_string())?;
    if let Some(retry_after) = retry_after {
        write!(stream, "Retry-After: {retry_after}\r\n").map_err(|error| error.to_string())?;
    }
    write!(stream, "\r\n{body}").map_err(|error| error.to_string())
}
