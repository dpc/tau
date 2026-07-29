//! `workdir` tool: inspect or update the extension instance's remembered
//! directory.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use tau_proto::{CborValue, ToolUseState, ToolUseStatus};

use crate::argument::argument_text;
use crate::display::{ToolFailure, ToolOutput};

/// Availability classification for persisted workdir metadata.
#[derive(Clone, Copy)]
enum WorkdirStatus {
    Available,
    Missing,
    Inaccessible,
    NotDirectory,
    Invalid,
}

impl WorkdirStatus {
    /// Preserve the established semantic result vocabulary.
    fn semantic_status(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Invalid => "invalid",
            Self::Missing | Self::Inaccessible | Self::NotDirectory => "unavailable",
        }
    }

    /// Return UI text only for a state that needs explicit attention.
    fn display_suffix(self) -> Option<&'static str> {
        match self {
            Self::Available => None,
            Self::Missing => Some("missing"),
            Self::Inaccessible => Some("inaccessible"),
            Self::NotDirectory => Some("not-directory"),
            Self::Invalid => Some("invalid"),
        }
    }
}

/// User-visible operation performed by a successful workdir call.
enum WorkdirOperation {
    Get,
    Set,
}

impl WorkdirOperation {
    /// Build an accessible path-only label whose mode distinguishes get/set.
    fn display(self, path: String) -> ToolUseState {
        let mode = match self {
            Self::Get => "get",
            Self::Set => "set",
        };
        ToolUseState {
            args: path,
            mode: mode.to_owned(),
            status: ToolUseStatus::Success,
            status_text: "ok".to_owned(),
            ..Default::default()
        }
    }
}

/// Parsed target directory for a `workdir` setter call.
pub(crate) fn target_dir(
    arguments: &CborValue,
    base: Option<&Path>,
) -> Result<PathBuf, ToolFailure> {
    let target = argument_text(arguments, "path").map_err(ToolFailure::from)?;
    if target.is_empty() {
        return Err(ToolFailure::new(
            "workdir path must not be empty".to_owned(),
        ));
    }
    let path = PathBuf::from(target);
    let path = if path.is_absolute() {
        path
    } else {
        base.ok_or_else(|| {
            ToolFailure::new(
                "relative workdir cannot repair invalid remembered metadata; use an absolute path"
                    .to_owned(),
            )
        })?
        .join(path)
    };
    path.canonicalize()
        .map_err(|error| ToolFailure::from(format!("failed to resolve directory: {error}")))
        .and_then(|path| {
            if path.is_dir() {
                Ok(path)
            } else {
                Err(ToolFailure::from(format!(
                    "not a directory: {}",
                    path.display()
                )))
            }
        })
}

/// Build a successful committed `workdir` setter result.
pub(crate) fn output(path: &Path) -> ToolOutput {
    let text = format!("Workdir changed to {}.", path.display());
    ToolOutput {
        result: CborValue::Text(text.clone()),
        provider_content: Vec::new(),
        display: WorkdirOperation::Set.display(path.display().to_string()),
    }
}

/// Build the current `workdir` read result, including whether it remains
/// usable.
pub(crate) fn status_output(path: Option<&Path>) -> ToolOutput {
    let workdir_status = workdir_status(path);
    let status = workdir_status.semantic_status();
    let mut entries = Vec::new();
    if let Some(path) = path {
        entries.push((
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        ));
    }
    entries.push((
        CborValue::Text("status".to_owned()),
        CborValue::Text(status.to_owned()),
    ));
    let result = CborValue::Map(entries);
    let display_path = path.map_or_else(
        || "<invalid> (invalid)".to_owned(),
        |path| {
            let path = path.display();
            match workdir_status.display_suffix() {
                Some(status) => format!("{path} ({status})"),
                None => path.to_string(),
            }
        },
    );
    ToolOutput {
        result,
        provider_content: Vec::new(),
        display: WorkdirOperation::Get.display(display_path),
    }
}

/// Classify remembered workdir state for both the semantic result and compact
/// UI.
fn workdir_status(path: Option<&Path>) -> WorkdirStatus {
    let Some(path) = path else {
        return WorkdirStatus::Invalid;
    };
    match path.metadata() {
        Ok(metadata) if metadata.is_dir() => WorkdirStatus::Available,
        Ok(_) => WorkdirStatus::NotDirectory,
        Err(error) if error.kind() == ErrorKind::NotFound => WorkdirStatus::Missing,
        Err(error) if error.kind() == ErrorKind::PermissionDenied => WorkdirStatus::Inaccessible,
        Err(_) => WorkdirStatus::Inaccessible,
    }
}

#[cfg(test)]
mod tests;
