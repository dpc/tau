//! `AGENTS.md` and `AGENTS.*.md` discovery used at `SessionStarted` time.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_AGENTS_FILE_BYTES: u64 = 1024 * 1024;

pub(crate) struct DiscoveredAgentsFile {
    pub(crate) file_path: PathBuf,
    pub(crate) content: String,
}

pub(crate) fn discover_session_agents_files() -> Vec<DiscoveredAgentsFile> {
    let mut roots = Vec::new();
    if let Some(home) = dirs::home_dir() {
        roots.extend(user_agents_roots(&home));
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.extend(ancestor_agents_roots(&cwd));
    }
    discover_agents_files_from_roots(roots)
}

#[cfg(test)]
pub(crate) fn discover_agents_files_from(cwd: &Path) -> Vec<DiscoveredAgentsFile> {
    discover_agents_files_from_roots(ancestor_agents_roots(cwd))
}

pub(crate) fn discover_agents_files_from_roots(
    roots: impl IntoIterator<Item = PathBuf>,
) -> Vec<DiscoveredAgentsFile> {
    let mut seen = std::collections::HashSet::new();
    let mut discovered = Vec::new();
    for dir in roots {
        for candidate in agents_file_candidates(&dir) {
            let Ok(metadata) = fs::metadata(&candidate) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            if MAX_AGENTS_FILE_BYTES < metadata.len() {
                continue;
            }

            let Ok(content) = read_agents_file(&candidate) else {
                continue;
            };
            if content.trim().is_empty() {
                continue;
            }

            let file_path = candidate.canonicalize().unwrap_or(candidate);
            if !seen.insert(file_path.clone()) {
                continue;
            }
            discovered.push(DiscoveredAgentsFile { file_path, content });
        }
    }

    discovered
}

fn read_agents_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut limited = file.by_ref().take(MAX_AGENTS_FILE_BYTES + 1);
    let mut content = String::new();
    limited.read_to_string(&mut content)?;
    if MAX_AGENTS_FILE_BYTES < content.len() as u64 {
        return Err(std::io::Error::other("AGENTS file exceeds safety cap"));
    }
    Ok(content)
}

fn agents_file_candidates(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: silent-filter-map-ok
    let mut candidates = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_agents_file_name)
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|path| agents_file_sort_key(path));
    candidates
}

fn is_agents_file_name(name: &str) -> bool {
    name == "AGENTS.md" || (name.starts_with("AGENTS.") && name.ends_with(".md"))
}

fn agents_file_sort_key(path: &Path) -> (u8, String) {
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: unwrap-or-default
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let rank = if name == "AGENTS.md" { 0 } else { 1 };
    (rank, name.to_owned())
}

fn ancestor_agents_roots(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for dir in ancestor_dirs(cwd) {
        dirs.push(dir.clone());
        dirs.push(dir.join(".agents.local"));
    }
    dirs
}

pub(crate) fn user_agents_roots(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".config").join("agents"),
        home.join(".config").join("agents.local"),
        home.join(".agents"),
        home.join(".agents.local"),
    ]
}

pub(crate) fn ancestor_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = cwd.to_path_buf();
    loop {
        dirs.push(current.clone());
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    dirs.reverse();
    dirs
}
