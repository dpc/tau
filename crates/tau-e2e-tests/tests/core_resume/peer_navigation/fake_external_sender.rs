//! Bounded same-process sender callback fixture.

use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::fd::AsFd as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, poll};
use tau_e2e_tests::bounded_runtime_tempdir;
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
    /// Lifetime session-keyed runtime claim.
    _claim: tau_harness::runtime_dir::SessionClaim,
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
        let mut claim =
            tau_harness::runtime_dir::claim_session_in(runtime_home, runtime_home, sender_session)?;
        claim.reclaim_stale_socket()?;
        let listener = tau_socket::SocketListener::bind_fresh(claim.socket_path())?;
        let raw_listener = listener.try_clone_raw_listener()?;
        raw_listener.set_nonblocking(true)?;
        claim.publish(false)?;
        Ok(Self {
            listener,
            raw_listener,
            _claim: claim,
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
                        Some(HarnessInputMessage::Hello(hello))
                            if hello.client_name.as_str() == "tau-runtime-probe" =>
                        {
                            serve_runtime_probe(
                                stream,
                                &mut reader,
                                &self.expected.sender_session_id,
                                deadline,
                            )?;
                            continue;
                        }
                        Some(HarnessInputMessage::Hello(hello)) => {
                            assert_callback_hello(&hello, &self.expected.sender_session_id)?;
                        }
                        Some(other) => {
                            return Err(format!("expected callback hello, got {other:?}").into());
                        }
                    }
                    let mut writer = HarnessOutputWriter::new(BufWriter::new(DeadlineUnixWriter {
                        stream: stream.try_clone()?,
                        deadline,
                    }));
                    writer.write_message(&HarnessOutputMessage::SessionAccepted(
                        tau_proto::SessionAccepted {
                            session_id: self.expected.sender_session_id.clone(),
                        },
                    ))?;
                    writer.flush()?;
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

    /// Finishes successful callback use and releases runtime ownership.
    pub(super) fn finish(self) -> Result<(), std::io::Error> {
        Ok(())
    }
}

/// Serves the resolver's exact, read-only current-session probe.
fn serve_runtime_probe(
    stream: UnixStream,
    reader: &mut HarnessInputReader<BufReader<DeadlineUnixReader>>,
    session_id: &SessionId,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut writer =
        HarnessOutputWriter::new(BufWriter::new(DeadlineUnixWriter { stream, deadline }));
    writer.write_message(&HarnessOutputMessage::SessionAccepted(
        tau_proto::SessionAccepted {
            session_id: session_id.clone(),
        },
    ))?;
    writer.flush()?;
    let request = match reader.read_message()? {
        Some(HarnessInputMessage::GetCurrentSession(request)) => request,
        other => return Err(format!("expected current-session probe, got {other:?}").into()),
    };
    writer.write_message(&HarnessOutputMessage::CurrentSessionResult(
        tau_proto::CurrentSessionResult {
            request_id: request.request_id,
            session_id: session_id.clone(),
            project_root: std::env::current_dir()?,
        },
    ))?;
    writer.flush()?;
    Ok(())
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

fn assert_callback_hello(
    hello: &tau_proto::Hello,
    session_id: &SessionId,
) -> Result<(), Box<dyn std::error::Error>> {
    let expected = callback_hello(session_id);
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

fn callback_hello(session_id: &SessionId) -> tau_proto::Hello {
    tau_proto::Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: tau_proto::ExtensionName::parse(CALLBACK_CLIENT_NAME)
            .expect("callback client name must satisfy the identifier grammar"),
        client_kind: tau_proto::ClientKind::External,
        expected_session_id: Some(session_id.clone()),
        capabilities: Default::default(),
    }
}

/// Missing callbacks stop at the deadline and release the session claim.
#[test]
fn no_callback_is_bounded_and_cleans_metadata() -> Result<(), Box<dyn std::error::Error>> {
    let root = bounded_runtime_tempdir()?;
    let session = SessionId::parse("no-callback").expect("known-safe SessionId must be valid");
    let mut sender = FakeExternalSender::start(root.path(), &session, fixture_request(&session))?;
    let result = sender.authorize(Instant::now() + Duration::from_millis(20));
    assert!(result.is_err());
    drop(sender);
    assert!(tau_harness::runtime_dir::find_harness_for_session(session.as_str())?.is_none());
    Ok(())
}

/// A complete Hello without its auth frame stops at the same absolute deadline.
#[test]
fn post_hello_stall_is_bounded_and_cleans_metadata() -> Result<(), Box<dyn std::error::Error>> {
    let root = bounded_runtime_tempdir()?;
    let session = SessionId::parse("stalled-callback").expect("known-safe SessionId must be valid");
    let mut sender = FakeExternalSender::start(root.path(), &session, fixture_request(&session))?;
    let mut stalled = tau_socket::SocketPeer::connect(sender.listener.path())?;
    stalled.send(&HarnessInputMessage::Hello(callback_hello(&session)))?;
    let result = sender.authorize(Instant::now() + Duration::from_millis(20));
    assert!(result.is_err());
    drop(stalled);
    drop(sender);
    assert!(tau_harness::runtime_dir::find_harness_for_session(session.as_str())?.is_none());
    Ok(())
}

fn fixture_request(sender_session: &SessionId) -> tau_proto::ExternalAgentMessageRequest {
    tau_proto::ExternalAgentMessageRequest {
        request_id: REQUEST_ID.to_owned(),
        message_id: tau_proto::AgentMessageId::parse("fixture-message")
            .expect("test identifier must satisfy its grammar"),
        capability: "fixture-capability".to_owned(),
        sender_session_id: sender_session.clone(),
        sender_id: tau_proto::AgentId::parse("fixture-sender").expect("static agent id"),
        recipient_session_id: "fixture-recipient"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
        kind: tau_proto::AgentMessageKind::Message,
        message: "fixture-message".to_owned(),
    }
}
