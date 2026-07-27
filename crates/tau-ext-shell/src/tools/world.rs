//! Low-level filesystem/process boundary for ext-shell tools.
//!
//! VCR replay substitutes primitive outside-world operations, not final tool
//! results, so tool parsing, formatting, truncation, and validation still run
//! during replay.

use std::io::{self, Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use tau_proto::{CborValue, ToolUseState};

use crate::display::ToolFailure;

pub(crate) const MAX_SAFE_FILE_READ_BYTES: usize = 10 * 1024 * 1024;

// Keep at zero per `GATE-no-backward-compatibility`.
const CASSETTE_VERSION: u32 = 0;

pub(crate) struct ShellWorld {
    mode: WorldMode,
    cwd: std::path::PathBuf,
}

enum WorldMode {
    Real,
    Recording {
        store: tau_vcr::VcrStore,
        key: String,
        cassette: WorldCassette,
        /// Saved rendering held until paired cassette/side publication.
        side_output: Option<Vec<u8>>,
    },
    Replay {
        /// Store owning the cassette and its bounded side artifact.
        store: tau_vcr::VcrStore,
        key: String,
        cassette: WorldCassette,
        next_op: usize,
    },
}

impl ShellWorld {
    #[cfg(test)]
    pub(crate) fn real() -> Self {
        Self {
            mode: WorldMode::Real,
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_tool(
        tool_name: &str,
        call_id: &str,
        arguments: &CborValue,
        config: Option<tau_vcr::VcrConfig>,
    ) -> Result<Self, ToolFailure> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        Self::for_tool_in_dir(tool_name, call_id, arguments, config, cwd)
    }

    pub(crate) fn for_tool_in_dir(
        tool_name: &str,
        call_id: &str,
        arguments: &CborValue,
        config: Option<tau_vcr::VcrConfig>,
        cwd: std::path::PathBuf,
    ) -> Result<Self, ToolFailure> {
        let Some(config) = config else {
            return Ok(Self {
                mode: WorldMode::Real,
                cwd,
            });
        };
        let key = call_id.to_owned();
        let store = config.store();
        let request = world_request(tool_name, arguments)?;
        if let Some(cassette) = store.get::<WorldCassette>(&key).map_err(vcr_failure)? {
            validate_cassette(&key, &cassette, &request)?;
            return Ok(Self {
                mode: WorldMode::Replay {
                    store,
                    key,
                    cassette,
                    next_op: 0,
                },
                cwd,
            });
        }
        if config.mode == tau_vcr::VcrMode::ReplayOnly {
            return Err(vcr_failure(tau_vcr::VcrError::Missing { key }));
        }
        Ok(Self {
            mode: WorldMode::Recording {
                store,
                key,
                cassette: WorldCassette {
                    version: CASSETTE_VERSION,
                    request,
                    ops: Vec::new(),
                },
                side_output: None,
            },
            cwd,
        })
    }

    pub(crate) fn current_dir(&self) -> &Path {
        &self.cwd
    }

    pub(crate) fn finish(self) -> Result<(), ToolFailure> {
        match self.mode {
            WorldMode::Real => Ok(()),
            WorldMode::Recording {
                store,
                key,
                cassette,
                side_output,
            } => match side_output {
                Some(side) => store
                    .put_with_side(
                        &key,
                        &tau_vcr::ArtifactKind::new("shell-output")
                            .expect("static artifact kind is valid"),
                        &cassette,
                        &side,
                        crate::shell_output_spool::MAX_SAVED_OUTPUT_BYTES as u64,
                    )
                    .map_err(vcr_failure),
                None => store.put(&key, &cassette).map_err(vcr_failure),
            },
            WorldMode::Replay {
                store: _,
                key,
                cassette,
                next_op,
                ..
            } => {
                if next_op == cassette.ops.len() {
                    Ok(())
                } else {
                    Err(ToolFailure::new(format!(
                        "vcr replay for {key} left {} unconsumed world op(s)",
                        cassette.ops.len() - next_op
                    )))
                }
            }
        }
    }

    pub(crate) fn is_dir(&mut self, path: &Path) -> io::Result<bool> {
        match &mut self.mode {
            WorldMode::Real => Ok(std::fs::metadata(path)?.is_dir()),
            WorldMode::Recording { cassette, .. } => {
                let result = std::fs::metadata(path).map(|metadata| metadata.is_dir());
                cassette.ops.push(WorldOp::IsDir {
                    path: cassette_path(path),
                    result: OpResult::from_io_result_ref(&result),
                });
                result
            }
            WorldMode::Replay {
                key,
                cassette,
                next_op,
                ..
            } => {
                let op = next_replay_op(key, cassette, next_op, "is_dir", path)?;
                let WorldOp::IsDir {
                    path: expected_path,
                    result,
                } = op
                else {
                    return Err(unexpected_replay_op(key, "is_dir", path));
                };
                check_replay_path(key, "is_dir", expected_path, path)?;
                result.clone().into_io_result()
            }
        }
    }

    pub(crate) fn read_dir_limited(
        &mut self,
        path: &Path,
        max_entries: usize,
    ) -> io::Result<Vec<WorldDirEntry>> {
        match &mut self.mode {
            WorldMode::Real => read_dir_entries(path, max_entries),
            WorldMode::Recording { cassette, .. } => {
                let result = read_dir_entries(path, max_entries);
                cassette.ops.push(WorldOp::ReadDir {
                    path: cassette_path(path),
                    result: OpResult::from_io_result_ref(&result).map_ok(|entries| {
                        entries.iter().map(RecordedDirEntry::from_world).collect()
                    }),
                });
                result
            }
            WorldMode::Replay {
                key,
                cassette,
                next_op,
                ..
            } => {
                let op = next_replay_op(key, cassette, next_op, "read_dir", path)?;
                let WorldOp::ReadDir {
                    path: expected_path,
                    result,
                } = op
                else {
                    return Err(unexpected_replay_op(key, "read_dir", path));
                };
                check_replay_path(key, "read_dir", expected_path, path)?;
                let mut entries = result.clone().into_io_result().map(|entries| {
                    entries
                        .into_iter()
                        .map(RecordedDirEntry::into_world)
                        .collect::<Vec<_>>()
                })?;
                entries.truncate(max_entries);
                Ok(entries)
            }
        }
    }

    pub(crate) fn read_file_limited(
        &mut self,
        path: &Path,
        max_bytes: usize,
    ) -> io::Result<Vec<u8>> {
        match &mut self.mode {
            WorldMode::Real => read_file_limited_real(path, max_bytes),
            WorldMode::Recording { cassette, .. } => {
                let result = read_file_limited_real(path, max_bytes);
                cassette.ops.push(WorldOp::ReadFile {
                    path: cassette_path(path),
                    result: OpResult::from_io_result_ref(&result)
                        .map_ok(tau_vcr::EscapedBytes::new),
                });
                result
            }
            WorldMode::Replay {
                key,
                cassette,
                next_op,
                ..
            } => {
                let op = next_replay_op(key, cassette, next_op, "read_file", path)?;
                let WorldOp::ReadFile {
                    path: expected_path,
                    result,
                } = op
                else {
                    return Err(unexpected_replay_op(key, "read_file", path));
                };
                check_replay_path(key, "read_file", expected_path, path)?;
                let bytes = result
                    .clone()
                    .map_ok(|bytes| bytes.into_vec())
                    .into_io_result()?;
                if max_bytes < bytes.len() {
                    return Err(file_too_large_error(max_bytes));
                }
                Ok(bytes)
            }
        }
    }

    pub(crate) fn write_file(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        match &mut self.mode {
            WorldMode::Real => atomic_write_file(path, bytes),
            WorldMode::Recording { cassette, .. } => {
                let result = atomic_write_file(path, bytes);
                cassette.ops.push(WorldOp::WriteFile {
                    path: cassette_path(path),
                    bytes: tau_vcr::EscapedBytes::new(bytes),
                    result: OpResult::from_io_result_ref(&result),
                });
                result
            }
            WorldMode::Replay {
                key,
                cassette,
                next_op,
                ..
            } => {
                let op = next_replay_op(key, cassette, next_op, "write_file", path)?;
                let WorldOp::WriteFile {
                    path: expected_path,
                    bytes: expected_bytes,
                    result,
                } = op
                else {
                    return Err(unexpected_replay_op(key, "write_file", path));
                };
                check_replay_path(key, "write_file", expected_path, path)?;
                if expected_bytes.as_slice() != bytes {
                    return Err(replay_io_error(format!(
                        "vcr replay for {key} expected write_file({}) with {} byte(s) but got {} byte(s)",
                        path.display(),
                        expected_bytes.as_slice().len(),
                        bytes.len()
                    )));
                }
                result.clone().into_io_result()
            }
        }
    }

    pub(crate) fn path_exists(&mut self, path: &Path) -> io::Result<bool> {
        match &mut self.mode {
            WorldMode::Real => match std::fs::metadata(path) {
                Ok(_) => Ok(true),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error),
            },
            WorldMode::Recording { cassette, .. } => {
                let result = match std::fs::metadata(path) {
                    Ok(_) => Ok(true),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
                    Err(error) => Err(error),
                };
                cassette.ops.push(WorldOp::PathExists {
                    path: cassette_path(path),
                    result: OpResult::from_io_result_ref(&result),
                });
                result
            }
            WorldMode::Replay {
                key,
                cassette,
                next_op,
                ..
            } => {
                let op = next_replay_op(key, cassette, next_op, "path_exists", path)?;
                let WorldOp::PathExists {
                    path: expected_path,
                    result,
                } = op
                else {
                    return Err(unexpected_replay_op(key, "path_exists", path));
                };
                check_replay_path(key, "path_exists", expected_path, path)?;
                result.clone().into_io_result()
            }
        }
    }

    pub(crate) fn create_dir_all(&mut self, path: &Path) -> io::Result<()> {
        match &mut self.mode {
            WorldMode::Real => std::fs::create_dir_all(path),
            WorldMode::Recording { cassette, .. } => {
                let result = std::fs::create_dir_all(path);
                cassette.ops.push(WorldOp::CreateDirAll {
                    path: cassette_path(path),
                    result: OpResult::from_io_result_ref(&result),
                });
                result
            }
            WorldMode::Replay {
                key,
                cassette,
                next_op,
                ..
            } => {
                let op = next_replay_op(key, cassette, next_op, "create_dir_all", path)?;
                let WorldOp::CreateDirAll {
                    path: expected_path,
                    result,
                } = op
                else {
                    return Err(unexpected_replay_op(key, "create_dir_all", path));
                };
                check_replay_path(key, "create_dir_all", expected_path, path)?;
                result.clone().into_io_result()
            }
        }
    }

    pub(crate) fn read_to_string_limited(
        &mut self,
        path: &Path,
        max_bytes: usize,
    ) -> io::Result<String> {
        String::from_utf8(self.read_file_limited(path, max_bytes)?)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    pub(crate) fn remove_file(&mut self, path: &Path) -> io::Result<()> {
        match &mut self.mode {
            WorldMode::Real => std::fs::remove_file(path),
            WorldMode::Recording { cassette, .. } => {
                let result = std::fs::remove_file(path);
                cassette.ops.push(WorldOp::RemoveFile {
                    path: cassette_path(path),
                    result: OpResult::from_io_result_ref(&result),
                });
                result
            }
            WorldMode::Replay {
                key,
                cassette,
                next_op,
                ..
            } => {
                let op = next_replay_op(key, cassette, next_op, "remove_file", path)?;
                let WorldOp::RemoveFile {
                    path: expected_path,
                    result,
                } = op
                else {
                    return Err(unexpected_replay_op(key, "remove_file", path));
                };
                check_replay_path(key, "remove_file", expected_path, path)?;
                result.clone().into_io_result()
            }
        }
    }
    pub(crate) fn replay_shell_outcome(
        &mut self,
    ) -> Result<Option<WorldShellOutcome>, ToolFailure> {
        match &mut self.mode {
            WorldMode::Real | WorldMode::Recording { .. } => Ok(None),
            WorldMode::Replay {
                store,
                key,
                cassette,
                next_op,
                ..
            } => {
                let Some(op) = cassette.ops.get(*next_op) else {
                    return Err(ToolFailure::new(format!(
                        "vcr replay for {key} expected shell outcome but cassette ended"
                    )));
                };
                *next_op += 1;
                let WorldOp::Shell { outcome } = op else {
                    return Err(ToolFailure::new(format!(
                        "vcr replay for {key} expected shell outcome but found different op"
                    )));
                };
                let mut outcome = outcome.clone();
                outcome.refresh_ephemeral_shell_artifact(store, key);
                Ok(Some(outcome))
            }
        }
    }

    pub(crate) fn record_shell_outcome(&mut self, outcome: WorldShellOutcome) {
        if let WorldMode::Recording {
            cassette,
            side_output,
            ..
        } = &mut self.mode
        {
            let (recorded, side) = outcome.for_recording();
            *side_output = side;
            cassette.ops.push(WorldOp::Shell { outcome: recorded });
        }
    }
}

fn read_file_limited_real(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    let file = open_for_limited_read(path)?;
    let metadata = file.metadata()?;
    if metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            format!("{}: Is a directory", path.display()),
        ));
    }
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if max_bytes < metadata.len() as usize {
        return Err(file_too_large_error(max_bytes));
    }
    let mut limited = file.take((max_bytes as u64).saturating_add(1));
    let mut bytes = Vec::new();
    limited.read_to_end(&mut bytes)?;
    if max_bytes < bytes.len() {
        return Err(file_too_large_error(max_bytes));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn open_for_limited_read(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_for_limited_read(path: &Path) -> io::Result<std::fs::File> {
    std::fs::File::open(path)
}

fn file_too_large_error(max_bytes: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("file is too large to read safely (limit: {max_bytes} bytes)"),
    )
}

#[derive(Clone, Debug)]
pub(crate) struct WorldDirEntry {
    pub(crate) name: tau_vcr::EscapedBytes,
    pub(crate) is_dir: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum WorldShellOutcome {
    Finished {
        result: CborValue,
        display: Box<ToolUseState>,
        elapsed_ms: u64,
        /// Bounded artifact regeneration data kept out of provider-visible
        /// CBOR.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        saved_output: Option<RecordedSavedOutput>,
    },
    Cancelled,
}

/// Bounded rendering used to regenerate an ephemeral artifact during replay.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RecordedSavedOutput {
    /// Exact VCR-owned side-artifact length.
    bytes: u64,
    /// BLAKE3 digest of the side artifact.
    digest: [u8; 32],
    /// Whether the live saved rendering hit its hard cap.
    incomplete: bool,
}

impl WorldShellOutcome {
    fn for_recording(mut self) -> (Self, Option<Vec<u8>>) {
        let Self::Finished {
            result,
            saved_output,
            ..
        } = &mut self
        else {
            return (self, None);
        };
        let path = take_text_field(result, "full_output_path")
            .map(|path| (path, false))
            .or_else(|| take_text_field(result, "saved_output_path").map(|path| (path, true)));
        remove_field(result, "saved_output_truncated");
        remove_field(result, "saved_output_bytes");
        let side = path.and_then(|(path, incomplete)| {
            let content = std::fs::read(path).ok()?;
            *saved_output = Some(RecordedSavedOutput {
                bytes: content.len() as u64,
                digest: *blake3::hash(&content).as_bytes(),
                incomplete,
            });
            Some(content)
        });
        (self, side)
    }

    fn refresh_ephemeral_shell_artifact(&mut self, store: &tau_vcr::VcrStore, key: &str) {
        let Self::Finished {
            result,
            saved_output,
            ..
        } = self
        else {
            return;
        };
        remove_field(result, "full_output_path");
        remove_field(result, "saved_output_path");
        remove_field(result, "saved_output_truncated");
        remove_field(result, "saved_output_bytes");
        remove_field(result, "saved_output_unavailable");
        enforce_current_shell_output_cap(result);
        let artifact_required = has_field(result, "truncated");
        if artifact_required && !has_field(result, "truncation_warning") {
            insert_text_field(
                result,
                "truncation_warning",
                "Fetching excessive output is inefficient; prefer narrower commands or filters."
                    .to_owned(),
            );
        }
        if let Some(saved) = saved_output
            && let Ok(content) = store.get_side(
                key,
                &tau_vcr::ArtifactKind::new("shell-output").expect("static artifact kind is valid"),
                crate::shell_output_spool::MAX_SAVED_OUTPUT_BYTES as u64,
            )
            && content.len() as u64 == saved.bytes
            && blake3::hash(&content).as_bytes() == &saved.digest
            && let Ok(content) = String::from_utf8(content)
            && let Ok(artifact) = crate::shell_output_spool::save(&content, saved.incomplete)
        {
            insert_text_field(
                result,
                if artifact.incomplete {
                    "saved_output_path"
                } else {
                    "full_output_path"
                },
                artifact
                    .path
                    .to_str()
                    .expect("spool accepts only safe UTF-8 paths")
                    .to_owned(),
            );
            if artifact.incomplete {
                insert_bool_field(result, "saved_output_truncated", true);
                insert_integer_field(result, "saved_output_bytes", artifact.saved_bytes as i64);
            }
        } else if artifact_required {
            insert_bool_field(result, "saved_output_unavailable", true);
        }
    }
}

fn enforce_current_shell_output_cap(result: &mut CborValue) {
    let Some(output) = text_field(result, "output") else {
        return;
    };
    if output.len() <= crate::tools::shell::MAX_MODEL_SHELL_OUTPUT_BYTES {
        return;
    }
    let truncated = crate::truncate::truncate_line_oriented_lines_with_byte_limit(
        output.lines(),
        output.lines().count(),
        output.len(),
        crate::tools::shell::MAX_MODEL_SHELL_OUTPUT_BYTES,
    );
    replace_text_field(result, "output", truncated.content);
    remove_field(result, "truncated");
    insert_bool_field(result, "truncated", true);
    if !has_field(result, "total_lines") {
        insert_integer_field(result, "total_lines", truncated.total_lines as i64);
    }
    if !has_field(result, "total_bytes") {
        insert_integer_field(result, "total_bytes", truncated.total_bytes as i64);
    }
}

fn has_field(value: &CborValue, key: &str) -> bool {
    matches!(value, CborValue::Map(entries) if entries.iter().any(
        |(name, _)| matches!(name, CborValue::Text(name) if name == key)
    ))
}

fn text_field<'a>(value: &'a CborValue, key: &str) -> Option<&'a str> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries
        .iter()
        .find_map(|(name, value)| match (name, value) {
            (CborValue::Text(name), CborValue::Text(text)) if name == key => Some(text.as_str()),
            _ => None,
        })
}

fn replace_text_field(value: &mut CborValue, key: &str, replacement: String) {
    if let CborValue::Map(entries) = value
        && let Some((_, CborValue::Text(text))) = entries
            .iter_mut()
            .find(|(name, _)| matches!(name, CborValue::Text(name) if name == key))
    {
        *text = replacement;
    }
}

fn remove_field(value: &mut CborValue, key: &str) {
    if let CborValue::Map(entries) = value {
        entries.retain(|(name, _)| name.as_text() != Some(key));
    }
}

fn take_text_field(value: &mut CborValue, key: &str) -> Option<String> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    let index = entries
        .iter()
        .position(|(name, _)| name.as_text() == Some(key))?;
    match entries.remove(index).1 {
        CborValue::Text(text) => Some(text),
        _ => None,
    }
}

fn insert_text_field(value: &mut CborValue, key: &str, text: String) {
    if let CborValue::Map(entries) = value {
        entries.push((CborValue::Text(key.to_owned()), CborValue::Text(text)));
    }
}

fn insert_bool_field(value: &mut CborValue, key: &str, flag: bool) {
    if let CborValue::Map(entries) = value {
        entries.push((CborValue::Text(key.to_owned()), CborValue::Bool(flag)));
    }
}

fn insert_integer_field(value: &mut CborValue, key: &str, integer: i64) {
    if let CborValue::Map(entries) = value {
        entries.push((
            CborValue::Text(key.to_owned()),
            CborValue::Integer(integer.into()),
        ));
    }
}

fn atomic_write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let target = final_write_path(path)?;
    let parent = target.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path has no parent: {}", target.display()),
        )
    })?;
    let file_name = target.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path has no file name: {}", target.display()),
        )
    })?;
    let temp_path = parent.join(format!(
        ".{}.tmp-{}-{}",
        file_name.to_string_lossy(),
        std::process::id(),
        unique_temp_suffix()
    ));

    atomic_write_file_to_temp(&target, &temp_path, bytes)
}

fn atomic_write_file_to_temp(target: &Path, temp_path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = target.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path has no parent: {}", target.display()),
        )
    })?;
    let mut created_temp = false;
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temp_path)?;
        created_temp = true;
        if let Ok(metadata) = std::fs::metadata(target) {
            file.set_permissions(metadata.permissions())?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(temp_path, target)?;
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if result.is_err() && created_temp {
        let _ = std::fs::remove_file(temp_path);
    }
    result
}

const MAX_FINAL_SYMLINK_HOPS: usize = 40;

pub(crate) fn final_write_path(path: &Path) -> io::Result<std::path::PathBuf> {
    let mut current = path.to_owned();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..MAX_FINAL_SYMLINK_HOPS {
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if !seen.insert(current.clone()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("symlink loop while resolving {}", path.display()),
                    ));
                }
                let link = std::fs::read_link(&current)?;
                current = if link.is_absolute() {
                    link
                } else {
                    current
                        .parent()
                        .unwrap_or_else(|| Path::new("."))
                        .join(link)
                };
            }
            Ok(_) => return Ok(current),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(current);
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("too many symlink hops while resolving {}", path.display()),
    ))
}

fn unique_temp_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn read_dir_entries(path: &Path, max_entries: usize) -> io::Result<Vec<WorldDirEntry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        entries.push(WorldDirEntry {
            name: os_str_name(&entry.file_name()),
            is_dir: entry.file_type()?.is_dir(),
        });
        if max_entries <= entries.len() {
            break;
        }
    }
    Ok(entries)
}

#[cfg(unix)]
fn os_str_name(value: &std::ffi::OsStr) -> tau_vcr::EscapedBytes {
    use std::os::unix::ffi::OsStrExt;

    tau_vcr::EscapedBytes::new(value.as_bytes())
}

#[cfg(not(unix))]
fn os_str_name(value: &std::ffi::OsStr) -> tau_vcr::EscapedBytes {
    tau_vcr::EscapedBytes::new(value.to_string_lossy().as_bytes())
}

fn cassette_path(path: &Path) -> String {
    if let Ok(cwd) = std::env::current_dir()
        && let Ok(relative) = path.strip_prefix(&cwd)
    {
        if relative.as_os_str().is_empty() {
            return ".".to_owned();
        }
        return relative.display().to_string();
    }
    path.display().to_string()
}

fn next_replay_op<'a>(
    key: &str,
    cassette: &'a WorldCassette,
    next_op: &mut usize,
    op_name: &str,
    path: &Path,
) -> io::Result<&'a WorldOp> {
    let Some(op) = cassette.ops.get(*next_op) else {
        return Err(replay_io_error(format!(
            "vcr replay for {key} expected {op_name}({}) but cassette ended",
            path.display()
        )));
    };
    *next_op += 1;
    Ok(op)
}

fn unexpected_replay_op(key: &str, op_name: &str, path: &Path) -> io::Error {
    replay_io_error(format!(
        "vcr replay for {key} expected {op_name}({}) but found different op",
        path.display()
    ))
}

fn check_replay_path(
    key: &str,
    op_name: &str,
    expected_path: &str,
    actual_path: &Path,
) -> io::Result<()> {
    let actual_path = cassette_path(actual_path);
    if expected_path == actual_path {
        return Ok(());
    }
    Err(replay_io_error(format!(
        "vcr replay for {key} expected {op_name}({expected_path}) but got {op_name}({actual_path})"
    )))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WorldCassette {
    version: u32,
    request: WorldRequest,
    ops: Vec<WorldOp>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct WorldRequest {
    tool: String,
    arguments: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum WorldOp {
    IsDir {
        path: String,
        result: OpResult<bool>,
    },
    ReadDir {
        path: String,
        result: OpResult<Vec<RecordedDirEntry>>,
    },
    ReadFile {
        path: String,
        result: OpResult<tau_vcr::EscapedBytes>,
    },
    WriteFile {
        path: String,
        bytes: tau_vcr::EscapedBytes,
        result: OpResult<()>,
    },
    PathExists {
        path: String,
        result: OpResult<bool>,
    },
    CreateDirAll {
        path: String,
        result: OpResult<()>,
    },
    RemoveFile {
        path: String,
        result: OpResult<()>,
    },
    Shell {
        outcome: WorldShellOutcome,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
enum OpResult<T> {
    Ok(T),
    Err(WorldIoError),
}

impl<T> OpResult<T> {
    fn from_io_result_ref(result: &io::Result<T>) -> Self
    where
        T: Clone,
    {
        match result {
            Ok(value) => Self::Ok(value.clone()),
            Err(error) => Self::Err(WorldIoError::from_io_error(error)),
        }
    }

    fn map_ok<U>(self, f: impl FnOnce(T) -> U) -> OpResult<U> {
        match self {
            Self::Ok(value) => OpResult::Ok(f(value)),
            Self::Err(error) => OpResult::Err(error),
        }
    }

    fn into_io_result(self) -> io::Result<T> {
        match self {
            Self::Ok(value) => Ok(value),
            Self::Err(error) => Err(error.into_io_error()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WorldIoError {
    kind: String,
    message: String,
}

impl WorldIoError {
    fn from_io_error(error: &io::Error) -> Self {
        Self {
            kind: format!("{:?}", error.kind()),
            message: error.to_string(),
        }
    }

    fn into_io_error(self) -> io::Error {
        io::Error::new(io_error_kind(&self.kind), self.message)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RecordedDirEntry {
    name: tau_vcr::EscapedBytes,
    is_dir: bool,
}

impl RecordedDirEntry {
    fn from_world(entry: &WorldDirEntry) -> Self {
        Self {
            name: entry.name.clone(),
            is_dir: entry.is_dir,
        }
    }

    fn into_world(self) -> WorldDirEntry {
        WorldDirEntry {
            name: self.name,
            is_dir: self.is_dir,
        }
    }
}

fn world_request(tool_name: &str, arguments: &CborValue) -> Result<WorldRequest, ToolFailure> {
    let arguments = serde_json::to_value(arguments).map_err(|error| {
        ToolFailure::new(format!("failed to serialize vcr tool arguments: {error}"))
    })?;
    Ok(WorldRequest {
        tool: tool_name.to_owned(),
        arguments,
    })
}

fn validate_cassette(
    key: &str,
    cassette: &WorldCassette,
    request: &WorldRequest,
) -> Result<(), ToolFailure> {
    if cassette.version != CASSETTE_VERSION {
        return Err(vcr_failure(tau_vcr::VcrError::UnsupportedVersion {
            key: key.to_owned(),
            version: cassette.version,
        }));
    }
    if &cassette.request != request {
        return Err(vcr_failure(tau_vcr::request_mismatch(
            key,
            &cassette.request,
            request,
        )));
    }
    Ok(())
}

fn vcr_failure(error: tau_vcr::VcrError) -> ToolFailure {
    ToolFailure::new(format!("vcr error: {error}"))
}

fn replay_io_error(message: String) -> io::Error {
    io::Error::other(message)
}

fn io_error_kind(kind: &str) -> io::ErrorKind {
    match kind {
        "NotFound" => io::ErrorKind::NotFound,
        "PermissionDenied" => io::ErrorKind::PermissionDenied,
        "AlreadyExists" => io::ErrorKind::AlreadyExists,
        "InvalidInput" => io::ErrorKind::InvalidInput,
        "InvalidData" => io::ErrorKind::InvalidData,
        "TimedOut" => io::ErrorKind::TimedOut,
        "WriteZero" => io::ErrorKind::WriteZero,
        "Interrupted" => io::ErrorKind::Interrupted,
        "UnexpectedEof" => io::ErrorKind::UnexpectedEof,
        _ => io::ErrorKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use tau_proto::CborValue;

    use super::*;

    fn ls_args(path: &std::path::Path) -> CborValue {
        CborValue::Map(vec![(
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        )])
    }

    /// Protects bounded mutation/read paths from blocking indefinitely on Unix
    /// special files such as FIFOs.
    #[cfg(unix)]
    #[test]
    fn read_file_limited_rejects_fifo_without_blocking() {
        let tempdir = tempfile::TempDir::new().expect("tempdir");
        let fifo_path = tempdir.path().join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .expect("run mkfifo");
        assert!(status.success(), "mkfifo failed");

        let error = read_file_limited_real(&fifo_path, MAX_SAFE_FILE_READ_BYTES)
            .expect_err("fifo should be rejected without blocking");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("not a regular file"));
    }

    /// Protects real-world writes against truncate-in-place updates by checking
    /// the atomic helper updates existing files through a same-directory
    /// rename.
    #[test]
    fn atomic_write_updates_existing_file() {
        let tempdir = tempfile::TempDir::new().expect("tempdir");
        let path = tempdir.path().join("file.txt");
        std::fs::write(&path, "old\n").expect("write old");

        atomic_write_file(&path, b"new\n").expect("atomic write");

        assert_eq!(std::fs::read_to_string(&path).expect("read"), "new\n");
    }

    /// Protects file creation semantics for mutation tools after switching from
    /// direct writes to same-directory atomic renames.
    #[test]
    fn atomic_write_creates_new_file() {
        let tempdir = tempfile::TempDir::new().expect("tempdir");
        let path = tempdir.path().join("created.txt");

        atomic_write_file(&path, b"created\n").expect("atomic write");

        assert_eq!(std::fs::read_to_string(&path).expect("read"), "created\n");
    }

    /// Ensures final symlinks keep the previous write-through behavior: editing
    /// a symlink updates its target instead of replacing the symlink itself.
    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_final_symlink() {
        use std::os::unix::fs::symlink;

        let tempdir = tempfile::TempDir::new().expect("tempdir");
        let target = tempdir.path().join("target.txt");
        let link = tempdir.path().join("link.txt");
        std::fs::write(&target, "old\n").expect("write target");
        symlink("target.txt", &link).expect("symlink");

        atomic_write_file(&link, b"new\n").expect("atomic write");

        assert_eq!(
            std::fs::read_to_string(&target).expect("read target"),
            "new\n"
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("link metadata")
                .file_type()
                .is_symlink()
        );
    }

    /// Protects chained final symlinks: atomic writes should update the real
    /// target at the end of the chain instead of replacing an intermediate
    /// link.
    #[cfg(unix)]
    #[test]
    fn atomic_write_follows_chained_final_symlink() {
        use std::os::unix::fs::symlink;

        let tempdir = tempfile::TempDir::new().expect("tempdir");
        let target = tempdir.path().join("target.txt");
        let link2 = tempdir.path().join("link2.txt");
        let link1 = tempdir.path().join("link1.txt");
        std::fs::write(&target, "old\n").expect("write target");
        symlink("target.txt", &link2).expect("link2");
        symlink("link2.txt", &link1).expect("link1");

        atomic_write_file(&link1, b"new\n").expect("atomic write");

        assert_eq!(
            std::fs::read_to_string(&target).expect("read target"),
            "new\n"
        );
        assert!(
            std::fs::symlink_metadata(&link1)
                .expect("link1 metadata")
                .file_type()
                .is_symlink()
        );
        assert!(
            std::fs::symlink_metadata(&link2)
                .expect("link2 metadata")
                .file_type()
                .is_symlink()
        );
    }

    /// Ensures cleanup after atomic write failure does not delete a colliding
    /// temp path that this call failed to create.
    #[test]
    fn atomic_write_temp_collision_preserves_existing_temp() {
        let tempdir = tempfile::TempDir::new().expect("tempdir");
        let target = tempdir.path().join("target.txt");
        let colliding_temp = tempdir.path().join(".target.txt.tmp-collision");
        std::fs::write(&target, "old\n").expect("write target");
        std::fs::write(&colliding_temp, "someone else\n").expect("write temp");

        let err = atomic_write_file_to_temp(&target, &colliding_temp, b"new\n")
            .expect_err("temp collision should fail");

        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(&target).expect("read target"),
            "old\n"
        );
        assert_eq!(
            std::fs::read_to_string(&colliding_temp).expect("read temp"),
            "someone else\n"
        );
    }

    #[test]
    fn ls_vcr_records_world_ops_and_replays_through_tool_logic() {
        let real_dir = tempfile::TempDir::new().expect("real dir");
        std::fs::write(real_dir.path().join("beta"), "b").expect("write beta");
        std::fs::create_dir(real_dir.path().join("alpha")).expect("create alpha");
        let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
        let args = ls_args(real_dir.path());

        let mut recording = ShellWorld::for_tool(
            "ls",
            "call_ls",
            &args,
            Some(tau_vcr::VcrConfig::new(
                tau_vcr::VcrMode::RecordIfMissing,
                cassette_dir.path(),
            )),
        )
        .expect("recording world");
        let recorded = crate::tools::ls::run_ls(&args, &mut recording).expect("recorded ls");
        recording.finish().expect("record cassette");
        std::fs::remove_file(real_dir.path().join("beta")).expect("remove live file");
        std::fs::remove_dir(real_dir.path().join("alpha")).expect("remove live dir");

        let mut replay = ShellWorld::for_tool(
            "ls",
            "call_ls",
            &args,
            Some(tau_vcr::VcrConfig::new(
                tau_vcr::VcrMode::ReplayOnly,
                cassette_dir.path(),
            )),
        )
        .expect("replay world");
        let replayed = crate::tools::ls::run_ls(&args, &mut replay).expect("replayed ls");
        replay.finish().expect("consume replay ops");

        assert_eq!(replayed.result, recorded.result);
        let cassette = std::fs::read_to_string(cassette_dir.path().join("call_ls.yaml"))
            .expect("read cassette");
        assert!(cassette.contains("op: is_dir"));
        assert!(cassette.contains("op: read_dir"));
        assert!(cassette.contains("name: alpha"));
        assert!(!cassette.contains("kind: utf8"));
        assert!(!cassette.contains("value: alpha"));
        assert!(!cassette.contains("1 alpha/"));
    }
    #[test]
    fn read_vcr_replays_file_bytes_through_read_logic() {
        let real_dir = tempfile::TempDir::new().expect("real dir");
        let file = real_dir.path().join("file.txt");
        std::fs::write(&file, b"alpha\n\xFFbeta").expect("write file");
        let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
        let args = CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(file.display().to_string()),
            ),
            (
                CborValue::Text("start_line".to_owned()),
                CborValue::Integer(2.into()),
            ),
        ]);

        let mut recording = ShellWorld::for_tool(
            "read",
            "call_read",
            &args,
            Some(tau_vcr::VcrConfig::new(
                tau_vcr::VcrMode::RecordIfMissing,
                cassette_dir.path(),
            )),
        )
        .expect("recording world");
        let recorded = crate::tools::read::read_file(&args, &mut recording).expect("recorded read");
        recording.finish().expect("record cassette");
        std::fs::write(&file, b"changed").expect("change live file");

        let mut replay = ShellWorld::for_tool(
            "read",
            "call_read",
            &args,
            Some(tau_vcr::VcrConfig::new(
                tau_vcr::VcrMode::ReplayOnly,
                cassette_dir.path(),
            )),
        )
        .expect("replay world");
        let replayed = crate::tools::read::read_file(&args, &mut replay).expect("replayed read");
        replay.finish().expect("consume replay ops");

        assert_eq!(replayed.result, recorded.result);
        let cassette = std::fs::read_to_string(cassette_dir.path().join("call_read.yaml"))
            .expect("read cassette");
        assert!(cassette.contains("op: read_file"));
        assert!(cassette.contains("\\uDCFFbeta"));
        assert!(!cassette.contains("- 255"));
    }
    #[test]
    fn edit_vcr_replay_asserts_write_without_mutating_live_file() {
        let real_dir = tempfile::TempDir::new().expect("real dir");
        let file = real_dir.path().join("file.txt");
        std::fs::write(&file, b"one\ntwo\n").expect("write file");
        let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
        let args = CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(file.display().to_string()),
            ),
            (
                CborValue::Text("edits".to_owned()),
                CborValue::Array(vec![CborValue::Map(vec![
                    (
                        CborValue::Text("start_line".to_owned()),
                        CborValue::Integer(2.into()),
                    ),
                    (
                        CborValue::Text("end_line_exclusive".to_owned()),
                        CborValue::Integer(3.into()),
                    ),
                    (
                        CborValue::Text("newText".to_owned()),
                        CborValue::Text("TWO\n".to_owned()),
                    ),
                    (
                        CborValue::Text("context_line".to_owned()),
                        CborValue::Text("two".to_owned()),
                    ),
                ])]),
            ),
        ]);

        let mut recording = ShellWorld::for_tool(
            "edit",
            "call_edit",
            &args,
            Some(tau_vcr::VcrConfig::new(
                tau_vcr::VcrMode::RecordIfMissing,
                cassette_dir.path(),
            )),
        )
        .expect("recording world");
        let recorded = crate::tools::edit::edit_file(&args, &mut recording).expect("recorded edit");
        recording.finish().expect("record cassette");
        assert_eq!(
            std::fs::read(&file).expect("read recorded file"),
            b"one\nTWO\n"
        );
        std::fs::write(&file, b"live should not change\n").expect("change live file");

        let mut replay = ShellWorld::for_tool(
            "edit",
            "call_edit",
            &args,
            Some(tau_vcr::VcrConfig::new(
                tau_vcr::VcrMode::ReplayOnly,
                cassette_dir.path(),
            )),
        )
        .expect("replay world");
        let replayed = crate::tools::edit::edit_file(&args, &mut replay).expect("replayed edit");
        replay.finish().expect("consume replay ops");

        assert_eq!(replayed.result, recorded.result);
        assert_eq!(
            std::fs::read(&file).expect("read live file"),
            b"live should not change\n"
        );
        let cassette = std::fs::read_to_string(cassette_dir.path().join("call_edit.yaml"))
            .expect("read cassette");
        assert!(cassette.contains("op: read_file"));
        assert!(cassette.contains("op: path_exists"));
        assert!(cassette.contains("op: write_file"));
    }
    #[test]
    fn apply_patch_vcr_replay_asserts_move_write_and_remove_without_mutating_live_files() {
        let real_dir = tempfile::TempDir::new().expect("real dir");
        let source = real_dir.path().join("source.txt");
        let dest = real_dir.path().join("dest.txt");
        std::fs::write(&source, "one\ntwo\n").expect("write source");
        let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
        let args = CborValue::Text(format!(
            "*** Begin Patch\n*** Update File: {}\n*** Move to: {}\n@@\n one\n-two\n+TWO\n*** End Patch",
            source.display(),
            dest.display()
        ));

        let mut recording = ShellWorld::for_tool(
            "apply_patch",
            "call_patch",
            &args,
            Some(tau_vcr::VcrConfig::new(
                tau_vcr::VcrMode::RecordIfMissing,
                cassette_dir.path(),
            )),
        )
        .expect("recording world");
        let recorded = crate::tools::apply_patch::apply_patch(&args, &mut recording)
            .expect("recorded apply_patch");
        recording.finish().expect("record cassette");
        assert!(!source.exists());
        assert_eq!(
            std::fs::read_to_string(&dest).expect("read dest"),
            "one\nTWO\n"
        );
        std::fs::write(&source, "live source\n").expect("restore live source");
        std::fs::write(&dest, "live dest\n").expect("change live dest");

        let mut replay = ShellWorld::for_tool(
            "apply_patch",
            "call_patch",
            &args,
            Some(tau_vcr::VcrConfig::new(
                tau_vcr::VcrMode::ReplayOnly,
                cassette_dir.path(),
            )),
        )
        .expect("replay world");
        let replayed = crate::tools::apply_patch::apply_patch(&args, &mut replay)
            .expect("replayed apply_patch");
        replay.finish().expect("consume replay ops");

        assert_eq!(replayed.result, recorded.result);
        assert_eq!(
            std::fs::read_to_string(&source).expect("read source"),
            "live source\n"
        );
        assert_eq!(
            std::fs::read_to_string(&dest).expect("read dest"),
            "live dest\n"
        );
        let cassette = std::fs::read_to_string(cassette_dir.path().join("call_patch.yaml"))
            .expect("read cassette");
        assert!(cassette.contains("op: read_file"));
        assert!(cassette.contains("op: create_dir_all"));
        assert!(cassette.contains("op: write_file"));
        assert!(cassette.contains("op: is_dir"));
        assert!(cassette.contains("op: remove_file"));
    }

    #[test]
    fn apply_patch_vcr_relative_paths_do_not_record_cwd_absolute_paths() {
        let cwd = std::env::current_dir().expect("current dir");
        let real_dir = tempfile::Builder::new()
            .prefix("world-relative-")
            .tempdir_in(&cwd)
            .expect("real dir under cwd");
        let source = real_dir.path().join("source.txt");
        let dest = real_dir.path().join("dest.txt");
        std::fs::write(&source, "one\ntwo\n").expect("write source");
        let source_rel = source.strip_prefix(&cwd).expect("relative source");
        let dest_rel = dest.strip_prefix(&cwd).expect("relative dest");
        let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
        let args = CborValue::Text(format!(
            "*** Begin Patch\n*** Update File: {}\n*** Move to: {}\n@@\n one\n-two\n+TWO\n*** End Patch",
            source_rel.display(),
            dest_rel.display()
        ));

        let mut recording = ShellWorld::for_tool(
            "apply_patch",
            "call_relative_patch",
            &args,
            Some(tau_vcr::VcrConfig::new(
                tau_vcr::VcrMode::RecordIfMissing,
                cassette_dir.path(),
            )),
        )
        .expect("recording world");
        crate::tools::apply_patch::apply_patch(&args, &mut recording)
            .expect("recorded apply_patch");
        recording.finish().expect("record cassette");

        let cassette =
            std::fs::read_to_string(cassette_dir.path().join("call_relative_patch.yaml"))
                .expect("read cassette");
        assert!(cassette.contains(&format!("path: {}", source_rel.display())));
        assert!(cassette.contains(&format!("path: {}", dest_rel.display())));
        assert!(!cassette.contains(cwd.to_str().expect("utf8 cwd")));
    }
}
