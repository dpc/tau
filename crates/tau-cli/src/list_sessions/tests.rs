use std::path::PathBuf;

use super::*;

fn session(session_id: &str, project_root: impl Into<PathBuf>) -> RunningSession {
    RunningSession {
        session_id: session_id.into(),
        project_root: project_root.into(),
    }
}

/// Bare output remains sorted, deduplicated, headerless, and control-safe even
/// when multiple responsive harnesses report the same current session id.
#[test]
fn human_output_preserves_running_session_list_contract() {
    let output = render(
        &[
            session("z-session", "/work/z"),
            session("a\nsession", "/work/a"),
            session("a\nsession", "/work/duplicate"),
        ],
        &crate::cli::SessionListArgs::default(),
    )
    .expect("human output");

    assert_eq!(output, "a\\nsession\nz-session\n");
}

/// JSON always emits one array with the two required stable fields and retains
/// one record per responsive harness so callers can detect duplicate
/// identities.
#[test]
fn json_output_preserves_each_responsive_harness_record() {
    let output = render(
        &[
            session("same-session", "/work/project"),
            session("same-session", "/work/project"),
        ],
        &crate::cli::SessionListArgs {
            dir: None,
            json: true,
        },
    )
    .expect("JSON output");

    let value: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
    assert_eq!(
        value,
        serde_json::json!([
            {
                "session_id": "same-session",
                "project_root": "/work/project"
            },
            {
                "session_id": "same-session",
                "project_root": "/work/project"
            }
        ])
    );
}

/// Empty JSON results use an explicit array rather than empty stdout or a human
/// placeholder, making zero matches unambiguous to automation.
#[test]
fn json_output_is_an_array_when_no_sessions_match() {
    let output = render(
        &[session("session", "/work/other")],
        &crate::cli::SessionListArgs {
            dir: Some(PathBuf::from("/work/requested")),
            json: true,
        },
    )
    .expect("empty JSON output");

    assert_eq!(output, "[]\n");
}

/// Directory filtering compares the already-canonical roots exactly rather than
/// admitting a parent, child, or textual-prefix path.
#[test]
fn directory_filter_matches_only_the_exact_project_root() {
    let output = render(
        &[
            session("parent", "/work"),
            session("exact", "/work/project"),
            session("child", "/work/project/child"),
            session("prefix", "/work/project-other"),
        ],
        &crate::cli::SessionListArgs {
            dir: Some(PathBuf::from("/work/project")),
            json: false,
        },
    )
    .expect("filtered output");

    assert_eq!(output, "exact\n");
}
