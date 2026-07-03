//! Optional building blocks for tau extension processes.
//!
//! Three independent utilities, each opt-in:
//!
//! - [`Handshake`] writes the standard Hello/Subscribe/Intercept/startup-event/
//!   Ready prelude every extension opens its session with.
//! - [`init_logging_for`] (or [`init_logging`] when there is no single target
//!   to scope to) installs a stderr `tracing_subscriber` filtered by the
//!   `TAU_LOG` env var.
//! - [`parse_config`] decodes the harness-supplied `configure.config` value
//!   into a typed struct, flattening `ciborium::value::Error`'s debug shape
//!   into a one-line message.
//!
//! Nothing here is required — an extension may use one helper, all
//! three, or none. The crate exists only to hold pieces that more
//! than one extension would otherwise copy-paste.
//!
//! `Handshake` is only the peer-to-harness startup prelude. Extensions that
//! receive `HarnessOutputMessage::Configure` must still parse and apply
//! `Configure.config` themselves and send `HarnessInputMessage::ConfigError` if
//! parsing or applying configuration fails.
//!
//! ## Per-extension log targets
//!
//! Each extension is expected to declare a short target string and
//! pass it as `target:` to the `tracing` macros. The convention is:
//!
//! ```ignore
//! pub const LOG_TARGET: &str = "std-notifications";
//!
//! tracing::info!(target: LOG_TARGET, "idle deadline armed");
//! ```
//!
//! Then `TAU_LOG=std-notifications=trace,info` filters that
//! extension at trace level while leaving everything else at info.
//! Targets are arbitrary `&'static str` — any name an extension
//! likes — so use one short, distinctive identifier per extension
//! and document it next to the const.
//!
//! ## Why a separate env var
//!
//! `RUST_LOG` is reserved for the wider host environment (the
//! harness, embedded crates, third-party libraries). Extensions
//! deserve their own knob so users can crank one extension to trace
//! without flooding stderr with everything else.
//!
//! ## Recommended shape for [`parse_config`] config structs
//!
//! Declare the extension's config struct with
//! `#[serde(default, deny_unknown_fields)]` so missing fields fall
//! back to `Default` while unknown fields surface as actionable
//! errors instead of being silently dropped:
//!
//! ```ignore
//! #[derive(serde::Deserialize, Default)]
//! #[serde(default, deny_unknown_fields)]
//! struct MyConfig { /* fields */ }
//! ```

mod handshake;

pub use handshake::{Handshake, HandshakeError};
use tracing_subscriber::EnvFilter;

/// Environment variable controlling extension log filtering. Same
/// syntax as `RUST_LOG` (per-target levels, with a default level).
pub const ENV_VAR: &str = "TAU_LOG";

/// Default filter applied when `TAU_LOG` is unset or fails to
/// parse: every target at `info` and above.
pub const DEFAULT_FILTER: &str = "info";

/// Initialize the global `tracing` subscriber for this extension
/// process with a generic default filter (every target at `info`).
///
/// Most extensions should prefer [`init_logging_for`], which scopes
/// the default to one named target — that makes
/// [`crate-doc`](self#per-extension-log-targets)'s `LOG_TARGET`
/// convention explicit at the call site and stops noisy third-party
/// crates from spamming stderr by default. Use this entry point only
/// when there is no single target to scope to.
pub fn init_logging() {
    install_subscriber(DEFAULT_FILTER);
}

/// Initialize the global `tracing` subscriber for this extension,
/// defaulting to `<log_target>=info,warn` when `TAU_LOG` is unset.
///
/// `log_target` should be the same `&'static str` the extension
/// passes as `target:` to `tracing::*!` macros (see crate-level doc).
/// Passing it here makes the convention legible at the call site:
///
/// ```ignore
/// pub const LOG_TARGET: &str = "std-notifications";
/// tau_extension::init_logging_for(LOG_TARGET);
/// ```
///
/// The default filter keeps the extension's own info logs visible
/// while pinning everything else (transitive deps, reqwest, hyper,
/// etc.) to `warn`. Users can still override with
/// `TAU_LOG=std-notifications=trace,debug` etc.
pub fn init_logging_for(log_target: &'static str) {
    // `EnvFilter` directive syntax: `<target>=<level>,<level>` —
    // first directive sets the named target, trailing bare level is
    // the global fallback. See `tracing_subscriber::EnvFilter`.
    install_subscriber(&format!("{log_target}=info,warn"));
}

fn install_subscriber(default_filter: &str) {
    let filter =
        EnvFilter::try_from_env(ENV_VAR).unwrap_or_else(|_| EnvFilter::new(default_filter));

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(true)
        .with_level(true)
        // Wall-clock timestamps (not monotonic) — they're chosen for
        // human readability when correlating an extension log against
        // harness logs, user actions, or external services. NTP
        // step-backs are a known caveat; in-process ordering can fall
        // back to log file line order.
        .with_timer(tracing_subscriber::fmt::time::SystemTime)
        .finish();

    if let Err(err) = tracing::subscriber::set_global_default(subscriber) {
        // Stderr is the right channel here — by definition the global
        // tracing subscriber may not be ready to capture our own
        // logs. Visible iff the existing subscriber is silent on
        // stderr, which is fine: a duplicate-init in tests is benign,
        // a duplicate-init in production is rare and worth surfacing.
        eprintln!("tau-extension: failed to install tracing subscriber: {err}");
    }
}

/// Decode the harness-provided `tau_proto::Configure::config` value into
/// the extension's typed configuration struct.
///
/// `Err` carries a human-readable message suitable for sending as
/// `HarnessInputMessage::ConfigError`; the harness surfaces it verbatim to the
/// user. `ciborium::value::Error::Custom`'s
/// `Display` is `{:?}` (so it would render as `Custom("...")`); we
/// unwrap it to just the inner serde message.
pub fn parse_config<C: serde::de::DeserializeOwned>(
    value: &tau_proto::CborValue,
) -> Result<C, String> {
    value.deserialized().map_err(|e| match e {
        ciborium::value::Error::Custom(msg) => msg,
    })
}

#[cfg(test)]
mod tests;
