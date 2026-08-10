//! Canonical named-secret source resolution shared by setup and harness
//! startup.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::{fmt, io};

use tau_proto::SecretValue;

use crate::settings::ExtensionSecretEntry;

/// Maximum complete file size accepted by Tau's extension Secret scope.
pub const MAX_SECRET_DATA_FILE_BYTES: u64 = 1024 * 1024;

/// Whether loading removes one-shot secret variables from the process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvironmentDisposition {
    /// Keep variables available to the setup process.
    Retain,
    /// Remove variables after capturing one startup snapshot.
    RemoveAfterSnapshot,
}

/// Error from resolving a named secret without exposing its value.
#[derive(Debug)]
pub enum SecretSourceError {
    /// The configured name is not one safe filesystem component.
    InvalidName(String),
    /// More than one environment variable normalized to one source name.
    EnvironmentCollision(String),
    /// A source file did not contain UTF-8.
    InvalidUtf8(PathBuf),
    /// A source file could not be read.
    Io { path: PathBuf, source: io::Error },
    /// A required declared source was unavailable.
    MissingRequired { extension: String, secret: String },
}

impl fmt::Display for SecretSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(name) => write!(formatter, "invalid secret name `{name}`"),
            Self::EnvironmentCollision(name) => {
                write!(
                    formatter,
                    "multiple TAU_SECRET_* variables normalize to `{name}`"
                )
            }
            Self::InvalidUtf8(path) => {
                write!(formatter, "secret file {} is not UTF-8", path.display())
            }
            Self::Io { path, source } => write!(
                formatter,
                "failed to read secret file {}: {source}",
                path.display()
            ),
            Self::MissingRequired { extension, secret } => write!(
                formatter,
                "required secret `{secret}` for extension `{extension}` is missing; create <state_dir>/secrets/{}.yaml or set TAU_SECRET_{}",
                secret.to_ascii_lowercase(),
                secret.to_ascii_uppercase()
            ),
        }
    }
}

impl std::error::Error for SecretSourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::InvalidName(_)
            | Self::EnvironmentCollision(_)
            | Self::InvalidUtf8(_)
            | Self::MissingRequired { .. } => None,
        }
    }
}

/// Snapshot of normalized environment sources. Its Debug output is
/// value-redacted.
#[derive(Default)]
pub struct SecretSources {
    /// Values keyed by their lowercased declaration name.
    environment: HashMap<String, String>,
}

impl fmt::Debug for SecretSources {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretSources")
            .field("names", &self.environment.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Load and optionally remove `TAU_SECRET_*` values using the harness's exact
/// case-normalization and collision semantics.
#[allow(unsafe_code)]
pub fn load_secret_sources(
    disposition: EnvironmentDisposition,
) -> Result<SecretSources, SecretSourceError> {
    let mut environment = HashMap::new();
    let mut keys = Vec::new();
    let mut failure = None;
    for (key, value) in std::env::vars() {
        let Some(suffix) = key.strip_prefix("TAU_SECRET_") else {
            continue;
        };
        let name = suffix.to_ascii_lowercase();
        keys.push(key);
        if let Err(error) = validate_secret_name(&name) {
            failure.get_or_insert(error);
            continue;
        }
        if !value.is_empty() && environment.insert(name.clone(), value).is_some() {
            failure.get_or_insert(SecretSourceError::EnvironmentCollision(name));
        }
    }
    if disposition == EnvironmentDisposition::RemoveAfterSnapshot {
        for key in keys {
            // Called by the single-threaded setup/startup boundary before children.
            unsafe { std::env::remove_var(key) };
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(SecretSources { environment })
}

/// Resolve one exact configured declaration with the canonical source
/// precedence and optionality semantics.
pub fn resolve_declared_secret(
    state_dir: &Path,
    sources: &SecretSources,
    extension: &str,
    name: &str,
    declaration: &ExtensionSecretEntry,
) -> Result<Option<SecretValue>, SecretSourceError> {
    let value = resolve_named_secret(state_dir, sources, name)?;
    match value {
        Some(value) => Ok(Some(SecretValue::new(value))),
        None if declaration.optional => Ok(None),
        None => Err(SecretSourceError::MissingRequired {
            extension: extension.to_owned(),
            secret: name.to_owned(),
        }),
    }
}

/// Resolve one configured name. Environment wins over a nonempty trimmed file.
pub fn resolve_named_secret(
    state_dir: &Path,
    sources: &SecretSources,
    name: &str,
) -> Result<Option<String>, SecretSourceError> {
    validate_secret_name(name)?;
    let normalized = name.to_ascii_lowercase();
    if let Some(value) = sources.environment.get(&normalized) {
        return Ok((!value.trim().is_empty()).then(|| value.trim().to_owned()));
    }
    let path = state_dir.join("secrets").join(format!("{normalized}.yaml"));
    match std::fs::read(&path) {
        Ok(bytes) => {
            let value =
                String::from_utf8(bytes).map_err(|_| SecretSourceError::InvalidUtf8(path))?;
            Ok((!value.trim().is_empty()).then(|| value.trim().to_owned()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SecretSourceError::Io { path, source }),
    }
}

/// Validate a secret declaration/source name before it can become a path
/// component.
pub fn validate_secret_name(name: &str) -> Result<(), SecretSourceError> {
    if !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Ok(());
    }
    Err(SecretSourceError::InvalidName(name.to_owned()))
}

#[cfg(test)]
mod tests;
