use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A validated, loaded skill.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    /// When true, the skill is listed in the system prompt for model-initiated
    /// invocation.  Corresponds to `!disable-model-invocation` in frontmatter.
    pub add_to_prompt: bool,
}

/// Non-fatal diagnostic emitted during skill loading.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillDiagnostic {
    pub path: PathBuf,
    pub kind: DiagnosticKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticKind {
    Warning,
    Collision,
    Skipped,
}

/// Result of loading skills from one or more directories.
pub struct LoadSkillsResult {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<SkillDiagnostic>,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;
const SKILL_FILENAME: &str = "SKILL.md";

// ---------------------------------------------------------------------------
// Frontmatter parsing
// ---------------------------------------------------------------------------

/// Parse YAML-style frontmatter delimited by `---` lines.
///
/// Returns a map of key→value pairs and the body (content after the closing
/// `---`).  If no frontmatter is present, returns an empty map and the full
/// content as body.
pub fn parse_frontmatter(content: &str) -> (BTreeMap<String, String>, &str) {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);

    let Some(rest) = content.strip_prefix("---") else {
        return (BTreeMap::new(), content);
    };
    let Some(rest) = rest.strip_prefix(['\n', '\r']) else {
        return (BTreeMap::new(), content);
    };

    // Find closing ---
    let Some(end_offset) = find_closing_fence(rest) else {
        return (BTreeMap::new(), content);
    };

    let yaml_block = &rest[..end_offset];
    let body_start = end_offset + 3; // skip "---"
    let body = rest[body_start..]
        .strip_prefix(['\n', '\r'])
        .unwrap_or(&rest[body_start..]);

    let mut map = BTreeMap::new();
    for line in yaml_block.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = parse_frontmatter_line(trimmed) {
            map.insert(key.to_owned(), value.to_owned());
        }
    }

    (map, body)
}

/// Strip frontmatter and return only the body.
pub fn strip_frontmatter(content: &str) -> &str {
    parse_frontmatter(content).1
}

fn find_closing_fence(s: &str) -> Option<usize> {
    let mut pos = 0;
    for line in s.lines() {
        if line.trim() == "---" {
            return Some(pos);
        }
        pos += line.len() + 1; // +1 for newline
    }
    None
}

fn parse_frontmatter_line(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    let key = line[..colon].trim();
    if key.is_empty() {
        return None;
    }
    let value = line[colon + 1..].trim();
    // Strip surrounding quotes
    let value = strip_quotes(value);
    Some((key, value))
}

fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        return &s[1..s.len() - 1];
    }
    s
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn validate_name(name: &str, parent_dir_name: Option<&str>, path: &Path) -> Vec<SkillDiagnostic> {
    let mut diagnostics = Vec::new();

    if let Some(parent) = parent_dir_name {
        if name != parent {
            diagnostics.push(SkillDiagnostic {
                path: path.to_owned(),
                kind: DiagnosticKind::Warning,
                message: format!("name \"{name}\" does not match parent directory \"{parent}\""),
            });
        }
    }

    if name.len() > MAX_NAME_LENGTH {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: format!("name exceeds {MAX_NAME_LENGTH} characters ({})", name.len()),
        });
    }

    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_owned(),
        });
    }

    if name.starts_with('-') || name.ends_with('-') {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: "name must not start or end with a hyphen".to_owned(),
        });
    }

    if name.contains("--") {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: "name must not contain consecutive hyphens".to_owned(),
        });
    }

    diagnostics
}

fn validate_description(description: &str, path: &Path) -> Vec<SkillDiagnostic> {
    let mut diagnostics = Vec::new();
    if description.len() > MAX_DESCRIPTION_LENGTH {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: format!(
                "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
                description.len()
            ),
        });
    }
    diagnostics
}

// ---------------------------------------------------------------------------
// Single-file loading
// ---------------------------------------------------------------------------

/// Load a single skill from file content and its path on disk.
///
/// Returns `None` for the skill if the description is missing (the one hard
/// requirement).  Diagnostics are returned in all cases.
pub fn load_skill_from_content(
    content: &str,
    file_path: &Path,
) -> (Option<Skill>, Vec<SkillDiagnostic>) {
    let mut diagnostics = Vec::new();
    let (fm, _body) = parse_frontmatter(content);

    let skill_dir = file_path.parent().unwrap_or(file_path);
    let parent_dir_name = skill_dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned);

    // Name: frontmatter > parent directory name
    let name = fm
        .get("name")
        .cloned()
        .or(parent_dir_name.clone())
        .unwrap_or_default();

    // Validate name
    diagnostics.extend(validate_name(&name, parent_dir_name.as_deref(), file_path));

    // Description (required)
    let description = fm.get("description").map(|s| s.trim().to_owned());
    match &description {
        None => {
            diagnostics.push(SkillDiagnostic {
                path: file_path.to_owned(),
                kind: DiagnosticKind::Skipped,
                message: "description is required".to_owned(),
            });
            return (None, diagnostics);
        }
        Some(d) if d.is_empty() => {
            diagnostics.push(SkillDiagnostic {
                path: file_path.to_owned(),
                kind: DiagnosticKind::Skipped,
                message: "description is required".to_owned(),
            });
            return (None, diagnostics);
        }
        Some(d) => {
            diagnostics.extend(validate_description(d, file_path));
        }
    }

    let disable_model_invocation = fm
        .get("disable-model-invocation")
        .map(|v| v == "true")
        .unwrap_or(false);

    let skill = Skill {
        name,
        description: description.unwrap_or_default(),
        file_path: file_path.to_owned(),
        base_dir: skill_dir.to_owned(),
        add_to_prompt: !disable_model_invocation,
    };

    (Some(skill), diagnostics)
}

// ---------------------------------------------------------------------------
// Directory scanning
// ---------------------------------------------------------------------------

/// Discover skill file paths under `root` using Pi-style discovery rules:
///
/// 1. If a directory contains `SKILL.md`, that file is the skill — stop
///    recursing into that directory.
/// 2. Otherwise, at root level only, treat direct `.md` children as individual
///    skills.
/// 3. Recurse into subdirectories to find `SKILL.md`.
/// 4. Skip dot-prefixed entries and `node_modules`.
pub fn discover_skill_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    discover_skill_paths_inner(root, true, &mut paths);
    paths
}

fn discover_skill_paths_inner(dir: &Path, is_root: bool, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut children: Vec<fs::DirEntry> = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        children.push(entry);
    }

    // Check for SKILL.md first
    let skill_md = dir.join(SKILL_FILENAME);
    if skill_md.is_file() {
        out.push(skill_md);
        return; // don't recurse further
    }

    for entry in &children {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };

        // Skip hidden entries and node_modules
        if name_str.starts_with('.') || name_str == "node_modules" {
            continue;
        }

        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if file_type.is_dir() || file_type.is_symlink() {
            // For symlinks, check if they point to a directory
            let is_dir = if file_type.is_symlink() {
                path.is_dir()
            } else {
                true
            };
            if is_dir {
                discover_skill_paths_inner(&path, false, out);
            }
        } else if file_type.is_file() && is_root && name_str.ends_with(".md") {
            // Root-level .md files are individual skills
            out.push(path);
        }
    }
}

// ---------------------------------------------------------------------------
// Multi-directory loading
// ---------------------------------------------------------------------------

/// Load skills from multiple directories, deduplicating by name.
///
/// The first skill with a given name wins; collisions produce a diagnostic.
pub fn load_skills_from_dirs(dirs: &[PathBuf]) -> LoadSkillsResult {
    let mut skills_by_name: HashMap<String, Skill> = HashMap::new();
    let mut all_diagnostics = Vec::new();

    for dir in dirs {
        let paths = discover_skill_paths(dir);
        for path in paths {
            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    all_diagnostics.push(SkillDiagnostic {
                        path: path.clone(),
                        kind: DiagnosticKind::Warning,
                        message: format!("failed to read: {e}"),
                    });
                    continue;
                }
            };

            let (skill, diags) = load_skill_from_content(&content, &path);
            all_diagnostics.extend(diags);

            if let Some(skill) = skill {
                if let Some(existing) = skills_by_name.get(&skill.name) {
                    all_diagnostics.push(SkillDiagnostic {
                        path: skill.file_path.clone(),
                        kind: DiagnosticKind::Collision,
                        message: format!(
                            "name \"{}\" collision — keeping {}",
                            skill.name,
                            existing.file_path.display()
                        ),
                    });
                } else {
                    skills_by_name.insert(skill.name.clone(), skill);
                }
            }
        }
    }

    LoadSkillsResult {
        skills: skills_by_name.into_values().collect(),
        diagnostics: all_diagnostics,
    }
}

/// Load skills from a single directory.
pub fn load_skills_from_dir(dir: &Path) -> LoadSkillsResult {
    load_skills_from_dirs(&[dir.to_owned()])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Frontmatter parsing ------------------------------------------------

    #[test]
    fn parse_frontmatter_basic() {
        let content = "---\nname: my-skill\ndescription: Does things\n---\n# Body\n";
        let (fm, body) = parse_frontmatter(content);
        assert_eq!(fm.get("name").map(String::as_str), Some("my-skill"));
        assert_eq!(
            fm.get("description").map(String::as_str),
            Some("Does things")
        );
        assert_eq!(body, "# Body\n");
    }

    #[test]
    fn parse_frontmatter_quoted_values() {
        let content = "---\nname: \"my-skill\"\ndescription: 'A quoted description'\n---\nBody";
        let (fm, body) = parse_frontmatter(content);
        assert_eq!(fm.get("name").map(String::as_str), Some("my-skill"));
        assert_eq!(
            fm.get("description").map(String::as_str),
            Some("A quoted description")
        );
        assert_eq!(body, "Body");
    }

    #[test]
    fn parse_frontmatter_boolean_field() {
        let content =
            "---\nname: hidden\ndescription: A hidden skill\ndisable-model-invocation: true\n---\n";
        let (fm, _body) = parse_frontmatter(content);
        assert_eq!(
            fm.get("disable-model-invocation").map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn parse_frontmatter_none_when_missing() {
        let content = "# No frontmatter\nJust body content.";
        let (fm, body) = parse_frontmatter(content);
        assert!(fm.is_empty());
        assert_eq!(body, content);
    }

    #[test]
    fn parse_frontmatter_unclosed() {
        let content = "---\nname: broken\nno closing fence";
        let (fm, body) = parse_frontmatter(content);
        assert!(fm.is_empty());
        assert_eq!(body, content);
    }

    #[test]
    fn parse_frontmatter_bom() {
        let content = "\u{feff}---\nname: bom-skill\ndescription: Has BOM\n---\nBody";
        let (fm, body) = parse_frontmatter(content);
        assert_eq!(fm.get("name").map(String::as_str), Some("bom-skill"));
        assert_eq!(body, "Body");
    }

    #[test]
    fn parse_frontmatter_comments_and_blanks() {
        let content = "---\n# comment\n\nname: foo\ndescription: bar\n---\n";
        let (fm, _body) = parse_frontmatter(content);
        assert_eq!(fm.get("name").map(String::as_str), Some("foo"));
        assert_eq!(fm.get("description").map(String::as_str), Some("bar"));
    }

    // -- Skill loading from content -----------------------------------------

    #[test]
    fn load_skill_valid() {
        let content = "---\nname: my-skill\ndescription: Does useful things\n---\n# Instructions";
        let path = Path::new("/skills/my-skill/SKILL.md");
        let (skill, diags) = load_skill_from_content(content, path);
        let skill = skill.expect("should load");
        assert_eq!(skill.name, "my-skill");
        assert_eq!(skill.description, "Does useful things");
        assert!(skill.add_to_prompt);
        assert!(diags.is_empty());
    }

    #[test]
    fn load_skill_missing_description() {
        let content = "---\nname: no-desc\n---\n# Body";
        let path = Path::new("/skills/no-desc/SKILL.md");
        let (skill, diags) = load_skill_from_content(content, path);
        assert!(skill.is_none());
        assert!(diags.iter().any(|d| d.kind == DiagnosticKind::Skipped));
    }

    #[test]
    fn load_skill_empty_description() {
        let content = "---\nname: empty\ndescription:\n---\nBody";
        let path = Path::new("/skills/empty/SKILL.md");
        let (skill, diags) = load_skill_from_content(content, path);
        assert!(skill.is_none());
        assert!(diags.iter().any(|d| d.kind == DiagnosticKind::Skipped));
    }

    #[test]
    fn load_skill_disable_model_invocation() {
        let content =
            "---\nname: hidden\ndescription: A hidden skill\ndisable-model-invocation: true\n---\n";
        let path = Path::new("/skills/hidden/SKILL.md");
        let (skill, _diags) = load_skill_from_content(content, path);
        let skill = skill.expect("should load");
        assert!(!skill.add_to_prompt);
    }

    #[test]
    fn load_skill_name_fallback_to_parent_dir() {
        let content = "---\ndescription: Inferred name\n---\n";
        let path = Path::new("/skills/inferred-name/SKILL.md");
        let (skill, _diags) = load_skill_from_content(content, path);
        let skill = skill.expect("should load");
        assert_eq!(skill.name, "inferred-name");
    }

    #[test]
    fn load_skill_name_mismatch_warning() {
        let content = "---\nname: wrong-name\ndescription: Mismatch test\n---\n";
        let path = Path::new("/skills/actual-dir/SKILL.md");
        let (_skill, diags) = load_skill_from_content(content, path);
        assert!(diags.iter().any(|d| d.message.contains("does not match")));
    }

    #[test]
    fn load_skill_invalid_name_chars() {
        let content = "---\nname: Bad_Name\ndescription: Invalid chars\n---\n";
        let path = Path::new("/skills/Bad_Name/SKILL.md");
        let (_skill, diags) = load_skill_from_content(content, path);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("invalid characters"))
        );
    }

    // -- Directory scanning -------------------------------------------------

    #[test]
    fn discover_skill_md_in_subdir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let skill_dir = tmp.path().join("my-skill");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: Test\n---\n",
        )
        .expect("write");

        let paths = discover_skill_paths(tmp.path());
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("my-skill/SKILL.md"));
    }

    #[test]
    fn discover_root_md_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::write(
            tmp.path().join("standalone.md"),
            "---\nname: standalone\ndescription: A standalone skill\n---\n",
        )
        .expect("write");

        let paths = discover_skill_paths(tmp.path());
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("standalone.md"));
    }

    #[test]
    fn discover_skips_dot_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hidden = tmp.path().join(".hidden");
        fs::create_dir_all(&hidden).expect("mkdir");
        fs::write(
            hidden.join("SKILL.md"),
            "---\nname: hidden\ndescription: Should be skipped\n---\n",
        )
        .expect("write");

        let paths = discover_skill_paths(tmp.path());
        assert!(paths.is_empty());
    }

    #[test]
    fn discover_skips_node_modules() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let nm = tmp.path().join("node_modules").join("some-skill");
        fs::create_dir_all(&nm).expect("mkdir");
        fs::write(
            nm.join("SKILL.md"),
            "---\nname: some-skill\ndescription: Should be skipped\n---\n",
        )
        .expect("write");

        let paths = discover_skill_paths(tmp.path());
        assert!(paths.is_empty());
    }

    #[test]
    fn discover_does_not_recurse_past_skill_md() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let parent = tmp.path().join("parent");
        let child = parent.join("child");
        fs::create_dir_all(&child).expect("mkdir");
        fs::write(
            parent.join("SKILL.md"),
            "---\nname: parent\ndescription: Parent skill\n---\n",
        )
        .expect("write");
        fs::write(
            child.join("SKILL.md"),
            "---\nname: child\ndescription: Should not be found\n---\n",
        )
        .expect("write");

        let paths = discover_skill_paths(tmp.path());
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("parent/SKILL.md"));
    }

    #[test]
    fn discover_nonexistent_dir() {
        let paths = discover_skill_paths(Path::new("/nonexistent/path"));
        assert!(paths.is_empty());
    }

    // -- Multi-directory loading --------------------------------------------

    #[test]
    fn load_from_dirs_dedup() {
        let dir1 = tempfile::tempdir().expect("tempdir");
        let dir2 = tempfile::tempdir().expect("tempdir");

        let s1 = dir1.path().join("my-skill");
        fs::create_dir_all(&s1).expect("mkdir");
        fs::write(
            s1.join("SKILL.md"),
            "---\nname: my-skill\ndescription: First\n---\n",
        )
        .expect("write");

        let s2 = dir2.path().join("my-skill");
        fs::create_dir_all(&s2).expect("mkdir");
        fs::write(
            s2.join("SKILL.md"),
            "---\nname: my-skill\ndescription: Second\n---\n",
        )
        .expect("write");

        let result = load_skills_from_dirs(&[dir1.path().to_owned(), dir2.path().to_owned()]);
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].description, "First");
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.kind == DiagnosticKind::Collision)
        );
    }

    #[test]
    fn load_from_empty_dirs() {
        let result = load_skills_from_dirs(&[]);
        assert!(result.skills.is_empty());
        assert!(result.diagnostics.is_empty());
    }

    // -- strip_frontmatter --------------------------------------------------

    #[test]
    fn strip_frontmatter_returns_body() {
        let content = "---\nname: x\n---\nThe body.";
        assert_eq!(strip_frontmatter(content), "The body.");
    }

    #[test]
    fn strip_frontmatter_no_frontmatter() {
        let content = "Just content.";
        assert_eq!(strip_frontmatter(content), "Just content.");
    }
}
