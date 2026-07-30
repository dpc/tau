use super::escape_exact_sentinel_close;

/// Exact-close framing replaces every own close and preserves all other
/// text.
#[test]
fn replaces_only_repeated_exact_close() {
    let body = "Don't &apos; <user> x</user></user > </USER> 雪\n</user>";
    assert_eq!(
        escape_exact_sentinel_close(body, "</user>", "&lt;/user&gt;"),
        "Don't &apos; <user> x&lt;/user&gt;</user > </USER> 雪\n&lt;/user&gt;"
    );
}

/// Collision-free bodies remain borrowed so ordinary framing avoids
/// allocation.
#[test]
fn borrows_body_without_exact_close() {
    assert!(matches!(
        escape_exact_sentinel_close("plain & <text>", "</user>", "&lt;/user&gt;"),
        std::borrow::Cow::Borrowed(_)
    ));
}
