//! Readable terminal-safe rendering of untrusted text bodies.

#[cfg(test)]
#[path = "terminal_text/tests.rs"]
mod tests;

/// Sanitize an untrusted terminal body without escaping its readable text.
///
/// The sanitizer removes complete ANSI/ECMA-48 control sequences, preserves
/// printable Unicode plus spaces, tabs, and line feeds, and omits nonrendering
/// controls and format characters. When omitting a nonrendering character would
/// concatenate two visible text runs, it inserts one replacement character.
///
/// This is intentionally not injective: terminal styling and invisible Unicode
/// do not survive presentation. The tradeoff keeps message bodies readable and
/// prevents terminal control while headings and metadata retain strict visible,
/// injective escaping.
pub(crate) fn sanitize_terminal_body(input: &str) -> String {
    let characters = input.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(input.len());
    let mut previous_is_content = false;
    let mut omitted_after_content = false;
    let mut index = 0;

    while index < characters.len() {
        let character = characters[index];
        let control = match character {
            '\u{001B}' => Some(scan_escape_sequence(&characters, index)),
            '\u{009B}' => Some(scan_csi(&characters, index + 1)),
            '\u{009D}' => Some(scan_string_control(&characters, index + 1, true)),
            '\u{0090}' | '\u{0098}' | '\u{009E}' | '\u{009F}' => {
                Some(scan_string_control(&characters, index + 1, false))
            }
            _ => None,
        };
        if let Some((next, complete)) = control {
            if complete {
                index = next;
                continue;
            }
            push_replacement(
                &mut output,
                &mut previous_is_content,
                &mut omitted_after_content,
            );
            index = next;
            continue;
        }

        if body_character_is_preserved(character) {
            push_preserved(
                &mut output,
                character,
                &mut previous_is_content,
                &mut omitted_after_content,
            );
        } else if previous_is_content {
            omitted_after_content = true;
        }
        index += 1;
    }

    output
}

/// Return whether a body scalar stays readable in terminal presentation.
fn body_character_is_preserved(character: char) -> bool {
    matches!(character, '\n' | '\t' | ' ')
        || (!character.is_control()
            && !tau_proto::requires_visible_escape(character)
            && !is_unicode_format_character(character))
}

/// Return whether a scalar belongs to Unicode's nonrendering `Cf` category.
///
/// The metadata escaper already handles several spoof-prone `Cf` ranges, but
/// terminal bodies omit the entire category so newly encountered format
/// controls do not become invisible presentation state.
fn is_unicode_format_character(character: char) -> bool {
    matches!(
        character as u32,
        0x00AD
            | 0x0600..=0x0605
            | 0x061C
            | 0x06DD
            | 0x070F
            | 0x0890..=0x0891
            | 0x08E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0001
            | 0xE0020..=0xE007F
    )
}

/// Append one preserved scalar, exposing an omitted interior boundary if
/// needed.
fn push_preserved(
    output: &mut String,
    character: char,
    previous_is_content: &mut bool,
    omitted_after_content: &mut bool,
) {
    let current_is_content = !character.is_whitespace();
    if *omitted_after_content && current_is_content {
        output.push('\u{FFFD}');
    }
    output.push(character);
    *previous_is_content = current_is_content;
    *omitted_after_content = false;
}

/// Append the minimal visible marker for an incomplete terminal control prefix.
fn push_replacement(
    output: &mut String,
    previous_is_content: &mut bool,
    omitted_after_content: &mut bool,
) {
    output.push('\u{FFFD}');
    *previous_is_content = false;
    *omitted_after_content = false;
}

/// Scan one ESC-prefixed ECMA-48 control sequence.
///
/// The returned index excludes a complete sequence. For an incomplete CSI or
/// ESC-intermediate sequence it consumes the recognized prefix; incomplete
/// string controls consume only their introducer so readable payload remains.
fn scan_escape_sequence(characters: &[char], start: usize) -> (usize, bool) {
    let Some(&introducer) = characters.get(start + 1) else {
        return (start + 1, false);
    };
    match introducer {
        '[' => scan_csi(characters, start + 2),
        ']' => scan_string_control(characters, start + 2, true),
        'P' | 'X' | '^' | '_' => scan_string_control(characters, start + 2, false),
        character if is_escape_intermediate(character) => scan_escape_final(characters, start + 2),
        character if is_escape_final(character) => (start + 2, true),
        _ => (start + 1, false),
    }
}

/// Scan a CSI payload after either the ESC `[` or C1 CSI introducer.
fn scan_csi(characters: &[char], mut index: usize) -> (usize, bool) {
    while let Some(&character) = characters.get(index) {
        if is_csi_parameter(character) || is_escape_intermediate(character) {
            index += 1;
            continue;
        }
        return if is_csi_final(character) {
            (index + 1, true)
        } else {
            (index, false)
        };
    }
    (index, false)
}

/// Scan OSC, DCS, SOS, PM, or APC payload through its permitted terminator.
fn scan_string_control(
    characters: &[char],
    mut index: usize,
    bell_terminates: bool,
) -> (usize, bool) {
    let payload_start = index;
    while let Some(&character) = characters.get(index) {
        if character == '\u{009C}' || (bell_terminates && character == '\u{0007}') {
            return (index + 1, true);
        }
        if character == '\u{001B}' {
            if characters.get(index + 1) == Some(&'\\') {
                return (index + 2, true);
            }
            return (index, false);
        }
        if is_string_control_introducer(character) {
            return (index, false);
        }
        index += 1;
    }
    (payload_start, false)
}

/// Return whether a scalar starts a string control that must be scanned itself.
fn is_string_control_introducer(character: char) -> bool {
    matches!(
        character,
        '\u{009D}' | '\u{0090}' | '\u{0098}' | '\u{009E}' | '\u{009F}'
    )
}

/// Scan an ESC sequence whose introducer starts an intermediate/final form.
fn scan_escape_final(characters: &[char], mut index: usize) -> (usize, bool) {
    while characters
        .get(index)
        .is_some_and(|character| is_escape_intermediate(*character))
    {
        index += 1;
    }
    if characters
        .get(index)
        .is_some_and(|character| is_escape_final(*character))
    {
        (index + 1, true)
    } else {
        (index, false)
    }
}

/// Return whether one scalar is an ECMA-48 parameter byte.
fn is_csi_parameter(character: char) -> bool {
    matches!(character, '\u{0030}'..='\u{003F}')
}

/// Return whether one scalar is an ECMA-48 intermediate byte.
fn is_escape_intermediate(character: char) -> bool {
    matches!(character, '\u{0020}'..='\u{002F}')
}

/// Return whether one scalar is an ESC-sequence final byte.
fn is_escape_final(character: char) -> bool {
    matches!(character, '\u{0030}'..='\u{007E}')
}

/// Return whether one scalar is a CSI final byte.
fn is_csi_final(character: char) -> bool {
    matches!(character, '\u{0040}'..='\u{007E}')
}
