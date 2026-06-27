use super::*;

/// Ensures malformed non-ASCII hex input is rejected instead of panicking on a
/// UTF-8 boundary while splitting the RGB components.
#[test]
fn non_ascii_hex_color_returns_error() {
    assert!(Color::parse("#aébcx").is_err());
}

/// Ensures a leading plus sign is rejected because theme hex colors must be
/// exactly six ASCII hex digits after the `#` prefix.
#[test]
fn plus_prefixed_hex_color_returns_error() {
    assert!(Color::parse("#+00000").is_err());
}
