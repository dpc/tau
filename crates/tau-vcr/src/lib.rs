//! Minimal YAML cassette storage helpers for Tau tests.
//!
//! `tau-vcr` deliberately stays below provider and tool semantics. It owns VCR
//! mode parsing, cassette directory/key handling, key validation, and YAML
//! `get`/`put` operations. Callers own cassette schemas, request validation,
//! live-vs-replay branching, timing, and response replay.
use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::{env as path_std_env, fmt, fs as path_std_fs, io as path_std_io};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de as path_serde_de};

const ENV_MODE: &str = "TAU_VCR";
const ENV_DIR: &str = "TAU_VCR_DIR";
const MAX_CASSETTE_BYTES: u64 = 1024 * 1024;

/// A non-validating maximum byte limit for bounded side artifacts.
///
/// The wrapper preserves every raw `u64` limit, including zero and
/// [`u64::MAX`], while owning the saturating extra-byte probe used to detect
/// read-time growth.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteLimit(u64);

impl ByteLimit {
    /// Wraps a raw maximum byte limit without validating or narrowing it.
    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    /// Returns the raw maximum byte limit.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns the bounded read probe, retaining the maximum raw limit.
    #[must_use]
    const fn read_probe(self) -> u64 {
        self.0.saturating_add(1)
    }
}

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
            Err(path_std_env::VarError::NotPresent) => VcrMode::Off,
            Err(path_std_env::VarError::NotUnicode(_)) => {
                panic!("{} is not valid Unicode", ENV_MODE)
            }
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
            .map_err(path_serde_de::Error::custom)
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
                // SAFETY: `valid_up_to` is guaranteed to end on a UTF-8
                // boundary.
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

/// Validated semantic name for one cassette-owned side artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactKind(
    /// Validated filename suffix without a leading dot.
    String,
);

impl ArtifactKind {
    /// Validates an ASCII alphanumeric/hyphen artifact kind.
    pub fn new(value: impl Into<String>) -> Result<Self, VcrError> {
        let value = value.into();
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(VcrError::InvalidArtifactKind(value));
        }
        Ok(Self(value))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

impl VcrStore {
    /// Publishes a cassette together with one private bounded side artifact.
    ///
    /// The filename is derived only from the validated cassette key and
    /// [`ArtifactKind`]. A private per-key advisory lock serializes paired
    /// publication and permits safe recovery of interrupted side-without-
    /// cassette states. The side is published exclusively before the cassette;
    /// cassette failure triggers best-effort side removal.
    pub fn put_with_side<T>(
        &self,
        key: &str,
        artifact_kind: &ArtifactKind,
        value: &T,
        side: &[u8],
        max_side_bytes: ByteLimit,
    ) -> Result<(), VcrError>
    where
        T: Serialize,
    {
        let cassette_path = self.path(key)?;
        let side_path = self.side_path(key, artifact_kind)?;
        self.ensure_private_dir(&cassette_path)?;
        let _lock = self.lock_key(key)?;
        self.remove_interrupted_stages(&cassette_path, Some(&side_path))?;
        // A process that died after publishing the side but before publishing
        // the cassette leaves a reclaimable orphan. The per-key advisory lock
        // proves no live cooperating publisher owns that transition.
        if !cassette_path.exists() && side_path.exists() {
            let metadata =
                std::fs::symlink_metadata(&side_path).map_err(|source| VcrError::Read {
                    path: side_path.clone(),
                    source,
                })?;
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(VcrError::UnsafePath { path: side_path });
            }
            std::fs::remove_file(&side_path).map_err(|source| VcrError::Write {
                path: side_path.clone(),
                source,
            })?;
        }
        if max_side_bytes.get() < u64::try_from(side.len()).unwrap_or(u64::MAX) {
            return Err(VcrError::TooLarge {
                path: side_path,
                bytes: u64::try_from(side.len()).unwrap_or(u64::MAX),
                limit: max_side_bytes.get(),
            });
        }
        write_bytes_exclusive(&side_path, side)?;
        if let Err(error) = write_yaml_exclusive(&cassette_path, value) {
            let _ = std::fs::remove_file(&side_path);
            return Err(error);
        }
        Ok(())
    }

    fn lock_key(&self, key: &str) -> Result<std::fs::File, VcrError> {
        let lock_path = self.dir.join(format!("{key}.bundle.lock"));
        let mut lock_options = OpenOptions::new();
        lock_options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            lock_options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let lock = lock_options
            .open(&lock_path)
            .map_err(|source| VcrError::Write {
                path: lock_path.clone(),
                source,
            })?;
        let lock_metadata = lock.metadata().map_err(|source| VcrError::Read {
            path: lock_path.clone(),
            source,
        })?;
        if !lock_metadata.is_file() {
            return Err(VcrError::UnsafePath { path: lock_path });
        }
        fs2::FileExt::lock_exclusive(&lock).map_err(|source| VcrError::Write {
            path: lock_path,
            source,
        })?;
        Ok(lock)
    }

    /// Reads the key-and-kind-derived private side artifact with the same
    /// confinement, no-follow, regular-file, and size rules as cassette reads.
    pub fn get_side(
        &self,
        key: &str,
        artifact_kind: &ArtifactKind,
        max_bytes: ByteLimit,
    ) -> Result<Vec<u8>, VcrError> {
        let path = self.side_path(key, artifact_kind)?;
        #[cfg(not(unix))]
        return Err(VcrError::UnsafePath { path });
        reject_symlink_components(&self.dir)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options.open(&path).map_err(|source| VcrError::Read {
            path: path.clone(),
            source,
        })?;
        let metadata = file.metadata().map_err(|source| VcrError::Read {
            path: path.clone(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(VcrError::UnsafePath { path });
        }
        if max_bytes.get() < metadata.len() {
            return Err(VcrError::TooLarge {
                path,
                bytes: metadata.len(),
                limit: max_bytes.get(),
            });
        }
        let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
        file.take(max_bytes.read_probe())
            .read_to_end(&mut bytes)
            .map_err(|source| VcrError::Read {
                path: path.clone(),
                source,
            })?;
        if max_bytes.get() < u64::try_from(bytes.len()).unwrap_or(u64::MAX) {
            return Err(VcrError::TooLarge {
                path,
                bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                limit: max_bytes.get(),
            });
        }
        Ok(bytes)
    }

    fn side_path(&self, key: &str, artifact_kind: &ArtifactKind) -> Result<PathBuf, VcrError> {
        self.path(key)?;
        Ok(self.dir.join(format!("{key}.{}", artifact_kind.as_str())))
    }

    fn ensure_private_dir(&self, path: &Path) -> Result<(), VcrError> {
        #[cfg(not(unix))]
        return Err(VcrError::UnsafePath {
            path: path.to_path_buf(),
        });
        if let Some(parent) = path.parent() {
            reject_symlink_components(parent)?;
            let existed = parent.exists();
            std::fs::create_dir_all(parent).map_err(|source| VcrError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
            reject_symlink_components(parent)?;
            #[cfg(unix)]
            if !existed {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(parent, path_std_fs::Permissions::from_mode(0o700))
                    .map_err(|source| VcrError::CreateDir {
                        path: parent.to_path_buf(),
                        source,
                    })?;
            }
        }
        Ok(())
    }

    fn remove_interrupted_stages(
        &self,
        cassette_path: &Path,
        side_path: Option<&Path>,
    ) -> Result<(), VcrError> {
        for path in std::iter::once(stage_path(cassette_path)).chain(side_path.map(stage_path)) {
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    std::fs::remove_file(&path)
                        .map_err(|source| VcrError::Write { path, source })?;
                }
                Ok(_) => return Err(VcrError::UnsafePath { path }),
                Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(VcrError::Read { path, source });
                }
            }
        }
        Ok(())
    }

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
    /// as `T`. Unsafe non-regular paths and cassettes over the byte limit
    /// return [`VcrError::UnsafePath`] and [`VcrError::TooLarge`],
    /// respectively.
    pub fn get<T>(&self, key: &str) -> Result<Option<T>, VcrError>
    where
        T: DeserializeOwned,
    {
        let path = self.path(key)?;
        #[cfg(not(unix))]
        return Err(VcrError::UnsafePath { path });
        reject_symlink_components(&self.dir)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = match options.open(&path) {
            Ok(file) => file,
            Err(source) if source.kind() == path_std_io::ErrorKind::NotFound => return Ok(None),
            #[cfg(unix)]
            Err(source) if source.raw_os_error() == Some(libc::ELOOP) => {
                return Err(VcrError::UnsafePath { path });
            }
            Err(source) => return Err(VcrError::Read { path, source }),
        };
        let metadata = file.metadata().map_err(|source| VcrError::Read {
            path: path.clone(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(VcrError::UnsafePath { path });
        }
        if metadata.len() > MAX_CASSETTE_BYTES {
            return Err(VcrError::TooLarge {
                path,
                bytes: metadata.len(),
                limit: MAX_CASSETTE_BYTES,
            });
        }
        let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
        file.take(MAX_CASSETTE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| VcrError::Read {
                path: path.clone(),
                source,
            })?;
        parse_yaml(&path, &bytes).map(Some)
    }

    /// Serializes and publishes a new cassette for `key`.
    ///
    /// Publishing is atomic and exclusive: an existing cassette is never
    /// overwritten. The temporary file and final hard link are private to the
    /// current user on Unix.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid keys, directory creation, serialization,
    /// size-limit, temporary write, sync, or exclusive publication failures.
    /// In particular, publishing an existing key fails rather than overwriting.
    pub fn put<T>(&self, key: &str, value: &T) -> Result<(), VcrError>
    where
        T: Serialize,
    {
        let path = self.path(key)?;
        self.ensure_private_dir(&path)?;
        let _lock = self.lock_key(key)?;
        self.remove_interrupted_stages(&path, None)?;
        write_yaml_exclusive(&path, value)
    }
}

fn write_bytes_exclusive(path: &Path, bytes: &[u8]) -> Result<(), VcrError> {
    let temporary = stage_path(path);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::hard_link(&temporary, path)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&temporary);
    result.map_err(|source| VcrError::Write {
        path: path.to_path_buf(),
        source,
    })
}

fn reject_symlink_components(path: &Path) -> Result<(), VcrError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(VcrError::UnsafePath {
            path: path.to_path_buf(),
        }),
        Ok(_) => Ok(()),
        Err(source) if source.kind() == path_std_io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(VcrError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Builds a request-mismatch error with bounded, opaque expected and actual
/// payload summaries for diagnostics.
///
/// Raw requests can contain prompts, tool output, identifiers, or credentials,
/// so mismatch diagnostics disclose no request-derived content.
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
    /// Side-artifact kind contained unsupported characters.
    InvalidArtifactKind(String),
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
    /// Cassette exceeds the storage resource limit.
    TooLarge {
        /// Cassette path.
        path: PathBuf,
        /// Observed serialized byte count.
        bytes: u64,
        /// Maximum permitted byte count.
        limit: u64,
    },
    /// Cassette path is a symbolic link or otherwise unsafe to follow.
    UnsafePath {
        /// Unsafe cassette path.
        path: PathBuf,
    },
    /// Cassette schema version is not supported by the caller.
    UnsupportedVersion {
        /// Logical cassette key.
        key: String,
        /// Version found in the cassette.
        version: u32,
    },
    /// Cassette violates a caller-owned schema or resource invariant.
    InvalidCassette {
        /// Logical cassette key.
        key: String,
        /// Bounded diagnostic that contains no captured payload.
        reason: String,
    },
    /// Replay cassette request did not match the actual request.
    RequestMismatch {
        /// Logical cassette key.
        key: String,
        /// Bounded redacted expected-request summary.
        expected: String,
        /// Bounded redacted actual-request summary.
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
            Self::InvalidArtifactKind(kind) => {
                write!(f, "invalid VCR side-artifact kind `{kind}`")
            }
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
            Self::TooLarge { path, bytes, limit } => write!(
                f,
                "cassette {} is too large: {bytes} bytes exceeds {limit}",
                path.display()
            ),
            Self::UnsafePath { path } => {
                write!(f, "refusing unsafe cassette path {}", path.display())
            }
            Self::UnsupportedVersion { key, version } => {
                write!(f, "cassette `{key}` has unsupported version {version}")
            }
            Self::InvalidCassette { key, reason } => {
                write!(f, "cassette `{key}` is invalid: {reason}")
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
            | Self::InvalidArtifactKind(_)
            | Self::Missing { .. }
            | Self::TooLarge { .. }
            | Self::UnsafePath { .. }
            | Self::UnsupportedVersion { .. }
            | Self::InvalidCassette { .. }
            | Self::RequestMismatch { .. } => None,
        }
    }
}

fn parse_yaml<T>(path: &Path, bytes: &[u8]) -> Result<T, VcrError>
where
    T: DeserializeOwned,
{
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_CASSETTE_BYTES {
        return Err(VcrError::TooLarge {
            path: path.to_path_buf(),
            bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            limit: MAX_CASSETTE_BYTES,
        });
    }
    serde_yaml_ng::from_slice(bytes).map_err(|source| VcrError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn write_yaml_exclusive<T>(path: &Path, cassette: &T) -> Result<(), VcrError>
where
    T: Serialize,
{
    let text = serde_yaml_ng::to_string(cassette).map_err(|source| VcrError::Serialize {
        path: path.to_path_buf(),
        source,
    })?;
    if u64::try_from(text.len()).unwrap_or(u64::MAX) > MAX_CASSETTE_BYTES {
        return Err(VcrError::TooLarge {
            path: path.to_path_buf(),
            bytes: u64::try_from(text.len()).unwrap_or(u64::MAX),
            limit: MAX_CASSETTE_BYTES,
        });
    }
    let temporary = stage_path(path);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::hard_link(&temporary, path)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&temporary);
    result.map_err(|source| VcrError::Write {
        path: path.to_path_buf(),
        source,
    })
}

fn mismatch_payload<T>(value: &T) -> String
where
    T: Serialize,
{
    let _ = value;
    "<redacted payload>".to_owned()
}

fn stage_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("cassette");
    path.with_file_name(format!(".{file_name}.stage"))
}

#[cfg(test)]
mod tests;
