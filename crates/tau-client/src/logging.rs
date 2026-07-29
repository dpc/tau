use tracing_subscriber::EnvFilter;

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

/// Installs the stderr subscriber used by first-party extension binaries.
fn install_subscriber(default_filter: &str) {
    let filter =
        EnvFilter::try_from_env(ENV_VAR).unwrap_or_else(|_| EnvFilter::new(default_filter));

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(true)
        .with_level(true)
        .with_timer(tracing_subscriber::fmt::time::SystemTime)
        .finish();

    if let Err(err) = tracing::subscriber::set_global_default(subscriber) {
        eprintln!("tau-client: failed to install tracing subscriber: {err}");
    }
}
