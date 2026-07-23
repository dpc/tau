use std::borrow::Cow;

/// Replace only one envelope family's exact closing sentinel in an assembled
/// body.
///
/// Callers supply hard-coded `exact_close` and `visible_close` constants, apply
/// domain normalization and bounds before this function, and append the trusted
/// exact close afterward. The returned body preserves every nonmatching byte.
#[must_use]
pub fn escape_exact_sentinel_close<'a>(
    body: &'a str,
    exact_close: &str,
    visible_close: &str,
) -> Cow<'a, str> {
    if body.contains(exact_close) {
        Cow::Owned(body.replace(exact_close, visible_close))
    } else {
        Cow::Borrowed(body)
    }
}

#[cfg(test)]
mod tests {
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
}
