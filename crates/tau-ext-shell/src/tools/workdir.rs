//! `workdir` tool: inspect or update the extension instance's remembered
//! directory.

use std::path::{Path, PathBuf};

use tau_proto::CborValue;

use crate::argument::argument_text;
use crate::display::{ToolFailure, ToolOutput, ok_display};

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
        display: ok_display(text),
    }
}

/// Build the current `workdir` read result, including whether it remains
/// usable.
pub(crate) fn status_output(path: Option<&Path>) -> ToolOutput {
    let status = if path.is_none() {
        "invalid"
    } else if path.is_some_and(Path::is_dir) {
        "available"
    } else {
        "unavailable"
    };
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
    let display_path =
        path.map_or_else(|| "<invalid>".to_owned(), |path| path.display().to_string());
    ToolOutput {
        result,
        provider_content: Vec::new(),
        display: ok_display(format!("{display_path} ({status})")),
    }
}
