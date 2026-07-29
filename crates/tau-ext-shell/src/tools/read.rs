//! `read` tool: read a file (optionally a line slice).

use std::path::{Path, PathBuf};

use tau_proto::{CborValue, nearest_name_suggestion};

use crate::argument::{argument_text, optional_argument_int_strict};
use crate::display::{ToolFailure, ToolOutput, ok_display, text_stats};
use crate::tools::world::{MAX_SAFE_FILE_READ_BYTES, ShellWorld};
use crate::truncate::truncate_line_oriented;

const MAX_READ_RANGES_PER_CALL: usize = 100;
const MAX_READ_FILE_BYTES: usize = MAX_SAFE_FILE_READ_BYTES;
const MAX_READ_RANGE_RENDERED_BYTES: usize = 2 * 1024 * 1024;
const MAX_PATH_SUGGESTION_SIBLINGS: usize = 64;

pub(crate) fn read_file(
    arguments: &CborValue,
    world: &mut ShellWorld,
) -> Result<ToolOutput, ToolFailure> {
    let path = argument_text(arguments, "path").map_err(ToolFailure::from)?;
    let request = parse_read_request(arguments)?;
    let path_buf = PathBuf::from(&path);
    let display_path = path_buf.display().to_string();
    let display_args = format!("{} {}", display_path, request.display_ranges.join(","));

    let bytes = world
        .read_file_limited(&path_buf, MAX_READ_FILE_BYTES)
        .map_err(|error| {
            let mut message = error.to_string();
            if error.kind() == std::io::ErrorKind::NotFound
                && let Some(suggestion) = near_sibling_path_suggestion(&path_buf, world)
            {
                // Preserve this behavior; the structural alternative is not semantics-neutral
                // here. ast-grep-ignore: push-str-format
                message.push_str(&format!("; did you mean `{suggestion}`?"));
            }
            ToolFailure::from(message).with_args(display_args.clone())
        })?;
    let file_bytes = bytes.len();
    validate_range_render_budget(&bytes, &request.ranges, &display_args)?;
    let sliced = slice_line_ranges(&bytes, &request.ranges);
    validate_ranges_with_total(&request.ranges, sliced.total_lines, &display_args)?;
    let total_lines = sliced.total_lines;
    let truncated = truncate_line_oriented(&sliced.content);
    let content_value = CborValue::Text(truncated.content.clone());
    let mut entries = vec![(
        CborValue::Text("line-numbered content".to_owned()),
        content_value,
    )];
    if !sliced.valid_utf8 {
        entries.push((
            CborValue::Text("valid_utf8".to_owned()),
            CborValue::Bool(false),
        ));
    }
    if truncated.was_truncated || total_lines == 0 {
        if truncated.was_truncated {
            entries.push((
                CborValue::Text("truncated".to_owned()),
                CborValue::Bool(true),
            ));
        }
        entries.push((
            CborValue::Text("total_lines".to_owned()),
            CborValue::Integer((total_lines as i64).into()),
        ));
        entries.push((
            CborValue::Text("total_bytes".to_owned()),
            CborValue::Integer((file_bytes as i64).into()),
        ));
    }
    if truncated.was_truncated {
        crate::shell_output_spool::append_metadata(&mut entries, &sliced.content);
    }
    let mut display = ok_display(display_args);
    display.stats = text_stats(&truncated.content);
    Ok(ToolOutput {
        result: CborValue::Map(entries),
        provider_content: Vec::new(),
        display,
    })
}

fn near_sibling_path_suggestion(path: &Path, world: &mut ShellWorld) -> Option<String> {
    let requested_name = path.file_name()?.to_str()?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())?;
    let entries = world
        .read_dir_limited(parent, MAX_PATH_SUGGESTION_SIBLINGS + 1)
        .ok()?;
    if MAX_PATH_SUGGESTION_SIBLINGS < entries.len() {
        return None;
    }
    let names = entries
        .iter()
        .filter_map(|entry| std::str::from_utf8(entry.name.as_slice()).ok());
    let suggestion = nearest_name_suggestion(requested_name, names)?;
    Some(parent.join(suggestion).display().to_string())
}

/// One 1-based line range requested from a file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReadLineRange {
    /// 1-based inclusive first line to include.
    pub(crate) start_line: usize,
    /// 1-based inclusive final line to include, or `None` for the rest of the
    /// file.
    pub(crate) end_line: Option<usize>,
}

impl ReadLineRange {
    fn contains_line(&self, line: usize) -> bool {
        if line < self.start_line {
            return false;
        }
        match self.end_line {
            Some(end_line) => line <= end_line,
            None => true,
        }
    }
}

#[derive(Debug)]

struct ReadRequest {
    ranges: Vec<ReadLineRange>,
    display_ranges: Vec<String>,
}

pub(crate) struct ReadSlice {
    pub(crate) content: String,
    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) line_count: usize,
    pub(crate) valid_utf8: bool,
    /// Total lines in the source. For [`slice_line_ranges`] this is computed
    /// by scanning the whole file after collecting requested ranges.
    pub(crate) total_lines: usize,
}

fn validate_range_render_budget(
    input: &[u8],
    ranges: &[ReadLineRange],
    display_args: &str,
) -> Result<(), ToolFailure> {
    if ranges.len() <= 1 {
        return Ok(());
    }
    let estimated = estimate_rendered_range_bytes(input, ranges);
    if MAX_READ_RANGE_RENDERED_BYTES < estimated {
        return Err(ToolFailure::new(
            "read ranges expand to too much rendered content; request fewer or smaller ranges",
        )
        .with_args(display_args.to_owned()));
    }
    Ok(())
}

fn estimate_rendered_range_bytes(input: &[u8], ranges: &[ReadLineRange]) -> usize {
    let mut total = ranges.len().saturating_sub(1).saturating_mul(2);
    let mut line_start = 0usize;
    let mut index = 0usize;
    let mut line_number = 0usize;
    while index < input.len() {
        match input[index] {
            b'\r' => {
                let is_crlf = index + 1 < input.len() && input[index + 1] == b'\n';
                line_number += 1;
                total = total.saturating_add(estimate_rendered_line_bytes(
                    line_number,
                    &input[line_start..index],
                    ranges,
                ));
                index += if is_crlf { 2 } else { 1 };
                line_start = index;
            }
            b'\n' => {
                line_number += 1;
                total = total.saturating_add(estimate_rendered_line_bytes(
                    line_number,
                    &input[line_start..index],
                    ranges,
                ));
                index += 1;
                line_start = index;
            }
            _ => index += 1,
        }
    }
    if line_start < input.len() {
        line_number += 1;
        total = total.saturating_add(estimate_rendered_line_bytes(
            line_number,
            &input[line_start..],
            ranges,
        ));
    }
    total
}

fn estimate_rendered_line_bytes(
    line_number: usize,
    line: &[u8],
    ranges: &[ReadLineRange],
) -> usize {
    let memberships = ranges
        .iter()
        .filter(|range| range.contains_line(line_number))
        .count();
    if memberships == 0 {
        return 0;
    }
    let line_digits = line_number.ilog10() as usize + 1;
    let marker_budget = 24usize;
    let lossy_content_budget = line.len().saturating_mul(3);
    let rendered_line_budget = line_digits
        .saturating_add(marker_budget)
        .saturating_add(1)
        .saturating_add(lossy_content_budget)
        .saturating_add(1);
    memberships.saturating_mul(rendered_line_budget)
}

/// Render line ranges from already-loaded file bytes using `read` output rules.
pub(crate) fn slice_line_ranges(input: &[u8], ranges: &[ReadLineRange]) -> ReadSlice {
    let mut state = SliceState::new(ranges);

    let mut line_start = 0usize;
    let mut index = 0usize;
    while index < input.len() {
        match input[index] {
            b'\r' => {
                let is_crlf = index + 1 < input.len() && input[index + 1] == b'\n';
                let ending = if is_crlf {
                    LineEndingKind::Crlf
                } else {
                    LineEndingKind::Cr
                };
                state.push_line(&input[line_start..index], Some(ending));
                index += if is_crlf { 2 } else { 1 };
                line_start = index;
            }
            b'\n' => {
                state.push_line(&input[line_start..index], Some(LineEndingKind::Lf));
                index += 1;
                line_start = index;
            }
            _ => index += 1,
        }
    }

    if line_start < input.len() {
        state.push_line(&input[line_start..], None);
    }

    state.finish()
}

struct SliceState {
    ranges: Vec<ReadLineRange>,
    chunks: Vec<Vec<ReadLine>>,
    total_lines: usize,
    valid_utf8: bool,
}

struct ReadLine {
    number: usize,
    content: String,
    invalid_utf8: bool,
    ending: Option<LineEndingKind>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LineEndingKind {
    Lf,
    Crlf,
    Cr,
}

impl SliceState {
    fn new(ranges: &[ReadLineRange]) -> Self {
        Self {
            ranges: ranges.to_vec(),
            chunks: ranges.iter().map(|_| Vec::new()).collect(),
            total_lines: 0,
            valid_utf8: true,
        }
    }

    fn push_line(&mut self, line: &[u8], ending: Option<LineEndingKind>) {
        self.total_lines += 1;
        let valid_line = std::str::from_utf8(line).ok();
        if valid_line.is_none() {
            self.valid_utf8 = false;
        }
        if !self
            .ranges
            .iter()
            .any(|range| range.contains_line(self.total_lines))
        {
            return;
        }
        let content = valid_line.map_or_else(
            || String::from_utf8_lossy(line).into_owned(),
            ToOwned::to_owned,
        );
        for (range, chunk) in self.ranges.iter().zip(self.chunks.iter_mut()) {
            if range.contains_line(self.total_lines) {
                chunk.push(ReadLine {
                    number: self.total_lines,
                    content: content.clone(),
                    invalid_utf8: valid_line.is_none(),
                    ending,
                });
            }
        }
    }

    fn finish(self) -> ReadSlice {
        let rendered_chunks = self
            .chunks
            .iter()
            .map(|lines| {
                lines
                    .iter()
                    .map(render_read_line)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect::<Vec<_>>();
        let line_count = self.chunks.iter().map(Vec::len).sum();
        ReadSlice {
            content: rendered_chunks.join("\n\n"),
            line_count,
            valid_utf8: self.valid_utf8,
            total_lines: self.total_lines,
        }
    }
}

fn render_read_line(line: &ReadLine) -> String {
    let mut markers = Vec::new();
    if line.invalid_utf8 {
        markers.push("invalid-utf8");
    }
    match line.ending {
        Some(LineEndingKind::Lf) => {}
        Some(LineEndingKind::Crlf) => markers.push("crlf"),
        Some(LineEndingKind::Cr) => markers.push("cr"),
        None => markers.push("no_nl"),
    }

    let marker = if markers.is_empty() {
        String::new()
    } else {
        format!("({})", markers.join(","))
    };
    format!("{}{marker} {}", line.number, line.content)
}

fn parse_read_request(arguments: &CborValue) -> Result<ReadRequest, ToolFailure> {
    reject_legacy_line_count(arguments)?;
    if let Some(ranges) = optional_argument_array(arguments, "ranges")? {
        return parse_read_ranges(arguments, ranges);
    }

    let start_line_arg =
        optional_argument_int_strict(arguments, "start_line").map_err(ToolFailure::new)?;
    let end_line_arg =
        optional_argument_int_strict(arguments, "end_line").map_err(ToolFailure::new)?;
    let start_line = parse_read_start_line(start_line_arg)?;
    let end_line = parse_read_end_line(end_line_arg, start_line)?;
    Ok(ReadRequest {
        ranges: vec![ReadLineRange {
            start_line,
            end_line,
        }],
        display_ranges: vec![format_read_range(
            start_line_arg.map(|_| start_line),
            end_line,
        )],
    })
}

fn parse_read_ranges(
    arguments: &CborValue,
    ranges: &[CborValue],
) -> Result<ReadRequest, ToolFailure> {
    if has_argument(arguments, "start_line") || has_argument(arguments, "end_line") {
        return Err(ToolFailure::new(
            "ranges cannot be combined with start_line or end_line",
        ));
    }
    if ranges.is_empty() {
        return Err(ToolFailure::new("ranges array must not be empty"));
    }
    if MAX_READ_RANGES_PER_CALL < ranges.len() {
        return Err(ToolFailure::new(format!(
            "requested range count exceeds limit of {MAX_READ_RANGES_PER_CALL}"
        )));
    }

    let mut parsed = Vec::new();
    let mut display_ranges = Vec::new();
    for range in ranges {
        reject_legacy_line_count(range)?;
        let start_line = parse_required_range_line(range, "start_line")?;
        let end_line = parse_required_range_line(range, "end_line")?;
        validate_read_end_line(start_line, end_line)?;
        parsed.push(ReadLineRange {
            start_line,
            end_line: Some(end_line),
        });
        display_ranges.push(format_read_range(Some(start_line), Some(end_line)));
    }
    Ok(ReadRequest {
        ranges: parsed,
        display_ranges,
    })
}

fn optional_argument_array<'a>(
    arguments: &'a CborValue,
    key: &str,
) -> Result<Option<&'a [CborValue]>, ToolFailure> {
    let CborValue::Map(entries) = arguments else {
        return Ok(None);
    };
    for (entry_key, value) in entries {
        if let CborValue::Text(entry_key) = entry_key
            && entry_key == key
        {
            return match value {
                CborValue::Array(array) => Ok(Some(array)),
                _ => Err(ToolFailure::new(format!(
                    "argument `{key}` must be an array"
                ))),
            };
        }
    }
    Ok(None)
}

fn has_argument(arguments: &CborValue, key: &str) -> bool {
    let CborValue::Map(entries) = arguments else {
        return false;
    };
    entries
        .iter()
        .any(|(entry_key, _)| matches!(entry_key, CborValue::Text(entry_key) if entry_key == key))
}

fn parse_required_range_line(range: &CborValue, key: &str) -> Result<usize, ToolFailure> {
    match optional_argument_int_strict(range, key).map_err(ToolFailure::new)? {
        Some(value) if value < 1 => Err(ToolFailure::new(format!("{key} must be >= 1"))),
        Some(value) => {
            // Preserve this behavior; the structural alternative is not semantics-neutral
            // here. ast-grep-ignore: silent-map-err
            usize::try_from(value).map_err(|_| ToolFailure::new(format!("{key} is too large")))
        }
        None => Err(ToolFailure::new(format!(
            "each range must have an integer {key}"
        ))),
    }
}

fn validate_ranges_with_total(
    ranges: &[ReadLineRange],
    total_lines: usize,
    display_args: &str,
) -> Result<(), ToolFailure> {
    let max_read_start_line = total_lines.max(1);
    for range in ranges {
        if max_read_start_line < range.start_line {
            return Err(ToolFailure::new(format!(
                "start_line {} is past end of file (total_lines: {total_lines})",
                range.start_line
            ))
            .with_args(display_args.to_owned()));
        }
    }
    Ok(())
}

fn parse_read_start_line(value: Option<i64>) -> Result<usize, ToolFailure> {
    match value {
        None => Ok(1),
        Some(value) if value < 1 => Err(ToolFailure::new("start_line must be >= 1")),
        Some(value) => {
            // Preserve this behavior; the structural alternative is not semantics-neutral
            // here. ast-grep-ignore: silent-map-err
            usize::try_from(value).map_err(|_| ToolFailure::new("start_line is too large"))
        }
    }
}

fn parse_read_end_line(
    value: Option<i64>,
    start_line: usize,
) -> Result<Option<usize>, ToolFailure> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value < 1 {
        return Err(ToolFailure::new("end_line must be >= 1"));
    }
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: silent-map-err
    let end_line = usize::try_from(value).map_err(|_| ToolFailure::new("end_line is too large"))?;
    validate_read_end_line(start_line, end_line)?;
    Ok(Some(end_line))
}

fn validate_read_end_line(start_line: usize, end_line: usize) -> Result<(), ToolFailure> {
    if end_line < start_line {
        return Err(ToolFailure::new("end_line must be >= start_line"));
    }
    Ok(())
}

fn reject_legacy_line_count(arguments: &CborValue) -> Result<(), ToolFailure> {
    if has_argument(arguments, "line_count") {
        return Err(ToolFailure::new(
            "line_count is no longer supported; use end_line",
        ));
    }
    Ok(())
}

pub(crate) fn format_read_range(start_line: Option<usize>, end_line: Option<usize>) -> String {
    match (start_line, end_line) {
        (None, None) => "..".to_owned(),
        (Some(start), None) => format!("{start}.."),
        (None, Some(end)) => format!("1..{end}"),
        (Some(start), Some(end)) => format!("{start}..{end}"),
    }
}

/// In-memory equivalent of single-range file slicing, retained for tests
/// that exercise the slicing logic on a string rather than a file.
#[cfg(test)]
pub(crate) fn slice_lines(input: &str, start_line: usize, end_line: Option<usize>) -> ReadSlice {
    let all_lines: Vec<&str> = input.lines().collect();
    let total_lines = all_lines.len();
    let start_idx = start_line.saturating_sub(1).min(total_lines);
    let end_idx = match end_line {
        Some(end_line) => end_line.min(total_lines),
        None => total_lines,
    };
    let end_idx = if end_idx < start_idx {
        start_idx
    } else {
        end_idx
    };
    ReadSlice {
        content: all_lines[start_idx..end_idx]
            .iter()
            .enumerate()
            .map(|(index, line)| format!("{} {line}", start_idx + index + 1))
            .collect::<Vec<_>>()
            .join("\n"),
        line_count: end_idx.saturating_sub(start_idx),
        valid_utf8: true,
        total_lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: Vec<(&str, CborValue)>) -> CborValue {
        CborValue::Map(
            entries
                .into_iter()
                .map(|(key, value)| (CborValue::Text(key.to_owned()), value))
                .collect(),
        )
    }

    /// Ensures optional read line arguments reject wrong CBOR types instead of
    /// silently falling back to the default range.
    #[test]
    fn read_rejects_wrong_type_optional_line_arguments() {
        let err = parse_read_request(&map(vec![("start_line", CborValue::Text("2".to_owned()))]))
            .expect_err("string start_line should be rejected");

        assert_eq!(err.message, "argument `start_line` must be an integer");
    }

    /// Ensures range entries reject wrong CBOR line types instead of reporting
    /// them as missing integer fields.
    #[test]
    fn read_ranges_reject_wrong_type_line_arguments() {
        let err = parse_read_request(&map(vec![(
            "ranges",
            CborValue::Array(vec![map(vec![
                ("start_line", CborValue::Text("1".to_owned())),
                ("end_line", CborValue::Integer(2.into())),
            ])]),
        )]))
        .expect_err("string range start_line should be rejected");

        assert_eq!(err.message, "argument `start_line` must be an integer");
    }
    /// Ensures the read tool refuses inputs above its safety cap before loading
    /// the whole file into memory.
    #[test]
    fn read_rejects_files_over_input_cap() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("huge.txt");
        std::fs::write(&path, vec![b'x'; MAX_READ_FILE_BYTES + 1]).expect("write huge file");
        let mut world = ShellWorld::real();

        let err = read_file(
            &map(vec![("path", CborValue::Text(path.display().to_string()))]),
            &mut world,
        )
        .expect_err("huge file should be rejected");

        assert!(
            err.message.contains("file is too large to read safely"),
            "unexpected error: {}",
            err.message
        );
    }

    /// Ensures missing-path diagnostics give a nearby sibling only when the
    /// sibling scan stays within the hard bound, preventing expensive or
    /// nondeterministic directory-wide suggestions.
    #[test]
    fn read_missing_path_suggestion_is_bounded() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("target_file.rs"), "content").expect("write target");
        let mut world = ShellWorld::real();

        let err = read_file(
            &map(vec![(
                "path",
                CborValue::Text(temp.path().join("target_fiel.rs").display().to_string()),
            )]),
            &mut world,
        )
        .expect_err("misspelled path should fail");

        assert!(
            err.message.contains("did you mean") && err.message.contains("target_file.rs"),
            "unexpected error: {}",
            err.message
        );

        for idx in 0..=MAX_PATH_SUGGESTION_SIBLINGS {
            std::fs::write(temp.path().join(format!("sibling-{idx}.txt")), "x")
                .expect("write sibling");
        }
        let mut world = ShellWorld::real();
        let err = read_file(
            &map(vec![(
                "path",
                CborValue::Text(temp.path().join("target_fiel.rs").display().to_string()),
            )]),
            &mut world,
        )
        .expect_err("misspelled path should still fail");

        assert!(
            !err.message.contains("did you mean"),
            "suggestion should be suppressed past sibling bound: {}",
            err.message
        );
    }

    /// Ensures overlapping multi-range reads cannot expand a modest input into
    /// very large intermediate rendered strings before normal output
    /// truncation.
    #[test]
    fn read_rejects_multi_range_render_expansion_over_cap() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("wide.txt");
        std::fs::write(&path, "x".repeat(32 * 1024)).expect("write wide file");
        let ranges = (0..100)
            .map(|_| {
                map(vec![
                    ("start_line", CborValue::Integer(1.into())),
                    ("end_line", CborValue::Integer(1.into())),
                ])
            })
            .collect::<Vec<_>>();
        let mut world = ShellWorld::real();

        let err = read_file(
            &map(vec![
                ("path", CborValue::Text(path.display().to_string())),
                ("ranges", CborValue::Array(ranges)),
            ]),
            &mut world,
        )
        .expect_err("range expansion should be rejected");

        assert!(
            err.message
                .contains("read ranges expand to too much rendered content"),
            "unexpected error: {}",
            err.message
        );
    }
}
