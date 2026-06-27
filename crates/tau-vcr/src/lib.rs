//! Minimal YAML cassette storage helpers for Tau tests.
//!
//! `tau-vcr` deliberately stays below provider and tool semantics. It owns VCR
//! mode parsing, cassette directory/key handling, key validation, and YAML
//! `get`/`put` operations. Callers own cassette schemas, request validation,
//! live-vs-replay branching, timing, and response replay.
use std::fmt;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const ENV_MODE: &str = "TAU_VCR";
const ENV_DIR: &str = "TAU_VCR_DIR";

/// VCR operating mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VcrMode {
    /// Do not read or write cassettes.
    Off,
    /// Replay an existing cassette, otherwise let the caller record a new one.
    RecordIfMissing,
    /// Require an existing cassette and replay it.
    ReplayOnly,
}

impl VcrMode {
    /// Parses a mode string such as `off`, `record-if-missing`, or
    /// `replay-only`.
    pub fn parse(value: &str) -> Result<Self, VcrError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "off" => Ok(Self::Off),
            "record-if-missing" => Ok(Self::RecordIfMissing),
            "replay-only" => Ok(Self::ReplayOnly),
            other => Err(VcrError::InvalidMode(other.to_owned())),
        }
    }
}

/// VCR mode and cassette storage directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VcrConfig {
    /// Operating mode for cassette reads/writes.
    pub mode: VcrMode,
    /// Directory containing cassette files.
    pub dir: PathBuf,
}

impl VcrConfig {
    /// Creates a VCR config rooted at `dir`.
    #[must_use]
    pub fn new(mode: VcrMode, dir: impl Into<PathBuf>) -> Self {
        Self {
            mode,
            dir: dir.into(),
        }
    }

    /// Reads VCR config from `TAU_VCR` and `TAU_VCR_DIR`.
    ///
    /// Returns `None` when `TAU_VCR` is unset or `off`. Invalid VCR
    /// environment values panic because Tau's test/runtime environment is
    /// misconfigured and should fail loudly. `TAU_VCR_DIR` is required for
    /// `record-if-missing` and `replay-only` modes.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let mode = match std::env::var(ENV_MODE) {
            Ok(value) => VcrMode::parse(&value).unwrap_or_else(|error| panic!("{error}")),
            Err(std::env::VarError::NotPresent) => VcrMode::Off,
            Err(std::env::VarError::NotUnicode(_)) => panic!("{} is not valid Unicode", ENV_MODE),
        };
        if mode == VcrMode::Off {
            return None;
        }
        let dir = std::env::var_os(ENV_DIR).unwrap_or_else(|| {
            panic!("{} must be set when TAU_VCR is enabled", ENV_DIR);
        });
        Some(Self::new(mode, PathBuf::from(dir)))
    }

    /// Returns a cassette store rooted at this config's directory.
    #[must_use]
    pub fn store(&self) -> VcrStore {
        VcrStore::new(&self.dir)
    }
}

/// Bytes serialized as a single UTF-8 string using escaped-byte text.
///
/// Valid UTF-8 bytes are stored literally, literal backslashes are escaped as
/// `\\\\`, and invalid UTF-8 bytes are stored as ASCII `\\uDCxx` escape text.
/// The string therefore stays valid YAML/UTF-8 while preserving arbitrary
/// bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EscapedBytes(Vec<u8>);

impl EscapedBytes {
    /// Wraps raw bytes for escaped-byte serialization.
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    /// Returns the wrapped raw bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// Consumes the wrapper and returns the raw bytes.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl Serialize for EscapedBytes {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&encode_escaped_bytes(&self.0))
    }
}

impl<'de> Deserialize<'de> for EscapedBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        decode_escaped_bytes(&text)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// Encodes arbitrary bytes as valid UTF-8 text using escaped byte markers.
#[must_use]
pub fn encode_escaped_bytes(bytes: &[u8]) -> String {
    let mut encoded = String::new();
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(text) => {
                push_escaped_valid_text(&mut encoded, text);
                break;
            }
            Err(error) => {
                let (valid, rest) = remaining.split_at(error.valid_up_to());
                // SAFETY: `valid_up_to` is guaranteed to end on a UTF-8 boundary.
                push_escaped_valid_text(
                    &mut encoded,
                    std::str::from_utf8(valid).expect("valid prefix"),
                );
                remaining = rest;
                let invalid_len = error.error_len().unwrap_or(remaining.len());
                let (invalid, rest) = remaining.split_at(invalid_len);
                for byte in invalid {
                    use std::fmt::Write as _;
                    write!(&mut encoded, "\\uDC{byte:02X}").expect("write to string");
                }
                remaining = rest;
            }
        }
    }
    encoded
}

/// Decodes text produced by [`encode_escaped_bytes`] back into bytes.
pub fn decode_escaped_bytes(text: &str) -> Result<Vec<u8>, String> {
    let mut decoded = Vec::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            let mut buffer = [0; 4];
            decoded.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
            continue;
        }
        match chars.next() {
            Some('\\') => decoded.push(b'\\'),
            Some('u') => decoded.push(decode_escaped_bytes_byte(&mut chars)?),
            Some(other) => return Err(format!("unsupported escaped byte escape \\{other}")),
            None => return Err("trailing escaped byte backslash".to_owned()),
        }
    }
    Ok(decoded)
}

fn push_escaped_valid_text(encoded: &mut String, text: &str) {
    for ch in text.chars() {
        if ch == '\\' {
            encoded.push_str("\\\\");
        } else {
            encoded.push(ch);
        }
    }
}

fn decode_escaped_bytes_byte(chars: &mut std::str::Chars<'_>) -> Result<u8, String> {
    match (chars.next(), chars.next(), chars.next(), chars.next()) {
        (Some('D'), Some('C'), Some(high), Some(low)) => {
            let high = high
                .to_digit(16)
                .ok_or_else(|| "invalid escaped byte hex".to_owned())?;
            let low = low
                .to_digit(16)
                .ok_or_else(|| "invalid escaped byte hex".to_owned())?;
            let byte = (high << 4) | low;
            let byte = u8::try_from(byte).map_err(|_| "invalid escaped byte".to_owned())?;
            if byte < 0x80 {
                return Err("escaped bytes must use \\uDC80 through \\uDCFF".to_owned());
            }
            Ok(byte)
        }
        _ => Err("escaped bytes must use \\uDCxx".to_owned()),
    }
}

/// Filesystem-backed YAML cassette key/value store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VcrStore {
    dir: PathBuf,
}

impl VcrStore {
    /// Creates a cassette store rooted at `dir`.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Returns the cassette path for `key`.
    ///
    /// Keys are logical identifiers, not paths. Only ASCII alphanumeric
    /// characters, `-`, and `_` are accepted.
    fn path(&self, key: &str) -> Result<PathBuf, VcrError> {
        if key.is_empty()
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(VcrError::InvalidKey(key.to_owned()));
        }
        Ok(self.dir.join(format!("{key}.yaml")))
    }

    /// Reads and parses the cassette for `key`.
    ///
    /// Returns `Ok(None)` when the cassette does not exist.
    /// Only [`std::io::ErrorKind::NotFound`] is treated as absence. Other read
    /// failures return [`VcrError::Read`] so record-if-missing callers do not
    /// silently fall through to live recording when a cassette path is present
    /// but unreadable.
    ///
    /// # Errors
    ///
    /// Returns [`VcrError::InvalidKey`] for unsupported keys,
    /// [`VcrError::Read`] for non-`NotFound` read failures, and
    /// [`VcrError::Parse`] when an existing cassette cannot be deserialized
    /// as `T`.
    pub fn get<T>(&self, key: &str) -> Result<Option<T>, VcrError>
    where
        T: DeserializeOwned,
    {
        let path = self.path(key)?;
        match std::fs::read(&path) {
            Ok(bytes) => parse_yaml(&path, &bytes).map(Some),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(VcrError::Read { path, source }),
        }
    }

    /// Serializes and writes the cassette for `key`, replacing any existing
    /// file.
    pub fn put<T>(&self, key: &str, value: &T) -> Result<(), VcrError>
    where
        T: Serialize,
    {
        let path = self.path(key)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| VcrError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        write_yaml(&path, value)
    }
}

/// Builds a request-mismatch error with serialized expected and actual request
/// payloads for diagnostics.
///
/// The serialized payloads are included in [`VcrError`]'s
/// [`std::fmt::Display`] output because callers commonly surface VCR failures
/// by converting the error directly to a string. If serialization fails, that
/// side is replaced with a compact serialization-error marker.
pub fn request_mismatch<T, U>(key: impl Into<String>, expected: &T, actual: &U) -> VcrError
where
    T: Serialize,
    U: Serialize,
{
    VcrError::RequestMismatch {
        key: key.into(),
        expected: mismatch_payload(expected),
        actual: mismatch_payload(actual),
    }
}

/// Error returned by cassette storage and mode parsing.
#[derive(Debug)]
pub enum VcrError {
    /// `TAU_VCR` contained an unknown mode.
    InvalidMode(String),
    /// Cassette key contained unsupported characters.
    InvalidKey(String),
    /// Requested cassette was not found.
    Missing {
        /// Logical cassette key.
        key: String,
    },
    /// Failed to create a cassette directory.
    CreateDir {
        /// Directory path that could not be created.
        path: PathBuf,
        /// Underlying IO error.
        source: std::io::Error,
    },
    /// Failed to read a cassette file.
    Read {
        /// Cassette path.
        path: PathBuf,
        /// Underlying IO error.
        source: std::io::Error,
    },
    /// Failed to write a cassette file.
    Write {
        /// Cassette path.
        path: PathBuf,
        /// Underlying IO error.
        source: std::io::Error,
    },
    /// Failed to parse a cassette file.
    Parse {
        /// Cassette path.
        path: PathBuf,
        /// Underlying YAML error.
        source: serde_yaml_ng::Error,
    },
    /// Failed to serialize a cassette file.
    Serialize {
        /// Cassette path.
        path: PathBuf,
        /// Underlying YAML error.
        source: serde_yaml_ng::Error,
    },
    /// Cassette schema version is not supported by the caller.
    UnsupportedVersion {
        /// Logical cassette key.
        key: String,
        /// Version found in the cassette.
        version: u32,
    },
    /// Replay cassette request did not match the actual request.
    RequestMismatch {
        /// Logical cassette key.
        key: String,
        /// Request stored in the cassette, serialized for diagnostics.
        expected: String,
        /// Actual request supplied by the caller, serialized for diagnostics.
        actual: String,
    },
}

impl fmt::Display for VcrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMode(mode) => write!(f, "invalid TAU_VCR mode `{mode}`"),
            Self::InvalidKey(key) => write!(
                f,
                "invalid cassette key `{key}`; expected only a-z, A-Z, 0-9, -, or _"
            ),
            Self::Missing { key } => write!(f, "missing cassette `{key}`"),
            Self::CreateDir { path, source } => {
                write!(
                    f,
                    "failed to create cassette dir {}: {source}",
                    path.display()
                )
            }
            Self::Read { path, source } => {
                write!(f, "failed to read cassette {}: {source}", path.display())
            }
            Self::Write { path, source } => {
                write!(f, "failed to write cassette {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(f, "failed to parse cassette {}: {source}", path.display())
            }
            Self::Serialize { path, source } => {
                write!(
                    f,
                    "failed to serialize cassette {}: {source}",
                    path.display()
                )
            }
            Self::UnsupportedVersion { key, version } => {
                write!(f, "cassette `{key}` has unsupported version {version}")
            }
            Self::RequestMismatch {
                key,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "cassette `{key}` request does not match\nexpected:\n{expected}\nactual:\n{actual}"
                )
            }
        }
    }
}

impl std::error::Error for VcrError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CreateDir { source, .. }
            | Self::Read { source, .. }
            | Self::Write { source, .. } => Some(source),
            Self::Parse { source, .. } | Self::Serialize { source, .. } => Some(source),
            Self::InvalidMode(_)
            | Self::InvalidKey(_)
            | Self::Missing { .. }
            | Self::UnsupportedVersion { .. }
            | Self::RequestMismatch { .. } => None,
        }
    }
}

fn parse_yaml<T>(path: &Path, bytes: &[u8]) -> Result<T, VcrError>
where
    T: DeserializeOwned,
{
    serde_yaml_ng::from_slice(bytes).map_err(|source| VcrError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn write_yaml<T>(path: &Path, cassette: &T) -> Result<(), VcrError>
where
    T: Serialize,
{
    let text = serde_yaml_ng::to_string(cassette).map_err(|source| VcrError::Serialize {
        path: path.to_path_buf(),
        source,
    })?;
    std::fs::write(path, text).map_err(|source| VcrError::Write {
        path: path.to_path_buf(),
        source,
    })
}

fn mismatch_payload<T>(value: &T) -> String
where
    T: Serialize,
{
    serde_yaml_ng::to_string(value).unwrap_or_else(|error| format!("<serialize error: {error}>"))
}

#[cfg(test)]
mod tests;
