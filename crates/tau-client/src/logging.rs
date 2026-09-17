use std::sync::OnceLock;

use tracing_subscriber::{EnvFilter, fmt as path_tracing_subscriber_fmt};

/// Fallback when the current executable did not supply its own source revision.
const UNKNOWN_BUILD_REVISION: &str = "unknown";

/// Current executable's process-local source revision, when supplied at
/// startup.
static DIAGNOSTIC_BUILD_REVISION: OnceLock<String> = OnceLock::new();

/// Environment variable controlling extension log filtering.
///
/// The value uses the same directive syntax as `RUST_LOG`: per-target levels
/// such as `websearch=debug`, optionally followed by a global fallback level.
pub const ENV_VAR: &str = "TAU_LOG";

/// Default filter used when [`ENV_VAR`] is unset or cannot be parsed.
pub const DEFAULT_FILTER: &str = "info";

/// Initializes the global `tracing` subscriber with a generic default filter.
///
/// Most extensions should prefer [`init_logging_for`], which scopes the default
/// filter to the extension's own log target and keeps third-party dependencies
/// quiet unless the operator explicitly opts into their logs.
pub fn init_logging() {
    install_subscriber(DEFAULT_FILTER);
}

/// Initializes the global `tracing` subscriber for one extension log target.
///
/// The default filter is `<log_target>=info,warn`, keeping the extension's own
/// info logs visible while limiting unrelated dependency logs to warnings. The
/// operator can override this completely with [`ENV_VAR`].
pub fn init_logging_for(log_target: &'static str) {
    install_subscriber(&format!("{log_target}=info,warn"));
}

/// Supplies the current executable's source revision for startup diagnostics.
///
/// This process-local value must come from the executable's own bundled
/// metadata, never inherited environment state. Repeated initialization keeps
/// the first value and cannot fail extension startup.
#[doc(hidden)]
pub fn initialize_diagnostic_build_revision(revision: String) {
    let _ = DIAGNOSTIC_BUILD_REVISION.set(revision);
}

/// Returns the current executable's source revision for startup diagnostics.
///
/// Standalone extension executables that have not supplied their own metadata
/// truthfully report `unknown`.
#[doc(hidden)]
pub fn diagnostic_build_revision() -> &'static str {
    diagnostic_build_revision_from(DIAGNOSTIC_BUILD_REVISION.get().map(String::as_str))
}

/// Resolves optional executable metadata without consulting inherited state.
fn diagnostic_build_revision_from(revision: Option<&str>) -> &str {
    revision.unwrap_or(UNKNOWN_BUILD_REVISION)
}

/// Installs the stderr subscriber used by first-party extension binaries.
fn install_subscriber(default_filter: &str) {
    let filter = filter_from_env(default_filter, std::env::var);

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(true)
        .with_level(true)
        .with_timer(path_tracing_subscriber_fmt::time::SystemTime)
        .finish();

    if let Err(err) = tracing::subscriber::set_global_default(subscriber) {
        eprintln!("tau-client: failed to install tracing subscriber: {err}");
    }
}

/// Select the explicit operator filter or retain the caller's default on error.
fn filter_from_env<F>(default_filter: &str, read_env: F) -> EnvFilter
where
    F: FnOnce(&'static str) -> Result<String, std::env::VarError>,
{
    read_env(ENV_VAR)
        .ok()
        .and_then(|filter| EnvFilter::try_new(filter).ok())
        .unwrap_or_else(|| EnvFilter::new(default_filter))
}

#[cfg(test)]
#[path = "logging_tests.rs"]
mod logging_tests;
