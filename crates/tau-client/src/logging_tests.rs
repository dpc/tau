//! Focused environment-filter contract tests for extension stderr logging.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use tracing::dispatcher::Dispatch;

use super::*;

/// Thread-safe in-memory stderr sink for one isolated tracing dispatch.
#[derive(Clone, Default)]
struct CapturedStderr {
    /// Formatted log bytes written by the subscriber.
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl CapturedStderr {
    /// Return the UTF-8 log stream captured from the isolated dispatch.
    fn text(&self) -> String {
        String::from_utf8(self.bytes.lock().expect("captured stderr lock").clone())
            .expect("tracing output is UTF-8")
    }
}

impl Write for CapturedStderr {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes
            .lock()
            .expect("captured stderr lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Run one emission under an isolated stderr subscriber with `filter`.
fn capture(filter: EnvFilter, emit: impl FnOnce()) -> String {
    let stderr = CapturedStderr::default();
    let writer = stderr.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .without_time()
        .finish();
    tracing::dispatcher::with_default(&Dispatch::new(subscriber), emit);
    stderr.text()
}

/// Proves extensions read the documented TAU_LOG variable rather than an
/// extension-specific lookalike and that a valid explicit filter replaces the
/// extension's default.
#[test]
fn explicit_tau_log_filter_replaces_the_extension_default() {
    let filter = filter_from_env("rostra=info,warn", |name| {
        assert_eq!(name, "TAU_LOG");
        Ok("warn".to_owned())
    });
    let output = capture(filter, || {
        tracing::info!(target: "rostra", "hidden Rostra info");
        tracing::warn!(target: "unrelated", "global warning fallback");
    });
    let default_output = capture(EnvFilter::new("rostra=info,warn"), || {
        tracing::info!(target: "rostra", "default Rostra info");
    });

    assert!(!output.contains("hidden Rostra info"));
    assert!(output.contains("global warning fallback"));
    assert!(default_output.contains("default Rostra info"));
}

/// Ensures the published `rostra=debug,warn` directive reaches both the
/// extension target and the upstream Rostra-client target while retaining
/// warnings from every other target.
#[test]
fn rostra_debug_prefix_and_global_warn_fallback_are_effective() {
    let filter = filter_from_env("rostra=info,warn", |_| Ok("rostra=debug,warn".to_owned()));
    let output = capture(filter, || {
        tracing::debug!(target: "rostra::tools::write", "extension debug");
        tracing::debug!(target: "rostra_client::publisher", "upstream debug");
        tracing::debug!(target: "unrelated", "hidden unrelated debug");
        tracing::warn!(target: "unrelated", "global warning fallback");
    });

    assert!(output.contains("extension debug"));
    assert!(output.contains("upstream debug"));
    assert!(!output.contains("hidden unrelated debug"));
    assert!(output.contains("global warning fallback"));
}
