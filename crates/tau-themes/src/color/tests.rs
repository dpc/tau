use super::*;

/// Ensures the public color grammar accepts every documented spelling and
/// rejects malformed RGB, prefixes, and names without relaxing ASCII checks.
#[test]
fn parses_public_color_grammar() {
    for (input, expected) in [
        (
            "#aB12f0",
            Color::Rgb {
                r: 0xab,
                g: 0x12,
                b: 0xf0,
            },
        ),
        (
            "#ABCDEF",
            Color::Rgb {
                r: 0xab,
                g: 0xcd,
                b: 0xef,
            },
        ),
        ("black", Color::Black),
        ("dark_red", Color::DarkRed),
        ("dark_green", Color::DarkGreen),
        ("dark_yellow", Color::DarkYellow),
        ("dark_blue", Color::DarkBlue),
        ("dark_magenta", Color::DarkMagenta),
        ("dark_cyan", Color::DarkCyan),
        ("dark_grey", Color::DarkGrey),
        ("red", Color::Red),
        ("green", Color::Green),
        ("yellow", Color::Yellow),
        ("blue", Color::Blue),
        ("magenta", Color::Magenta),
        ("cyan", Color::Cyan),
        ("white", Color::White),
        ("grey", Color::Grey),
        ("GRAY", Color::Grey),
        ("DarkRed", Color::DarkRed),
        ("darkgreen", Color::DarkGreen),
        ("darkyellow", Color::DarkYellow),
        ("darkblue", Color::DarkBlue),
        ("darkmagenta", Color::DarkMagenta),
        ("darkcyan", Color::DarkCyan),
        ("darkgrey", Color::DarkGrey),
        ("dark_gray", Color::DarkGrey),
        ("darkgray", Color::DarkGrey),
    ] {
        assert_eq!(
            Color::parse(input).unwrap_or_else(|error| panic!("{input} should parse: {error}")),
            expected
        );
    }

    for input in [
        "#aébcx",
        "#+00000",
        "ff8800",
        "#fff",
        "#fffffff",
        "#ff88gg",
        "#12 345",
        "bright_red",
    ] {
        assert!(Color::parse(input).is_err(), "{input} should be rejected");
    }
}
