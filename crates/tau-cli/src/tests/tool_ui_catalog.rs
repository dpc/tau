//! Manual, hermetic capture of producer-owned generic UI descriptors.

use std::fmt::Write as _;

use serde::Deserialize;
use tau_proto::ToolUseState;

use super::*;

/// One synthetic example exported by a tool's production display code.
#[derive(Deserialize)]
struct DescriptorFixture {
    /// Stable sample label, including the outcome being demonstrated.
    case: String,
    /// Model-visible tool name.
    tool_name: String,
    /// Actual producer-generated descriptor, not a rendered approximation.
    display: ToolUseState,
    /// Use the existing native-provider projection instead of ordinary tools.
    #[serde(default)]
    native: bool,
}

/// Capture compact/full layouts at both widths through the real renderer while
/// checking theme ownership for every input descriptor. This intentionally
/// takes local fixture paths, so extension owners can verify their displays
/// without copying private transcripts or adding tool-specific CLI logic.
#[test]
#[ignore = "set TAU_TOOL_UI_FIXTURES and TAU_TOOL_UI_CATALOG to capture synthetic samples"]
fn render_tool_ui_catalog() {
    let input = std::env::var("TAU_TOOL_UI_FIXTURES").expect("fixture JSON path");
    let output = std::env::var("TAU_TOOL_UI_CATALOG").expect("catalog Markdown path");
    let fixtures: Vec<DescriptorFixture> =
        serde_json::from_slice(&std::fs::read(input).expect("read fixture JSON"))
            .expect("decode fixture JSON");
    assert!(!fixtures.is_empty());
    let theme = tau_themes::Theme::parse(
        r##"{
        styles: {
            "tool.name": { fg: "blue" },
            "tool.args": { fg: "cyan" },
            "tool.output": { bg: "black" },
            "tool.mode": { fg: "yellow" },
            "tool.range": { fg: "magenta" },
            "tool.status.success": { fg: "green" },
            "tool.status.error": { fg: "red" },
            "tool.status.warning": { fg: "yellow" },
            "tool.status.info": { fg: "dark_cyan" },
        }
    }"##,
    )
    .expect("sample theme");
    let mut catalog = String::from(
        "# Actual tool UI samples\n\nSynthetic producer descriptors, rendered by the generic CLI at 100 and 36 columns. Fixed elapsed time: 0s. Full text is wrapped by the terminal layout, not manually formatted. Every case checks tool-name and text-body theme styles.\n",
    );
    for fixture in fixtures {
        let mut rendered = if fixture.native {
            crate::event_renderer::provider_native_tool_display(
                &tau_proto::ProviderNativeToolStatusUpdate {
                    call_id: fixture.case.clone(),
                    tool_name: tau_proto::ToolName::new(&fixture.tool_name),
                    display: fixture.display.clone(),
                    phase: if fixture.display.status == tau_proto::ToolUseStatus::InProgress {
                        tau_proto::ProviderNativeToolPhase::Started
                    } else {
                        tau_proto::ProviderNativeToolPhase::Completed
                    },
                },
            )
        } else {
            render_tool_use_state(&fixture.tool_name, &fixture.display)
        };
        let status_index = rendered.suffixes.len().saturating_sub(1);
        if !fixture.native {
            rendered.suffixes.insert(
                status_index,
                crate::tool_render::tool_duration_suffix(Duration::ZERO),
            );
        }
        write!(
            &mut catalog,
            "\n## {} ({})\n",
            fixture.case, fixture.tool_name
        )
        .expect("format catalog");
        for width in [100, 36] {
            for full in [false, true] {
                let block = if full {
                    render_tool_block(&theme, &rendered)
                } else {
                    crate::tool_render::render_tool_header_block(&theme, &rendered)
                };
                let cells = priority_header_cells(&block, width);
                assert_eq!(
                    cells[0].style.fg,
                    tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_NAME).fg,
                    "{} name style at {width}",
                    fixture.case
                );
                let header: String = cells.iter().map(|cell| cell.ch).collect();
                for suffix in &rendered.suffixes {
                    use crate::tool_render::ToolStatus;
                    let name = match suffix.status {
                        ToolStatus::Success => tau_themes::names::TOOL_STATUS_SUCCESS,
                        ToolStatus::Error => tau_themes::names::TOOL_STATUS_ERROR,
                        ToolStatus::Warning
                        | ToolStatus::Pending
                        | ToolStatus::Info
                        | ToolStatus::Counter
                        | ToolStatus::Agent => tau_themes::names::TOOL_STATUS_INFO,
                        ToolStatus::Context => tau_themes::names::STATUS_CONTEXT,
                        _ => continue,
                    };
                    let label = if matches!(suffix.status, ToolStatus::Error) {
                        "err"
                    } else {
                        &suffix.text
                    };
                    // Match complete chips: a zero-match counter is not the
                    // zero in a later duration such as `0s`.
                    if let Some((index, _)) = header.match_indices(label).find(|(index, _)| {
                        (*index == 0 || header[..*index].ends_with(' '))
                            && (header[*index + label.len()..].starts_with(' ')
                                || *index + label.len() == header.len()
                                || (label == "err"
                                    && header[*index + label.len()..].starts_with(':')))
                    }) {
                        let column = header[..index].chars().count();
                        assert_eq!(
                            cells[column].style.fg,
                            tau_cli_term::resolve::resolve(&theme, name).fg,
                            "{} {name} at {width}",
                            fixture.case
                        );
                    }
                }
                if full
                    && let Some(tau_proto::ToolUsePayload::Text { text }) = &fixture.display.payload
                    && !text.is_empty()
                {
                    let body = block.priority_line_body_content().expect("text detail");
                    for span in body.spans() {
                        assert_eq!(
                            span.style.fg,
                            tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_ARGS).fg,
                            "{} body style",
                            fixture.case
                        );
                        assert_eq!(
                            span.style.bg,
                            tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_OUTPUT)
                                .bg
                        );
                    }
                }
                let rows = tau_term_screen::layout_block(&block, width);
                let text = rows
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(|cell| cell.ch)
                            .collect::<String>()
                            .trim_end()
                            .to_owned()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                write!(
                    &mut catalog,
                    "\n### {} at {width} columns\n\n```text\n{text}\n```\n",
                    if full { "Full" } else { "Compact" }
                )
                .expect("format catalog");
            }
        }
    }
    std::fs::write(output, catalog).expect("write catalog");
}
