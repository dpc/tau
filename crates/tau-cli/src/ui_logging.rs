use std::fs::File;
use std::io::{self, LineWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::{self as path_tracing_subscriber_fmt, MakeWriter};

use crate::mint_short_id;

const UI_LOG_ENV: &str = "TAU_LOG";
const DEFAULT_FILTER: &str = "tau_cli=info,warn";

/// One serialized line writer shared by tracing and mandatory UI diagnostics.
#[derive(Clone)]
struct SharedUiLogWriter {
    /// Single line-buffered descriptor for this UI log.
    inner: Arc<Mutex<LineWriter<File>>>,
}

impl SharedUiLogWriter {
    /// Creates a shared writer around the UI log descriptor.
    fn new(file: File) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LineWriter::new(file))),
        }
    }

    /// Locks the writer, retaining logging after a formatter panic.
    fn lock(&self) -> MutexGuard<'_, LineWriter<File>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Locked shared UI writer returned to one tracing formatter invocation.
struct SharedUiLogGuard<'a> {
    /// Serialized access to the one line-buffered UI log descriptor.
    inner: MutexGuard<'a, LineWriter<File>>,
}

impl Write for SharedUiLogGuard<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.inner.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<'a> MakeWriter<'a> for SharedUiLogWriter {
    type Writer = SharedUiLogGuard<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        SharedUiLogGuard { inner: self.lock() }
    }
}

/// Initialize stderr tracing for component subcommands that do not
/// have their own logging setup. Uses `TAU_LOG` so startup can be
/// traced across the parent CLI and harness child with one knob.
pub fn init_stderr_from_env(default_filter: &str) {
    let filter = EnvFilter::try_from_env(UI_LOG_ENV)
        .or_else(|_| EnvFilter::try_new(default_filter))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(true)
        .with_level(true)
        .with_timer(path_tracing_subscriber_fmt::time::SystemTime)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
}

/// Metadata for the current terminal UI instance log.
pub struct UiLogging {
    /// Stable process-local UI identifier.
    ui_id: String,
    /// Private directory containing this UI's diagnostics.
    dir: PathBuf,
    /// Private UI log path.
    log_path: PathBuf,
    /// Filter-independent writer for safety-critical bounded diagnostics.
    diagnostic_writer: Option<SharedUiLogWriter>,
}

impl UiLogging {
    #[must_use]
    pub fn ui_id(&self) -> &str {
        &self.ui_id
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    #[must_use]
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// Writes one bounded foreground-restoration failure before UI teardown.
    pub fn write_foreground_restoration_failure(
        &self,
        diagnostic: tau_cli_term::ForegroundRestorationDiagnostic,
    ) {
        self.write_foreground_restoration_fields(diagnostic.class(), diagnostic.errno());
    }

    fn write_foreground_restoration_fields(&self, class: &str, errno: Option<i32>) {
        let Some(writer) = &self.diagnostic_writer else {
            return;
        };
        let mut writer = writer.lock();
        let errno = errno.map_or_else(|| "none".to_owned(), |errno| errno.to_string());
        let _ = writeln!(
            writer,
            "terminal_foreground_restoration_failure restoration_class={} restoration_errno={}",
            class, errno
        );
    }
}

/// Initialize tracing for this CLI terminal UI instance.
///
/// Logs go to `$XDG_STATE_HOME/tau/uis/<ui-id>/ui.log` (normally
/// `~/.local/state/tau/uis/<ui-id>/ui.log`). The filter comes from
/// `TAU_LOG`, defaulting to first-party `tau_cli` info and global warnings.
pub fn init(state_dir: &Path) -> io::Result<UiLogging> {
    let ui_id = mint_ui_id();
    let dir = state_dir.join("uis").join(&ui_id);
    std::fs::create_dir_all(&dir)?;

    let log_path = dir.join("ui.log");
    let mut file = File::create(&log_path)?;
    writeln!(file, "# tau ui log")?;
    writeln!(file, "ui_id={ui_id}")?;
    writeln!(file, "pid={}", std::process::id())?;
    if let Ok(cwd) = std::env::current_dir() {
        writeln!(file, "cwd={}", cwd.display())?;
    }
    writeln!(file)?;
    drop(file);
    let log_file = File::options().append(true).open(&log_path)?;
    let log_writer = SharedUiLogWriter::new(log_file);

    let filter = EnvFilter::try_from_env(UI_LOG_ENV)
        .or_else(|_| EnvFilter::try_new(DEFAULT_FILTER))
        .map_err(io::Error::other)?;
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        // Keep one descriptor and coalesce formatter fragments through each
        // newline. This is best-effort OS-cache I/O: no path flushes or syncs.
        .with_writer(log_writer.clone())
        .with_ansi(false)
        .with_timer(path_tracing_subscriber_fmt::time::SystemTime)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    Ok(UiLogging {
        ui_id,
        dir,
        log_path,
        diagnostic_writer: Some(log_writer),
    })
}

/// Initialize tracing for an ephemeral terminal UI without creating a UI log.
///
/// This preserves normal in-process tracing setup while directing records to an
/// in-memory sink so `tau --ephemeral` does not leave UI log artifacts on disk.
/// The returned paths are display-only sentinels.
pub fn init_ephemeral() -> UiLogging {
    let filter = EnvFilter::try_from_env(UI_LOG_ENV)
        .or_else(|_| EnvFilter::try_new(DEFAULT_FILTER))
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::sink)
        .with_ansi(false)
        .with_timer(path_tracing_subscriber_fmt::time::SystemTime)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    UiLogging {
        ui_id: "ephemeral".to_owned(),
        dir: PathBuf::from("<ephemeral>"),
        log_path: PathBuf::from("<ephemeral>"),
        diagnostic_writer: None,
    }
}

fn mint_ui_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    mint_short_id(&format!("ui-{millis}"))
}

#[cfg(test)]
mod tests;
