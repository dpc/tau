//! Local inspection and clearing for the standard papercut reporter.
//!
//! The reporter owns the JSONL records in its User-scope extension directory.
//! This module reads that canonical file directly and takes the same advisory
//! directory lock as the harness before clearing it.

use std::fs as path_std_fs;
use std::io::{ErrorKind, Read as _};
use std::path::{Path, PathBuf};

use fs2::FileExt as _;
use tau_ext_utils::{PAPERCUT_FILE_NAME, PapercutRecord, PapercutRecordParseError};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::cli::PapercutCommand;
use crate::{CliError, line_output};

/// Configured instance name for Tau's standard utility extension.
const STD_UTILS_INSTANCE: &str = "std-utils";

/// Runs one `tau dev papercut` command.
pub(crate) fn run(command: PapercutCommand) -> Result<(), CliError> {
    match command {
        PapercutCommand::List {
            markdown,
            state_dir,
        } => {
            let records = PapercutStore::new(&state_dir).list()?;
            let output = if markdown {
                format_markdown(&records)?
            } else {
                format_plain(&records)?
            };
            line_output::write_stdout(&output)
        }
        PapercutCommand::Clear { state_dir } => {
            let count = PapercutStore::new(&state_dir).clear()?;
            line_output::write_stdout(&format!("cleared {count} papercut report(s)\n"))
        }
    }
}

/// Canonical User-scope storage owned by one standard papercut reporter
/// instance.
#[derive(Clone)]
struct PapercutStore {
    /// Existing Tau state root selected by the caller.
    root: PathBuf,
    /// Deterministic test-only boundary after removal and before lock release.
    #[cfg(test)]
    clear_midpoint: Option<std::sync::Arc<std::sync::Barrier>>,
}

impl PapercutStore {
    /// Constructs the standard reporter's canonical User-scope storage paths.
    fn new(state_dir: &Path) -> Self {
        let root = tau_config::settings::extension_state_dir_of(state_dir, STD_UTILS_INSTANCE)
            .expect("built-in extension instance name must be valid");
        Self {
            root,
            #[cfg(test)]
            clear_midpoint: None,
        }
    }

    /// Returns the reporter-owned JSONL path below this store's locked root.
    fn file(&self) -> PathBuf {
        self.root.join(PAPERCUT_FILE_NAME)
    }

    /// Lists records from one lock-consistent snapshot, oldest timestamp first.
    fn list(&self) -> Result<Vec<PapercutRecord>, CliError> {
        self.with_existing_lock(|| self.read_records())
            .map(|records| records.unwrap_or_default())
    }

    /// Clears exactly the records present while holding the reporter's append
    /// lock.
    fn clear(&self) -> Result<usize, CliError> {
        self.with_existing_lock(|| {
            let records = self.read_records()?;
            let file = self.file();
            if file.exists() {
                path_std_fs::remove_file(&file).map_err(|error| {
                    storage_error("failed to clear the papercut records", error)
                })?;
                sync_parent_dir(&file)?;
            }
            #[cfg(test)]
            self.wait_at_clear_midpoint();
            Ok(records.len())
        })
        .map(|count| count.unwrap_or(0))
    }

    /// Runs one read or mutation while sharing the reporter's exclusive
    /// directory lock.
    ///
    /// An absent extension directory means no reporter has created storage yet.
    fn with_existing_lock<T>(
        &self,
        operation: impl FnOnce() -> Result<T, CliError>,
    ) -> Result<Option<T>, CliError> {
        let root = match open_existing_directory_no_follow(&self.root) {
            Ok(root) => root,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(storage_error("failed to open papercut storage", error)),
        };
        root.lock_exclusive()
            .map_err(|error| storage_error("failed to lock papercut storage", error))?;
        let result = operation();
        let _ = root.unlock();
        result.map(Some)
    }

    /// Parses and stably orders every supported record in the canonical file.
    fn read_records(&self) -> Result<Vec<PapercutRecord>, CliError> {
        let file_path = self.file();
        let mut file = match open_existing_file_no_follow(&file_path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(storage_error("failed to open papercut records", error)),
        };
        let metadata = file
            .metadata()
            .map_err(|error| storage_error("failed to inspect papercut records", error))?;
        if !metadata.is_file() {
            return Err(CliError::Participant(
                "papercut records are not a regular file".to_owned(),
            ));
        }
        let mut contents = Vec::new();
        file.by_ref()
            .take(tau_harness::EXTENSION_DATA_MAX_FILE_BYTES + 1)
            .read_to_end(&mut contents)
            .map_err(|error| storage_error("failed to read papercut records", error))?;
        if tau_harness::EXTENSION_DATA_MAX_FILE_BYTES < contents.len() as u64 {
            return Err(CliError::Participant(
                "papercut records exceed the extension data file limit".to_owned(),
            ));
        }
        let contents = String::from_utf8(contents).map_err(|_| {
            CliError::Participant("papercut records are not valid UTF-8".to_owned())
        })?;
        let mut records = Vec::new();
        for (line_number, line) in contents.lines().enumerate() {
            let record = match PapercutRecord::parse_json_line(line) {
                Ok(record) => record,
                Err(PapercutRecordParseError::Invalid) => {
                    return Err(CliError::Participant(format!(
                        "invalid papercut record at line {}",
                        line_number + 1
                    )));
                }
                Err(PapercutRecordParseError::UnsupportedSchema) => {
                    return Err(CliError::Participant(format!(
                        "unsupported papercut record schema at line {}",
                        line_number + 1
                    )));
                }
            };
            let _ = format_timestamp(record.timestamp_us())?;
            records.push(record);
        }
        records.sort_unstable_by(|left, right| {
            left.timestamp_us()
                .cmp(&right.timestamp_us())
                .then_with(|| left.agent_id().cmp(right.agent_id()))
                .then_with(|| left.session_id().cmp(right.session_id()))
                .then_with(|| left.report().cmp(right.report()))
        });
        Ok(records)
    }

    /// Synchronizes a deterministic test at the post-removal clear boundary.
    #[cfg(test)]
    fn wait_at_clear_midpoint(&self) {
        if let Some(midpoint) = &self.clear_midpoint {
            midpoint.wait();
            midpoint.wait();
        }
    }

    /// Configures a test-only pause after deletion while clear still owns the
    /// lock.
    #[cfg(test)]
    fn with_clear_midpoint(mut self, midpoint: std::sync::Arc<std::sync::Barrier>) -> Self {
        self.clear_midpoint = Some(midpoint);
        self
    }
}

/// Formats the concise, line-oriented representation used by default.
fn format_plain(records: &[PapercutRecord]) -> Result<String, CliError> {
    if records.is_empty() {
        return Ok("no papercut reports\n".to_owned());
    }
    let mut output = String::new();
    for record in records {
        output.push_str(&format_timestamp(record.timestamp_us())?);
        output.push(' ');
        output.push_str(record.agent_id().as_str());
        output.push_str(" [");
        output.push_str(record.session_id().as_str());
        output.push_str("] ");
        output.push_str(&line_output::escape_field(record.report()));
        output.push('\n');
    }
    Ok(output)
}

/// Formats records as copyable Markdown without interpreting report text as
/// Markdown.
fn format_markdown(records: &[PapercutRecord]) -> Result<String, CliError> {
    let mut output = String::from("# Papercuts\n\n");
    if records.is_empty() {
        output.push_str("No papercut reports.\n");
        return Ok(output);
    }
    for record in records {
        output.push_str("## ");
        output.push_str(&format_timestamp(record.timestamp_us())?);
        output.push_str("\n\n- Agent: `");
        output.push_str(record.agent_id().as_str());
        output.push_str("`\n- Session: `");
        output.push_str(record.session_id().as_str());
        output.push_str("`\n\n");
        let fence = markdown_fence(record.report());
        output.push_str(&fence);
        output.push_str("text\n");
        output.push_str(&escape_markdown_code(record.report()));
        if !record.report().ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&fence);
        output.push_str("\n\n");
    }
    Ok(output)
}

/// Formats a stored operation timestamp as an RFC 3339 UTC value.
fn format_timestamp(timestamp_us: tau_proto::UnixMicros) -> Result<String, CliError> {
    let timestamp = OffsetDateTime::from_unix_timestamp_nanos(
        i128::from(timestamp_us.get()) * 1_000,
    )
    .map_err(|_| CliError::Participant("papercut record has an invalid timestamp".to_owned()))?;
    timestamp
        .format(&Rfc3339)
        .map_err(|_| CliError::Participant("could not format papercut timestamp".to_owned()))
}

/// Escapes terminal controls while retaining report line boundaries in a code
/// block.
fn escape_markdown_code(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        match character {
            '\n' => escaped.push('\n'),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(escaped, "\\u{{{:x}}}", character as u32);
            }
            character => escaped.push(character),
        }
    }
    escaped
}

/// Selects a fence longer than any backtick run in one report.
fn markdown_fence(report: &str) -> String {
    let longest = report
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    "`".repeat(3.max(longest + 1))
}

/// Opens an existing extension data root without following a final symlink.
#[cfg(unix)]
fn open_existing_directory_no_follow(path: &Path) -> Result<std::fs::File, std::io::Error> {
    use std::os::unix::fs::OpenOptionsExt as _;

    path_std_fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

/// Opens an existing records file without following a final symlink.
#[cfg(unix)]
fn open_existing_file_no_follow(path: &Path) -> Result<std::fs::File, std::io::Error> {
    use std::os::unix::fs::OpenOptionsExt as _;

    path_std_fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Opens an existing records file on platforms without no-follow flags.
#[cfg(not(unix))]
fn open_existing_file_no_follow(path: &Path) -> Result<std::fs::File, std::io::Error> {
    path_std_fs::File::open(path)
}

/// Opens an existing extension data root on platforms without no-follow flags.
#[cfg(not(unix))]
fn open_existing_directory_no_follow(path: &Path) -> Result<std::fs::File, std::io::Error> {
    path_std_fs::File::open(path)
}

/// Flushes the parent directory after removing the canonical records file.
fn sync_parent_dir(path: &Path) -> Result<(), CliError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    path_std_fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| storage_error("failed to persist clearing papercut records", error))
}

/// Converts local canonical-storage I/O failures into one CLI diagnostic.
fn storage_error(context: &str, error: std::io::Error) -> CliError {
    CliError::Participant(format!("{context}: {error}"))
}

#[cfg(test)]
mod tests;
