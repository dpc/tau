use super::sanitize_terminal_body;

/// Body text keeps ordinary Unicode, tabs, line feeds, and literal escape prose
/// readable instead of converting them to metadata-style visible escapes.
#[test]
fn preserves_readable_multiline_unicode_and_literal_escape_prose() {
    assert_eq!(
        sanitize_terminal_body("first\t行\nliteral \\u{000A}"),
        "first\t行\nliteral \\u{000A}"
    );
}

/// Complete CSI styling controls disappear atomically so their printable
/// parameter suffixes cannot leak as terminal-looking text fragments.
#[test]
fn removes_complete_csi_sequences_without_parameter_fragments() {
    let rendered = sanitize_terminal_body("plain\u{001B}[31mred\u{001B}[0mplain");
    assert_eq!(rendered, "plainredplain");
    assert!(!rendered.contains("[31m"));
    assert_eq!(sanitize_terminal_body("left\u{009B}31mright"), "leftright");
    assert_eq!(sanitize_terminal_body("left\u{001B}(0right"), "leftright");
}

/// OSC and string-family controls consume their complete payloads for both
/// seven-bit and C1 forms, leaving ordinary surrounding body text readable.
#[test]
fn removes_complete_osc_and_string_control_sequences() {
    for control in [
        "\u{001B}]8;;https://example.invalid\u{0007}",
        "\u{001B}]title\u{001B}\\",
        "\u{009D}title\u{009C}",
        "\u{001B}Ppayload\u{001B}\\",
        "\u{001B}Xpayload\u{001B}\\",
        "\u{001B}^payload\u{001B}\\",
        "\u{001B}_payload\u{001B}\\",
        "\u{0090}payload\u{009C}",
        "\u{0098}payload\u{009C}",
        "\u{009E}payload\u{009C}",
        "\u{009F}payload\u{009C}",
    ] {
        assert_eq!(
            sanitize_terminal_body(&format!("left{control}right")),
            "leftright"
        );
    }
}

/// Incomplete terminal prefixes become one visible replacement while retaining
/// readable following text instead of silently discarding an unbounded suffix.
#[test]
fn retains_payload_after_incomplete_terminal_prefixes() {
    assert_eq!(sanitize_terminal_body("a\u{001B}[31"), "a�");
    assert_eq!(
        sanitize_terminal_body("a\u{001B}]unterminated"),
        "a�unterminated"
    );
    assert_eq!(
        sanitize_terminal_body("a\u{001B}Punterminated"),
        "a�unterminated"
    );
    assert_eq!(sanitize_terminal_body("a\u{001B}"), "a�");
}

/// Isolated controls and invisible format characters disappear at safe edges
/// but produce one boundary marker when omission would join visible text runs.
#[test]
fn preserves_boundaries_while_omitting_nonrendering_characters() {
    assert_eq!(
        sanitize_terminal_body("\0start\u{000D}\nend\u{007F}"),
        "start\nend"
    );
    assert_eq!(sanitize_terminal_body("a\0b"), "a�b");
    assert_eq!(sanitize_terminal_body("a\u{202E}b"), "a�b");
    assert_eq!(sanitize_terminal_body("a\u{200B}b"), "a�b");
    assert_eq!(sanitize_terminal_body("👩\u{200D}💻"), "👩�💻");
    for format in ['\u{0600}', '\u{070F}', '\u{0890}', '\u{FFF9}'] {
        assert_eq!(sanitize_terminal_body(&format!("a{format}b")), "a�b");
    }
}

/// The raw message bound remains the only body bound: sanitizer output is
/// deterministic, never truncates visible runs, and expands by at most three
/// bytes per input byte when isolated controls need a visible boundary marker.
#[test]
fn remains_bounded_without_truncating_visible_text() {
    let input = "x\0".repeat(65_536);
    let rendered = sanitize_terminal_body(&input);
    assert_eq!(rendered.matches('x').count(), 65_536);
    assert!(rendered.len() <= input.len() * 3);
}

/// Repeated unterminated string-control introducers remain linear work: each
/// introducer emits one marker without rescanning the rest of the admitted
/// body.
#[test]
fn repeated_unterminated_string_controls_do_not_rescan_the_suffix() {
    let input = "\u{001B}]".repeat(65_536);
    let rendered = sanitize_terminal_body(&input);
    assert_eq!(rendered, "�".repeat(65_536));
}
