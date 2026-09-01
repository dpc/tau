//! `ls` tool: directory listing with truncation.

use std::path::PathBuf;

use tau_proto::{CborValue, ToolUseStats};

use crate::argument::{optional_argument_int_strict, optional_argument_text};
use crate::display::{ToolFailure, ToolOutput, ok_display, text_stats};
use crate::tools::world::ShellWorld;
use crate::truncate::{MAX_OUTPUT_LINES, truncate_line_oriented_lines};

pub(crate) const DEFAULT_LS_LIMIT: usize = 500;
const MAX_LS_LIMIT: usize = MAX_OUTPUT_LINES + 1;

pub(crate) fn run_ls(
    arguments: &CborValue,
    world: &mut ShellWorld,
) -> Result<ToolOutput, ToolFailure> {
    let path = optional_argument_text(arguments, "path")
        .map_err(ToolFailure::from)?
        .unwrap_or_else(|| ".".to_owned());
    let limit = parse_limit(arguments)?;
    let dir_path = PathBuf::from(&path);
    let display_args = dir_path.display().to_string();
    let with_args = |f: ToolFailure| f.with_args(display_args.clone());

    if !world.is_dir(&dir_path).map_err(|e| {
        with_args(ToolFailure::from(format!(
            "failed to access {}: {e}",
            dir_path.display()
        )))
    })? {
        return Err(with_args(ToolFailure::from(format!(
            "not a directory: {}",
            dir_path.display()
        ))));
    }

    let collection_cap = limit.saturating_add(1);
    let mut entries = Vec::new();
    for entry in world
        .read_dir_limited(&dir_path, collection_cap)
        .map_err(|e| {
            with_args(ToolFailure::from(format!(
                "failed to read {}: {e}",
                dir_path.display()
            )))
        })?
    {
        entries.push(render_entry_name(&entry.name, entry.is_dir));
    }

    Ok(render_listing(entries, limit, display_args))
}

fn render_listing(mut entries: Vec<LsEntry>, limit: usize, display_args: String) -> ToolOutput {
    entries.sort_by_key(|entry| entry.sort_key());

    if entries.is_empty() {
        let mut display = ok_display(display_args.clone());
        display.stats = ToolUseStats {
            matches: None,
            lines: Some(0),
            bytes: Some(0),
        };
        return ToolOutput {
            result: CborValue::Map(vec![
                (
                    CborValue::Text("entries".to_owned()),
                    CborValue::Integer(0.into()),
                ),
                (
                    CborValue::Text("output".to_owned()),
                    CborValue::Text(String::new()),
                ),
            ]),
            provider_content: Vec::new(),
            display,
        };
    }
    let observed_entries = entries.len();
    let rendered_lines = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| entry.render_line(index + 1))
        .collect::<Vec<_>>();
    let limited_lines = rendered_lines
        .iter()
        .take(limit)
        .map(String::as_str)
        .collect::<Vec<_>>();
    let displayed_line_count = limited_lines.len();
    let displayed_bytes = line_oriented_len(limited_lines.iter().copied());
    let full_output_text = limited_lines.join("\n");
    let limit_reached = displayed_line_count < observed_entries;
    let truncated =
        truncate_line_oriented_lines(limited_lines, displayed_line_count, displayed_bytes);
    let output_text = truncated.content;
    let was_truncated = limit_reached || truncated.was_truncated;

    let mut display = ok_display(display_args.clone());
    display.stats = text_stats(&output_text);
    let mut result_entries = vec![
        (
            CborValue::Text("entries".to_owned()),
            CborValue::Integer((displayed_line_count as i64).into()),
        ),
        (
            CborValue::Text("output".to_owned()),
            CborValue::Text(output_text),
        ),
    ];
    if was_truncated {
        result_entries.push((
            CborValue::Text("truncated".to_owned()),
            CborValue::Bool(true),
        ));
        if limit_reached {
            result_entries.push((
                CborValue::Text("limit_reached".to_owned()),
                CborValue::Bool(true),
            ));
        }
        if truncated.was_truncated {
            result_entries.push((
                CborValue::Text("total_lines".to_owned()),
                CborValue::Integer((displayed_line_count as i64).into()),
            ));
            result_entries.push((
                CborValue::Text("total_bytes".to_owned()),
                CborValue::Integer((displayed_bytes as i64).into()),
            ));
        }
        if truncated.was_truncated {
            crate::shell_output_spool::append_metadata(&mut result_entries, &full_output_text);
        }
    }
    ToolOutput {
        result: CborValue::Map(result_entries),
        provider_content: Vec::new(),
        display,
    }
}

fn parse_limit(arguments: &CborValue) -> Result<usize, ToolFailure> {
    match optional_argument_int_strict(arguments, "limit").map_err(ToolFailure::from)? {
        None => Ok(DEFAULT_LS_LIMIT),
        Some(value) if value < 1 => Err(ToolFailure::new("limit must be >= 1")),
        Some(value) => {
            let limit =
                usize::try_from(value).map_err(|_| ToolFailure::new("limit is too large"))?;
            if MAX_LS_LIMIT < limit {
                return Err(ToolFailure::new(format!("limit must be <= {MAX_LS_LIMIT}")));
            }
            Ok(limit)
        }
    }
}

#[derive(Clone, Debug)]
struct LsEntry {
    name: String,
    flags: Vec<&'static str>,
}

impl LsEntry {
    fn sort_key(&self) -> String {
        self.name.to_lowercase()
    }

    fn render_line(&self, line_number: usize) -> String {
        let flags = if self.flags.is_empty() {
            String::new()
        } else {
            format!("({})", self.flags.join(","))
        };
        format!("{line_number}{flags} {}", self.name)
    }
}

fn line_oriented_len<'a>(lines: impl IntoIterator<Item = &'a str>) -> usize {
    let mut count = 0usize;
    let mut bytes = 0usize;
    for line in lines {
        count += 1;
        bytes += line.len();
    }
    bytes + count.saturating_sub(1)
}

fn render_entry_name(name: &tau_vcr::EscapedBytes, is_dir: bool) -> LsEntry {
    render_entry_bytes(name.as_slice(), is_dir)
}

fn render_entry_bytes(bytes: &[u8], is_dir: bool) -> LsEntry {
    match std::str::from_utf8(bytes) {
        Ok(text) => render_entry_text(text, is_dir, false),
        Err(_) => render_entry_text(&String::from_utf8_lossy(bytes), is_dir, true),
    }
}

fn render_entry_text(text: &str, is_dir: bool, invalid_utf8: bool) -> LsEntry {
    let mut name = String::new();
    let mut escaped = false;
    for ch in text.chars() {
        match ch {
            '\\' => {
                name.push_str("\\\\");
                escaped = true;
            }
            '\n' => {
                name.push_str("\\n");
                escaped = true;
            }
            '\r' => {
                name.push_str("\\r");
                escaped = true;
            }
            '\t' => {
                name.push_str("\\t");
                escaped = true;
            }
            ch if ch.is_control() => {
                name.extend(ch.escape_default());
                escaped = true;
            }
            ch => name.push(ch),
        }
    }
    if is_dir {
        name.push('/');
    }
    let mut flags = Vec::new();
    if invalid_utf8 {
        flags.push("invalid-utf8");
    }
    if escaped {
        flags.push("escaped");
    }
    LsEntry { name, flags }
}

#[cfg(test)]
mod tests;
