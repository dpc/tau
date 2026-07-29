//! Harness-owned secret loading and per-extension resolution.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

use tau_config::settings::ExtensionSecretEntry;
use tau_proto::SecretValue;

use crate::settings::{Config, ExtensionStartupDiagnostic};

const ENV_PREFIX: &str = "TAU_SECRET_";

/// Errors reported while collecting or resolving extension secrets.
#[derive(Debug)]
pub enum SecretsError {
    /// A configured or environment-derived secret name is unsafe.
    InvalidName { name: String, context: String },
    /// A normalized environment secret name was provided more than once.
    EnvCollision { name: String },
    /// The secret file could not be read as UTF-8 text.
    InvalidUtf8 { path: PathBuf },
    /// A required configured secret is unavailable.
    MissingRequired { extension: String, secret: String },
    /// A secret file could not be read.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for SecretsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { name, context } => {
                write!(
                    f,
                    "invalid secret name `{name}` in {context}; secret names may contain only ASCII letters, digits, '.', '_' and '-'"
                )
            }
            Self::EnvCollision { name } => write!(
                f,
                "multiple TAU_SECRET_* environment variables normalize to secret `{name}`"
            ),
            Self::InvalidUtf8 { path } => {
                write!(f, "secret file {} is not valid UTF-8", path.display())
            }
            Self::MissingRequired { extension, secret } => write!(
                f,
                "required secret `{secret}` for extension `{extension}` is missing; create <state_dir>/secrets/{}.yaml or set TAU_SECRET_{}",
                secret.to_ascii_lowercase(),
                secret.to_ascii_uppercase()
            ),
            Self::Io { path, source } => {
                write!(f, "failed to read secret file {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for SecretsError {}

/// Resolved secret source snapshot. Values must not be logged.
#[derive(Default)]
pub struct SecretSources {
    env: HashMap<String, String>,
}

impl fmt::Debug for SecretSources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretSources")
            .field("env_secret_count", &self.env.len())
            .field("env_secret_names", &self.env.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Return true when `name` is a conservative single path component.
#[must_use]
pub fn is_valid_secret_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn validate_secret_name(name: &str, context: impl Into<String>) -> Result<(), SecretsError> {
    if is_valid_secret_name(name) {
        Ok(())
    } else {
        Err(SecretsError::InvalidName {
            name: name.to_owned(),
            context: context.into(),
        })
    }
}

/// Collect all `TAU_SECRET_*` variables and remove them from this process.
#[allow(unsafe_code)]
pub fn load_secret_sources() -> Result<SecretSources, SecretsError> {
    let mut env = HashMap::new();
    let mut remove = Vec::new();
    let mut error = None;
    for (key, value) in std::env::vars() {
        if let Some(suffix) = key.strip_prefix(ENV_PREFIX) {
            remove.push(key.clone());
            let name = suffix.to_ascii_lowercase();
            if let Err(err) = validate_secret_name(&name, "environment") {
                error.get_or_insert(err);
                continue;
            }
            if !value.is_empty() && env.insert(name.clone(), value).is_some() {
                error.get_or_insert(SecretsError::EnvCollision { name });
            }
        }
    }
    for key in remove {
        // SAFETY: `Harness::from_config` calls this during startup before any
        // supervised extensions are spawned, and before Tau starts background
        // threads that read environment variables. Removing these one-shot
        // secret variables here prevents later child processes from inheriting
        // them from the harness process environment.
        unsafe { std::env::remove_var(key) };
    }
    if let Some(error) = error {
        return Err(error);
    }
    Ok(SecretSources { env })
}

fn read_file_secret(state_dir: &Path, name: &str) -> Result<Option<String>, SecretsError> {
    let path = state_dir.join("secrets").join(format!("{name}.yaml"));
    match std::fs::read(&path) {
        Ok(bytes) => {
            // Preserve this behavior; the structural alternative is not semantics-neutral
            // here. ast-grep-ignore: silent-map-err
            let text = String::from_utf8(bytes)
                .map_err(|_| SecretsError::InvalidUtf8 { path: path.clone() })?;
            let value = text.trim().to_owned();
            Ok((!value.is_empty()).then_some(value))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SecretsError::Io { path, source }),
    }
}

fn resolve_one_secret(
    state_dir: &Path,
    sources: &SecretSources,
    extension: &str,
    name: &str,
    declaration: &ExtensionSecretEntry,
) -> Result<Option<SecretValue>, SecretsError> {
    validate_secret_name(name, format!("extension `{extension}` secrets"))?;
    let normalized_name = name.to_ascii_lowercase();
    let file = read_file_secret(state_dir, &normalized_name)?;
    let value = sources.env.get(&normalized_name).cloned().or(file);
    match value {
        Some(value) if !value.is_empty() => Ok(Some(SecretValue::new(value))),
        _ if declaration.optional => Ok(None),
        _ => Err(SecretsError::MissingRequired {
            extension: extension.to_owned(),
            secret: name.to_owned(),
        }),
    }
}

/// Secret resolution result for all configured extensions.
#[derive(Debug)]
pub struct ResolvedExtensionSecrets {
    /// Per-extension secrets authorized for Configure messages.
    pub secrets: BTreeMap<String, BTreeMap<String, SecretValue>>,
    /// Optional extensions skipped because their secret declarations could not
    /// resolve safely.
    pub skipped_extensions: BTreeSet<String>,
    /// Mandatory warning diagnostics explaining optional secret-resolution
    /// skips.
    pub diagnostics: Vec<ExtensionStartupDiagnostic>,
}

/// Resolve all configured extension secrets from files and one-shot env vars.
pub fn resolve_extension_secrets(
    config: &Config,
    state_dir: &Path,
    sources: &SecretSources,
) -> Result<ResolvedExtensionSecrets, SecretsError> {
    let mut out = BTreeMap::new();
    let mut skipped_extensions = BTreeSet::new();
    let mut diagnostics = Vec::new();
    for (extension, extension_config) in &config.extensions {
        let mut secrets = BTreeMap::new();
        for (name, declaration) in &extension_config.secrets {
            match resolve_one_secret(state_dir, sources, extension, name, declaration) {
                Ok(Some(value)) => {
                    secrets.insert(name.clone(), value);
                }
                Ok(None) => {}
                Err(error) if !extension_config.require => {
                    tracing::warn!(
                        target: "tau_harness::startup",
                        extension = %extension,
                        error = %error,
                        "optional extension did not initialize during secret resolution"
                    );
                    diagnostics.push(ExtensionStartupDiagnostic {
                        extension: extension.clone(),
                        message: format!("optional extension {extension} did not initialize"),
                    });
                    skipped_extensions.insert(extension.clone());
                    secrets.clear();
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        if !skipped_extensions.contains(extension) {
            out.insert(extension.clone(), secrets);
        }
    }
    Ok(ResolvedExtensionSecrets {
        secrets: out,
        skipped_extensions,
        diagnostics,
    })
}

#[cfg(test)]
mod tests;
