//! `edit` tool: line-oriented replacements on a file.

use std::path::{Path, PathBuf};

use tau_proto::{CborValue, ToolUsePayload, ToolUseState, ToolUseStatus};

use crate::argument::{argument_array, argument_text, cbor_map_int, cbor_map_text};
use crate::diff::compute_diff;
use crate::display::{ToolFailure, ToolOutput, text_stats};
use crate::tools::read::{ReadLineRange, slice_line_ranges};
use crate::tools::world::{MAX_SAFE_FILE_READ_BYTES, ShellWorld};
use crate::truncate::truncate_line_oriented;

const MAX_EDITS_PER_CALL: usize = 100;
const CONTEXT_LINE_MISMATCH_CONTEXT_LINES: usize = 10;

pub(crate) fn edit_file(
    arguments: &CborValue,
    world: &mut ShellWorld,
) -> Result<ToolOutput, ToolFailure> {
    let path = argument_text(arguments, "path").map_err(ToolFailure::from)?;
    let path_buf = PathBuf::from(&path);
    let display_path = path_buf.display().to_string();
    let mut display_args = display_path.clone();

    let edits = argument_array(arguments, "edits")
        .map_err(|error| with_display_args(&display_args, ToolFailure::from(error)))?;
    validate_edit_count(edits.len(), &display_args)?;

    let (original_bytes, original_missing) =
        read_original_or_empty(&path_buf, &display_args, world)?;
    let original_lines = LineIndex::new(&original_bytes);

    let mut replacements = collect_line_replacements(
        edits,
        &display_path,
        &mut display_args,
        &original_bytes,
        &original_lines,
    )?;

    validate_non_overlapping(&replacements, &display_args)?;
    validate_context_lines(
        &replacements,
        &original_bytes,
        &original_lines,
        &display_args,
    )?;

    let result = apply_line_replacements(&original_bytes, &mut replacements);

    let changed = original_missing || result != original_bytes;
    if changed {
        create_missing_parent_dirs(&path_buf, &display_args, world)?;
        world.write_file(&path_buf, &result).map_err(|error| {
            with_display_args(&display_args, ToolFailure::from(error.to_string()))
        })?;
    }

    let display = ToolUseState {
        args: display_args.clone(),
        status: ToolUseStatus::Success,
        status_text: "ok".to_owned(),
        payload: edit_display_payload(&original_bytes, &result, changed),
        ..Default::default()
    };
    let result_lines = LineIndex::new(&result);
    Ok(ToolOutput {
        result: edit_result_value(
            replacements.len(),
            changed,
            result_lines.max_valid_start_line(),
            result.len(),
        ),
        provider_content: Vec::new(),
        display,
    })
}

fn validate_edit_count(edits: usize, display_args: &str) -> Result<(), ToolFailure> {
    if edits == 0 {
        return Err(with_display_args(
            display_args,
            ToolFailure::new("edits array must not be empty"),
        ));
    }
    if MAX_EDITS_PER_CALL < edits {
        return Err(with_display_args(
            display_args,
            ToolFailure::new(format!(
                "requested edit count exceeds limit of {MAX_EDITS_PER_CALL}"
            )),
        ));
    }
    Ok(())
}

fn collect_line_replacements<'a>(
    edits: &'a [CborValue],
    display_path: &str,
    display_args: &mut String,
    original_bytes: &[u8],
    original_lines: &LineIndex,
) -> Result<Vec<LineReplacement<'a>>, ToolFailure> {
    let mut replacements = Vec::new();
    let mut requested_ranges = Vec::new();
    for edit in edits {
        reject_legacy_line_count(edit, display_args)?;
        let range = parse_edit_range(edit, original_lines, display_args)?;
        let new_text = cbor_map_text(edit, "newText").ok_or_else(|| {
            with_display_args(
                display_args,
                ToolFailure::new("each edit must have a string newText"),
            )
        })?;
        requested_ranges.push(range.display.clone());
        *display_args = edit_display_args(display_path, &requested_ranges);
        let context_line = parse_required_context_line(edit, display_args)?;
        let start_byte = original_lines.byte_start_for_line(range.start_line, original_bytes.len());
        let end_byte =
            original_lines.byte_start_for_line(range.end_line_exclusive, original_bytes.len());
        let mut new_text = new_text.as_bytes().to_vec();
        normalize_new_text_line_ending(
            &mut new_text,
            original_bytes,
            original_lines,
            &range,
            start_byte,
        );
        replacements.push(LineReplacement {
            start_line: range.start_line,
            end_line_exclusive: range.end_line_exclusive,
            start_byte,
            end_byte,
            new_text,
            context_line,
        });
    }
    Ok(replacements)
}

fn apply_line_replacements(
    original_bytes: &[u8],
    replacements: &mut [LineReplacement<'_>],
) -> Vec<u8> {
    let mut result = original_bytes.to_vec();
    replacements.sort_by_key(|replacement| std::cmp::Reverse(replacement.start_byte));
    for replacement in replacements {
        result.splice(
            replacement.start_byte..replacement.end_byte,
            replacement.new_text.iter().copied(),
        );
    }
    result
}

fn edit_display_payload(
    original_bytes: &[u8],
    result: &[u8],
    changed: bool,
) -> Option<ToolUsePayload> {
    if !changed {
        return None;
    }
    match (
        std::str::from_utf8(original_bytes),
        std::str::from_utf8(result),
    ) {
        (Ok(original), Ok(result)) => Some(ToolUsePayload::Diff(compute_diff(original, result))),
        _ => Some(ToolUsePayload::Text {
            text: "[diff skipped: file is not valid UTF-8]".to_owned(),
        }),
    }
}

struct EditRange {
    start_line: usize,
    end_line_exclusive: usize,
    display: String,
}

impl EditRange {
    fn is_empty(&self) -> bool {
        self.start_line == self.end_line_exclusive
    }
}

struct LineReplacement<'a> {
    start_line: usize,
    end_line_exclusive: usize,
    start_byte: usize,
    end_byte: usize,
    new_text: Vec<u8>,
    context_line: &'a str,
}

fn normalize_new_text_line_ending(
    new_text: &mut Vec<u8>,
    original_bytes: &[u8],
    original_lines: &LineIndex,
    range: &EditRange,
    start_byte: usize,
) -> bool {
    if new_text.is_empty() {
        return false;
    }

    let mut changed = false;
    if range.is_empty()
        && 0 < start_byte
        && start_byte == original_bytes.len()
        && !original_lines.has_trailing_line_ending()
    {
        changed |= maybe_prepend_missing_boundary_line_ending(new_text);
    }

    if new_text.ends_with(b"\n") || new_text.ends_with(b"\r") {
        return changed;
    }

    let line_ending = if range.is_empty() {
        if range.start_line <= original_lines.total_lines() {
            original_lines
                .line_ending_for_line(range.start_line, original_bytes)
                .unwrap_or(b"\n")
        } else {
            b"\n"
        }
    } else {
        let line = range.end_line_exclusive.saturating_sub(1);
        let Some(line_ending) = original_lines.line_ending_for_line(line, original_bytes) else {
            return changed;
        };
        line_ending
    };
    new_text.extend_from_slice(line_ending);
    true
}

fn maybe_prepend_missing_boundary_line_ending(new_text: &mut Vec<u8>) -> bool {
    if new_text.starts_with(b"\n") || new_text.starts_with(b"\r") {
        return false;
    }
    new_text.splice(0..0, b"\n".iter().copied());
    true
}

struct LineIndex {
    spans: Vec<LineSpan>,
    has_trailing_line_ending: bool,
}

struct LineSpan {
    start: usize,
    content_end: usize,
}

impl LineIndex {
    fn new(input: &[u8]) -> Self {
        let mut spans = Vec::new();
        let mut line_start = 0usize;
        let mut index = 0usize;
        while index < input.len() {
            match input[index] {
                b'\r' => {
                    spans.push(LineSpan {
                        start: line_start,
                        content_end: index,
                    });
                    index += if index + 1 < input.len() && input[index + 1] == b'\n' {
                        2
                    } else {
                        1
                    };
                    line_start = index;
                }
                b'\n' => {
                    spans.push(LineSpan {
                        start: line_start,
                        content_end: index,
                    });
                    index += 1;
                    line_start = index;
                }
                _ => index += 1,
            }
        }

        let has_trailing_line_ending = !input.is_empty() && line_start == input.len();
        if line_start < input.len() {
            spans.push(LineSpan {
                start: line_start,
                content_end: input.len(),
            });
        }

        Self {
            spans,
            has_trailing_line_ending,
        }
    }

    fn line_ending_for_line<'a>(&self, line: usize, input: &'a [u8]) -> Option<&'a [u8]> {
        let span = self.spans.get(line.checked_sub(1)?)?;
        let next_start = self
            .spans
            .get(line)
            .map_or(input.len(), |next_span| next_span.start);
        if span.content_end == next_start {
            return None;
        }
        Some(&input[span.content_end..next_start])
    }

    fn has_trailing_line_ending(&self) -> bool {
        self.has_trailing_line_ending
    }

    fn total_lines(&self) -> usize {
        self.spans.len()
    }

    fn has_line(&self, line: usize) -> bool {
        line.checked_sub(1)
            .is_some_and(|index| self.spans.get(index).is_some())
    }

    fn max_valid_start_line(&self) -> usize {
        self.spans.len().saturating_add(1)
    }

    fn byte_start_for_line(&self, line: usize, eof: usize) -> usize {
        self.spans
            .get(line.saturating_sub(1))
            .map(|span| span.start)
            .unwrap_or(eof)
    }

    fn line_content_text<'a>(&self, line: usize, input: &'a [u8]) -> Option<&'a str> {
        let Some(span) = self.spans.get(line.saturating_sub(1)) else {
            return (line <= self.max_valid_start_line()).then_some("");
        };
        std::str::from_utf8(&input[span.start..span.content_end]).ok()
    }
}

fn validate_non_overlapping(
    replacements: &[LineReplacement<'_>],
    display_args: &str,
) -> Result<(), ToolFailure> {
    let mut ranges: Vec<_> = replacements.iter().collect();
    ranges.sort_by_key(|replacement| replacement.start_line);
    for pair in ranges.windows(2) {
        if pair[1].start_line == pair[0].start_line
            || pair[1].start_line < pair[0].end_line_exclusive
        {
            return Err(with_display_args(
                display_args,
                ToolFailure::new("overlapping edits"),
            ));
        }
    }
    Ok(())
}

fn validate_context_lines(
    replacements: &[LineReplacement<'_>],
    original_bytes: &[u8],
    original_lines: &LineIndex,
    display_args: &str,
) -> Result<(), ToolFailure> {
    for replacement in replacements {
        let context_line = replacement.context_line;
        let context_line_number = replacement.start_line;
        let actual_context_line =
            original_lines.line_content_text(context_line_number, original_bytes);
        if actual_context_line == Some(context_line) {
            continue;
        }
        let current_context_line_invalid_utf8 =
            original_lines.has_line(context_line_number) && actual_context_line.is_none();
        return Err(context_line_mismatch_failure(
            replacement,
            original_bytes,
            display_args,
            current_context_line_invalid_utf8,
        ));
    }
    Ok(())
}

fn context_line_mismatch_failure(
    replacement: &LineReplacement<'_>,
    original_bytes: &[u8],
    display_args: &str,
    current_context_line_invalid_utf8: bool,
) -> ToolFailure {
    let start_line = replacement.start_line;
    let context_line_number = start_line;
    let context_anchor_line = context_line_number.max(1);
    let context_start_line = context_anchor_line
        .saturating_sub(CONTEXT_LINE_MISMATCH_CONTEXT_LINES)
        .max(1);
    let context_end_line = context_anchor_line.saturating_add(CONTEXT_LINE_MISMATCH_CONTEXT_LINES);
    let ranges = vec![ReadLineRange {
        start_line: context_start_line,
        end_line: Some(context_end_line),
    }];
    let rendered = slice_line_ranges(original_bytes, &ranges);
    let truncated = truncate_line_oriented(&rendered.content);
    let mut details = vec![
        (
            CborValue::Text("line-numbered content".to_owned()),
            CborValue::Text(truncated.content.clone()),
        ),
        (
            CborValue::Text("context_line_number".to_owned()),
            CborValue::Integer((context_line_number as i64).into()),
        ),
    ];
    if !rendered.valid_utf8 {
        details.push((
            CborValue::Text("valid_utf8".to_owned()),
            CborValue::Bool(false),
        ));
    }
    if truncated.was_truncated {
        details.push((
            CborValue::Text("truncated".to_owned()),
            CborValue::Bool(true),
        ));
        crate::shell_output_spool::append_metadata(&mut details, &rendered.content);
    }
    if truncated.was_truncated || truncated.content.is_empty() {
        details.push((
            CborValue::Text("total_lines".to_owned()),
            CborValue::Integer((rendered.total_lines as i64).into()),
        ));
        details.push((
            CborValue::Text("total_bytes".to_owned()),
            CborValue::Integer((original_bytes.len() as i64).into()),
        ));
    }

    let message = if current_context_line_invalid_utf8 {
        format!(
            "context_line wrong - current line {context_line_number} is not valid UTF-8, so no context_line string can match it; see current content in the response"
        )
    } else if !LineIndex::new(original_bytes).has_line(context_line_number) {
        format!(
            "context_line wrong - must equal \"\" for missing line {context_line_number}, see current content in the response"
        )
    } else {
        format!(
            "context_line wrong - must equal current line {context_line_number}, see current content in the response"
        )
    };
    let mut failure = ToolFailure::new(message)
        .with_args(display_args.to_owned())
        .with_details(CborValue::Map(details));
    failure.display.stats = text_stats(&truncated.content);
    failure
}

fn read_original_or_empty(
    path: &Path,
    display_args: &str,
    world: &mut ShellWorld,
) -> Result<(Vec<u8>, bool), ToolFailure> {
    match world.read_file_limited(path, MAX_SAFE_FILE_READ_BYTES) {
        Ok(bytes) => Ok((bytes, false)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((Vec::new(), true)),
        Err(error) => Err(with_display_args(
            display_args,
            ToolFailure::from(error.to_string()),
        )),
    }
}

fn create_missing_parent_dirs(
    path: &Path,
    display_args: &str,
    world: &mut ShellWorld,
) -> Result<(), ToolFailure> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty()
        || world.path_exists(parent).map_err(|error| {
            with_display_args(display_args, ToolFailure::from(error.to_string()))
        })?
    {
        return Ok(());
    }
    world
        .create_dir_all(parent)
        .map_err(|error| with_display_args(display_args, ToolFailure::from(error.to_string())))
}

fn parse_edit_range(
    edit: &CborValue,
    original_lines: &LineIndex,
    display_args: &str,
) -> Result<EditRange, ToolFailure> {
    if has_field(edit, "after_line") || has_field(edit, "before_line") {
        return Err(with_display_args(
            display_args,
            ToolFailure::new(
                "after_line and before_line are no longer supported; use start_line and end_line_exclusive",
            ),
        ));
    }
    if has_field(edit, "end_line") {
        return Err(with_display_args(
            display_args,
            ToolFailure::new(
                "edit uses end_line_exclusive; to replace read output lines A through B, use start_line A and end_line_exclusive B+1",
            ),
        ));
    }

    if !has_field(edit, "start_line") || !has_field(edit, "end_line_exclusive") {
        return Err(with_display_args(
            display_args,
            ToolFailure::new("each edit must have integer start_line and end_line_exclusive"),
        ));
    }

    parse_half_open_edit_range(edit, original_lines, display_args)
}

fn parse_half_open_edit_range(
    edit: &CborValue,
    original_lines: &LineIndex,
    display_args: &str,
) -> Result<EditRange, ToolFailure> {
    let start_line = parse_required_line(edit, "start_line", display_args)?;
    let end_line_exclusive = parse_required_line(edit, "end_line_exclusive", display_args)?;
    if end_line_exclusive < start_line {
        return Err(with_display_args(
            display_args,
            ToolFailure::new("end_line_exclusive must be at least start_line"),
        ));
    }
    let max_valid_start_line = original_lines.total_lines().saturating_add(1);
    if max_valid_start_line < start_line {
        return Err(with_display_args(
            display_args,
            ToolFailure::new(format!(
                "start_line {start_line} is past end of file (max_valid_start_line: {max_valid_start_line})"
            )),
        ));
    }
    if max_valid_start_line < end_line_exclusive {
        return Err(with_display_args(
            display_args,
            ToolFailure::new(format!(
                "end_line_exclusive {end_line_exclusive} is past end of file (max_valid_start_line: {max_valid_start_line})"
            )),
        ));
    }
    Ok(EditRange {
        start_line,
        end_line_exclusive,
        display: format!("{start_line}..<{end_line_exclusive}"),
    })
}

fn has_field(value: &CborValue, field: &str) -> bool {
    let CborValue::Map(entries) = value else {
        return false;
    };
    entries
        .iter()
        .any(|(key, _)| matches!(key, CborValue::Text(key) if key == field))
}

fn reject_legacy_line_count(edit: &CborValue, display_args: &str) -> Result<(), ToolFailure> {
    let CborValue::Map(entries) = edit else {
        return Ok(());
    };
    if entries
        .iter()
        .any(|(key, _)| matches!(key, CborValue::Text(key) if key == "line_count"))
    {
        return Err(with_display_args(
            display_args,
            ToolFailure::new("line_count is no longer supported; use end_line_exclusive"),
        ));
    }
    Ok(())
}

fn parse_required_line(
    edit: &CborValue,
    key: &str,
    display_args: &str,
) -> Result<usize, ToolFailure> {
    match cbor_map_int(edit, key) {
        Some(n) if n < 1 => Err(with_display_args(
            display_args,
            ToolFailure::new(format!("{key} must be at least 1")),
        )),
        // Preserve behavior at this site.
        // ast-grep-ignore: silent-map-err
        Some(n) => usize::try_from(n).map_err(|_| {
            with_display_args(
                display_args,
                ToolFailure::new(format!("{key} is too large")),
            )
        }),
        None => Err(with_display_args(
            display_args,
            ToolFailure::new(format!("each edit must have an integer {key}")),
        )),
    }
}

fn parse_required_context_line<'a>(
    edit: &'a CborValue,
    display_args: &str,
) -> Result<&'a str, ToolFailure> {
    let CborValue::Map(entries) = edit else {
        return Err(with_display_args(
            display_args,
            ToolFailure::new("each edit must have a string context_line"),
        ));
    };
    for (key, value) in entries {
        if let CborValue::Text(key) = key
            && key == "context_line"
        {
            return match value {
                CborValue::Text(value) => {
                    let value = value.trim_end_matches(['\n', '\r']);
                    if value.contains('\n') || value.contains('\r') {
                        return Err(with_display_args(
                            display_args,
                            ToolFailure::new(
                                "context_line must not include embedded newline characters",
                            ),
                        ));
                    }
                    Ok(value)
                }
                _ => Err(with_display_args(
                    display_args,
                    ToolFailure::new("context_line must be a string"),
                )),
            };
        }
    }
    Err(with_display_args(
        display_args,
        ToolFailure::new("each edit must have a string context_line"),
    ))
}

fn with_display_args(args: &str, failure: ToolFailure) -> ToolFailure {
    failure.with_args(args.to_owned())
}

fn edit_display_args(path: &str, ranges: &[String]) -> String {
    if ranges.is_empty() {
        return path.to_owned();
    }

    let mut unique_ranges: Vec<&str> = Vec::new();
    ranges.iter().for_each(|range| {
        if unique_ranges
            .iter()
            .all(|existing| *existing != range.as_str())
        {
            unique_ranges.push(range.as_str());
        }
    });
    format!("{path} {}", unique_ranges.join(","))
}

fn edit_result_value(
    edits: usize,
    changed: bool,
    new_max_valid_start_line: usize,
    total_bytes: usize,
) -> CborValue {
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
            CborValue::Text("new_max_valid_start_line".to_owned()),
            CborValue::Integer((new_max_valid_start_line as i64).into()),
        ),
        (
            CborValue::Text("total_bytes".to_owned()),
            CborValue::Integer((total_bytes as i64).into()),
        ),
    ])
}
