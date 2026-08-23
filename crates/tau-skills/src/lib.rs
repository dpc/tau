//! Skill discovery and frontmatter parsing.
//!
//! The frontmatter parser delegates to `serde_yaml_ng`, so the YAML inside
//! `---` fences is the real thing: quoted strings, escapes, block scalars,
//! flow style, comments, anchors. Two project-level conventions on top of
//! that:
//!
//! - Only top-level scalar values (string, bool, number) are exposed. Lists,
//!   mappings and `null` are dropped silently.
//! - All scalars are stringified before being returned. `BTreeMap<String,
//!   String>` is the contract callers see.
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_yaml_ng::Value as YamlValue;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A validated, loaded skill.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Skill {
    /// Validated skill name.
    pub name: String,
    /// Short human-facing description from frontmatter.
    pub description: String,
    /// Path to the Markdown skill file.
    pub file_path: PathBuf,
    /// When true, the skill is listed in the system prompt at session
    /// start so the agent sees its name + description without having
    /// to search. Use `skill { query: "…" }` to discover/load hidden
    /// skills.
    pub add_to_prompt: bool,
    /// True when the skill file explicitly set `advertise:`. Scoped
    /// directory defaults only apply when this is false.
    pub add_to_prompt_explicit: bool,
    /// Whether users may explicitly invoke this skill with `:skill`.
    pub user_invocable: bool,
    /// True when `user-invocable:` was explicitly present.
    pub user_invocable_explicit: bool,
    /// Whether model-side skill discovery/loading should hide this skill.
    /// Implies that the skill remains user-invocable.
    pub disable_model_invocation: bool,
    /// Optional UI hint for arguments accepted by this skill.
    pub argument_hint: Option<String>,
}

/// A skill search root plus policy that applies to every skill loaded
/// from that root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillDir {
    /// Filesystem directory scanned for Markdown skills.
    pub path: PathBuf,
    /// When true, skills from this directory are added to the initial
    /// prompt when their frontmatter omits `advertise:`. Explicit
    /// `advertise: false` remains a hard opt-out.
    pub add_to_prompt_by_default: bool,
    /// Optional root precedence used before modified-time collision
    /// resolution when both colliding candidates provide one. Lower
    /// values win. `None` keeps legacy modified-time-only selection for
    /// callers that do not need explicit root precedence.
    pub source_precedence: Option<u32>,
}

/// A skill bundled into Tau at compile time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltInSkill {
    /// Validated skill name from frontmatter.
    pub name: String,
    /// Validated and possibly truncated description from frontmatter.
    pub description: String,
    /// Full Markdown source included into the binary at build time,
    /// with runtime placeholders resolved.
    pub content: Cow<'static, str>,
    /// Whether this skill should appear in the initial system prompt.
    pub add_to_prompt: bool,
    /// Whether users may explicitly invoke this skill with `:skill`.
    pub user_invocable: bool,
    /// Whether model-side skill discovery/loading should hide this skill.
    /// Implies that the skill remains user-invocable.
    pub disable_model_invocation: bool,
    /// Optional UI hint for arguments accepted by this skill.
    pub argument_hint: Option<String>,
}

struct BuiltInSkillSource {
    diagnostic_path: &'static str,
    content: &'static str,
}

impl Skill {
    /// Directory containing this skill's file. Always
    /// `file_path.parent()` (falling back to `file_path` if there is
    /// no parent, which is unreachable for any real on-disk skill).
    pub fn base_dir(&self) -> &Path {
        self.file_path.parent().unwrap_or(&self.file_path)
    }
}

/// Non-fatal diagnostic emitted during skill loading.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillDiagnostic {
    /// Path of the skill file, directory entry, or root associated with the
    /// diagnostic.
    pub path: PathBuf,
    /// Machine-readable diagnostic category.
    pub kind: DiagnosticKind,
    /// Human-readable diagnostic text suitable for notices/logs.
    pub message: String,
}

/// Category for a skill loading diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticKind {
    /// Soft issue — the skill still loads.
    Warning,
    /// Duplicate skill name; the loader selected a winner using explicit source
    /// precedence when available, then modified-time ordering with a stable
    /// tie-break.
    Collision,
    /// Fatal issue — the skill is not loaded.
    Skipped,
}

/// Result of loading skills from one or more directories.
pub struct LoadSkillsResult {
    /// Valid skills selected after loading, validation, and duplicate-name
    /// collision resolution.
    pub skills: Vec<Skill>,
    /// Warnings, skips, and collision notes emitted while scanning/loading.
    pub diagnostics: Vec<SkillDiagnostic>,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAX_NAME_LENGTH: usize = 64;
pub const MAX_DESCRIPTION_LENGTH: usize = 1024;
pub const MAX_ARGUMENT_HINT_LENGTH: usize = 256;
const MAX_SKILL_DISCOVERY_BYTES: usize = 64 * 1024;
const SKILL_FILENAME: &str = "SKILL.md";
const SELF_KNOWLEDGE_VERSION_TOKEN: &str = "__TAU_SELF_KNOWLEDGE_VERSION__";
const TAU_VERSION: &str = env!("CARGO_PKG_VERSION");

const BUILT_IN_SKILL_SOURCES: &[BuiltInSkillSource] = &[
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge.md",
        content: include_str!("../self-knowledge/tau-self-knowledge.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-introduction.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-introduction.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-architecture.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-architecture.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-harness.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-harness.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-config.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-config.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-secrets.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-secrets.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-isolation.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-isolation.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-cli-ui.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-cli-ui.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-email.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-email.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-ext-pim.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-ext-pim.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-ext-rostra.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-ext-rostra.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-ext-provider-builtin.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-ext-provider-builtin.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-ext-rhai.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-ext-rhai.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-ext-shell.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-ext-shell.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-ext-slack.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-ext-slack.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-ext-zulip.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-ext-zulip.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-ext-swarm.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-ext-swarm.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-ext-std-notifications.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-ext-std-notifications.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-ext-test-dummy.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-ext-test-dummy.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-ext-websearch.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-ext-websearch.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-prompt-templating.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-prompt-templating.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-source-code.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-source-code.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-community.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-community.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-debugging.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-debugging.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-tracing.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-tracing.md"),
    },
    BuiltInSkillSource {
        diagnostic_path: "tau-self-knowledge-e2e-testing.md",
        content: include_str!("../self-knowledge/tau-self-knowledge-e2e-testing.md"),
    },
];

// ---------------------------------------------------------------------------
// Frontmatter parsing
// ---------------------------------------------------------------------------

/// Parse YAML frontmatter delimited by `---` lines.
///
/// Returns a map of key→value pairs and the body (content after the closing
/// `---`). If no frontmatter is present, returns an empty map and the full
/// content as body. If the YAML inside a closed fence fails to parse, returns
/// an empty map and the post-fence body.
///
/// Top-level scalars are stringified; non-scalar values (lists, mappings)
/// and `null` are dropped silently — see the module-level docs.
pub fn parse_frontmatter(content: &str) -> (BTreeMap<String, String>, &str) {
    let parsed = parse_frontmatter_inner(content);
    (parsed.fields, parsed.body)
}

/// Strip frontmatter and return only the body.
pub fn strip_frontmatter(content: &str) -> &str {
    parse_frontmatter(content).1
}

/// Returns true when `content` starts with a frontmatter opening fence
/// but does not include the closing fence.
pub fn has_unclosed_frontmatter(content: &str) -> bool {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let Some(rest) = content.strip_prefix("---") else {
        return false;
    };
    let Some(rest) = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
    else {
        return false;
    };
    find_closing_fence(rest).is_none()
}

/// Locate the closing `---` fence. Returns `(yaml_end, body_start)` as
/// byte offsets into `s`, where `yaml_end` is the start of the closing
/// fence line and `body_start` is the first byte after that line's
/// terminator (handles both `\n` and `\r\n`).
fn find_closing_fence(s: &str) -> Option<(usize, usize)> {
    let mut pos = 0;
    for line in s.split_inclusive('\n') {
        let stripped = line.trim_end_matches('\n').trim_end_matches('\r');
        if stripped.trim_end() == "---" {
            return Some((pos, pos + line.len()));
        }
        pos += line.len();
    }
    None
}

struct ParsedFrontmatter<'a> {
    fields: BTreeMap<String, String>,
    body: &'a str,
    yaml_error: Option<String>,
}

fn parse_frontmatter_inner(content: &str) -> ParsedFrontmatter<'_> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);

    let Some(rest) = content.strip_prefix("---") else {
        return ParsedFrontmatter {
            fields: BTreeMap::new(),
            body: content,
            yaml_error: None,
        };
    };
    let Some(rest) = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
    else {
        return ParsedFrontmatter {
            fields: BTreeMap::new(),
            body: content,
            yaml_error: None,
        };
    };

    let Some((yaml_end, body_start)) = find_closing_fence(rest) else {
        return ParsedFrontmatter {
            fields: BTreeMap::new(),
            body: content,
            yaml_error: None,
        };
    };

    let yaml_block = &rest[..yaml_end];
    let body = &rest[body_start..];

    match serde_yaml_ng::from_str::<YamlValue>(yaml_block) {
        Ok(YamlValue::Mapping(m)) => ParsedFrontmatter {
            fields: m
                .into_iter()
                .filter_map(|(k, v)| {
                    let YamlValue::String(key) = k else {
                        return None;
                    };
                    Some((key, scalar_to_string(&v)?))
                })
                .collect(),
            body,
            yaml_error: None,
        },
        Ok(_) => ParsedFrontmatter {
            fields: BTreeMap::new(),
            body,
            yaml_error: None,
        },
        Err(err) => ParsedFrontmatter {
            fields: BTreeMap::new(),
            body,
            yaml_error: Some(err.to_string()),
        },
    }
}

/// Stringify a YAML scalar. Non-scalar values (lists, maps, null,
/// tagged) return None and are dropped from the public map.
fn scalar_to_string(v: &YamlValue) -> Option<String> {
    match v {
        YamlValue::String(s) => Some(s.clone()),
        YamlValue::Bool(b) => Some(b.to_string()),
        YamlValue::Number(n) => Some(n.to_string()),
        YamlValue::Null | YamlValue::Sequence(_) | YamlValue::Mapping(_) | YamlValue::Tagged(_) => {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Outcome of name validation. `skip` is true when the name is unusable
/// (empty, wrong charset, badly placed hyphens, too long) — the caller
/// must not produce a `Skill` in that case.
struct NameValidation {
    diagnostics: Vec<SkillDiagnostic>,
    skip: bool,
}

fn validate_name(name: &str, parent_dir_name: Option<&str>, path: &Path) -> NameValidation {
    let mut diagnostics = Vec::new();
    let mut skip = false;

    if name.is_empty() {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: "name is empty (no `name:` field and no usable parent directory name)"
                .to_owned(),
        });
        return NameValidation {
            diagnostics,
            skip: true,
        };
    }

    if let Some(parent) = parent_dir_name
        && name != parent
    {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: format!("name \"{name}\" does not match parent directory \"{parent}\""),
        });
    }

    if name.len() > MAX_NAME_LENGTH {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: format!("name exceeds {MAX_NAME_LENGTH} characters ({})", name.len()),
        });
        skip = true;
    }

    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_owned(),
        });
        skip = true;
    }

    if name.starts_with('-') || name.ends_with('-') {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: "name must not start or end with a hyphen".to_owned(),
        });
        skip = true;
    }

    if name.contains("--") {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: "name must not contain consecutive hyphens".to_owned(),
        });
        skip = true;
    }

    NameValidation { diagnostics, skip }
}

fn validate_description(description: &str, path: &Path) -> Vec<SkillDiagnostic> {
    let mut diagnostics = Vec::new();
    if MAX_DESCRIPTION_LENGTH < description.len() {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: format!(
                "description exceeds {MAX_DESCRIPTION_LENGTH} bytes ({}); truncating",
                description.len()
            ),
        });
    }
    diagnostics
}

fn parse_bool_frontmatter(
    fields: &BTreeMap<String, String>,
    key: &str,
    default: bool,
    path: &Path,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> (bool, bool) {
    let Some(value) = fields.get(key) else {
        return (default, false);
    };
    let parsed = if value.eq_ignore_ascii_case("true") || value == "1" {
        Some(true)
    } else if value.eq_ignore_ascii_case("false") || value == "0" {
        Some(false)
    } else {
        None
    };
    match parsed {
        Some(value) => (value, true),
        None => {
            diagnostics.push(SkillDiagnostic {
                path: path.to_owned(),
                kind: DiagnosticKind::Warning,
                message: format!("{key}: invalid boolean value {value:?}; using default {default}"),
            });
            (default, false)
        }
    }
}

fn truncate_argument_hint(argument_hint: &str) -> Cow<'_, str> {
    if argument_hint.len() <= MAX_ARGUMENT_HINT_LENGTH {
        return Cow::Borrowed(argument_hint);
    }
    let suffix = "…";
    let mut end = MAX_ARGUMENT_HINT_LENGTH.saturating_sub(suffix.len());
    while !argument_hint.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = String::from(&argument_hint[..end]);
    truncated.push_str(suffix);
    Cow::Owned(truncated)
}

fn parse_argument_hint(
    fields: &BTreeMap<String, String>,
    path: &Path,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> Option<String> {
    let hint = fields.get("argument-hint")?.trim();
    if hint.is_empty() {
        return None;
    }
    if MAX_ARGUMENT_HINT_LENGTH < hint.len() {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: format!(
                "argument-hint exceeds {MAX_ARGUMENT_HINT_LENGTH} bytes ({}); truncating",
                hint.len()
            ),
        });
    }
    Some(truncate_argument_hint(hint).into_owned())
}

/// Truncate a skill description to [`MAX_DESCRIPTION_LENGTH`] bytes.
///
/// The returned value is borrowed when no truncation is needed. Truncation is
/// UTF-8 safe and reserves room for a trailing ellipsis (`…`).
pub fn truncate_description(description: &str) -> Cow<'_, str> {
    if description.len() <= MAX_DESCRIPTION_LENGTH {
        return Cow::Borrowed(description);
    }

    let suffix = "…";
    let mut end = MAX_DESCRIPTION_LENGTH.saturating_sub(suffix.len());
    while !description.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = String::from(&description[..end]);
    truncated.push_str(suffix);
    Cow::Owned(truncated)
}

/// Return the first validation error for a proposed skill name.
///
/// Valid names are non-empty, at most 64 bytes, lowercase ASCII
/// alphanumeric-with-hyphens, and do not start/end with a hyphen or contain
/// consecutive hyphens.
pub fn skill_name_validation_message(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("name is empty".to_owned());
    }
    if MAX_NAME_LENGTH < name.len() {
        return Some(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({})",
            name.len()
        ));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Some(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_owned(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Some("name must not start or end with a hyphen".to_owned());
    }
    if name.contains("--") {
        return Some("name must not contain consecutive hyphens".to_owned());
    }
    None
}

/// Return whether `name` satisfies Tau's skill-name rules.
pub fn is_valid_skill_name(name: &str) -> bool {
    skill_name_validation_message(name).is_none()
}

// ---------------------------------------------------------------------------
// Single-file loading
// ---------------------------------------------------------------------------

/// Load a single skill from file content and its path on disk.
///
/// Returns `None` for the skill if the description is missing/empty or the
/// name is invalid. Diagnostics are returned in all cases.
pub fn load_skill_from_content(
    content: &str,
    file_path: &Path,
) -> (Option<Skill>, Vec<SkillDiagnostic>) {
    let mut diagnostics = Vec::new();
    // The body is intentionally discarded here. Consumers re-read the
    // file on demand via `Skill::file_path` so edits to a skill's
    // instructions are picked up without a daemon restart; caching the
    // body on `Skill` would freeze the contents at discovery time.
    let parsed = parse_frontmatter_inner(content);
    if let Some(err) = parsed.yaml_error {
        diagnostics.push(SkillDiagnostic {
            path: file_path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: format!("frontmatter YAML failed to parse: {err}"),
        });
        return (None, diagnostics);
    }
    let fm = parsed.fields;

    let skill_dir = file_path.parent().unwrap_or(file_path);
    let parent_dir_name = skill_dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned);
    let file_name = file_path.file_name().and_then(|n| n.to_str());
    let fallback_name = if file_name == Some(SKILL_FILENAME) {
        parent_dir_name.clone()
    } else {
        file_path
            .file_stem()
            .and_then(|n| n.to_str())
            .map(str::to_owned)
    };
    let parent_name_for_validation = if file_name == Some(SKILL_FILENAME) {
        parent_dir_name.as_deref()
    } else {
        None
    };

    let name = fm
        .get("name")
        .cloned()
        .or(fallback_name)
        .unwrap_or_default();

    let name_check = validate_name(&name, parent_name_for_validation, file_path);
    diagnostics.extend(name_check.diagnostics);
    if name_check.skip {
        return (None, diagnostics);
    }

    let description = fm.get("description").map(|s| s.trim().to_owned());
    let description = match description {
        Some(d) if !d.is_empty() => {
            diagnostics.extend(validate_description(&d, file_path));
            truncate_description(&d).into_owned()
        }
        _ => {
            diagnostics.push(SkillDiagnostic {
                path: file_path.to_owned(),
                kind: DiagnosticKind::Skipped,
                message: "description is required".to_owned(),
            });
            return (None, diagnostics);
        }
    };

    let (add_to_prompt, add_to_prompt_explicit) =
        parse_bool_frontmatter(&fm, "advertise", false, file_path, &mut diagnostics);
    let (parsed_user_invocable, user_invocable_explicit) =
        parse_bool_frontmatter(&fm, "user-invocable", true, file_path, &mut diagnostics);
    let (disable_model_invocation, _) = parse_bool_frontmatter(
        &fm,
        "disable-model-invocation",
        false,
        file_path,
        &mut diagnostics,
    );
    let user_invocable = parsed_user_invocable || disable_model_invocation;
    let argument_hint = parse_argument_hint(&fm, file_path, &mut diagnostics);
    if !parsed_user_invocable && disable_model_invocation {
        diagnostics.push(SkillDiagnostic {
            path: file_path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: "skill has both user-invocable: false and disable-model-invocation: true; disable-model-invocation implies user-invocable".to_owned(),
        });
    }

    let skill = Skill {
        name,
        description,
        file_path: file_path.to_owned(),
        add_to_prompt,
        add_to_prompt_explicit,
        user_invocable,
        user_invocable_explicit,
        disable_model_invocation,
        argument_hint,
    };

    (Some(skill), diagnostics)
}

// ---------------------------------------------------------------------------
// Directory scanning
// ---------------------------------------------------------------------------

/// Maximum directories visited while discovering skills below a single root.
const MAX_SKILL_DISCOVERY_DIRS_PER_ROOT: usize = 1024;
/// Maximum entries inspected in a single directory before skipping it.
const MAX_SKILL_DISCOVERY_ENTRIES_PER_DIR: usize = 1024;
/// Maximum entries inspected while discovering skills below a single root.
const MAX_SKILL_DISCOVERY_ENTRIES_PER_ROOT: usize = 8192;
/// Maximum recursive directory depth below a skill root.
const MAX_SKILL_DISCOVERY_DEPTH: usize = 32;

/// Mutable traversal state for one skill discovery root.
struct DiscoveryState {
    /// Canonical directory paths already visited, used to avoid symlink cycles.
    visited_dirs: BTreeSet<PathBuf>,
    /// Number of unique directories visited below this root.
    visited_dir_count: usize,
    /// Number of directory entries inspected below this root.
    inspected_entry_count: usize,
    /// Diagnostics emitted while enforcing traversal budgets.
    diagnostics: Vec<SkillDiagnostic>,
    /// True when a per-root budget is exhausted and the caller should stop
    /// traversing the rest of the root.
    stop_root: bool,
}

impl DiscoveryState {
    fn new() -> Self {
        Self {
            visited_dirs: BTreeSet::new(),
            visited_dir_count: 0,
            inspected_entry_count: 0,
            diagnostics: Vec::new(),
            stop_root: false,
        }
    }

    fn push_warning(&mut self, path: &Path, message: impl Into<String>) {
        self.diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: message.into(),
        });
    }
}

/// Discover skill file paths under `root` using Pi-style discovery rules:
///
/// 1. If a directory contains `SKILL.md`, that file is the skill — stop
///    recursing into that directory.
/// 2. Otherwise, at root level only, treat direct `.md` children as individual
///    skills.
/// 3. Recurse into subdirectories to find `SKILL.md`.
/// 4. Skip dot-prefixed entries and `node_modules`.
/// 5. Follow symlinked roots and entries while using canonical directory paths
///    to avoid recursing forever through symlink cycles, as chosen by
///    `GATE-instruction-symlink-discovery`.
/// 6. Bound traversal by directory, entry, and depth budgets; diagnostics are
///    available to loading callers when a budget is exceeded.
///
/// The wider discovery and prompt-trust boundary is recorded in
/// `ARCH-tau-skills`.
pub fn discover_skill_paths(root: &Path) -> Vec<PathBuf> {
    discover_skill_paths_with_diagnostics(root).0
}

fn discover_skill_paths_with_diagnostics(root: &Path) -> (Vec<PathBuf>, Vec<SkillDiagnostic>) {
    discover_skill_paths_with_entry_limit(root, MAX_SKILL_DISCOVERY_ENTRIES_PER_DIR)
}

fn discover_skill_paths_with_entry_limit(
    root: &Path,
    entry_limit: usize,
) -> (Vec<PathBuf>, Vec<SkillDiagnostic>) {
    let mut paths = Vec::new();
    let mut state = DiscoveryState::new();
    discover_skill_paths_inner(root, true, 0, entry_limit, &mut paths, &mut state);
    (paths, state.diagnostics)
}

fn discover_skill_paths_inner(
    dir: &Path,
    is_root: bool,
    depth: usize,
    entry_limit: usize,
    out: &mut Vec<PathBuf>,
    state: &mut DiscoveryState,
) {
    if state.stop_root {
        return;
    }

    if MAX_SKILL_DISCOVERY_DEPTH < depth {
        state.push_warning(
            dir,
            format!(
                "skipping skill directory: discovery depth budget exceeded (max {MAX_SKILL_DISCOVERY_DEPTH})"
            ),
        );
        return;
    }

    if let Ok(canonical) = dir.canonicalize()
        && !state.visited_dirs.insert(canonical)
    {
        return;
    }

    if state.visited_dir_count >= MAX_SKILL_DISCOVERY_DIRS_PER_ROOT {
        state.push_warning(
            dir,
            format!(
                "stopping skill discovery: directory budget exceeded (max {MAX_SKILL_DISCOVERY_DIRS_PER_ROOT} per root)"
            ),
        );
        state.stop_root = true;
        return;
    }
    state.visited_dir_count += 1;

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut children = Vec::new();
    for entry in entries.flatten() {
        if children.len() >= entry_limit {
            state.push_warning(
                dir,
                format!(
                    "skipping skill directory: entry budget exceeded (max {entry_limit} per directory)"
                ),
            );
            return;
        }
        if state.inspected_entry_count >= MAX_SKILL_DISCOVERY_ENTRIES_PER_ROOT {
            state.push_warning(
                dir,
                format!(
                    "stopping skill discovery: entry budget exceeded (max {MAX_SKILL_DISCOVERY_ENTRIES_PER_ROOT} per root)"
                ),
            );
            state.stop_root = true;
            return;
        }
        state.inspected_entry_count += 1;
        children.push(entry);
    }
    children.sort_by_key(|entry: &fs::DirEntry| entry.path());

    // Single-pass search: if SKILL.md exists among the children as a regular
    // file, that's this directory's skill and we stop recursing here.
    let skill_md = children.iter().find(|e| {
        if e.file_name() != SKILL_FILENAME {
            return false;
        }
        fs::metadata(e.path())
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
    });
    if let Some(entry) = skill_md {
        out.push(entry.path());
        return;
    }

    for entry in &children {
        if state.stop_root {
            return;
        }

        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };

        if name_str.starts_with('.') || name_str == "node_modules" {
            continue;
        }

        let path = entry.path();
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };

        if metadata.is_dir() {
            discover_skill_paths_inner(&path, false, depth + 1, entry_limit, out, state);
        } else if metadata.is_file() && is_root && name_str.ends_with(".md") {
            out.push(path);
        }
    }
}

/// Skill candidate selected as the current winner for a skill name collision.
struct SelectedSkill {
    /// Parsed skill metadata and content for the winning candidate.
    skill: Skill,
    /// Filesystem modification time used as the fallback collision tie-breaker.
    modified: Option<SystemTime>,
    /// Optional root precedence where lower values beat higher values before
    /// modification time is considered.
    source_precedence: Option<u32>,
}

fn skill_modified_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn compare_skill_modified(a: Option<SystemTime>, b: Option<SystemTime>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

fn compare_skill_candidate(
    candidate_precedence: Option<u32>,
    candidate_modified: Option<SystemTime>,
    existing_precedence: Option<u32>,
    existing_modified: Option<SystemTime>,
) -> Ordering {
    if let (Some(candidate), Some(existing)) = (candidate_precedence, existing_precedence) {
        match candidate.cmp(&existing).reverse() {
            Ordering::Equal => {}
            ordering => return ordering,
        }
    }
    compare_skill_modified(candidate_modified, existing_modified)
}

fn collision_message(name: &str, kept_path: &Path, ignored_path: &Path, reason: &str) -> String {
    format!(
        "name \"{name}\" collision — keeping {} over {} ({reason})",
        kept_path.display(),
        ignored_path.display()
    )
}

fn read_skill_discovery_content(path: &Path) -> Result<String, SkillDiagnostic> {
    let mut file = fs::File::open(path).map_err(|error| SkillDiagnostic {
        path: path.to_owned(),
        kind: DiagnosticKind::Warning,
        message: format!("failed to read: {error}"),
    })?;
    let total_bytes = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_SKILL_DISCOVERY_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: format!("failed to read: {error}"),
        })?;

    let truncated = MAX_SKILL_DISCOVERY_BYTES < bytes.len();
    if truncated {
        bytes.truncate(MAX_SKILL_DISCOVERY_BYTES);
    }
    let content = String::from_utf8_lossy(&bytes).into_owned();
    if truncated && has_unclosed_frontmatter(&content) {
        return Err(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: format!(
                "frontmatter closing fence was not found before the {MAX_SKILL_DISCOVERY_BYTES} byte discovery read limit; file has {total_bytes} bytes"
            ),
        });
    }

    Ok(content)
}

// ---------------------------------------------------------------------------
// Multi-directory loading
// ---------------------------------------------------------------------------

/// Load skills from multiple directories, deduplicating by name.
///
/// The newest skill file with a given name wins; collisions produce a
/// diagnostic. Output skills are sorted by name so successive runs see the same
/// order.
pub fn load_skills_from_dirs(dirs: &[PathBuf]) -> LoadSkillsResult {
    load_skills_from_dirs_with_entry_limit(dirs, MAX_SKILL_DISCOVERY_ENTRIES_PER_DIR)
}

fn load_skills_from_dirs_with_entry_limit(
    dirs: &[PathBuf],
    entry_limit: usize,
) -> LoadSkillsResult {
    let dirs = dirs
        .iter()
        .cloned()
        .map(|path| SkillDir {
            path,
            add_to_prompt_by_default: false,
            source_precedence: None,
        })
        .collect::<Vec<_>>();
    load_skills_from_skill_dirs_with_entry_limit(&dirs, entry_limit)
}

/// Load skills from scoped directories, deduplicating by name.
///
/// Directory scope can force skills into the initial prompt, which is
/// useful for project-local skills that are likely relevant to the
/// current repository. When both colliding roots provide explicit source
/// precedence, lower precedence wins before modified time is considered.
pub fn load_skills_from_skill_dirs(dirs: &[SkillDir]) -> LoadSkillsResult {
    load_skills_from_skill_dirs_with_entry_limit(dirs, MAX_SKILL_DISCOVERY_ENTRIES_PER_DIR)
}

fn load_skills_from_skill_dirs_with_entry_limit(
    dirs: &[SkillDir],
    entry_limit: usize,
) -> LoadSkillsResult {
    let mut skills_by_name: BTreeMap<String, SelectedSkill> = BTreeMap::new();
    let mut all_diagnostics = Vec::new();

    for dir in dirs {
        let (paths, discovery_diagnostics) =
            discover_skill_paths_with_entry_limit(&dir.path, entry_limit);
        all_diagnostics.extend(discovery_diagnostics);
        for path in paths {
            let content = match read_skill_discovery_content(&path) {
                Ok(content) => content,
                Err(diagnostic) => {
                    all_diagnostics.push(diagnostic);
                    continue;
                }
            };

            let (skill, diags) = load_skill_from_content(&content, &path);
            all_diagnostics.extend(diags);

            if let Some(mut skill) = skill {
                if !skill.add_to_prompt_explicit {
                    skill.add_to_prompt |= dir.add_to_prompt_by_default;
                }
                let modified = skill_modified_time(&skill.file_path);
                if let Some(existing) = skills_by_name.get_mut(&skill.name) {
                    let ordering = compare_skill_candidate(
                        dir.source_precedence,
                        modified,
                        existing.source_precedence,
                        existing.modified,
                    );
                    if ordering == Ordering::Greater {
                        let reason = if dir.source_precedence != existing.source_precedence
                            && dir.source_precedence.is_some()
                            && existing.source_precedence.is_some()
                        {
                            "higher-priority skill root"
                        } else {
                            "newer modified time"
                        };
                        let message = collision_message(
                            &skill.name,
                            &skill.file_path,
                            &existing.skill.file_path,
                            reason,
                        );
                        all_diagnostics.push(SkillDiagnostic {
                            path: existing.skill.file_path.clone(),
                            kind: DiagnosticKind::Collision,
                            message,
                        });
                        *existing = SelectedSkill {
                            skill,
                            modified,
                            source_precedence: dir.source_precedence,
                        };
                    } else {
                        let reason = if dir.source_precedence != existing.source_precedence
                            && dir.source_precedence.is_some()
                            && existing.source_precedence.is_some()
                        {
                            "higher-priority skill root"
                        } else if ordering == Ordering::Equal {
                            "same or unavailable modified time"
                        } else {
                            "newer modified time"
                        };
                        all_diagnostics.push(SkillDiagnostic {
                            path: skill.file_path.clone(),
                            kind: DiagnosticKind::Collision,
                            message: collision_message(
                                &skill.name,
                                &existing.skill.file_path,
                                &skill.file_path,
                                reason,
                            ),
                        });
                    }
                } else {
                    skills_by_name.insert(
                        skill.name.clone(),
                        SelectedSkill {
                            skill,
                            modified,
                            source_precedence: dir.source_precedence,
                        },
                    );
                }
            }
        }
    }

    LoadSkillsResult {
        skills: skills_by_name
            .into_values()
            .map(|selected| selected.skill)
            .collect(),
        diagnostics: all_diagnostics,
    }
}

/// Load skills from a single directory.
pub fn load_skills_from_dir(dir: &Path) -> LoadSkillsResult {
    load_skills_from_dirs(&[dir.to_owned()])
}

fn render_built_in_skill_content(content: &'static str) -> Cow<'static, str> {
    if content.contains(SELF_KNOWLEDGE_VERSION_TOKEN) {
        Cow::Owned(content.replace(SELF_KNOWLEDGE_VERSION_TOKEN, TAU_VERSION))
    } else {
        Cow::Borrowed(content)
    }
}

/// Load Tau's compile-time bundled self-knowledge skills.
///
/// Built-ins are stored as normal Markdown skill files in this crate and
/// embedded with `include_str!`, but they intentionally do not expose an
/// on-disk path to callers.
pub fn built_in_skills() -> Vec<BuiltInSkill> {
    BUILT_IN_SKILL_SOURCES
        .iter()
        .map(|source| {
            let path = Path::new(source.diagnostic_path);
            let content = render_built_in_skill_content(source.content);
            let (skill, diagnostics) = load_skill_from_content(&content, path);
            let fatal = diagnostics
                .iter()
                .find(|diagnostic| diagnostic.kind == DiagnosticKind::Skipped);
            if let Some(diagnostic) = fatal {
                panic!(
                    "invalid built-in skill {}: {}",
                    source.diagnostic_path, diagnostic.message
                );
            }
            let skill = skill.unwrap_or_else(|| {
                panic!(
                    "invalid built-in skill {}: missing skill",
                    source.diagnostic_path
                )
            });
            BuiltInSkill {
                name: skill.name,
                description: skill.description,
                content,
                add_to_prompt: skill.add_to_prompt,
                user_invocable: skill.user_invocable,
                disable_model_invocation: skill.disable_model_invocation,
                argument_hint: skill.argument_hint,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
