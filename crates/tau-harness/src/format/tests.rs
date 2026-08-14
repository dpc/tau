use tau_proto::{ContentPart, ContextItem, ContextRole, MessageItem};

use super::*;

fn user_entry(text: &str) -> AgentEntry {
    AgentEntry::UserInput {
        items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::User,
            content: vec![ContentPart::Text {
                text: text.to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        submission_source: None,
        inference_activation: true,
    }
}

/// Hostile prompt previews preserve the source-scalar window while making
/// every terminal control, layout control, and escape-spoofing backslash
/// visible.
#[test]
fn tree_preview_visibly_encodes_hostile_source_scalars() {
    let cases = [
        ("csi", "A\u{1b}[2JB", r"user: A\u{001B}[2JB"),
        (
            "sgr",
            "A\u{1b}[31mred\u{1b}[0mB",
            r"user: A\u{001B}[31mred\u{001B}[0mB",
        ),
        (
            "osc with bel",
            "A\u{1b}]52;c;YQ==\u{7}B",
            r"user: A\u{001B}]52;c;YQ==\u{0007}B",
        ),
        (
            "osc with st",
            "A\u{1b}]0;t\u{1b}\\B",
            r"user: A\u{001B}]0;t\u{001B}\\B",
        ),
        (
            "dcs with st",
            "A\u{1b}Pq\u{1b}\\B",
            r"user: A\u{001B}Pq\u{001B}\\B",
        ),
        (
            "c1",
            "A\u{9b}B\u{9d}C\u{90}D\u{9c}E",
            r"user: A\u{009B}B\u{009D}C\u{0090}D\u{009C}E",
        ),
        (
            "c0 and delimiters",
            "left\rforged\nnext\t\u{b}\u{c}\0end",
            r"user: left\u{000D}forged next\u{0009}\u{000B}\u{000C}\u{0000}end",
        ),
        (
            "bidi and default ignorables",
            "\u{202e}A\u{2066}B\u{2069}\u{200e}\u{200f}\u{2028}\u{2029}",
            r"user: \u{202E}A\u{2066}B\u{2069}\u{200E}\u{200F}\u{2028}\u{2029}",
        ),
        (
            "mixed unicode",
            "e\u{301} 雪 🦀 سلام",
            "user: e\u{301} 雪 🦀 سلام",
        ),
        ("emoji joiner", "👩\u{200d}💻", r"user: 👩\u{200D}💻"),
        ("non-bmp tag", "A\u{e0001}B", r"user: A\u{E0001}B"),
        ("literal escape spoof", r"\u{001B}\", r"user: \\u{001B}\\"),
        (
            "row marker spoof",
            "\n    9 * before prompt\rspoof",
            r"user:      9 * before prompt\u{000D}spoof",
        ),
    ];

    for (name, prompt, expected) in cases {
        let preview = render_entry_preview(&user_entry(prompt));
        assert_eq!(preview, expected, "{name}");
        assert!(
            preview
                .chars()
                .all(|character| !tau_proto::requires_visible_escape(character)),
            "{name} retained an unsafe scalar: {preview:?}"
        );
    }
}

/// Preview truncation counts source scalars before escaping, so expanded
/// escapes remain complete and the existing 60-scalar boundary does not
/// move.
#[test]
fn tree_preview_escapes_after_selecting_source_scalar_window() {
    let fits = "x".repeat(54);
    assert_eq!(
        render_entry_preview(&user_entry(&fits)),
        format!("user: {fits}")
    );

    let overflows = "x".repeat(55);
    assert_eq!(
        render_entry_preview(&user_entry(&overflows)),
        format!("user: {}…", "x".repeat(54))
    );

    let boundary_control = format!("{}\u{1b}[31m", "x".repeat(53));
    assert_eq!(
        render_entry_preview(&user_entry(&boundary_control)),
        format!(r"user: {}\u{{001B}}…", "x".repeat(53))
    );

    let split_osc = format!("{}\u{1b}]52;c;payload\u{7}", "x".repeat(52));
    assert_eq!(
        render_entry_preview(&user_entry(&split_osc)),
        format!(r"user: {}\u{{001B}}]…", "x".repeat(52))
    );
}
