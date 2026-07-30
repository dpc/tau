/// Provider detail that has passed Codex's control, secret, and size filters.
#[derive(Clone)]
pub struct RedactedProviderDetail(
    /// Bounded provider prose after secret and control filtering.
    String,
);

impl RedactedProviderDetail {
    /// Borrow the sanitized text for ordinary live status rendering.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for RedactedProviderDetail {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RedactedProviderDetail(<redacted>)")
    }
}

/// Sanitize provider prose into the bounded opaque live-detail type.
pub(super) fn sanitize_live_detail(
    raw: &str,
    access_token: &str,
    account_id: Option<&str>,
) -> Option<RedactedProviderDetail> {
    if contains_token_shape(raw) {
        return Some(RedactedProviderDetail("[redacted]".to_owned()));
    }
    let secrets = [Some(access_token), account_id];
    let mut normalized = String::new();
    let mut previous_space = false;
    let mut scalars = 0_usize;
    let mut index = 0_usize;
    while index < raw.len() {
        if let Some(secret) = secrets
            .into_iter()
            .flatten()
            .filter(|secret| !secret.is_empty() && raw[index..].starts_with(secret))
            .max_by_key(|secret| secret.len())
        {
            if !push_bounded_text(&mut normalized, "[redacted]", &mut scalars) {
                break;
            }
            previous_space = false;
            index += secret.len();
            continue;
        }
        let character = raw[index..]
            .chars()
            .next()
            .expect("index remains on a character boundary");
        index += character.len_utf8();
        let character = if character.is_whitespace() {
            ' '
        } else if is_forbidden_scalar(character) {
            '\u{fffd}'
        } else {
            character
        };
        if character == ' ' {
            if previous_space {
                continue;
            }
            previous_space = true;
        } else {
            previous_space = false;
        }
        if !push_bounded_character(&mut normalized, character, &mut scalars) {
            break;
        }
    }
    normalized = normalized.trim().to_owned();
    if contains_token_shape(&normalized) {
        normalized = "[redacted]".to_owned();
    }
    (!normalized.is_empty()).then_some(RedactedProviderDetail(normalized))
}

fn push_bounded_text(output: &mut String, text: &str, scalars: &mut usize) -> bool {
    for character in text.chars() {
        if !push_bounded_character(output, character, scalars) {
            return false;
        }
    }
    true
}

fn push_bounded_character(output: &mut String, character: char, scalars: &mut usize) -> bool {
    if 256 <= *scalars || 1_024 < output.len().saturating_add(character.len_utf8()) {
        return false;
    }
    output.push(character);
    *scalars += 1;
    true
}

/// Detect common API-token and JWT shapes in provider-controlled text.
pub(super) fn contains_token_shape(input: &str) -> bool {
    if [b"sk-".as_slice(), b"api_key", b"api-key", b"apikey"]
        .iter()
        .any(|marker| contains_ascii_case_insensitive(input.as_bytes(), marker))
        || input.as_bytes().windows(b"bearer".len() + 1).any(|window| {
            window[..b"bearer".len()].eq_ignore_ascii_case(b"bearer")
                && (window[b"bearer".len()].is_ascii_whitespace()
                    || matches!(window[b"bearer".len()], b':' | b'='))
        })
    {
        return true;
    }
    input
        .split(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
        })
        .any(|candidate| {
            let candidate = candidate.trim_matches('.');
            20 < candidate.len()
                && candidate.matches('.').count() == 2
                && candidate
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
        })
}

fn contains_ascii_case_insensitive(input: &[u8], marker: &[u8]) -> bool {
    input
        .windows(marker.len())
        .any(|window| window.eq_ignore_ascii_case(marker))
}

fn is_forbidden_scalar(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{007f}'
                | '\u{0080}'..='\u{009f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{fdd0}'..='\u{fdef}'
                | '\u{fffe}'
                | '\u{ffff}'
        )
        || (character as u32 & 0xffff == 0xfffe)
        || (character as u32 & 0xffff == 0xffff)
}
