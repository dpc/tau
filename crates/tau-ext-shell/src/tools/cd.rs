//! `cd` tool: update the shell extension's remembered working directory.

use std::path::{Path, PathBuf};

use tau_proto::CborValue;

use crate::argument::argument_text;
use crate::display::{ToolFailure, ToolOutput, ok_display};

/// Parsed target directory for a `cd` tool call.
pub(crate) fn target_dir(arguments: &CborValue, base: &Path) -> Result<PathBuf, ToolFailure> {
    let target = argument_text(arguments, "path").map_err(ToolFailure::from)?;
    let path = PathBuf::from(target);
    let path = if path.is_absolute() {
        path
    } else {
        base.join(path)
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

/// Build a successful `cd` tool result.
pub(crate) fn output(path: &Path) -> ToolOutput {
    let text = format!("Working directory change requested: {}", path.display());
    ToolOutput {
        result: CborValue::Text(text.clone()),
        provider_content: Vec::new(),
        display: ok_display(text),
    }
}
