//! `replace` tool: exact, snapshot-based text replacements in one UTF-8 file.

use std::io::ErrorKind;
use std::path::PathBuf;

use tau_proto::{CborValue, ToolUsePayload, ToolUseState, ToolUseStatus};

use crate::diff::compute_diff;
use crate::display::{ToolFailure, ToolOutput};
use crate::tools::world::{MAX_SAFE_FILE_READ_BYTES, ShellWorld};

/// Maximum replacement entries accepted in one request.
const MAX_EDITS_PER_CALL: usize = 100;
/// UTF-8 byte-order mark retained in source files while excluded from matching.
const UTF8_BOM: &[u8] = b"\xef\xbb\xbf";

/// Replaces exact normalized text ranges in an existing UTF-8 file atomically.
pub(crate) fn replace_file(
    arguments: &CborValue,
    world: &mut ShellWorld,
) -> Result<ToolOutput, ToolFailure> {
    let request = ReplaceRequest::parse(arguments)?;
    let source = world
        .read_file_limited(&request.path, MAX_SAFE_FILE_READ_BYTES)
        .map_err(read_failure)?;
    let original =
        std::str::from_utf8(&source).map_err(|_| ToolFailure::new("file is not valid UTF-8"))?;
    let normalized = NormalizedText::from_source(&source);

    let mut replacements = Vec::with_capacity(request.edits.len());
    for edit in &request.edits {
        let old = normalize_line_endings(&edit.old_text);
        if old.is_empty() {
            return Err(ToolFailure::new("oldText must not be empty"));
        }
        let matches: Vec<_> = normalized.text.match_indices(&old).collect();
        if matches.len() != 1 {
            return Err(ToolFailure::new("each oldText must match exactly once"));
        }
        let (start, _) = matches[0];
        let end = start + old.len();
        replacements.push(Replacement {
            start: normalized.source_offsets[start],
            end: normalized.source_offsets[end],
            new_text: replacement_bytes(
                &edit.new_text,
                &source,
                normalized.source_offsets[start],
                normalized.source_offsets[end],
            ),
        });
    }
    validate_non_overlapping(&mut replacements)?;

    let mut result = source.clone();
    for replacement in replacements.iter().rev() {
        result.splice(
            replacement.start..replacement.end,
            replacement.new_text.iter().copied(),
        );
    }
    let changed = result != source;
    if changed {
        world
            .write_file(&request.path, &result)
            .map_err(|_| ToolFailure::new("file could not be written"))?;
    }

    let mut display = ToolUseState {
        args: request.path.display().to_string(),
        status: ToolUseStatus::Success,
        status_text: "ok".to_owned(),
        ..Default::default()
    };
    if changed {
        // Source and result are UTF-8 because source and all request text are UTF-8.
        display.payload = Some(ToolUsePayload::Diff(compute_diff(
            original,
            std::str::from_utf8(&result).expect("UTF-8 replacement preserves UTF-8"),
        )));
    }
    Ok(ToolOutput {
        result: result_value(request.edits.len(), changed, result.len()),
        provider_content: Vec::new(),
        display,
    })
}

/// Validates a replace request before lock planning and returns its file path.
///
/// This keeps lock-enabled admission subject to the same strict request
/// validation and compact error contract as normal replacement execution.
pub(crate) fn replace_lock_path(arguments: &CborValue) -> Result<PathBuf, ToolFailure> {
    Ok(ReplaceRequest::parse(arguments)?.path)
}

/// Validated request arguments for one replacement operation.
#[derive(Debug)]
struct ReplaceRequest {
    /// Existing file resolved by the shell extension's frozen workdir rewrite.
    path: PathBuf,
    /// Replacement entries applied against the same original source snapshot.
    edits: Vec<ReplaceEdit>,
}

impl ReplaceRequest {
    /// Parses the strict provider-visible request shape without legacy aliases.
    fn parse(arguments: &CborValue) -> Result<Self, ToolFailure> {
        let fields =
            map_fields(arguments).ok_or_else(|| ToolFailure::new("replace expects an object"))?;
        reject_unknown_fields(fields, &["path", "edits"])?;
        let path = required_text(fields, "path")?;
        if path.is_empty() {
            return Err(ToolFailure::new("path must not be empty"));
        }
        let edits = required_array(fields, "edits")?;
        if edits.is_empty() || MAX_EDITS_PER_CALL < edits.len() {
            return Err(ToolFailure::new(
                "edits must contain from 1 through 100 entries",
            ));
        }
        let edits = edits
            .iter()
            .map(ReplaceEdit::parse)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            path: PathBuf::from(path),
            edits,
        })
    }
}

/// One exact old-to-new replacement requested by the model.
#[derive(Debug)]
struct ReplaceEdit {
    /// Required nonempty text to find after line-ending normalization.
    old_text: String,
    /// Replacement text, which may be empty to delete the target.
    new_text: String,
}

impl ReplaceEdit {
    /// Parses one strict replacement object.
    fn parse(value: &CborValue) -> Result<Self, ToolFailure> {
        let fields =
            map_fields(value).ok_or_else(|| ToolFailure::new("each edit must be an object"))?;
        reject_unknown_fields(fields, &["oldText", "newText"])?;
        Ok(Self {
            old_text: required_text(fields, "oldText")?.to_owned(),
            new_text: required_text(fields, "newText")?.to_owned(),
        })
    }
}

/// A source replacement range and its locally encoded replacement bytes.
struct Replacement {
    /// Inclusive source byte offset.
    start: usize,
    /// Exclusive source byte offset.
    end: usize,
    /// Replacement bytes using the selected local line ending.
    new_text: Vec<u8>,
}

/// Normalized source text plus one source offset for each normalized byte
/// boundary.
struct NormalizedText {
    /// Source with BOM excluded and all line endings represented as LF.
    text: String,
    /// Maps normalized byte boundaries back to original source byte boundaries.
    source_offsets: Vec<usize>,
}

impl NormalizedText {
    /// Builds a reversible boundary map while normalizing only CRLF and CR
    /// endings.
    fn from_source(source: &[u8]) -> Self {
        let start = usize::from(source.starts_with(UTF8_BOM)) * UTF8_BOM.len();
        let mut text = Vec::with_capacity(source.len().saturating_sub(start));
        let mut offsets = Vec::with_capacity(source.len().saturating_sub(start) + 1);
        offsets.push(start);
        let mut index = start;
        while index < source.len() {
            if source[index] == b'\r' {
                text.push(b'\n');
                index += if source.get(index + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
                offsets.push(index);
            } else {
                text.push(source[index]);
                index += 1;
                offsets.push(index);
            }
        }
        Self {
            text: String::from_utf8(text).expect("source was validated as UTF-8"),
            source_offsets: offsets,
        }
    }
}

/// Normalizes CRLF and CR line endings to LF without any fuzzy text changes.
fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Encodes replacement newlines with the target's first ending or nearby source
/// ending.
fn replacement_bytes(new_text: &str, source: &[u8], start: usize, end: usize) -> Vec<u8> {
    let line_ending = first_line_ending(&source[start..end])
        .or_else(|| nearest_line_ending(source, start))
        .unwrap_or(b"\n");
    let normalized = normalize_line_endings(new_text);
    normalized
        .replace(
            '\n',
            std::str::from_utf8(line_ending).expect("ASCII line ending"),
        )
        .into_bytes()
}

/// Finds the first original line ending in a byte range.
fn first_line_ending(bytes: &[u8]) -> Option<&[u8]> {
    bytes
        .iter()
        .position(|byte| *byte == b'\n' || *byte == b'\r')
        .map(|index| {
            if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
                &bytes[index..index + 2]
            } else {
                &bytes[index..index + 1]
            }
        })
}

/// Finds the closest original line ending to a source offset.
fn nearest_line_ending(source: &[u8], target: usize) -> Option<&[u8]> {
    let mut best: Option<(usize, &[u8])> = None;
    for index in 0..source.len() {
        if source[index] == b'\n' && 0 < index && source[index - 1] == b'\r' {
            continue;
        }
        if source[index] != b'\r' && source[index] != b'\n' {
            continue;
        }
        let ending = if source[index] == b'\r' && source.get(index + 1) == Some(&b'\n') {
            &source[index..index + 2]
        } else {
            &source[index..index + 1]
        };
        let distance = index.abs_diff(target);
        if best.is_none_or(|(best_distance, _)| distance < best_distance) {
            best = Some((distance, ending));
        }
    }
    best.map(|(_, ending)| ending)
}

/// Rejects overlapping ranges before any file write can occur.
fn validate_non_overlapping(replacements: &mut [Replacement]) -> Result<(), ToolFailure> {
    replacements.sort_by_key(|replacement| replacement.start);
    for pair in replacements.windows(2) {
        if pair[1].start < pair[0].end {
            return Err(ToolFailure::new("replacement targets overlap"));
        }
    }
    Ok(())
}

/// Returns the intentionally small model-visible success result.
fn result_value(edits: usize, changed: bool, total_bytes: usize) -> CborValue {
    CborValue::Map(vec![
        (
            CborValue::Text("edits".to_owned()),
            CborValue::Integer((edits as i64).into()),
        ),
        (
            CborValue::Text("changed".to_owned()),
            CborValue::Bool(changed),
        ),
        (
            CborValue::Text("total_bytes".to_owned()),
            CborValue::Integer((total_bytes as i64).into()),
        ),
    ])
}

/// Converts filesystem failures to compact messages that do not reveal paths or
/// source.
fn read_failure(error: std::io::Error) -> ToolFailure {
    if error.kind() == ErrorKind::NotFound {
        ToolFailure::new("file does not exist")
    } else {
        ToolFailure::new("file could not be read")
    }
}

/// Returns a CBOR map's fields when every key is text.
fn map_fields(value: &CborValue) -> Option<&[(CborValue, CborValue)]> {
    match value {
        CborValue::Map(fields) => Some(fields),
        _ => None,
    }
}

/// Rejects keys outside the strict request schema.
fn reject_unknown_fields(
    fields: &[(CborValue, CborValue)],
    allowed: &[&str],
) -> Result<(), ToolFailure> {
    if fields
        .iter()
        .all(|(key, _)| matches!(key, CborValue::Text(key) if allowed.contains(&key.as_str())))
    {
        Ok(())
    } else {
        Err(ToolFailure::new("request contains an unknown field"))
    }
}

/// Extracts one required text field.
fn required_text<'a>(
    fields: &'a [(CborValue, CborValue)],
    name: &str,
) -> Result<&'a str, ToolFailure> {
    fields
        .iter()
        .find_map(|(key, value)| {
            matches!(key, CborValue::Text(key) if key == name).then_some(value)
        })
        .and_then(|value| match value {
            CborValue::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .ok_or_else(|| ToolFailure::new(format!("{name} must be a string")))
}

/// Extracts one required array field.
fn required_array<'a>(
    fields: &'a [(CborValue, CborValue)],
    name: &str,
) -> Result<&'a [CborValue], ToolFailure> {
    fields
        .iter()
        .find_map(|(key, value)| {
            matches!(key, CborValue::Text(key) if key == name).then_some(value)
        })
        .and_then(|value| match value {
            CborValue::Array(values) => Some(values.as_slice()),
            _ => None,
        })
        .ok_or_else(|| ToolFailure::new(format!("{name} must be an array")))
}

#[cfg(test)]
mod tests;
