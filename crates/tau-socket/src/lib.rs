//! Unix socket listener and transport adapters.
//!
//! This crate exposes a small transport-agnostic socket peer that reuses the
//! same self-delimiting CBOR event codec as stdio transports.

use std::io::{self, BufWriter};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;
use std::{fmt, fs, thread};

use tau_proto::{
    DecodeError, HarnessInputMessage, HarnessInputReader, HarnessOutputMessage,
    HarnessOutputWriter, PeerInputReader, PeerOutputWriter,
};

/// Errors returned by the Unix socket transport.
#[derive(Debug)]
pub enum SocketTransportError {
    /// Creating the parent directory for a socket path failed.
    CreateParentDirectory {
        /// Directory that could not be created.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// A pre-existing non-socket path blocked binding the listener.
    RefuseNonSocketPath {
        /// Path that already existed but was not a Unix socket.
        path: PathBuf,
    },
    /// A pre-existing Unix socket accepted connections and was treated as live.
    ActiveSocketExists {
        /// Socket path that appears to already have a listener.
        path: PathBuf,
    },
    /// A pre-existing Unix socket could not be proven inactive.
    ProbeExistingSocket {
        /// Socket path whose liveness probe failed.
        path: PathBuf,
        /// Underlying probe error.
        source: io::Error,
    },
    /// Removing an inactive stale Unix socket path failed.
    RemoveStaleSocket {
        /// Stale socket path that could not be removed.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// Binding the Unix listener failed.
    Bind {
        /// Socket path that could not be bound.
        path: PathBuf,
        /// Underlying bind error.
        source: io::Error,
    },
    /// Reading metadata for the bound socket path failed.
    BoundSocketMetadata {
        /// Socket path whose metadata could not be read after bind.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// Accepting an attached Unix socket client failed.
    Accept {
        /// Underlying accept error.
        source: io::Error,
    },
    /// Connecting to a listener failed.
    Connect {
        /// Socket path that could not be connected.
        path: PathBuf,
        /// Underlying connect error.
        source: io::Error,
    },
    /// Cloning a Unix stream failed.
    Clone {
        /// Underlying stream clone error.
        source: io::Error,
    },
    /// Spawning the bounded background socket reader failed.
    SpawnReader {
        /// Underlying thread admission error.
        source: io::Error,
    },
    /// Encoding a protocol message failed.
    Encode {
        /// Underlying protocol encode error.
        source: tau_proto::EncodeError,
    },
    /// Flushing a protocol message failed.
    Flush {
        /// Underlying writer flush error.
        source: io::Error,
    },
    /// Decoding a protocol message failed.
    Decode {
        /// Underlying protocol decode error.
        source: DecodeError,
    },
}

impl fmt::Display for SocketTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateParentDirectory { path, source } => write!(
                f,
                "failed to create socket parent directory {}: {source}",
                path.display()
            ),
            Self::RefuseNonSocketPath { path } => {
                write!(f, "refusing to replace non-socket path {}", path.display())
            }
            Self::ActiveSocketExists { path } => {
                write!(
                    f,
                    "refusing to replace active Unix socket {}",
                    path.display()
                )
            }
            Self::ProbeExistingSocket { path, source } => write!(
                f,
                "refusing to replace Unix socket {} after liveness probe failed: {source}",
                path.display()
            ),
            Self::RemoveStaleSocket { path, source } => write!(
                f,
                "failed to remove stale socket {}: {source}",
                path.display()
            ),
            Self::Bind { path, source } => {
                write!(f, "failed to bind Unix socket {}: {source}", path.display())
            }
            Self::BoundSocketMetadata { path, source } => write!(
                f,
                "failed to inspect bound Unix socket {}: {source}",
                path.display()
            ),
            Self::Accept { source } => write!(f, "failed to accept Unix socket client: {source}"),
            Self::Connect { path, source } => {
                write!(
                    f,
                    "failed to connect to Unix socket {}: {source}",
                    path.display()
                )
            }
            Self::Clone { source } => write!(f, "failed to clone Unix socket stream: {source}"),
            Self::SpawnReader { source } => {
                write!(f, "failed to spawn Unix socket reader: {source}")
            }
            Self::Encode { source } => write!(f, "failed to encode socket event: {source}"),
            Self::Flush { source } => write!(f, "failed to flush socket stream: {source}"),
            Self::Decode { source } => write!(f, "failed to decode socket event: {source}"),
        }
    }
}

impl std::error::Error for SocketTransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CreateParentDirectory { source, .. } => Some(source),
            Self::RefuseNonSocketPath { .. } | Self::ActiveSocketExists { .. } => None,
            Self::ProbeExistingSocket { source, .. } => Some(source),
            Self::RemoveStaleSocket { source, .. } => Some(source),
            Self::Bind { source, .. } => Some(source),
            Self::BoundSocketMetadata { source, .. } => Some(source),
            Self::Accept { source } => Some(source),
            Self::Connect { source, .. } => Some(source),
            Self::Clone { source } => Some(source),
            Self::SpawnReader { source } => Some(source),
            Self::Encode { source } => Some(source),
            Self::Flush { source } => Some(source),
            Self::Decode { source } => Some(source),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    // Device number from the bound socket metadata.
    dev: u64,
    // Inode number from the bound socket metadata.
    ino: u64,
}

impl SocketIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

/// Unix socket listener for later-attached protocol clients.
///
/// When dropped, a listener removes only the socket path it created and only if
/// that path still refers to the same device/inode recorded after binding.
pub struct SocketListener {
    /// Filesystem path occupied by the listener socket.
    path: PathBuf,
    /// Bound Unix listener accepting client streams.
    listener: UnixListener,
    /// Device/inode pair of the socket created by this listener.
    socket_identity: SocketIdentity,
}

impl SocketListener {
    /// Binds a Unix socket listener at the given path.
    ///
    /// Parent directories are created if needed. An inactive stale Unix socket
    /// may be removed, but non-socket paths and active listeners are refused.
    /// Active-listener detection opens a short-lived connection that can be
    /// observed by an already-running daemon. Existing socket paths are treated
    /// as stale only when that probe fails with
    /// [`io::ErrorKind::ConnectionRefused`]; other probe failures return
    /// [`SocketTransportError::ProbeExistingSocket`] without unlinking the
    /// path.
    ///
    /// # Errors
    ///
    /// Returns an error when directory creation, stale socket cleanup, binding,
    /// post-bind socket metadata inspection, non-socket refusal, or
    /// active/unverified socket refusal fails or applies.
    pub fn bind(path: impl Into<PathBuf>) -> Result<Self, SocketTransportError> {
        let path = path.into();
        create_socket_parent_if_needed(&path)?;
        remove_inactive_stale_socket(&path)?;

        let listener = UnixListener::bind(&path).map_err(|source| SocketTransportError::Bind {
            path: path.clone(),
            source,
        })?;
        let metadata = fs::symlink_metadata(&path).map_err(|source| {
            SocketTransportError::BoundSocketMetadata {
                path: path.clone(),
                source,
            }
        })?;
        Ok(Self {
            path,
            listener,
            socket_identity: SocketIdentity::from_metadata(&metadata),
        })
    }

    /// Returns the filesystem path of the listener socket.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Clones the raw Unix listener for daemon/internal stream handoff loops.
    ///
    /// This is an escape hatch for callers that need to pass accepted raw
    /// streams to higher-level server code instead of using [`Self::accept`].
    /// The `SocketListener` still owns identity-checked path cleanup; callers
    /// must ensure cloned listeners are shut down before dropping it.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying listener cannot be cloned.
    pub fn try_clone_raw_listener(&self) -> Result<UnixListener, SocketTransportError> {
        self.listener
            .try_clone()
            .map_err(|source| SocketTransportError::Clone { source })
    }

    /// Accepts one attached client for harness/server-side protocol handling.
    ///
    /// The returned client reads [`HarnessInputMessage`] values from the peer
    /// and writes [`HarnessOutputMessage`] values back to it.
    ///
    /// # Errors
    ///
    /// Returns an error when accepting the Unix stream or cloning it for split
    /// reader/writer ownership fails.
    pub fn accept(&self) -> Result<SocketAcceptedClient, SocketTransportError> {
        let (stream, _) = self
            .listener
            .accept()
            .map_err(|source| SocketTransportError::Accept { source })?;
        SocketAcceptedClient::new(stream)
    }
}

impl Drop for SocketListener {
    fn drop(&mut self) {
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return;
        };
        if !metadata.file_type().is_socket() {
            return;
        }
        if SocketIdentity::from_metadata(&metadata) == self.socket_identity {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// One server-side accepted Unix socket client speaking the protocol.
pub struct SocketAcceptedClient {
    /// Reader for peer/client-to-harness input messages.
    reader: HarnessInputReader<UnixStream>,
    /// Writer for harness-to-peer/client output messages.
    writer: HarnessOutputWriter<BufWriter<UnixStream>>,
}

impl SocketAcceptedClient {
    fn new(stream: UnixStream) -> Result<Self, SocketTransportError> {
        let writer_stream = stream
            .try_clone()
            .map_err(|source| SocketTransportError::Clone { source })?;
        Ok(Self {
            reader: HarnessInputReader::new(stream),
            writer: HarnessOutputWriter::new(BufWriter::new(writer_stream)),
        })
    }

    /// Reads one peer → harness protocol message from the accepted client.
    ///
    /// Returns `Ok(None)` only when the client closes cleanly at a message
    /// boundary.
    ///
    /// # Errors
    ///
    /// Returns a decode error for malformed or truncated protocol input.
    pub fn recv(&mut self) -> Result<Option<HarnessInputMessage>, SocketTransportError> {
        self.reader
            .read_message()
            .map_err(|source| SocketTransportError::Decode { source })
    }

    /// Sends one harness → peer protocol message to the accepted client.
    ///
    /// # Errors
    ///
    /// Returns an error when encoding or flushing the message fails.
    pub fn send(&mut self, message: &HarnessOutputMessage) -> Result<(), SocketTransportError> {
        self.writer
            .write_message(message)
            .map_err(|source| SocketTransportError::Encode { source })?;
        self.writer
            .flush()
            .map_err(|source| SocketTransportError::Flush { source })
    }
}

/// Result of attempting to receive a harness → peer message from a socket.
#[derive(Debug, PartialEq)]
pub enum SocketReceive {
    /// A protocol message was received.
    Message {
        /// Decoded harness → peer output message.
        message: HarnessOutputMessage,
    },
    /// No message arrived before the requested timeout elapsed.
    Timeout,
    /// The socket closed cleanly at a message boundary.
    Closed,
}

/// One connected Unix socket peer speaking the protocol.
///
/// Each peer owns a bounded background reader thread. Dropping the peer shuts
/// down the stream, drops the receive queue, and joins that reader thread.
pub struct SocketPeer {
    /// Writer for peer/client-to-harness input messages.
    writer: PeerOutputWriter<BufWriter<UnixStream>>,
    /// Bounded queue of decoded harness-to-peer/client output messages.
    reader_frames: Option<Receiver<Result<HarnessOutputMessage, DecodeError>>>,
    /// Stream clone used to wake the reader thread during peer drop.
    shutdown_stream: UnixStream,
    /// Background reader thread that owns the read side of the socket.
    reader_thread: Option<thread::JoinHandle<()>>,
}

impl SocketPeer {
    /// Connects to an existing Unix socket listener.
    ///
    /// # Errors
    ///
    /// Returns an error when the Unix socket cannot be connected or cloned for
    /// independent reader/writer ownership.
    pub fn connect(path: impl Into<PathBuf>) -> Result<Self, SocketTransportError> {
        let path = path.into();
        let stream =
            UnixStream::connect(&path).map_err(|source| SocketTransportError::Connect {
                path: path.clone(),
                source,
            })?;
        Self::new(stream)
    }

    /// Connects to an existing Unix socket and bounds subsequent stream I/O.
    ///
    /// This is intended for short-lived runtime discovery and control RPCs
    /// whose caller owns an absolute deadline. The caller must still bound
    /// how long it waits for a complete protocol response.
    ///
    /// # Errors
    ///
    /// Returns an error when the socket cannot be connected, its I/O timeouts
    /// cannot be configured, or it cannot be cloned for split I/O ownership.
    pub fn connect_with_io_timeout(
        path: impl Into<PathBuf>,
        timeout: Duration,
    ) -> Result<Self, SocketTransportError> {
        let path = path.into();
        let stream = connect_unix_with_timeout(&path, timeout).map_err(|source| {
            SocketTransportError::Connect {
                path: path.clone(),
                source,
            }
        })?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|source| SocketTransportError::Connect {
                path: path.clone(),
                source,
            })?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|source| SocketTransportError::Connect { path, source })?;
        Self::new(stream)
    }

    fn new(stream: UnixStream) -> Result<Self, SocketTransportError> {
        let writer_stream = stream
            .try_clone()
            .map_err(|source| SocketTransportError::Clone { source })?;
        let shutdown_stream = stream
            .try_clone()
            .map_err(|source| SocketTransportError::Clone { source })?;
        let (reader_frames, reader_thread) = spawn_reader(stream)?;
        Ok(Self {
            writer: PeerOutputWriter::new(BufWriter::new(writer_stream)),
            reader_frames: Some(reader_frames),
            shutdown_stream,
            reader_thread: Some(reader_thread),
        })
    }

    /// Sends one peer → harness protocol message over the Unix socket.
    ///
    /// # Errors
    ///
    /// Returns an error when encoding or flushing the message fails.
    pub fn send(&mut self, message: &HarnessInputMessage) -> Result<(), SocketTransportError> {
        self.writer
            .write_message(message)
            .map_err(|source| SocketTransportError::Encode { source })?;
        self.writer
            .flush()
            .map_err(|source| SocketTransportError::Flush { source })
    }

    /// Updates the write timeout for the next bounded control-plane operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the connected stream rejects the timeout update.
    pub fn set_write_timeout(&self, timeout: Duration) -> Result<(), SocketTransportError> {
        self.writer
            .get_ref()
            .get_ref()
            .set_write_timeout(Some(timeout))
            .map_err(|source| SocketTransportError::Flush { source })
    }

    /// Reads one harness → peer protocol message or an explicit timeout/close
    /// outcome.
    ///
    /// # Errors
    ///
    /// Returns a decode error for malformed or truncated protocol output.
    pub fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<SocketReceive, SocketTransportError> {
        let reader_frames = self
            .reader_frames
            .as_ref()
            .expect("socket peer reader missing before drop");
        match reader_frames.recv_timeout(timeout) {
            Ok(Ok(frame)) => Ok(SocketReceive::Message { message: frame }),
            Ok(Err(error)) => Err(SocketTransportError::Decode { source: error }),
            Err(RecvTimeoutError::Timeout) => Ok(SocketReceive::Timeout),
            Err(RecvTimeoutError::Disconnected) => Ok(SocketReceive::Closed),
        }
    }
}

fn connect_unix_with_timeout(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    socket.connect_timeout(&socket2::SockAddr::unix(path)?, timeout)?;
    let fd: OwnedFd = socket.into();
    Ok(fd.into())
}

impl Drop for SocketPeer {
    fn drop(&mut self) {
        self.reader_frames.take();
        let _ = self.shutdown_stream.shutdown(std::net::Shutdown::Both);
        if let Some(reader_thread) = self.reader_thread.take() {
            let _ = reader_thread.join();
        }
    }
}

type ReaderWorker = (
    Receiver<Result<HarnessOutputMessage, DecodeError>>,
    thread::JoinHandle<()>,
);

fn spawn_reader(stream: UnixStream) -> Result<ReaderWorker, SocketTransportError> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let reader_thread = thread::Builder::new()
        .name("tau-socket-reader".to_owned())
        .spawn(move || read_frames(stream, sender))
        .map_err(|source| SocketTransportError::SpawnReader { source })?;
    Ok((receiver, reader_thread))
}

fn read_frames(stream: UnixStream, sender: SyncSender<Result<HarnessOutputMessage, DecodeError>>) {
    let mut reader = PeerInputReader::new(stream);
    loop {
        match reader.read_message() {
            Ok(Some(frame)) => {
                if sender.send(Ok(frame)).is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(error) => {
                let _ = sender.send(Err(error));
                return;
            }
        }
    }
}

fn create_socket_parent_if_needed(path: &Path) -> Result<(), SocketTransportError> {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };
    fs::create_dir_all(parent).map_err(|source| SocketTransportError::CreateParentDirectory {
        path: parent.to_path_buf(),
        source,
    })
}

fn remove_inactive_stale_socket(path: &Path) -> Result<(), SocketTransportError> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if !metadata.file_type().is_socket() {
        return Err(SocketTransportError::RefuseNonSocketPath {
            path: path.to_path_buf(),
        });
    }
    match UnixStream::connect(path) {
        Ok(_) => {
            return Err(SocketTransportError::ActiveSocketExists {
                path: path.to_path_buf(),
            });
        }
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {}
        Err(source) => {
            return Err(SocketTransportError::ProbeExistingSocket {
                path: path.to_path_buf(),
                source,
            });
        }
    }
    fs::remove_file(path).map_err(|source| SocketTransportError::RemoveStaleSocket {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests;
