//! Shared token boundaries for completion rule selection and path replacement.

/// One cursor token, retaining raw ranges and a decoded path prefix.
pub struct CompletionToken<'a> {
    /// Raw token bytes up to the cursor.
    pub prefix: &'a str,
    /// Raw token bytes after the cursor.
    pub suffix: &'a str,
    /// Draft text before the complete token.
    pub before: &'a str,
    /// Draft text after the complete token.
    pub after: &'a str,
    /// Token prefix with shell-like quotes and escapes removed.
    pub decoded: String,
}

/// Find the cursor token without evaluating shell expansion or substitutions.
///
/// Ordinary prompts treat apostrophes inside words as literal prose, not
/// opening quotes. Explicit bang commands retain shell quoting, including
/// concatenation. Both rule selection and remote replacement must use these
/// same boundaries.
pub fn completion_token(buffer: &str, cursor: usize) -> Option<CompletionToken<'_>> {
    buffer.get(..cursor)?;
    let mut start = 0;
    let mut end = buffer.len();
    let mut quote = None;
    let mut escaped = false;
    let mut decoded = String::new();
    let shell = buffer.starts_with('!');
    let mut previous = None;
    let mut chars = buffer.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        let apostrophe = !shell
            && ch == '\''
            && previous.is_some_and(char::is_alphanumeric)
            && chars.peek().is_some_and(|(_, next)| next.is_alphanumeric());
        if !escaped && quote.is_none() && ch.is_whitespace() {
            if cursor <= index {
                end = index;
                break;
            }
            start = index + ch.len_utf8();
            decoded.clear();
        } else if escaped {
            if index < cursor {
                if quote == Some('"') && !matches!(ch, '$' | '`' | '"' | '\\' | '\n') {
                    decoded.push('\\');
                }
                if ch != '\n' {
                    decoded.push(ch);
                }
            }
            escaped = false;
        } else if ch == '\\' && quote != Some('\'') {
            escaped = true;
        } else if quote == Some(ch) {
            quote = None;
        } else if quote.is_none() && matches!(ch, '\'' | '"') && !apostrophe {
            quote = Some(ch);
        } else if index < cursor {
            decoded.push(ch);
        }
        previous = Some(ch);
    }
    Some(CompletionToken {
        prefix: &buffer[start..cursor],
        suffix: &buffer[cursor..end],
        before: &buffer[..start],
        after: &buffer[end..],
        decoded,
    })
}
