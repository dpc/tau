//! Tests for configured-extension configuration-error diagnostics.

use crate::harness::{
    EXTENSION_CONFIG_ERROR_TRUNCATION_MARKER, MAX_EXTENSION_CONFIG_ERROR_BYTES,
    bounded_extension_config_error,
};

/// Ensures the helper reserves its suffix and preserves UTF-8 so its diagnostic
/// never exceeds the advertised byte limit.
#[test]
fn bounded_extension_config_error_reserves_marker_bytes() {
    for message in [
        "".to_owned(),
        "short diagnostic".to_owned(),
        "x".repeat(4 * 1024),
    ] {
        assert_eq!(bounded_extension_config_error(message.clone()), message);
    }

    let ascii = bounded_extension_config_error("x".repeat(MAX_EXTENSION_CONFIG_ERROR_BYTES + 1));
    assert_eq!(ascii.len(), MAX_EXTENSION_CONFIG_ERROR_BYTES);
    assert!(ascii.ends_with(EXTENSION_CONFIG_ERROR_TRUNCATION_MARKER));

    let multibyte_input = "é".repeat((MAX_EXTENSION_CONFIG_ERROR_BYTES / 2) + 1);
    let multibyte = bounded_extension_config_error(multibyte_input.clone());
    assert!(
        multibyte
            .is_char_boundary(multibyte.len() - EXTENSION_CONFIG_ERROR_TRUNCATION_MARKER.len())
    );
    assert!(multibyte.len() <= MAX_EXTENSION_CONFIG_ERROR_BYTES);
    assert!(multibyte.ends_with(EXTENSION_CONFIG_ERROR_TRUNCATION_MARKER));
    assert!(
        multibyte
            .strip_suffix(EXTENSION_CONFIG_ERROR_TRUNCATION_MARKER)
            .is_some_and(|prefix| !prefix.is_empty() && multibyte_input.starts_with(prefix))
    );
}
