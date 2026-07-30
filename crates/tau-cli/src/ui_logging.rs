use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::{EnvFilter, fmt as path_tracing_subscriber_fmt};

use crate::mint_short_id;

const UI_LOG_ENV: &str = "TAU_LOG";
const DEFAULT_FILTER: &str = "info";

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

/// File-backed tracing writer for one terminal UI instance.
#[derive(Clone)]
struct UiLogWriter {
    path: PathBuf,
}

impl<'a> MakeWriter<'a> for UiLogWriter {
    type Writer = Box<dyn Write + Send + 'a>;

    fn make_writer(&'a self) -> Self::Writer {
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(file) => Box::new(file),
            Err(_) => Box::new(io::sink()),
        }
    }
}

/// Metadata for the current terminal UI instance log.
pub struct UiLogging {
    ui_id: String,
    dir: PathBuf,
    log_path: PathBuf,
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
}

/// Initialize tracing for this CLI terminal UI instance.
///
/// Logs go to `$XDG_STATE_HOME/tau/uis/<ui-id>/ui.log` (normally
/// `~/.local/state/tau/uis/<ui-id>/ui.log`). The filter comes from
/// `TAU_LOG`, defaulting to `info`.
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

    let filter = EnvFilter::try_from_env(UI_LOG_ENV)
        .or_else(|_| EnvFilter::try_new(DEFAULT_FILTER))
        .map_err(io::Error::other)?;
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(UiLogWriter {
            path: log_path.clone(),
        })
        .with_ansi(false)
        .with_timer(path_tracing_subscriber_fmt::time::SystemTime)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    Ok(UiLogging {
        ui_id,
        dir,
        log_path,
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
    }
}

fn mint_ui_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    mint_short_id(&format!("ui-{millis}"))
}
