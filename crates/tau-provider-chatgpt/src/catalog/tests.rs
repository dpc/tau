//! Account-catalog parsing without network or credentials.

use super::*;

/// The picker must preserve account ordering and never expose hidden models.
#[test]
fn catalog_filters_visibility_without_sorting_or_api_key_fallback() {
    let models = parse(
        br#"{"models":[
        {"slug":"z","display_name":"Z","visibility":"list"},
        {"slug":"hidden","display_name":"Hidden","visibility":"hide"},
        {"slug":"a","display_name":"A","visibility":"list","context_window":120000}
    ]}"#,
    )
    .expect("catalog");
    assert_eq!(
        models
            .iter()
            .map(|model| model.slug.as_str())
            .collect::<Vec<_>>(),
        ["z", "a"]
    );
    assert!(parse(br#"{"data":[{"id":"api-only"}]}"#).is_err());
    assert!(
        parse(
            br#"{"models":[
        {"slug":"same","display_name":"A","visibility":"list"},
        {"slug":"same","display_name":"B","visibility":"list"}
    ]}"#
        )
        .is_err()
    );
}
