use std::path::PathBuf;

use super::*;
use crate::cli as path_crate_cli;

fn session(session_id: &str, project_root: impl Into<PathBuf>) -> RunningSession {
    RunningSession {
        session_id: session_id
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        project_root: project_root.into(),
    }
}

/// Bare output remains sorted, deduplicated, and headerless even
/// when multiple responsive harnesses report the same current session id.
#[test]
fn human_output_preserves_running_session_list_contract() {
    let output = render(
        &[
            session("z-session", "/work/z"),
            session("a_session", "/work/a"),
            session("a_session", "/work/duplicate"),
        ],
        &path_crate_cli::SessionListArgs::default(),
    )
    .expect("human output");

    assert_eq!(output, "a_session\nz-session\n");
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

/// Complete discovery stays silent so routine human and machine-readable
/// listings do not gain an unconditional stderr side channel.
#[test]
fn complete_listing_has_no_warning() {
    assert_eq!(incomplete_claim_warning(0), None);
}

/// Partial discovery reports one count-bearing warning without changing either
/// stdout representation.
#[test]
fn incomplete_listing_warning_is_counted_and_pluralized() {
    assert_eq!(
        incomplete_claim_warning(1).as_deref(),
        Some(
            "warning: omitted 1 contended runtime claim that did not complete compatible exact-session admission"
        )
    );
    assert_eq!(
        incomplete_claim_warning(3).as_deref(),
        Some(
            "warning: omitted 3 contended runtime claims that did not complete compatible exact-session admission"
        )
    );
}
