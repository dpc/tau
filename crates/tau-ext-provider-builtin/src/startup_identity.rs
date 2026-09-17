/// Logs the built-in provider extension's diagnostic startup identity.
pub(super) fn log() {
    tracing::info!(
        target: super::LOG_TARGET,
        package = env!("CARGO_PKG_NAME"),
        version = env!("CARGO_PKG_VERSION"),
        revision = tau_client::diagnostic_build_revision(),
        "extension startup identity"
    );
}
