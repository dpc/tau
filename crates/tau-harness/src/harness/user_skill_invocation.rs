//! User-facing `:skill` command parsing and prompt expansion helpers.

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
    // closing fence, filter the model-facing body, and append a truncation note
    // at the call site.
    let loaded = match source {
        DiscoveredSkillSource::File(path) => {
            tau_skills::read_skill_file_prefix(path, MAX_USER_INVOKED_SKILL_BYTES)
                .map_err(|error| error.to_string())?
        }
        DiscoveredSkillSource::BuiltIn { content } => {
            tau_skills::read_skill_text_prefix(content.as_ref(), MAX_USER_INVOKED_SKILL_BYTES)
        }
    };
    let total_bytes = loaded.total_bytes;
    let prepared = loaded.prepare().map_err(|error| match error {
        tau_skills::SkillContentPreparationError::FrontmatterTruncated => format!(
            "frontmatter closing fence was not found before the {MAX_USER_INVOKED_SKILL_BYTES} byte read limit; file has {total_bytes} bytes"
        ),
    })?;
    Ok(LoadedSkillBody {
        body: prepared.model_body,
        truncated: prepared.truncated,
        total_bytes: prepared.total_bytes,
    })
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
