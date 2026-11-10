use std::path::Path;

use super::*;
use crate::config::ShellConfig;
use crate::tools::world::ShellWorld;

/// Real dispatch failures keep diagnostics in shared-rendered payloads while
/// retaining separate UI labels and omitting rejected argument maps.
#[test]
fn dispatched_failures_render_identifiers_and_explanations() {
    for (tool, arguments, expected) in [
        (
            crate::tools::FIND_TOOL_NAME,
            CborValue::Map(vec![
                (
                    CborValue::Text("pattern".into()),
                    CborValue::Text("*".into()),
                ),
                (CborValue::Text("limit".into()), CborValue::Bool(false)),
            ]),
            "argument `limit` must be an integer",
        ),
        (
            crate::tools::REPLACE_TOOL_NAME,
            CborValue::Map(vec![
                (
                    CborValue::Text("path".into()),
                    CborValue::Text("private\n\u{1b}".into()),
                ),
                (
                    CborValue::Text("legacy".into()),
                    CborValue::Text("do not echo".into()),
                ),
            ]),
            "request contains an unknown field",
        ),
    ] {
        let invoke = tau_proto::ToolStarted {
            invocation_policy: Default::default(),
            call_id: "conformance".into(),
            tool_name: tau_proto::ToolName::new(tool),
            arguments,
            agent_id: "agent".parse().expect("agent"),
            originator: Default::default(),
        };
        let events = crate::tools::execute_tool(invoke, ShellWorld::real());
        let tau_proto::Event::ToolError(error) = events.last().expect("terminal") else {
            panic!("expected failure");
        };
        let mut rendered = String::new();
        tau_proto::write_provider_tool_result_text(
            error.details.as_ref().expect("diagnostic"),
            tau_proto::ProviderToolResultStatus::Error {
                message: &error.message,
            },
            &mut rendered,
        )
        .expect("shared renderer");
        assert_eq!(rendered, format!("error: shell-tool-failed\n\n{expected}"));
        assert_eq!(error.call_id.as_str(), "conformance");
        assert_eq!(error.tool_name.as_str(), tool);
        assert_eq!(error.display.as_ref().expect("UI").status_text, expected);
    }
}

/// Ensures error chips use the trimmed first meaningful line, leaving later
/// detail and any renderer-specific abbreviation outside tool-side formatting.
#[test]
fn error_chip_text_keeps_full_first_line_for_renderer_abbreviation() {
    let message = "\n \t\n  failed to load extension configuration because the selected profile \
                   references a missing credential source and requires operator action before \
                   retrying  \nignored detail";
    let failure = ToolFailure::new(message);

    assert_eq!(
        failure.display.status_text,
        "failed to load extension configuration because the selected profile references a missing \
         credential source and requires operator action before retrying"
    );
    assert!(!failure.display.status_text.contains("ignored detail"));
}

/// Native producers expose real truncation/limit and recovery facts without
/// repeating stdout or file bodies in full-mode tool presentation.
#[test]
fn native_output_limit_ui_keeps_recovery_without_output_duplication() {
    use crate::tools::world::ShellWorld;
    use crate::tools::{find, grep, ls, read, shell};
    let directory = tempfile::tempdir().expect("fixture directory");
    let path = directory.path().join("data.txt");
    std::fs::write(&path, "retry item\n".repeat(3000)).expect("fixture content");
    std::fs::write(directory.path().join("other.txt"), "retry other\n").expect("other fixture");
    let file_args = CborValue::Map(vec![(
        CborValue::Text("path".into()),
        CborValue::Text(path.display().to_string()),
    )]);
    let list_args = CborValue::Map(vec![
        (
            CborValue::Text("path".into()),
            CborValue::Text(directory.path().display().to_string()),
        ),
        (
            CborValue::Text("limit".into()),
            CborValue::Integer(1.into()),
        ),
    ]);
    let mut search_args = list_args.clone();
    let CborValue::Map(entries) = &mut search_args else {
        unreachable!()
    };
    entries.push((
        CborValue::Text("pattern".into()),
        CborValue::Text("*.txt".into()),
    ));
    let found = find::run_find(&search_args).expect("find");
    let CborValue::Map(entries) = &mut search_args else {
        unreachable!()
    };
    entries.retain(|(key, _)| key != &CborValue::Text("pattern".into()));
    entries.push((
        CborValue::Text("pattern".into()),
        CborValue::Text("retry".into()),
    ));
    let outputs = [
        (
            "read",
            read::read_file(&file_args, &mut ShellWorld::real()).expect("read"),
        ),
        (
            "ls",
            ls::run_ls(&list_args, &mut ShellWorld::real()).expect("ls"),
        ),
        ("find", found),
        ("grep", grep::run_grep(&search_args).expect("grep")),
    ];
    let mut fixtures = Vec::new();
    let invalid_path = directory.path().join("invalid.txt");
    std::fs::write(&invalid_path, [b'a', 0xff]).expect("invalid UTF-8 fixture");
    let invalid = read::read_file(
        &CborValue::Map(vec![(
            CborValue::Text("path".into()),
            CborValue::Text(invalid_path.display().to_string()),
        )]),
        &mut ShellWorld::real(),
    )
    .expect("read invalid UTF-8");
    assert_eq!(invalid.display.info_chips, ["invalid UTF-8"]);
    assert!(invalid.display.payload.is_none());
    fixtures.push(serde_json::json!({"case":"read-invalid-utf8", "tool_name":"read", "display":invalid.display}));
    for (tool, output) in outputs {
        assert!(!output.display.info_chips.is_empty(), "{tool} qualifier");
        let Some(ToolUsePayload::Text { text }) = &output.display.payload else {
            panic!("{tool} recovery detail")
        };
        assert!(
            !text.contains("retry item"),
            "{tool} must not duplicate contents"
        );
        match tool {
            "read" => assert_saved_output_detail(&output.display, &output.result),
            "ls" | "find" => assert!(text.contains("limit_reached: true"), "{tool} limit"),
            "grep" => assert!(text.contains("1 matches limit reached."), "grep limit"),
            _ => unreachable!(),
        }
        fixtures.push(serde_json::json!({"case":format!("{tool}-limited"), "tool_name":tool, "display":output.display}));
    }
    for (name, tail) in [
        ("shell-truncated", ""),
        ("shell-error-truncated", "; exit 7"),
    ] {
        let command = format!("printf '%030000d\\n' 1\nprintf 'done\\n'{tail}");
        let args = CborValue::Map(vec![(
            CborValue::Text("command".into()),
            CborValue::Text(command.clone()),
        )]);
        let shell::CommandOutcome::Finished(output) = shell::run_command_cancellable(
            name,
            &args,
            &ShellConfig::default(),
            shell::ShellCommandMode::READ_WRITE_HIDDEN,
            false,
            None,
            &mut ShellWorld::real(),
        )
        .expect("shell") else {
            panic!("finished shell")
        };
        assert_eq!(output.display.info_chips, ["truncated"]);
        assert_saved_output_detail(&output.display, &output.result);
        let Some(ToolUsePayload::Text { text }) = &output.display.payload else {
            panic!("command and recovery")
        };
        assert!(
            text.starts_with(&command),
            "full command must survive recovery details"
        );
        assert_eq!(
            output.display.status,
            if tail.is_empty() {
                ToolUseStatus::Success
            } else {
                ToolUseStatus::Error
            }
        );
        fixtures
            .push(serde_json::json!({"case":name, "tool_name":"shell", "display":output.display}));
    }
    if let Ok(directory) = std::env::var("TAU_TOOL_UI_FIXTURE_DIR") {
        std::fs::write(
            Path::new(&directory).join("shell-files.json"),
            serde_json::to_vec_pretty(&fixtures).expect("serialize samples"),
        )
        .expect("write samples");
    }
}

/// Require exact complete-output recovery path and totals from the native
/// result, not merely the presence of some arbitrary text body.
fn assert_saved_output_detail(display: &ToolUseState, result: &CborValue) {
    let Some(ToolUsePayload::Text { text }) = &display.payload else {
        panic!("recovery body")
    };
    let path =
        tau_proto::cbor_text_field(result, "full_output_path").expect("complete saved output");
    assert!(text.contains(&format!("full_output_path: {path}")));
    for key in ["total_lines", "total_bytes"] {
        let count = tau_proto::cbor_int_field(result, key).expect("native total");
        assert!(text.contains(&format!("{key}: {count}")), "{key}");
    }
}
