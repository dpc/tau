//! User-facing `:skill` command parsing and prompt expansion helpers.

use std::fs as path_std_fs;
use std::io::Read as _;

use crate::discovery::DiscoveredSkillSource;

pub(super) const MAX_USER_INVOKED_SKILL_BYTES: usize = 64 * 1024;

pub(super) fn parse_user_skill_command(text: &str) -> Option<(&str, &str)> {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix(":skill:") {
        return Some(split_skill_name_and_args(rest));
    }
    if let Some(rest) = trimmed.strip_prefix(":skill") {
        if rest.is_empty() {
            return Some(("", ""));
        }
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let (name, args) = split_skill_name_and_args(rest.trim_start());
        return Some((name, args));
    }
    None
}

fn split_skill_name_and_args(rest: &str) -> (&str, &str) {
    let name_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..name_end];
    let args = rest[name_end..].trim_start();
    (name, args)
}

#[derive(Debug)]
pub(super) struct LoadedSkillBody {
    pub(super) body: String,
    pub(super) truncated: bool,
    pub(super) total_bytes: u64,
}

pub(super) fn read_user_invoked_skill_body(
    source: &DiscoveredSkillSource,
) -> Result<LoadedSkillBody, String> {
    // Keep this behavior in sync with tau-harness-tools' model-visible `skill`
    // tool: both read a bounded prefix, reject frontmatter truncated before the
    // closing fence, strip frontmatter from the loaded prefix, and append a
    // truncation note at the call site.
    let (text, truncated, total_bytes) = match source {
        DiscoveredSkillSource::File(path) => {
            read_text_file_prefix(path, MAX_USER_INVOKED_SKILL_BYTES)
                .map_err(|error| error.to_string())?
        }
        DiscoveredSkillSource::BuiltIn { content } => {
            read_text_prefix(content.as_ref(), MAX_USER_INVOKED_SKILL_BYTES)
        }
    };
    if truncated && tau_skills::has_unclosed_frontmatter(&text) {
        return Err(format!(
            "frontmatter closing fence was not found before the {MAX_USER_INVOKED_SKILL_BYTES} byte read limit; file has {total_bytes} bytes"
        ));
    }
    Ok(LoadedSkillBody {
        body: tau_skills::strip_frontmatter(&text).to_owned(),
        truncated,
        total_bytes,
    })
}

fn read_text_file_prefix(
    path: &std::path::Path,
    max_bytes: usize,
) -> std::io::Result<(String, bool, u64)> {
    let mut file = path_std_fs::File::open(path)?;
    let total_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut bytes = Vec::new();
    file.by_ref()
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    let truncated = max_bytes < bytes.len();
    if truncated {
        bytes.truncate(max_bytes);
    }
    Ok((
        String::from_utf8_lossy(&bytes).into_owned(),
        truncated,
        total_bytes,
    ))
}

fn read_text_prefix(text: &str, max_bytes: usize) -> (String, bool, u64) {
    let total_bytes = text.len() as u64;
    if text.len() <= max_bytes {
        return (text.to_owned(), false, total_bytes);
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true, total_bytes)
}

pub(super) fn format_user_invoked_skill_prompt(
    name: &str,
    source: &DiscoveredSkillSource,
    body: &str,
    truncated_total_bytes: Option<u64>,
    args: &str,
) -> String {
    let location = source.label();
    let base_dir = match source {
        DiscoveredSkillSource::File(path) => path
            .parent()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| path.display().to_string()),
        DiscoveredSkillSource::BuiltIn { .. } => "<builtin>".to_owned(),
    };
    let mut prompt = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}",
        xml_attr_escape(name),
        xml_attr_escape(&location),
        base_dir,
        body
    );
    if let Some(total_bytes) = truncated_total_bytes {
        prompt.push_str(&format!(
            "\n\n[skill content truncated at {MAX_USER_INVOKED_SKILL_BYTES} bytes; file has {total_bytes} bytes]"
        ));
    }
    prompt.push_str("\n</skill>");
    if !args.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(args);
    }
    prompt
}

fn xml_attr_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests;
