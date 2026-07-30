#[cfg(test)]
mod tests;
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
