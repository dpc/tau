use super::*;

/// Ensures the explicit default index always means "unstyled", even after the
/// style table grows beyond the old 16-bit sentinel slot.
#[test]
fn default_idx_is_never_a_registered_style() {
    let mut text = ThemedText::new();
    let mut last_style = StyleIdx::DEFAULT;
    for idx in 0..=u16::MAX {
        let style = text.add_style(format!("style.{idx}"));
        assert_ne!(style, StyleIdx::DEFAULT);
        assert_eq!(style.raw(), usize::from(idx));
        last_style = style;
    }

    assert_eq!(text.style_name(StyleIdx::DEFAULT), None);
    assert_eq!(
        text.style_name(last_style).map(StyleName::as_str),
        Some("style.65535")
    );
}
