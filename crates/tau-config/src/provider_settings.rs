//! Shared provider-profile bounds, safe reads, lifecycle locking, and closed
//! credential-selection schema.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _};
use std::path::Path;

use serde::Serialize;
use tau_proto::{ExtensionDataPath, ProviderName};

mod instance_lock;

pub use instance_lock::{ProviderSettingsInstanceLock, ProviderSettingsLockAttempt};

/// Maximum provider profiles accepted for one extension instance.
pub const MAX_PROVIDER_PROFILE_FILES: usize = 4_096;
/// Maximum bytes accepted from one provider profile.
pub const MAX_PROVIDER_PROFILE_FILE_BYTES: u64 = 1024 * 1024;
/// Maximum merged profile bytes, reserving one MiB for the Configure envelope.
pub const MAX_PROVIDER_PROFILE_SNAPSHOT_BYTES: u64 =
    tau_proto::MAX_PROTOCOL_MESSAGE_BYTES - MAX_PROVIDER_PROFILE_FILE_BYTES;

/// Leaf-symlink policy for one provider profile read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderProfileLeafSymlinkPolicy {
    /// Follow a leaf symlink after validating the opened descriptor.
    Follow,
    /// Reject a leaf symlink.
    Reject,
}

/// Read one bounded provider profile after validating the opened descriptor.
pub fn read_provider_profile(
    path: &Path,
    leaf_symlink_policy: ProviderProfileLeafSymlinkPolicy,
) -> io::Result<Vec<u8>> {
    let file = open_provider_profile(path, leaf_symlink_policy)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "provider profile does not resolve to a regular file",
        ));
    }
    if MAX_PROVIDER_PROFILE_FILE_BYTES < metadata.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("provider profile exceeds {MAX_PROVIDER_PROFILE_FILE_BYTES} bytes"),
        ));
    }
    let mut contents = Vec::new();
    file.take(MAX_PROVIDER_PROFILE_FILE_BYTES + 1)
        .read_to_end(&mut contents)?;
    if MAX_PROVIDER_PROFILE_FILE_BYTES < contents.len() as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("provider profile exceeds {MAX_PROVIDER_PROFILE_FILE_BYTES} bytes"),
        ));
    }
    Ok(contents)
}

#[cfg(unix)]
fn open_provider_profile(
    path: &Path,
    leaf_symlink_policy: ProviderProfileLeafSymlinkPolicy,
) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let no_follow = match leaf_symlink_policy {
        ProviderProfileLeafSymlinkPolicy::Follow => 0,
        ProviderProfileLeafSymlinkPolicy::Reject => libc::O_NOFOLLOW,
    };
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | no_follow)
        .open(path)
}

#[cfg(not(unix))]
fn open_provider_profile(
    path: &Path,
    _leaf_symlink_policy: ProviderProfileLeafSymlinkPolicy,
) -> io::Result<File> {
    File::open(path)
}

/// The only credential slots owned by a built-in provider profile.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ProviderCredentialSlot {
    /// ChatGPT OAuth credential record.
    OAuth,
    /// API-key credential record.
    ApiKey,
}

impl ProviderCredentialSlot {
    /// Return every credential slot owned by the built-in provider schema.
    #[must_use]
    pub fn all() -> [Self; 2] {
        [Self::OAuth, Self::ApiKey]
    }

    /// Returns the canonical Secret-scope path for this credential identity and
    /// slot.
    #[must_use]
    pub fn path(self, identity: &ProviderCredentialIdentity) -> ExtensionDataPath {
        let file = match self {
            Self::OAuth => "oauth.json",
            Self::ApiKey => "api-key.json",
        };
        ExtensionDataPath::new(format!("providers/{identity}/{file}"))
    }

    fn kind(self) -> &'static str {
        match self {
            Self::OAuth => "oauth",
            Self::ApiKey => "api_key",
        }
    }
}

/// Stable opaque identity of one provider profile's credential storage.
///
/// The identity survives a provider namespace rename. Its closed lowercase
/// hexadecimal representation can name only the corresponding two credential
/// slots under the selected extension's Secret scope.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ProviderCredentialIdentity {
    /// Canonical 128-bit lowercase hexadecimal identity.
    value: String,
}

impl ProviderCredentialIdentity {
    /// Creates a fresh cryptographically random credential-storage identity.
    #[must_use]
    pub fn random() -> Self {
        let bytes: [u8; 16] = rand::random();
        Self {
            value: bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
        }
    }

    /// Parses one canonical opaque credential-storage identity.
    pub fn parse(value: &str) -> Result<Self, ProviderCredentialError> {
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(invalid("provider credential identity is invalid"));
        }
        Ok(Self {
            value: value.to_owned(),
        })
    }

    /// Returns the canonical identity string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for ProviderCredentialIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value.fmt(formatter)
    }
}

/// A validated credential destination and optional named source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderCredentialReference {
    /// Closed credential slot selected by the profile kind.
    slot: ProviderCredentialSlot,
    /// Stable opaque owner of the credential storage.
    identity: ProviderCredentialIdentity,
    /// Exact canonical Secret-scope record path.
    path: ExtensionDataPath,
    /// Declared named secret that setup/startup materializes, if any.
    named_source: Option<String>,
}

/// One explicit provider authentication mode selected by portable settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderCredential {
    /// Load a typed credential record from the configured instance's Secret
    /// scope.
    Stored(ProviderCredentialReference),
    /// Send requests without authentication and perform no Secret-scope lookup.
    Keyless,
}

/// Error returned for an invalid stored or keyless provider credential
/// selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderCredentialError {
    /// Redacted explanation of the schema violation.
    message: String,
}

impl fmt::Display for ProviderCredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(formatter)
    }
}

impl std::error::Error for ProviderCredentialError {}

fn invalid(message: impl Into<String>) -> ProviderCredentialError {
    ProviderCredentialError {
        message: message.into(),
    }
}

impl ProviderCredentialReference {
    /// Construct a reference whose destination and source combination satisfy
    /// the closed provider credential schema.
    pub fn new(
        identity: ProviderCredentialIdentity,
        slot: ProviderCredentialSlot,
        named_source: Option<&str>,
    ) -> Result<Self, ProviderCredentialError> {
        if slot == ProviderCredentialSlot::OAuth && named_source.is_some() {
            return Err(invalid("OAuth credentials cannot bind a named source"));
        }
        if let Some(name) = named_source {
            crate::secret_sources::validate_secret_name(name)
                .map_err(|_| invalid("provider credential source is invalid"))?;
        }
        Ok(Self {
            slot,
            path: slot.path(&identity),
            identity,
            named_source: named_source.map(str::to_owned),
        })
    }

    /// Return the closed credential slot.
    #[must_use]
    pub fn slot(&self) -> ProviderCredentialSlot {
        self.slot
    }

    /// Returns the stable opaque owner of the credential storage.
    #[must_use]
    pub fn identity(&self) -> &ProviderCredentialIdentity {
        &self.identity
    }

    /// Return the canonical Secret-scope destination.
    #[must_use]
    pub fn path(&self) -> &ExtensionDataPath {
        &self.path
    }

    /// Return the validated named source, when configured.
    #[must_use]
    pub fn named_source(&self) -> Option<&str> {
        self.named_source.as_deref()
    }

    /// Serialize this validated reference into provider settings JSON.
    #[must_use]
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(SerializedReference {
            kind: self.slot.kind(),
            identity: self.identity.as_str(),
            source: self.named_source().map(|name| SerializedSource {
                kind: "named_secret",
                name,
            }),
        })
        .expect("validated credential reference must serialize")
    }
}

/// Borrowed named-source representation used only after validation.
#[derive(Serialize)]
struct SerializedSource<'a> {
    /// Closed source discriminator.
    kind: &'static str,
    /// Validated configured declaration name.
    name: &'a str,
}

/// Borrowed credential-reference representation used only after validation.
#[derive(Serialize)]
struct SerializedReference<'a> {
    /// Closed credential-slot discriminator.
    kind: &'static str,
    /// Stable opaque owner of the credential storage.
    identity: &'a str,
    /// Optional validated named source.
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<SerializedSource<'a>>,
}

/// Parse the only credential reference form accepted in one provider settings
/// object. The caller retains ownership of all non-credential settings fields.
pub fn parse_provider_credential_reference(
    _provider: &ProviderName,
    settings: &serde_json::Map<String, serde_json::Value>,
) -> Result<ProviderCredentialReference, ProviderCredentialError> {
    match parse_provider_credential(_provider, settings)? {
        ProviderCredential::Stored(reference) => Ok(reference),
        ProviderCredential::Keyless => Err(invalid(
            "provider settings select keyless authentication".to_owned(),
        )),
    }
}

/// Parse the closed stored-credential or explicitly keyless provider schema.
pub fn parse_provider_credential(
    _provider: &ProviderName,
    settings: &serde_json::Map<String, serde_json::Value>,
) -> Result<ProviderCredential, ProviderCredentialError> {
    if settings.contains_key("auth")
        || settings.contains_key("api_key")
        || settings.contains_key("api_key_secret")
    {
        return Err(invalid(
            "provider settings must not contain credential fields".to_owned(),
        ));
    }
    let credential = settings.get("credential").ok_or_else(|| {
        invalid("provider settings are missing a credential reference".to_owned())
    })?;
    let credential = credential
        .as_object()
        .ok_or_else(|| invalid("provider credential reference must be an object".to_owned()))?;
    let kind = credential
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid("provider credential reference is missing kind".to_owned()))?;
    if kind == "none" {
        if credential.len() != 1 {
            return Err(invalid(
                "keyless provider credential has unknown fields".to_owned(),
            ));
        }
        return Ok(ProviderCredential::Keyless);
    }
    let slot = match kind {
        "oauth" => ProviderCredentialSlot::OAuth,
        "api_key" => ProviderCredentialSlot::ApiKey,
        _ => {
            return Err(invalid(
                "unknown provider credential reference kind".to_owned(),
            ));
        }
    };
    let identity = credential
        .get("identity")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid("provider credential reference is missing identity".to_owned()))?;
    let identity = ProviderCredentialIdentity::parse(identity)?;
    let named_source = match credential.get("source") {
        None => None,
        Some(value) if slot == ProviderCredentialSlot::ApiKey => {
            let source = value.as_object().ok_or_else(|| {
                invalid("provider credential source must be an object".to_owned())
            })?;
            if source.len() != 2
                || source.get("kind").and_then(serde_json::Value::as_str) != Some("named_secret")
            {
                return Err(invalid("provider credential source is invalid".to_owned()));
            }
            let name = source
                .get("name")
                .and_then(serde_json::Value::as_str)
                .filter(|name| crate::secret_sources::validate_secret_name(name).is_ok())
                .ok_or_else(|| invalid("provider credential source is invalid".to_owned()))?;
            Some(name.to_owned())
        }
        Some(_) => {
            return Err(invalid(
                "OAuth credentials cannot bind a named source".to_owned(),
            ));
        }
    };
    if credential.len() != usize::from(named_source.is_some()) + 2 {
        return Err(invalid(
            "provider credential reference has unknown fields".to_owned(),
        ));
    }
    ProviderCredentialReference::new(identity, slot, named_source.as_deref())
        .map(ProviderCredential::Stored)
}

#[cfg(test)]
mod tests;
