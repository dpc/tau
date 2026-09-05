//! Harness-owned secret loading and per-extension resolution.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use tau_config::secret_sources::{
    EnvironmentDisposition, load_secret_sources as load_config_secret_sources,
    resolve_declared_secret,
};
pub use tau_config::secret_sources::{SecretSourceError as SecretsError, SecretSources};
use tau_proto::SecretValue;

use crate::settings::{Config, ExtensionStartupDiagnostic, ExtensionStartupDiagnosticKind};

/// Collect all `TAU_SECRET_*` variables and remove them from this process.
#[allow(unsafe_code)]
pub fn load_secret_sources() -> Result<SecretSources, SecretsError> {
    load_config_secret_sources(EnvironmentDisposition::RemoveAfterSnapshot)
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
#[cfg(test)]
pub fn resolve_extension_secrets(
    config: &Config,
    state_dir: &Path,
    sources: &SecretSources,
) -> Result<ResolvedExtensionSecrets, SecretsError> {
    resolve_extension_secrets_excluding(config, state_dir, sources, &BTreeMap::new())
}

/// Resolve configured extension secrets while keeping provider-bound
/// declarations out of Configure.
pub fn resolve_extension_secrets_excluding(
    config: &Config,
    state_dir: &Path,
    sources: &SecretSources,
    provider_bound_names: &BTreeMap<String, BTreeSet<String>>,
) -> Result<ResolvedExtensionSecrets, SecretsError> {
    let mut out = BTreeMap::new();
    let mut skipped_extensions = BTreeSet::new();
    let mut diagnostics = Vec::new();
    for (extension, extension_config) in &config.extensions {
        let mut secrets = BTreeMap::new();
        for (name, declaration) in &extension_config.secrets {
            if provider_bound_names
                .get(extension)
                .is_some_and(|names| names.contains(name))
            {
                continue;
            }
            match resolve_declared_secret(state_dir, sources, extension, name, declaration) {
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
                        message: format!(
                            "optional extension `{extension}` was skipped: its configured secrets \
                             could not be resolved. Check `extensions.{extension}.secrets`"
                        ),
                        kind: ExtensionStartupDiagnosticKind::OptionalSkip,
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
