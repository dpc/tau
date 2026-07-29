//! Shared protocol types and CBOR stream codec helpers.
//!
//! The wire format is a sequence of self-delimiting CBOR items. Each item is a
//! directionally typed protocol message:
//!
//! - [`HarnessInputMessage`]: messages the harness receives from peers (UI
//!   clients and extensions), including `Emit` requests that wrap peer-authored
//!   events; or
//! - [`HarnessOutputMessage`]: messages the harness sends to peers, including
//!   `Deliver` payloads that wrap event delivery metadata.
//!
//! Bare top-level [`Event`] values are not valid protocol items. Peers emit
//! events with [`HarnessInputMessage::Emit`], and the harness delivers events
//! with [`HarnessOutputMessage::Deliver`]. The typed codec aliases make the
//! intended direction explicit for both harness-side and peer-side transports.

mod context;
mod diff;
mod discovery_snapshot;
mod duration_format;
mod estimated_cost;
mod event_name;
mod events;
mod interception;
mod message_extension_data;
mod message_fact;
mod messages;
pub mod notice_kind;
mod observation;
mod prompt_envelope;
mod prompt_fragment;
mod prompt_submission;
mod provider_quota;
mod suggestion;
mod token_usage;
mod tool_name_prefix;

use std::io::{BufReader, Cursor, Read, Write};
use std::marker::PhantomData;

pub use ciborium::value::Value as CborValue;
pub use context::*;
pub use diff::{DiffHunk, DiffLine, DiffSegment, DiffSummary, FileDiffSummary};
pub use discovery_snapshot::*;
pub use duration_format::format_approximate_duration_secs;
pub use estimated_cost::*;
pub use event_name::*;
pub use events::*;
pub use interception::*;
pub use message_extension_data::*;
pub use message_fact::*;
pub use messages::*;
pub use observation::*;
pub use prompt_envelope::*;
pub use prompt_fragment::*;
pub use prompt_submission::*;
pub use provider_quota::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
pub use suggestion::*;
pub use tau_actions::*;
pub use token_usage::*;
pub use tool_name_prefix::{
    InvalidToolNamePrefix, ToolNameCompositionError, ToolNamePrefix, ToolNameTarget,
};

/// Current protocol version implemented by this crate.
///
/// This stays at zero under
/// `GATE-no-backward-compatibility`; Tau does not support old protocols.
pub const PROTOCOL_VERSION: u32 = 0;

/// UI marker text for responses, thinking blocks, and tool calls that
/// are still in progress.
pub const PROGRESS_INDICATOR_TEXT: &str = "…";

/// Header name used to mark model-visible internal Tau messages.
pub const TAU_INTERNAL_HEADER_NAME: &str = "tau_internal";

macro_rules! string_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Default, Eq, PartialEq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
            pub fn as_str(&self) -> &str { &self.0 }
            pub fn into_string(self) -> String { self.0 }
            pub fn is_empty(&self) -> bool { self.0.is_empty() }
        }

        impl std::ops::Deref for $name {
            type Target = str;
            fn deref(&self) -> &str { &self.0 }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self { Self(s) }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self { Self(s.to_owned()) }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool { self.0 == other }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool { self.0 == *other }
        }

        impl PartialEq<String> for $name {
            fn eq(&self, other: &String) -> bool { self.0 == *other }
        }

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str { &self.0 }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str { &self.0 }
        }
    };
}

/// Maximum length for validated session and prompt identifiers.
pub const SESSION_SCOPED_ID_MAX_LEN: usize = 128;

macro_rules! validated_string_newtype {
    ($(#[$meta:meta])* $name:ident, $error:ident, $label:literal, $max:expr) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord, serde::Serialize)]
        #[serde(transparent)]
        pub struct $name(
            #[doc = concat!("Validated ", $label, " text.")] String
        );

        #[doc = concat!("Error returned when parsing ", $label, ".")]
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub enum $error {
            /// Identifiers must not be empty.
            Empty,
            #[doc = concat!(
                "Identifiers must not exceed `",
                stringify!($max),
                "` bytes."
            )]
            TooLong {
                /// Maximum accepted byte length.
                max: usize,
                /// Rejected byte length.
                actual: usize,
            },
            /// Identifiers may contain only ASCII letters, digits, `_`, and `-`.
            InvalidByte {
                /// Byte offset of the rejected value.
                index: usize,
                /// Rejected byte.
                byte: u8,
            },
        }

        impl std::fmt::Display for $error {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    Self::Empty => write!(f, "{} must not be empty", $label),
                    Self::TooLong { max, actual } => {
                        write!(f, "{} is too long: {actual} bytes; maximum is {max}", $label)
                    }
                    Self::InvalidByte { index, byte } => write!(
                        f,
                        "{} contains invalid byte 0x{byte:02x} at byte offset {index}",
                        $label
                    ),
                }
            }
        }

        impl std::error::Error for $error {}

        impl $name {
            #[doc = concat!(
                "Parses and validates ",
                $label,
                ".\n\nAccepts 1 through `",
                stringify!($max),
                "` bytes containing only ASCII letters, digits, `_`, and `-`."
            )]
            pub fn parse(value: impl Into<String>) -> Result<Self, $error> {
                let value = value.into();
                if value.is_empty() {
                    return Err($error::Empty);
                }
                if $max < value.len() {
                    return Err($error::TooLong {
                        max: $max,
                        actual: value.len(),
                    });
                }
                if let Some((index, byte)) = value
                    .bytes()
                    .enumerate()
                    .find(|(_, byte)| !byte.is_ascii_alphanumeric() && *byte != b'_' && *byte != b'-')
                {
                    return Err($error::InvalidByte { index, byte });
                }
                Ok(Self(value))
            }

            /// Returns the validated identifier as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consumes the identifier and returns its validated string.
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl std::str::FromStr for $name {
            type Err = $error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = <String as serde::Deserialize>::deserialize(deserializer)?;
                Self::parse(value).map_err(serde::de::Error::custom)
            }
        }

        impl std::ops::Deref for $name {
            type Target = str;

            fn deref(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }

        impl PartialEq<String> for $name {
            fn eq(&self, other: &String) -> bool {
                self.0 == *other
            }
        }

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }
    };
}
pub(crate) use validated_string_newtype;

validated_string_newtype!(
    /// Stable identifier for one Tau session.
    ///
    /// Values contain 1 through 128 bytes of ASCII letters, digits, `_`, or
    /// `-`. Construction and deserialization validate this grammar.
    SessionId,
    SessionIdParseError,
    "session id",
    SESSION_SCOPED_ID_MAX_LEN
);
/// Maximum length for a durable agent identifier.
pub const AGENT_ID_MAX_LEN: usize = 64;

/// Error returned when parsing a durable agent identifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentIdParseError {
    /// Agent identifiers must not be empty.
    Empty,
    /// Agent identifiers must not exceed [`AGENT_ID_MAX_LEN`] bytes.
    TooLong { max: usize, actual: usize },
    /// Agent identifiers may contain only ASCII letters, digits, `_`, and `-`.
    InvalidByte { index: usize, byte: u8 },
}

impl std::fmt::Display for AgentIdParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("agent id must not be empty"),
            Self::TooLong { max, actual } => {
                write!(f, "agent id is too long: {actual} bytes > {max}")
            }
            Self::InvalidByte { index, byte } => write!(
                f,
                "agent id contains invalid byte 0x{byte:02x} at byte offset {index}"
            ),
        }
    }
}

impl std::error::Error for AgentIdParseError {}

/// Global durable agent identifier.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct AgentId(String);

impl AgentId {
    /// Parse a durable agent identifier.
    pub fn parse(s: impl AsRef<str>) -> Result<Self, AgentIdParseError> {
        s.as_ref().parse()
    }

    /// Parse a user-entered agent reference with an optional leading `@`.
    ///
    /// The returned identifier always uses its canonical, unsigiled form. Wire
    /// decoding and durable identifier parsing remain strict through
    /// [`Self::parse`].
    pub fn parse_reference(s: impl AsRef<str>) -> Result<Self, AgentIdParseError> {
        let value = s.as_ref();
        Self::parse(value.strip_prefix('@').unwrap_or(value))
    }

    /// Borrow this identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume this identifier into its string representation.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::str::FromStr for AgentId {
    type Err = AgentIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() {
            return Err(AgentIdParseError::Empty);
        }
        if value.len() > AGENT_ID_MAX_LEN {
            return Err(AgentIdParseError::TooLong {
                max: AGENT_ID_MAX_LEN,
                actual: value.len(),
            });
        }
        for (index, byte) in value.bytes().enumerate() {
            if !(byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-') {
                return Err(AgentIdParseError::InvalidByte { index, byte });
            }
        }
        Ok(Self(value.to_owned()))
    }
}

impl serde::Serialize for AgentId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for AgentId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        AgentId::parse(&value).map_err(serde::de::Error::custom)
    }
}

impl std::ops::Deref for AgentId {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::borrow::Borrow<str> for AgentId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for AgentId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

validated_string_newtype!(
    /// Stable identifier for one agent transcript prompt.
    ///
    /// Values contain 1 through 128 bytes of ASCII letters, digits, `_`, or
    /// `-`. Construction and deserialization validate this grammar.
    AgentPromptId,
    AgentPromptIdParseError,
    "agent prompt id",
    SESSION_SCOPED_ID_MAX_LEN
);
validated_string_newtype!(
    /// Stable identifier for one global agent message.
    ///
    /// Values contain 1 through 128 bytes of ASCII letters, digits, `_`, or
    /// `-`. Construction and deserialization validate this grammar.
    AgentMessageId,
    AgentMessageIdParseError,
    "agent message id",
    128
);
// ToolName is defined manually below with validation.
string_newtype!(/// Tool call identifier.
    ToolCallId);
validated_string_newtype!(
    /// User-interface action invocation identifier.
    ActionInvocationId,
    ActionInvocationIdParseError,
    "action invocation id",
    64
);
validated_string_newtype!(
    /// Identifier assigned to one live harness connection.
    ///
    /// Values contain 1 through 128 bytes of ASCII letters, digits, `_`, or
    /// `-`. Construction and deserialization validate this grammar.
    ConnectionId,
    ConnectionIdParseError,
    "connection id",
    128
);

validated_string_newtype!(
    /// Canonical configured extension name.
    ///
    /// Values contain 1 through 128 bytes of ASCII letters, digits, `_`, or
    /// `-`. Construction and deserialization validate this grammar.
    ExtensionName,
    ExtensionNameParseError,
    "extension name",
    EXTENSION_NAME_MAX_BYTES
);

/// Maximum encoded bytes in a configured extension name.
pub const EXTENSION_NAME_MAX_BYTES: usize = 128;

string_newtype!(/// Agent-scoped context key published by an extension.
    AgentContextKey);
string_newtype!(/// Durable agent metadata key visible to extensions.
    AgentMetadataKey);
/// Maximum encoded bytes in a metadata mutation correlation id.
pub const AGENT_METADATA_MUTATION_ID_MAX_BYTES: usize = 256;

/// Opaque bounded correlation identity for one metadata mutation.
#[derive(
    Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct AgentMetadataMutationId(String);

impl AgentMetadataMutationId {
    /// Parse a non-empty correlation id within the protocol byte bound.
    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidAgentMetadataMutationId> {
        let value = value.into();
        if value.is_empty() || AGENT_METADATA_MUTATION_ID_MAX_BYTES < value.len() {
            return Err(InvalidAgentMetadataMutationId);
        }
        Ok(Self(value))
    }

    /// Borrow the encoded correlation id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AgentMetadataMutationId {
    type Error = InvalidAgentMetadataMutationId;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<AgentMetadataMutationId> for String {
    fn from(value: AgentMetadataMutationId) -> Self {
        value.0
    }
}

/// Invalid empty or oversized metadata mutation id.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidAgentMetadataMutationId;

impl std::fmt::Display for InvalidAgentMetadataMutationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("metadata mutation id must contain 1..=256 bytes")
    }
}

impl std::error::Error for InvalidAgentMetadataMutationId {}
// ProviderName / ModelName / ModelId are defined manually below — they
// validate at construction (no '/', non-empty, etc.) so the rest of
// the codebase can stop re-parsing `"provider/model"` strings.
string_newtype!(/// Skill name (e.g. `"jujutsu"`, `"preview-site"`).
    SkillName);
validated_string_newtype!(
    /// Identifier correlating a user-initiated `!`/`!!` shell command's
    /// lifecycle events (progress, finished).
    ShellCommandId,
    ShellCommandIdParseError,
    "shell command id",
    64
);

// ---------------------------------------------------------------------------
// ProviderName / ModelName / ModelId
// ---------------------------------------------------------------------------

/// Provider name (e.g. `"openai"`, `"anthropic"`, `"github-copilot"`).
///
/// Validated at construction: non-empty, no `/` (which would collide
/// with the [`ModelId`] separator), and only filename-safe characters
/// (ASCII letters/digits, `_`, `-`, `.`) so a `ProviderName` is also
/// safe to embed in `auth.d/<name>.json` paths.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct ProviderName(String);

/// Model name as understood by the provider (e.g.
/// `"claude-sonnet-4-20250514"`, `"gpt-5.5"`, `"llama3.2:latest"`).
///
/// Validated at construction: non-empty. Otherwise permissive — provider
/// model IDs include `/`, `:`, `.`, `-`, `_`, etc.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct ModelName(String);

/// Qualified model identifier — a [`ProviderName`] and [`ModelName`]
/// joined by the first `/` on the wire (e.g. `"openai/gpt-4o"`).
///
/// Round-trips through serde as a flat `"provider/model"` string so
/// existing CBOR events, JSON5 config files and persisted session
/// logs keep working unchanged.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct ModelId {
    pub provider: ProviderName,
    pub model: ModelName,
}

/// Error returned when parsing a string fails one of the
/// [`ProviderName`] / [`ModelName`] / [`ModelId`] validators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseNameError(String);

impl std::fmt::Display for ParseNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseNameError {}

impl ProviderName {
    /// Try to construct a `ProviderName`, returning `Err` on validation
    /// failure. Use [`ProviderName::new`] when the input is statically
    /// known to be valid.
    pub fn try_new(s: impl Into<String>) -> Result<Self, ParseNameError> {
        let s = s.into();
        Self::validate(&s)?;
        Ok(Self(s))
    }

    /// Construct a `ProviderName`, panicking on validation failure.
    /// Intended for tests and statically-known constants.
    pub fn new(s: impl Into<String>) -> Self {
        Self::try_new(s).expect("invalid provider name")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    fn validate(name: &str) -> Result<(), ParseNameError> {
        if name.is_empty() {
            return Err(ParseNameError("provider name must be non-empty".to_owned()));
        }
        if name.starts_with('.') || name.starts_with('-') {
            return Err(ParseNameError(format!(
                "provider name '{name}' may not start with '.' or '-'"
            )));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            return Err(ParseNameError(format!(
                "provider name '{name}' may only contain ASCII letters, digits, '_', '-', '.'"
            )));
        }
        Ok(())
    }
}

impl std::str::FromStr for ProviderName {
    type Err = ParseNameError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_new(s.to_owned())
    }
}

impl std::fmt::Display for ProviderName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::ops::Deref for ProviderName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ProviderName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for ProviderName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for ProviderName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for ProviderName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl serde::Serialize for ProviderName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for ProviderName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::try_new(s).map_err(serde::de::Error::custom)
    }
}

impl ModelName {
    /// Try to construct a `ModelName`, returning `Err` on validation
    /// failure. Use [`ModelName::new`] when the input is statically
    /// known to be valid.
    pub fn try_new(s: impl Into<String>) -> Result<Self, ParseNameError> {
        let s = s.into();
        Self::validate(&s)?;
        Ok(Self(s))
    }

    /// Construct a `ModelName`, panicking on validation failure.
    pub fn new(s: impl Into<String>) -> Self {
        Self::try_new(s).expect("invalid model name")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    fn validate(name: &str) -> Result<(), ParseNameError> {
        if name.is_empty() {
            return Err(ParseNameError("model name must be non-empty".to_owned()));
        }
        Ok(())
    }
}

impl std::str::FromStr for ModelName {
    type Err = ParseNameError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_new(s.to_owned())
    }
}

impl std::fmt::Display for ModelName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::ops::Deref for ModelName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ModelName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for ModelName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for ModelName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for ModelName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl serde::Serialize for ModelName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for ModelName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::try_new(s).map_err(serde::de::Error::custom)
    }
}

impl ModelId {
    pub fn new(provider: ProviderName, model: ModelName) -> Self {
        Self { provider, model }
    }
}

impl std::str::FromStr for ModelId {
    type Err = ParseNameError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (provider, model) = s.split_once('/').ok_or_else(|| {
            ParseNameError(format!(
                "model id '{s}' must be of the form 'provider/model'"
            ))
        })?;
        Ok(Self {
            provider: ProviderName::try_new(provider.to_owned())?,
            model: ModelName::try_new(model.to_owned())?,
        })
    }
}

impl std::fmt::Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.provider, self.model)
    }
}

/// Convenience `&str` → `ModelId` that panics on invalid input.
/// Intended for tests, fixtures, and statically-known constants
/// (`"openai/gpt-5.5".into()` and friends). Use `ModelId::from_str`
/// when handling user input.
impl From<&str> for ModelId {
    fn from(s: &str) -> Self {
        s.parse().expect("invalid model id")
    }
}

/// See `From<&str> for ModelId`. Panics on invalid input.
impl From<String> for ModelId {
    fn from(s: String) -> Self {
        s.parse().expect("invalid model id")
    }
}

impl From<ModelId> for String {
    fn from(id: ModelId) -> String {
        id.to_string()
    }
}

impl serde::Serialize for ModelId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Wire form is the flat `"provider/model"` string — same
        // bytes as the previous flat-string newtype, so existing
        // CBOR / JSON5 / persisted logs keep working.
        self.to_string().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for ModelId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// ModelTag / ToolTag (validated newtypes)
// ---------------------------------------------------------------------------

fn is_valid_tag_identifier(s: &str, max_len: usize) -> bool {
    !s.is_empty()
        && s.len() <= max_len
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.' | b':')
        })
}

/// Provider-published model capability tag used by harness-owned tool policy.
///
/// Tags are deterministic lowercase ASCII identifiers, optionally namespaced
/// with `:` segments, such as `shell:chatgpt` or `tools:custom-text`.
#[derive(Clone, Debug, Eq, PartialEq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct ModelTag(String);

impl ModelTag {
    /// Maximum allowed length for a model tag, in bytes.
    pub const MAX_LEN: usize = 256;
    /// Create a new `ModelTag`, panicking if the tag is invalid.
    pub fn new(s: impl Into<String>) -> Self {
        let s = s.into();
        assert!(Self::is_valid(&s), "invalid model tag: {s:?}");
        Self(s)
    }
    /// Try to create a `ModelTag`, returning `None` if invalid.
    pub fn try_new(s: impl Into<String>) -> Option<Self> {
        let s = s.into();
        Self::is_valid(&s).then_some(Self(s))
    }
    /// Borrow the tag as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Convert the tag into its owned string.
    pub fn into_string(self) -> String {
        self.0
    }
    fn is_valid(s: &str) -> bool {
        is_valid_tag_identifier(s, Self::MAX_LEN)
    }
}
impl std::ops::Deref for ModelTag {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Display for ModelTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl AsRef<str> for ModelTag {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl<'de> serde::Deserialize<'de> for ModelTag {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if Self::is_valid(&s) {
            Ok(Self(s))
        } else {
            Err(serde::de::Error::custom(format!(
                "invalid model tag: {s:?}"
            )))
        }
    }
}

/// Extension-published neutral tool capability tag used by harness-owned
/// policy.
///
/// Tags describe what a tool is, not which model should receive it; examples
/// include `shell:edit:line`, `shell:edit:apply_patch`, and `shell:exec`.
#[derive(Clone, Debug, Eq, PartialEq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct ToolTag(String);

impl ToolTag {
    /// Maximum allowed length for a tool tag, in bytes.
    pub const MAX_LEN: usize = 256;
    /// Create a new `ToolTag`, panicking if the tag is invalid.
    pub fn new(s: impl Into<String>) -> Self {
        let s = s.into();
        assert!(Self::is_valid(&s), "invalid tool tag: {s:?}");
        Self(s)
    }
    /// Try to create a `ToolTag`, returning `None` if invalid.
    pub fn try_new(s: impl Into<String>) -> Option<Self> {
        let s = s.into();
        Self::is_valid(&s).then_some(Self(s))
    }
    /// Borrow the tag as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Convert the tag into its owned string.
    pub fn into_string(self) -> String {
        self.0
    }
    fn is_valid(s: &str) -> bool {
        is_valid_tag_identifier(s, Self::MAX_LEN)
    }
}
impl std::ops::Deref for ToolTag {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Display for ToolTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl AsRef<str> for ToolTag {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl<'de> serde::Deserialize<'de> for ToolTag {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if Self::is_valid(&s) {
            Ok(Self(s))
        } else {
            Err(serde::de::Error::custom(format!("invalid tool tag: {s:?}")))
        }
    }
}

// ---------------------------------------------------------------------------
// ToolName (validated newtype)
// ---------------------------------------------------------------------------

fn is_valid_ascii_identifier(s: &str, max_len: usize) -> bool {
    !s.is_empty() && s.len() <= max_len && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Tool name: must be non-empty, at most [`ToolName::MAX_LEN`] bytes,
/// and contain only ASCII alphanumerics or underscores (`[a-zA-Z0-9_]+`).
///
/// The length cap matches every real provider — 256 bytes is more
/// than enough for any well-formed tool identifier and stops a
/// pathological model emission (e.g. a hundred-megabyte hallucinated
/// name) from being faithfully round-tripped through the wire codec.
#[derive(Clone, Debug, Eq, PartialEq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct ToolName(String);

impl ToolName {
    /// Maximum allowed length for a tool name, in bytes.
    pub const MAX_LEN: usize = 256;

    /// Create a new `ToolName`, panicking if the name is invalid.
    pub fn new(s: impl Into<String>) -> Self {
        let s = s.into();
        assert!(Self::is_valid(&s), "invalid tool name: {s:?}");
        Self(s)
    }

    /// Try to create a `ToolName`, returning `None` if invalid.
    pub fn try_new(s: impl Into<String>) -> Option<Self> {
        let s = s.into();
        Self::is_valid(&s).then_some(Self(s))
    }

    /// Borrow this validated tool name as its wire string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Consume this tool name and return its validated wire string.
    pub fn into_string(self) -> String {
        self.0
    }

    fn is_valid(s: &str) -> bool {
        is_valid_ascii_identifier(s, Self::MAX_LEN)
    }
}

impl std::ops::Deref for ToolName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ToolName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<str> for ToolName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for ToolName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for ToolName {
    fn eq(&self, other: &String) -> bool {
        self.0 == *other
    }
}

impl std::borrow::Borrow<str> for ToolName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ToolName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> serde::Deserialize<'de> for ToolName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if Self::is_valid(&s) {
            Ok(Self(s))
        } else {
            Err(serde::de::Error::custom(format!(
                "invalid tool name: {s:?}"
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// ToolGroupName (validated newtype)
// ---------------------------------------------------------------------------

/// Tool group name: must be non-empty, at most [`ToolGroupName::MAX_LEN`]
/// bytes, and contain only ASCII alphanumerics or underscores
/// (`[a-zA-Z0-9_]+`).
#[derive(Clone, Debug, Eq, PartialEq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct ToolGroupName(String);

impl ToolGroupName {
    /// Maximum allowed length for a tool group name, in bytes.
    pub const MAX_LEN: usize = 256;

    /// Create a new `ToolGroupName`, panicking if the name is invalid.
    pub fn new(s: impl Into<String>) -> Self {
        let s = s.into();
        assert!(Self::is_valid(&s), "invalid tool group name: {s:?}");
        Self(s)
    }

    /// Try to create a `ToolGroupName`, returning `None` if invalid.
    pub fn try_new(s: impl Into<String>) -> Option<Self> {
        let s = s.into();
        Self::is_valid(&s).then_some(Self(s))
    }

    /// Borrow the group name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Convert the group name into its owned string.
    pub fn into_string(self) -> String {
        self.0
    }

    fn is_valid(s: &str) -> bool {
        is_valid_ascii_identifier(s, Self::MAX_LEN)
    }
}

impl std::ops::Deref for ToolGroupName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ToolGroupName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<str> for ToolGroupName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for ToolGroupName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for ToolGroupName {
    fn eq(&self, other: &String) -> bool {
        self.0 == *other
    }
}

impl std::borrow::Borrow<str> for ToolGroupName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ToolGroupName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> serde::Deserialize<'de> for ToolGroupName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if Self::is_valid(&s) {
            Ok(Self(s))
        } else {
            Err(serde::de::Error::custom(format!(
                "invalid tool group name: {s:?}"
            )))
        }
    }
}

/// Unique identifier for one extension instance (monotonic counter).
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct ExtensionInstanceId(u64);

impl ExtensionInstanceId {
    pub fn new(v: u64) -> Self {
        Self(v)
    }
    pub fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for ExtensionInstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u64> for ExtensionInstanceId {
    fn from(v: u64) -> Self {
        Self(v)
    }
}

/// CBOR serialization error used by protocol encoders and writers.
pub type EncodeError = ciborium::ser::Error<std::io::Error>;

/// CBOR deserialization error used by protocol decoders and readers.
pub type DecodeError = ciborium::de::Error<std::io::Error>;

/// Maximum encoded size accepted for one streaming protocol message.
///
/// This bounds peer-controlled buffering before higher-level connection and
/// activation quotas can apply.
pub const MAX_PROTOCOL_MESSAGE_BYTES: u64 = 16 * 1024 * 1024;

/// Encoded byte count for one complete protocol message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolMessageBytes {
    /// Nonzero size of the complete encoded message.
    bytes: std::num::NonZeroU64,
}

impl ProtocolMessageBytes {
    /// Construct a nonzero count observed by the protocol codec.
    #[must_use]
    pub const fn new(bytes: u64) -> Option<Self> {
        // Preserve this behavior; the structural alternative is not semantics-neutral
        // here. ast-grep-ignore: match-option-verbose
        match std::num::NonZeroU64::new(bytes) {
            Some(bytes) => Some(Self { bytes }),
            None => None,
        }
    }

    /// Return the observed encoded byte count.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.bytes.get()
    }
}

/// One decoded protocol message with its observed encoded size.
#[derive(Debug, Eq, PartialEq)]
pub struct DecodedMessage<M> {
    /// Decoded directionally typed message.
    pub message: M,
    /// Bytes consumed while decoding this exact message.
    pub encoded_bytes: ProtocolMessageBytes,
}

// ---------------------------------------------------------------------------
// Codec
// ---------------------------------------------------------------------------

/// Encodes one directionally typed protocol message as a self-delimiting CBOR
/// item.
pub fn encode_message<W, M>(writer: W, message: &M) -> Result<(), EncodeError>
where
    W: Write,
    M: Serialize,
{
    ciborium::into_writer(message, writer)
}

/// Decodes one directionally typed protocol message from a self-delimiting CBOR
/// item.
pub fn decode_message<R, M>(reader: R) -> Result<M, DecodeError>
where
    R: Read,
    M: DeserializeOwned,
{
    ciborium::from_reader(reader)
}

/// Encodes one protocol message into an owned byte buffer.
pub fn encode_message_to_vec<M>(message: &M) -> Result<Vec<u8>, EncodeError>
where
    M: Serialize,
{
    let mut bytes = Vec::new();
    encode_message(&mut bytes, message)?;
    Ok(bytes)
}

/// Decodes exactly one protocol message from a byte slice.
///
/// The slice must contain a single self-delimiting CBOR item and no trailing
/// bytes. Use [`MessageReader`] when decoding multiple concatenated messages
/// from a stream.
pub fn decode_message_from_slice<M>(bytes: &[u8]) -> Result<M, DecodeError>
where
    M: DeserializeOwned,
{
    let mut cursor = Cursor::new(bytes);
    let message = decode_message(&mut cursor)?;
    if cursor.position() != bytes.len() as u64 {
        return Err(DecodeError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "trailing bytes after protocol message",
        )));
    }
    Ok(message)
}

/// Encodes one harness input message into an owned byte buffer.
pub fn encode_harness_input_to_vec(message: &HarnessInputMessage) -> Result<Vec<u8>, EncodeError> {
    encode_message_to_vec(message)
}

/// Decodes exactly one harness input message from a byte slice.
///
/// Returns an error when bytes remain after the first self-delimiting CBOR
/// item.
pub fn decode_harness_input_from_slice(bytes: &[u8]) -> Result<HarnessInputMessage, DecodeError> {
    decode_message_from_slice(bytes)
}

/// Encodes one harness output message into an owned byte buffer.
pub fn encode_harness_output_to_vec(
    message: &HarnessOutputMessage,
) -> Result<Vec<u8>, EncodeError> {
    encode_message_to_vec(message)
}

/// Decodes exactly one harness output message from a byte slice.
///
/// Returns an error when bytes remain after the first self-delimiting CBOR
/// item.
pub fn decode_harness_output_from_slice(bytes: &[u8]) -> Result<HarnessOutputMessage, DecodeError> {
    decode_message_from_slice(bytes)
}

/// Looks up `key` in a [`CborValue::Map`] and returns the matching
/// sub-value. Returns `None` if `value` is not a map or the key is
/// absent. Key lookup is linear over the map entries — fine for the
/// small CBOR trees produced by tools, where the alternative would be
/// converting to a `HashMap` per access.
#[must_use]
pub fn cbor_field<'a>(value: &'a CborValue, key: &str) -> Option<&'a CborValue> {
    if let CborValue::Map(entries) = value {
        for (k, v) in entries {
            if let CborValue::Text(k) = k
                && k == key
            {
                return Some(v);
            }
        }
    }
    None
}

/// Convenience accessor for a [`CborValue::Text`] field by key.
#[must_use]
pub fn cbor_text_field(value: &CborValue, key: &str) -> Option<String> {
    match cbor_field(value, key)? {
        CborValue::Text(s) => Some(s.clone()),
        _ => None,
    }
}

/// Convenience accessor for a [`CborValue::Bool`] field by key.
#[must_use]
pub fn cbor_bool_field(value: &CborValue, key: &str) -> Option<bool> {
    match cbor_field(value, key)? {
        CborValue::Bool(b) => Some(*b),
        _ => None,
    }
}

/// Convenience accessor for a [`CborValue::Array`] field by key.
#[must_use]
pub fn cbor_array_field<'a>(value: &'a CborValue, key: &str) -> Option<&'a [CborValue]> {
    match cbor_field(value, key)? {
        CborValue::Array(arr) => Some(arr.as_slice()),
        _ => None,
    }
}

/// Convenience accessor for a [`CborValue::Integer`] field by key.
#[must_use]
pub fn cbor_int_field(value: &CborValue, key: &str) -> Option<i128> {
    match cbor_field(value, key)? {
        CborValue::Integer(n) => Some((*n).into()),
        _ => None,
    }
}

/// Convert a `serde_json::Value` into a [`CborValue`].
///
/// Numbers are preserved as integers when possible, otherwise as
/// floats. Signed and unsigned JSON integers are preserved exactly when they
/// fit CBOR's integer representation. Other numeric values are encoded as
/// floats when possible; values that do not round-trip through serde_json's
/// numeric accessors are logged via `tracing::warn!` and lowered to
/// [`CborValue::Null`] rather than crashing the wire codec.
#[must_use]
pub fn json_to_cbor(v: &serde_json::Value) -> CborValue {
    match v {
        serde_json::Value::Null => CborValue::Null,
        serde_json::Value::Bool(b) => CborValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                CborValue::Integer(i.into())
            } else if let Some(u) = n.as_u64() {
                CborValue::Integer(u.into())
            // ast-grep-ignore: if-let-some-else
            } else if let Some(f) = n.as_f64() {
                CborValue::Float(f)
            } else {
                tracing::warn!(
                    number = %n,
                    "json_to_cbor: number is not representable as i64, u64, or f64, dropping to Null"
                );
                CborValue::Null
            }
        }
        serde_json::Value::String(s) => CborValue::Text(s.clone()),
        serde_json::Value::Array(arr) => CborValue::Array(arr.iter().map(json_to_cbor).collect()),
        serde_json::Value::Object(map) => CborValue::Map(
            map.iter()
                .map(|(k, v)| (CborValue::Text(k.clone()), json_to_cbor(v)))
                .collect(),
        ),
    }
}

/// Stateful writer for a stream of directionally typed protocol messages.
#[derive(Debug)]
pub struct MessageWriter<W, M> {
    inner: W,
    _message: PhantomData<fn() -> M>,
}

impl<W, M> MessageWriter<W, M> {
    /// Wraps an arbitrary writer.
    #[must_use]
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            _message: PhantomData,
        }
    }

    /// Returns the wrapped writer.
    #[must_use]
    pub fn into_inner(self) -> W {
        self.inner
    }

    /// Borrows the wrapped writer for transport-specific configuration.
    #[must_use]
    pub fn get_ref(&self) -> &W {
        &self.inner
    }
}

impl<W, M> MessageWriter<W, M>
where
    W: Write,
    M: Serialize,
{
    /// Writes one protocol message to the stream.
    pub fn write_message(&mut self, message: &M) -> Result<(), EncodeError> {
        self.write_message_with_size(message).map(|_| ())
    }

    /// Writes one protocol message and returns the encoded bytes written.
    ///
    /// The count is collected by the real serialization write rather than a
    /// separate accounting-only encoding.
    pub fn write_message_with_size(
        &mut self,
        message: &M,
    ) -> Result<ProtocolMessageBytes, EncodeError> {
        let mut writer = CountingWriter {
            inner: &mut self.inner,
            bytes: 0,
        };
        encode_message(&mut writer, message)?;
        Ok(ProtocolMessageBytes::new(writer.bytes)
            .expect("a complete encoded CBOR message is nonempty"))
    }

    /// Flushes the wrapped writer.
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Writer adapter that counts bytes accepted by the underlying transport.
struct CountingWriter<W> {
    /// Real protocol destination.
    inner: W,
    /// Bytes successfully written through the adapter.
    bytes: u64,
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.bytes = self.bytes.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Stateful reader for a stream of directionally typed protocol messages.
///
/// Wraps the inner reader in a [`BufReader`] internally so per-byte decoding
/// (which `ciborium` issues during deserialization) doesn't translate to
/// per-byte syscalls on stdio or socket transports.
#[derive(Debug)]
pub struct MessageReader<R, M> {
    inner: BufReader<R>,
    _message: PhantomData<fn() -> M>,
}

impl<R, M> MessageReader<R, M>
where
    R: Read,
    M: DeserializeOwned,
{
    /// Wraps an arbitrary reader.
    #[must_use]
    pub fn new(inner: R) -> Self {
        Self {
            inner: BufReader::new(inner),
            _message: PhantomData,
        }
    }

    /// Returns the wrapped reader. Any data already buffered but not yet
    /// consumed by a message decode is discarded.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.inner.into_inner()
    }

    /// Reads one protocol message from the stream.
    ///
    /// Returns `Ok(None)` on clean end-of-stream (EOF at a message boundary).
    /// Returns `Err` only for actual corruption, wrong-direction payloads, or
    /// truncated data.
    pub fn read_message(&mut self) -> Result<Option<M>, DecodeError> {
        self.read_message_with_size()
            .map(|decoded| decoded.map(|decoded| decoded.message))
    }

    /// Reads one protocol message and returns its consumed encoded byte size.
    ///
    /// The size comes from the same bounded reader that decodes the message, so
    /// callers can account for transport bytes without encoding the decoded
    /// value again.
    pub fn read_message_with_size(&mut self) -> Result<Option<DecodedMessage<M>>, DecodeError> {
        // Peek one byte to distinguish clean EOF from a real read; if none is
        // available, the stream is at a message boundary.
        match std::io::BufRead::fill_buf(&mut self.inner) {
            Ok([]) => return Ok(None),
            Ok(_) => {}
            Err(e) => return Err(DecodeError::Io(e)),
        }
        let limit = MAX_PROTOCOL_MESSAGE_BYTES + 1;
        let mut limited = (&mut self.inner).take(limit);
        let decoded = ciborium::from_reader(&mut limited);
        let consumed = limit - limited.limit();
        if MAX_PROTOCOL_MESSAGE_BYTES < consumed {
            return Err(DecodeError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "protocol message exceeds {} encoded bytes",
                    MAX_PROTOCOL_MESSAGE_BYTES
                ),
            )));
        }
        decoded.map(|message| {
            Some(DecodedMessage {
                message,
                encoded_bytes: ProtocolMessageBytes::new(consumed)
                    .expect("a decoded CBOR message consumes at least one byte"),
            })
        })
    }
}

/// Harness-side reader: messages received by the harness from peers.
pub type HarnessInputReader<R> = MessageReader<R, HarnessInputMessage>;

/// Harness-side writer: messages sent by the harness to peers.
pub type HarnessOutputWriter<W> = MessageWriter<W, HarnessOutputMessage>;

/// Harness-side writer for tests or in-process peers that need to feed input.
pub type HarnessInputWriter<W> = MessageWriter<W, HarnessInputMessage>;

/// Harness-side reader for tests or in-process peers that inspect output.
pub type HarnessOutputReader<R> = MessageReader<R, HarnessOutputMessage>;

/// Peer-side reader: messages received from the harness.
pub type PeerInputReader<R> = HarnessOutputReader<R>;

/// Peer-side writer: messages sent to the harness.
pub type PeerOutputWriter<W> = HarnessInputWriter<W>;

/// Peer-side input message type.
pub type PeerInputMessage = HarnessOutputMessage;

/// Peer-side output message type.
pub type PeerOutputMessage = HarnessInputMessage;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
