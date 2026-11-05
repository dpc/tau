use super::built_info;

/// Logs the built-in provider extension's diagnostic startup identity.
pub(super) fn log() {
    tracing::info!(
        target: super::LOG_TARGET,
        package = env!("CARGO_PKG_NAME"),
        version = env!("CARGO_PKG_VERSION"),
        revision = tau_client::package_build_identity!(built_info).revision.as_deref().unwrap_or("unknown"),
        "extension startup identity"
    );
}
