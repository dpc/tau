//! Bounded same-process sender callback fixture.

use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::fd::AsFd as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, poll};
use tau_proto::{
    HarnessInputMessage, HarnessInputReader, HarnessOutputMessage, HarnessOutputWriter, SessionId,
};

use super::{AUTH_REQUEST_ID, CALLBACK_CLIENT_NAME, REQUEST_ID};

/// Owns a private fake sender record and its synchronous callback listener.
pub(super) struct FakeExternalSender {
    /// Identity-checking owner that unlinks the callback socket on drop.
    listener: tau_socket::SocketListener,
    /// Nonblocking clone used for absolute-deadline accept readiness.
    raw_listener: UnixListener,
    /// Published runtime metadata removed by [`Self::finish`] or [`Drop`].
    metadata: Option<PathBuf>,
    /// Sole callback request this endpoint authorizes.
    expected: tau_proto::ExternalAgentMessageRequest,
}

impl FakeExternalSender {
    /// Publishes one private sender endpoint that authorizes only `expected`.
    pub(super) fn start(
        runtime_home: &Path,
        sender_session: &SessionId,
        expected: tau_proto::ExternalAgentMessageRequest,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let harnesses = runtime_home.join("tau/harnesses");
        std::fs::create_dir_all(&harnesses)?;
        let stem = harnesses.join(std::process::id().to_string());
        let listener = tau_socket::SocketListener::bind(stem.with_extension("sock"))?;
        let raw_listener = listener.try_clone_raw_listener()?;
        raw_listener.set_nonblocking(true)?;
        let metadata = stem.with_extension("json");
        std::fs::write(
            &metadata,
            serde_json::to_vec(&tau_harness::runtime_dir::DaemonMetadata {
                version: 0,
                pid: std::process::id(),
                project_root: None,
                session_id: sender_session.to_string(),
                peer_entrypoint: false,
            })?,
        )?;
        Ok(Self {
            listener,
            raw_listener,
            metadata: Some(metadata),
            expected,
        })
    }

    /// Serves the liveness probe and exact authenticated callback by
    /// `deadline`.
    pub(super) fn authorize(
        &mut self,
        deadline: Instant,
    ) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            wait_for_accept(&self.raw_listener, deadline)?;
            match self.raw_listener.accept() {
                Ok((stream, _)) => {
                    let reader_stream = stream.try_clone()?;
                    let mut reader = HarnessInputReader::new(BufReader::new(DeadlineUnixReader {
                        stream: reader_stream,
                        deadline,
                    }));
                    match reader.read_message()? {
                        // Targeted discovery first opens a payload-free liveness
                        // connection. Ignore only that clean empty connection.
                        None => continue,
                        Some(HarnessInputMessage::Hello(hello)) => {
                            assert_callback_hello(&hello)?;
                        }
                        Some(other) => {
                            return Err(format!("expected callback hello, got {other:?}").into());
                        }
                    }
                    let auth = match reader.read_message()? {
                        Some(HarnessInputMessage::ExternalAgentMessageAuth(auth)) => auth,
                        other => {
                            return Err(format!("expected callback auth, got {other:?}").into());
                        }
                    };
                    assert_callback_auth(&auth, &self.expected)?;
                    let mut writer = HarnessOutputWriter::new(BufWriter::new(DeadlineUnixWriter {
                        stream,
                        deadline,
                    }));
                    writer.write_message(&HarnessOutputMessage::ExternalAgentMessageAuthResult(
                        tau_proto::ExternalAgentMessageAuthResult {
                            request_id: auth.request_id,
                            authorized: true,
                            error: None,
                        },
                    ))?;
                    writer.flush()?;
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Removes the published metadata after successful callback use.
    pub(super) fn finish(mut self) -> Result<(), std::io::Error> {
        self.remove_metadata()
    }

    fn remove_metadata(&mut self) -> Result<(), std::io::Error> {
        if let Some(path) = self.metadata.take() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
}

impl Drop for FakeExternalSender {
    fn drop(&mut self) {
        let _ = self.remove_metadata();
    }
}

/// Waits for one listener notification without polling or exceeding `deadline`.
fn wait_for_accept(
    listener: &UnixListener,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("timed out waiting for external sender callback")?;
        let timeout_ms = remaining.as_millis().clamp(1, u16::MAX.into()) as u16;
        let mut descriptors = [PollFd::new(listener.as_fd(), PollFlags::POLLIN)];
        match poll(&mut descriptors, timeout_ms) {
            Ok(0) => {
                if Instant::now() >= deadline {
                    return Err("timed out waiting for external sender callback".into());
                }
            }
            Ok(_) => return Ok(()),
            Err(Errno::EINTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

/// Unix reader that reapplies one absolute deadline before every codec read.
struct DeadlineUnixReader {
    /// Accepted callback stream used by the input codec.
    stream: UnixStream,
    /// Absolute end shared by every partial read in this callback.
    deadline: Instant,
}

impl Read for DeadlineUnixReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "external callback deadline elapsed",
                )
            })?;
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(buffer)
    }
}

/// Unix writer that reapplies the same deadline before every codec write.
struct DeadlineUnixWriter {
    /// Accepted callback stream used by the output codec.
    stream: UnixStream,
    /// Absolute end shared by every partial write in this callback.
    deadline: Instant,
}

impl Write for DeadlineUnixWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "external callback deadline elapsed",
                )
            })?;
        self.stream.set_write_timeout(Some(remaining))?;
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "external callback deadline elapsed",
            ));
        }
        self.stream.flush()
    }
}

fn assert_callback_hello(hello: &tau_proto::Hello) -> Result<(), Box<dyn std::error::Error>> {
    let expected = callback_hello();
    if hello != &expected {
        return Err(format!("unexpected callback hello: {hello:?}").into());
    }
    Ok(())
}

fn assert_callback_auth(
    auth: &tau_proto::ExternalAgentMessageAuthRequest,
    expected: &tau_proto::ExternalAgentMessageRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    if auth.request_id != AUTH_REQUEST_ID
        || auth.message_id != expected.message_id
        || auth.capability != expected.capability
        || auth.sender_session_id != expected.sender_session_id
        || auth.sender_id != expected.sender_id
        || auth.recipient_session_id != expected.recipient_session_id
        || auth.recipient != expected.recipient
        || auth.kind != expected.kind
        || auth.message != expected.message
    {
        return Err(format!("unexpected external sender auth request: {auth:?}").into());
    }
    Ok(())
}

fn callback_hello() -> tau_proto::Hello {
    tau_proto::Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: CALLBACK_CLIENT_NAME.into(),
        client_kind: tau_proto::ClientKind::External,
        capabilities: Default::default(),
    }
}

/// Missing callbacks stop at the deadline and RAII removes sender metadata.
#[test]
fn no_callback_is_bounded_and_cleans_metadata() -> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let session = SessionId::from("no-callback");
    let mut sender = FakeExternalSender::start(root.path(), &session, fixture_request(&session))?;
    let metadata = sender.metadata.clone().ok_or("missing sender metadata")?;
    let result = sender.authorize(Instant::now() + Duration::from_millis(20));
    assert!(result.is_err());
    drop(sender);
    assert!(!metadata.exists());
    Ok(())
}

/// A complete Hello without its auth frame stops at the same absolute deadline.
#[test]
fn post_hello_stall_is_bounded_and_cleans_metadata() -> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let session = SessionId::from("stalled-callback");
    let mut sender = FakeExternalSender::start(root.path(), &session, fixture_request(&session))?;
    let metadata = sender.metadata.clone().ok_or("missing sender metadata")?;
    let mut stalled = tau_socket::SocketPeer::connect(sender.listener.path())?;
    stalled.send(&HarnessInputMessage::Hello(callback_hello()))?;
    let result = sender.authorize(Instant::now() + Duration::from_millis(20));
    assert!(result.is_err());
    drop(stalled);
    drop(sender);
    assert!(!metadata.exists());
    Ok(())
}

fn fixture_request(sender_session: &SessionId) -> tau_proto::ExternalAgentMessageRequest {
    tau_proto::ExternalAgentMessageRequest {
        request_id: REQUEST_ID.to_owned(),
        message_id: "fixture-message".into(),
        capability: "fixture-capability".to_owned(),
        sender_session_id: sender_session.clone(),
        sender_id: tau_proto::AgentId::parse("fixture-sender").expect("static agent id"),
        recipient_session_id: "fixture-recipient".into(),
        recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
        kind: tau_proto::AgentMessageKind::Message,
        message: "fixture-message".to_owned(),
    }
}
