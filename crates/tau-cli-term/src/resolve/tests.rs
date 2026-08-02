use std::process::Command;

use crossterm::QueueableCommand;
use crossterm::style::SetForegroundColor;

use super::convert_color;

const COLOR_LOWERING_CHILD: &str = "TAU_CLI_TERM_COLOR_LOWERING_CHILD";
const COLOR_LOWERING_SENTINEL: &str = "tau-cli-term color lowering verified";
const COLOR_LOWERING_TEST: &str =
    "resolve::tests::named_white_and_grey_lower_to_distinct_ansi_palette_indices";

/// Ensures the named bright-white and ordinary-white theme colors retain their
/// distinct ANSI palette indices even when the parent process sets `NO_COLOR`.
#[test]
fn named_white_and_grey_lower_to_distinct_ansi_palette_indices() {
    if std::env::var_os(COLOR_LOWERING_CHILD).is_none() {
        let output = Command::new(std::env::current_exe().expect("test executable path"))
            .args(["--exact", COLOR_LOWERING_TEST, "--nocapture"])
            .env(COLOR_LOWERING_CHILD, "1")
            .env_remove("NO_COLOR")
            .output()
            .expect("color-lowering child runs");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stderr.contains(COLOR_LOWERING_SENTINEL),
            "color-lowering child failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            stderr
        );
        return;
    }

    let mut output = Vec::new();
    output
        .queue(SetForegroundColor(convert_color(tau_themes::Color::White)))
        .expect("bright-white color emits");
    assert_eq!(output, b"\x1b[38;5;15m");

    output.clear();
    output
        .queue(SetForegroundColor(convert_color(tau_themes::Color::Grey)))
        .expect("ordinary-white color emits");
    assert_eq!(output, b"\x1b[38;5;7m");
    eprintln!("{COLOR_LOWERING_SENTINEL}");
}
