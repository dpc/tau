//! `apply_patch` custom tool: parse Codex-style patch text and apply it.

use std::path::{Path, PathBuf};

use tau_proto::{CborValue, ToolUsePayload};

use crate::diff::compute_diff;
use crate::display::{ToolFailure, ToolOutput};
use crate::tools::find::escape_path_text;
use crate::tools::world::{MAX_SAFE_FILE_READ_BYTES, ShellWorld};

const SUMMARY_HEADER: &str = "Success. Updated the following files:";

#[expect(unused)]
pub(crate) const APPLY_PATCH_LARK_GRAMMAR: &str = include_str!("apply_patch.lark");

pub(crate) fn apply_patch(
    arguments: &CborValue,
    world: &mut ShellWorld,
) -> Result<ToolOutput, ToolFailure> {
    let patch = patch_text(arguments)?;

    let hunks = parse_patch(patch).map_err(ToolFailure::new)?;
    let changes = match apply_hunks(&hunks, world) {
        Ok(changes) => changes,
        Err(failure) => {
            let details = partial_changes_details(&failure.changes);
            let mut tool_failure = ToolFailure::new(failure.message)
                .with_payload(display_payload_for_failure(&failure.changes));
            if let Some(details) = details {
                tool_failure = tool_failure.with_details(details);
            }
            return Err(tool_failure);
        }
    };

    let summary = format_summary(&changes);
    let payload = display_payload_for_changes(&changes, &summary);
    let result = CborValue::Text(summary.clone());

    let mut display = crate::display::ok_display("apply_patch");
    display.payload = payload;
    Ok(ToolOutput { result, display })
}

pub(crate) fn lock_directories_in_dir(
    arguments: &CborValue,
    cwd: &Path,
) -> Result<Vec<PathBuf>, ToolFailure> {
    let patch = patch_text(arguments)?;
    let hunks = parse_patch(patch).map_err(ToolFailure::new)?;
    let mut dirs = Vec::new();

    for hunk in &hunks {
        match hunk {
            Hunk::Add { path, .. } => {
                let abs = resolve_path(cwd, path);
                dirs.push(crate::dir_lock::canonical_write_lock_dir(&abs)?);
            }
            Hunk::Delete { path } => {
                let abs = resolve_path(cwd, path);
                dirs.push(crate::dir_lock::canonical_path_parent(&abs)?);
            }
            Hunk::Update {
                path, move_path, ..
            } => {
                let abs = resolve_path(cwd, path);
                if let Some(move_path) = move_path {
                    dirs.push(crate::dir_lock::canonical_path_parent(&abs)?);
                    let dest_abs = resolve_path(cwd, move_path);
                    dirs.push(crate::dir_lock::canonical_write_lock_dir(&dest_abs)?);
                } else {
                    dirs.push(crate::dir_lock::canonical_update_lock_dir(&abs)?);
                }
            }
        }
    }

    Ok(dirs)
}

fn patch_text(arguments: &CborValue) -> Result<&str, ToolFailure> {
    match arguments {
        CborValue::Text(text) => Ok(text),
        _ => Err(ToolFailure::new(
            "apply_patch expects freeform patch text, not a structured payload",
        )),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Hunk {
    Add {
        path: PathBuf,
        contents: String,
    },
    Delete {
        path: PathBuf,
    },
    Update {
        path: PathBuf,
        move_path: Option<PathBuf>,
        chunks: Vec<UpdateChunk>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct UpdateChunk {
    change_context: Option<String>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    is_end_of_file: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChangeStatus {
    Add,
    Modify,
    Delete,
}

impl ChangeStatus {
    fn short_name(self) -> &'static str {
        match self {
            Self::Add => "A",
            Self::Modify => "M",
            Self::Delete => "D",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AppliedChange {
    display_path: String,
    path: PathBuf,
    status: ChangeStatus,
    old_content: String,
    new_content: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
struct ApplyPatchFailure {
    message: String,
    changes: Vec<AppliedChange>,
}

impl ApplyPatchFailure {
    fn new(message: impl Into<String>, changes: &[AppliedChange]) -> Self {
        Self {
            message: message.into(),
            changes: changes.to_vec(),
        }
    }
}

fn apply_hunks(
    hunks: &[Hunk],
    world: &mut ShellWorld,
) -> Result<Vec<AppliedChange>, ApplyPatchFailure> {
    if hunks.is_empty() {
        return Err(ApplyPatchFailure::new("No files were modified.", &[]));
    }

    let cwd = world.current_dir().to_path_buf();
    let mut changes = Vec::with_capacity(hunks.len());

    for hunk in hunks {
        match hunk {
            Hunk::Add { path, contents } => {
                let abs = resolve_path(&cwd, path);
                if read_optional_file(&abs, world)
                    .map_err(|message| ApplyPatchFailure::new(message, &changes))?
                    .is_some()
                {
                    return Err(ApplyPatchFailure::new(
                        format!("Add File target already exists: {}", render_path(&abs)),
                        &changes,
                    ));
                }
                let old_content = String::new();
                write_file_creating_parent(&abs, contents, world).map_err(|error| {
                    ApplyPatchFailure::new(
                        format!(
                            "Failed to write file {}: {}",
                            render_path(&abs),
                            render_diagnostic(error)
                        ),
                        &changes,
                    )
                })?;
                changes.push(AppliedChange {
                    display_path: render_path(path),
                    path: abs.clone(),
                    status: ChangeStatus::Add,
                    old_content,
                    new_content: Some(contents.clone()),
                });
            }
            Hunk::Delete { path } => {
                let abs = resolve_path(&cwd, path);
                if world.is_dir(&abs).map_err(|_| {
                    ApplyPatchFailure::new(
                        format!("Failed to delete file {}", render_path(&abs)),
                        &changes,
                    )
                })? {
                    return Err(ApplyPatchFailure::new(
                        format!("Failed to delete file {}", render_path(&abs)),
                        &changes,
                    ));
                }
                let old_content = world
                    .read_to_string_limited(&abs, MAX_SAFE_FILE_READ_BYTES)
                    .map_err(|_| {
                        ApplyPatchFailure::new(
                            format!("Failed to delete file {}", render_path(&abs)),
                            &changes,
                        )
                    })?;
                world.remove_file(&abs).map_err(|_| {
                    ApplyPatchFailure::new(
                        format!("Failed to delete file {}", render_path(&abs)),
                        &changes,
                    )
                })?;
                changes.push(AppliedChange {
                    display_path: render_path(path),
                    path: abs.clone(),
                    status: ChangeStatus::Delete,
                    old_content,
                    new_content: None,
                });
            }
            Hunk::Update {
                path,
                move_path,
                chunks,
            } => {
                let abs = resolve_path(&cwd, path);
                let old_content = world
                    .read_to_string_limited(&abs, MAX_SAFE_FILE_READ_BYTES)
                    .map_err(|error| {
                        ApplyPatchFailure::new(
                            format!(
                                "Failed to read file to update {}: {}",
                                render_path(&abs),
                                render_diagnostic(error)
                            ),
                            &changes,
                        )
                    })?;
                let new_content = derive_new_contents_from_chunks(&abs, &old_content, chunks)
                    .map_err(|message| ApplyPatchFailure::new(message, &changes))?;

                let (change_path, display_path) = if let Some(move_path) = move_path {
                    let dest_abs = resolve_path(&cwd, move_path);
                    if read_optional_file(&dest_abs, world)
                        .map_err(|message| ApplyPatchFailure::new(message, &changes))?
                        .is_some()
                    {
                        return Err(ApplyPatchFailure::new(
                            format!(
                                "Move destination already exists: {}",
                                render_path(&dest_abs)
                            ),
                            &changes,
                        ));
                    }
                    write_file_creating_parent(&dest_abs, &new_content, world).map_err(
                        |error| {
                            ApplyPatchFailure::new(
                                format!(
                                    "Failed to write file {}: {}",
                                    render_path(&dest_abs),
                                    render_diagnostic(error)
                                ),
                                &changes,
                            )
                        },
                    )?;
                    let dest_write_change_index = changes.len();
                    changes.push(AppliedChange {
                        display_path: render_path(move_path),
                        path: dest_abs.clone(),
                        status: ChangeStatus::Add,
                        old_content: String::new(),
                        new_content: Some(new_content.clone()),
                    });
                    if world.is_dir(&abs).map_err(|_| {
                        ApplyPatchFailure::new(
                            format!("Failed to remove original {}", render_path(&abs)),
                            &changes,
                        )
                    })? {
                        return Err(ApplyPatchFailure::new(
                            format!("Failed to remove original {}", render_path(&abs)),
                            &changes,
                        ));
                    }
                    world.remove_file(&abs).map_err(|_| {
                        ApplyPatchFailure::new(
                            format!("Failed to remove original {}", render_path(&abs)),
                            &changes,
                        )
                    })?;
                    changes[dest_write_change_index] = AppliedChange {
                        display_path: render_path(move_path),
                        path: abs.clone(),
                        status: ChangeStatus::Modify,
                        old_content: old_content.clone(),
                        new_content: Some(new_content.clone()),
                    };
                    continue;
                } else {
                    world
                        .write_file(&abs, new_content.as_bytes())
                        .map_err(|error| {
                            ApplyPatchFailure::new(
                                format!(
                                    "Failed to write file {}: {}",
                                    render_path(&abs),
                                    render_diagnostic(error)
                                ),
                                &changes,
                            )
                        })?;
                    (abs.clone(), render_path(path))
                };

                changes.push(AppliedChange {
                    display_path,
                    path: change_path,
                    status: ChangeStatus::Modify,
                    old_content,
                    new_content: Some(new_content),
                });
            }
        }
    }

    Ok(changes)
}

fn display_payload_for_changes(changes: &[AppliedChange], summary: &str) -> Option<ToolUsePayload> {
    if changes.len() == 1 {
        let change = &changes[0];
        let new_content = change.new_content.as_deref().unwrap_or_default();
        return Some(ToolUsePayload::Diff(compute_diff(
            &change.old_content,
            new_content,
        )));
    }

    let files = changes
        .iter()
        .map(|change| {
            let new_content = change.new_content.as_deref().unwrap_or_default();
            tau_proto::FileDiffSummary {
                path: change.display_path.clone(),
                diff: compute_diff(&change.old_content, new_content),
            }
        })
        .collect::<Vec<_>>();
    if files.is_empty() {
        Some(ToolUsePayload::Text {
            text: summary.to_owned(),
        })
    } else {
        Some(ToolUsePayload::Diffs { files })
    }
}

fn display_payload_for_failure(changes: &[AppliedChange]) -> Option<ToolUsePayload> {
    if changes.is_empty() {
        return None;
    }

    let summary = format_partial_summary(changes);
    display_payload_for_changes(changes, &summary)
}

fn partial_changes_details(changes: &[AppliedChange]) -> Option<CborValue> {
    if changes.is_empty() {
        return None;
    }

    let changes = changes
        .iter()
        .map(|change| {
            CborValue::Map(vec![
                (
                    CborValue::Text("status".to_owned()),
                    CborValue::Text(change.status.short_name().to_owned()),
                ),
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(change.display_path.clone()),
                ),
            ])
        })
        .collect();
    Some(CborValue::Map(vec![(
        CborValue::Text("partial_changes".to_owned()),
        CborValue::Array(changes),
    )]))
}

fn format_partial_summary(changes: &[AppliedChange]) -> String {
    let mut lines = vec!["Partial changes applied before failure:".to_owned()];
    for status in [
        ChangeStatus::Add,
        ChangeStatus::Modify,
        ChangeStatus::Delete,
    ] {
        for change in changes.iter().filter(|change| change.status == status) {
            lines.push(format!(
                "{} {}",
                change.status.short_name(),
                change.display_path
            ));
        }
    }
    lines.join("\n")
}

fn format_summary(changes: &[AppliedChange]) -> String {
    let mut lines = vec![SUMMARY_HEADER.to_owned()];
    for status in [
        ChangeStatus::Add,
        ChangeStatus::Modify,
        ChangeStatus::Delete,
    ] {
        for change in changes.iter().filter(|change| change.status == status) {
            lines.push(format!(
                "{} {}",
                change.status.short_name(),
                change.display_path
            ));
        }
    }
    lines.join("\n")
}

fn render_path(path: &Path) -> String {
    escape_path_text(&path.display().to_string())
}

fn render_diagnostic(error: impl std::fmt::Display) -> String {
    escape_path_text(&error.to_string())
}

fn read_optional_file(path: &Path, world: &mut ShellWorld) -> Result<Option<String>, String> {
    match world.read_to_string_limited(path, MAX_SAFE_FILE_READ_BYTES) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(render_diagnostic(error)),
    }
}

fn write_file_creating_parent(
    path: &Path,
    contents: &str,
    world: &mut ShellWorld,
) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        world.create_dir_all(parent)?;
    }
    world.write_file(path, contents.as_bytes())
}

fn resolve_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn derive_new_contents_from_chunks(
    path: &Path,
    original_contents: &str,
    chunks: &[UpdateChunk],
) -> Result<String, String> {
    let mut original_lines: Vec<String> = original_contents.split('\n').map(String::from).collect();
    if original_lines.last().is_some_and(String::is_empty) {
        original_lines.pop();
    }

    let replacements = compute_replacements(&original_lines, path, chunks)?;
    let mut new_lines = apply_replacements(original_lines, &replacements);
    if !new_lines.last().is_some_and(String::is_empty) {
        new_lines.push(String::new());
    }
    Ok(new_lines.join("\n"))
}

fn compute_replacements(
    original_lines: &[String],
    path: &Path,
    chunks: &[UpdateChunk],
) -> Result<Vec<(usize, usize, Vec<String>)>, String> {
    let mut replacements = Vec::new();
    let mut line_index = 0usize;

    for chunk in chunks {
        if let Some(ctx_line) = &chunk.change_context {
            if let Some(idx) = seek_sequence(
                original_lines,
                std::slice::from_ref(ctx_line),
                line_index,
                false,
            ) {
                line_index = idx + 1;
            } else {
                return Err(format!(
                    "Failed to find context '{}' in {}",
                    ctx_line,
                    render_path(path)
                ));
            }
        }

        if chunk.old_lines.is_empty() {
            let insertion_idx = if original_lines.last().is_some_and(String::is_empty) {
                original_lines.len().saturating_sub(1)
            } else {
                original_lines.len()
            };
            replacements.push((insertion_idx, 0, chunk.new_lines.clone()));
            continue;
        }

        let mut pattern: &[String] = &chunk.old_lines;
        let mut new_slice: &[String] = &chunk.new_lines;
        let mut found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);

        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }
            found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);
        }

        if let Some(start_idx) = found {
            replacements.push((start_idx, pattern.len(), new_slice.to_vec()));
            line_index = start_idx + pattern.len();
        } else {
            return Err(format!(
                "Failed to find expected lines in {}:\n{}",
                render_path(path),
                chunk.old_lines.join("\n")
            ));
        }
    }

    replacements.sort_by_key(|(start_idx, _, _)| *start_idx);
    Ok(replacements)
}

fn apply_replacements(
    mut lines: Vec<String>,
    replacements: &[(usize, usize, Vec<String>)],
) -> Vec<String> {
    for (start_idx, old_len, new_segment) in replacements.iter().rev() {
        for _ in 0..*old_len {
            if *start_idx < lines.len() {
                lines.remove(*start_idx);
            }
        }
        for (offset, new_line) in new_segment.iter().enumerate() {
            lines.insert(*start_idx + offset, new_line.clone());
        }
    }
    lines
}

fn seek_sequence(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    if pattern.len() > lines.len() {
        return None;
    }

    let search_start = if eof && lines.len() >= pattern.len() {
        lines.len() - pattern.len()
    } else {
        start
    };

    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        if lines[i..i + pattern.len()] == *pattern {
            return Some(i);
        }
    }
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        let mut ok = true;
        for (p_idx, pat) in pattern.iter().enumerate() {
            if lines[i + p_idx].trim_end() != pat.trim_end() {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(i);
        }
    }
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        let mut ok = true;
        for (p_idx, pat) in pattern.iter().enumerate() {
            if lines[i + p_idx].trim() != pat.trim() {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(i);
        }
    }
    None
}

fn parse_patch(patch: &str) -> Result<Vec<Hunk>, String> {
    PatchParser::new(patch)?.parse()
}

// Parser section. This manually implements `apply_patch.lark`: begin/end
// sentinels are validated before parsing starts, and `index` only advances
// through body lines between the begin header and end footer.
struct PatchParser<'a> {
    lines: Vec<&'a str>,
    index: usize,
}

impl<'a> PatchParser<'a> {
    fn new(patch: &'a str) -> Result<Self, String> {
        let lines: Vec<&str> = patch.trim().lines().collect();
        if lines.first().copied() != Some("*** Begin Patch") {
            return Err("invalid patch: missing '*** Begin Patch' header".to_owned());
        }
        if lines.last().copied() != Some("*** End Patch") {
            return Err("invalid patch: missing '*** End Patch' footer".to_owned());
        }

        Ok(Self { lines, index: 1 })
    }

    fn parse(mut self) -> Result<Vec<Hunk>, String> {
        let mut hunks = Vec::new();
        while self.has_body_line() {
            hunks.push(self.parse_hunk()?);
        }

        if hunks.is_empty() {
            return Err("invalid patch: no file operations found".to_owned());
        }
        Ok(hunks)
    }

    fn parse_hunk(&mut self) -> Result<Hunk, String> {
        let line = self.current_line();
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            return self.parse_add(path);
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            return Ok(self.parse_delete(path));
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            return self.parse_update(path);
        }

        Err(format!(
            "invalid patch operation: {}",
            escape_path_text(line)
        ))
    }

    fn parse_add(&mut self, path: &str) -> Result<Hunk, String> {
        self.advance();
        let mut contents = Vec::new();
        while self.has_body_line() && !self.current_line().starts_with("*** ") {
            let Some(content) = self.current_line().strip_prefix('+') else {
                return Err(format!(
                    "invalid add-file line: {}",
                    escape_path_text(self.current_line())
                ));
            };
            contents.push(content.to_owned());
            self.advance();
        }

        if contents.is_empty() {
            return Err(format!(
                "Add File hunk for {} must contain at least one line",
                escape_path_text(path)
            ));
        }

        Ok(Hunk::Add {
            path: PathBuf::from(path),
            contents: contents.join("\n") + "\n",
        })
    }

    fn parse_delete(&mut self, path: &str) -> Hunk {
        self.advance();
        Hunk::Delete {
            path: PathBuf::from(path),
        }
    }

    fn parse_update(&mut self, path: &str) -> Result<Hunk, String> {
        self.advance();
        let move_path = self.parse_move_path();
        let mut chunks = Vec::new();

        while self.has_body_line() && !self.current_line().starts_with("*** ") {
            chunks.push(self.parse_update_chunk(path)?);
        }

        if chunks.is_empty() {
            return Err(format!(
                "Update File hunk for {} must contain at least one chunk",
                escape_path_text(path)
            ));
        }

        Ok(Hunk::Update {
            path: PathBuf::from(path),
            move_path,
            chunks,
        })
    }

    fn parse_move_path(&mut self) -> Option<PathBuf> {
        if self.has_body_line()
            && let Some(dest) = self.current_line().strip_prefix("*** Move to: ")
        {
            let move_path = PathBuf::from(dest);
            self.advance();
            Some(move_path)
        } else {
            None
        }
    }

    fn parse_update_chunk(&mut self, path: &str) -> Result<UpdateChunk, String> {
        let change_context = self.parse_update_header()?;
        let mut chunk = ParsedUpdateChunk::default();

        while self.has_body_line()
            && !self.current_line().starts_with("@@")
            && !self.current_line_is_patch_operation_boundary()
        {
            if self.current_line() == "*** End of File" {
                chunk.is_end_of_file = true;
                self.advance();
                break;
            }
            self.parse_update_line(&mut chunk)?;
            self.advance();
        }

        if chunk.old_lines.is_empty() && chunk.new_lines.is_empty() {
            return Err(format!(
                "Update File hunk for {} must contain at least one line",
                escape_path_text(path)
            ));
        }

        Ok(UpdateChunk {
            change_context,
            old_lines: chunk.old_lines,
            new_lines: chunk.new_lines,
            is_end_of_file: chunk.is_end_of_file,
        })
    }

    fn parse_update_header(&mut self) -> Result<Option<String>, String> {
        let header = self.current_line();
        let change_context = if header == "@@" {
            None
        } else if let Some(context) = header.strip_prefix("@@ ") {
            Some(context.to_owned())
        } else {
            return Err(format!(
                "invalid update hunk header: {}",
                escape_path_text(header)
            ));
        };
        self.advance();
        Ok(change_context)
    }

    fn parse_update_line(&self, chunk: &mut ParsedUpdateChunk) -> Result<(), String> {
        let line = self.current_line();
        let mut chars = line.chars();
        match chars.next() {
            None => {
                chunk.old_lines.push(String::new());
                chunk.new_lines.push(String::new());
            }
            Some(' ') => {
                let rest = chars.as_str().to_owned();
                chunk.old_lines.push(rest.clone());
                chunk.new_lines.push(rest);
            }
            Some('-') => {
                chunk.old_lines.push(chars.as_str().to_owned());
            }
            Some('+') => {
                chunk.new_lines.push(chars.as_str().to_owned());
            }
            _ => {
                return Err(format!(
                    "invalid update hunk line: {}",
                    escape_path_text(line)
                ));
            }
        }
        Ok(())
    }

    fn current_line(&self) -> &'a str {
        self.lines[self.index]
    }

    fn advance(&mut self) {
        self.index += 1;
    }

    fn has_body_line(&self) -> bool {
        self.index + 1 < self.lines.len()
    }

    fn current_line_is_patch_operation_boundary(&self) -> bool {
        self.current_line().starts_with("*** ") && self.current_line() != "*** End of File"
    }
}

#[derive(Default)]
struct ParsedUpdateChunk {
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    is_end_of_file: bool,
}

#[cfg(test)]
mod tests;
