use std::fmt;

/// Result type used by the Tau client runtime and its handlers.
pub type ClientResult<T = ()> = Result<T, ClientError>;

/// Error returned by the Tau client runtime or extension handlers.
#[derive(Debug)]
pub enum ClientError {
    /// Reading a harness-to-peer protocol frame failed.
    Decode(tau_proto::DecodeError),
    /// Writing a peer-to-harness protocol frame failed.
    Encode(tau_proto::EncodeError),
    /// Flushing the protocol writer failed.
    Io(std::io::Error),
    /// A handler rejected the message it was processing.
    Handler(String),
    /// The initial Configure was rejected cleanly before startup Ready.
    InitialConfigureRejected,
    /// The writer thread stopped before accepting the outbound frame.
    WriterClosed,
    /// A frame exceeds 8 MiB or detached output exhausted its bounded FIFO.
    Overloaded,
    /// The reader thread stopped before reporting input EOF or decode failure.
    ReaderClosed,
    /// The reader thread panicked while decoding inbound frames.
    ReaderPanicked,
    /// The writer thread panicked while processing outbound frames.
    WriterPanicked,
    /// The extension builder recorded an invalid startup declaration.
    Builder(String),
    /// A logical tool identifier could not be mapped through the installed
    /// scope.
    NameScope(String),
}

impl ClientError {
    /// Creates a handler error with a human-readable message.
    #[must_use]
    pub fn handler(message: impl Into<String>) -> Self {
        Self::Handler(message.into())
    }

    /// Creates a builder error with a human-readable message.
    #[must_use]
    pub fn builder(message: impl Into<String>) -> Self {
        Self::Builder(message.into())
    }

    /// Creates a structural tool-name scope error.
    #[must_use]
    pub fn name_scope(message: impl Into<String>) -> Self {
        Self::NameScope(message.into())
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => write!(f, "{error}"),
            Self::Encode(error) => write!(f, "{error}"),
            Self::Io(error) => write!(f, "{error}"),
            Self::Handler(message) | Self::Builder(message) | Self::NameScope(message) => {
                f.write_str(message)
            }
            Self::InitialConfigureRejected => {
                f.write_str("initial Configure was rejected before Ready")
            }
            Self::WriterClosed => f.write_str("tau client writer thread is closed"),
            Self::Overloaded => {
                f.write_str("tau client detached FIFO or frame byte limit is exhausted")
            }
            Self::ReaderClosed => f.write_str("tau client reader thread is closed"),
            Self::ReaderPanicked => f.write_str("tau client reader thread panicked"),
            Self::WriterPanicked => f.write_str("tau client writer thread panicked"),
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Decode(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Handler(_)
            | Self::InitialConfigureRejected
            | Self::Builder(_)
            | Self::NameScope(_)
            | Self::WriterClosed
            | Self::Overloaded
            | Self::ReaderClosed
            | Self::ReaderPanicked
            | Self::WriterPanicked => None,
        }
    }
}

impl From<tau_proto::DecodeError> for ClientError {
    fn from(error: tau_proto::DecodeError) -> Self {
        Self::Decode(error)
    }
}

impl From<tau_proto::EncodeError> for ClientError {
    fn from(error: tau_proto::EncodeError) -> Self {
        Self::Encode(error)
    }
}

impl From<std::io::Error> for ClientError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<String> for ClientError {
    fn from(message: String) -> Self {
        Self::handler(message)
    }
}
