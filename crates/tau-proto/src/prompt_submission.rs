//! Generic prompt provenance and safe visible metadata escaping.

use serde::{Deserialize, Serialize};

use crate::ExtensionName;

/// Harness-owned subtype selecting specialized UI treatment for an internal
/// prompt, including mandatory display or lifecycle suppression.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InternalPromptKind {
    /// Advisory prompt delivered after a named context-size threshold crossing.
    ContextSizeAlert,
    /// Harness lifecycle notice emitted for a completed background tool.
    BackgroundToolCompletion,
}

/// Prompt submission provenance stamped by the harness boundary.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptSubmissionSource {
    /// Authenticated interactive UI.
    HumanUi,
    /// Extension-originated input.
    Extension {
        /// Authenticated extension instance name.
        name: ExtensionName,
    },
    /// Harness-internal input.
    HarnessInternal,
    /// Legacy record without explicit provenance.
    #[default]
    Legacy,
}

/// Return whether untrusted metadata must be rendered as a visible escape.
///
/// This includes controls, bidi/zero-width structure, Unicode default
/// ignorables used to spoof visible labels, variation selectors, and
/// noncharacters.
#[must_use]
pub fn requires_visible_escape(character: char) -> bool {
    let scalar = character as u32;
    character.is_control()
        || matches!(
            scalar,
            0x00AD
                | 0x034F
                | 0x061C
                | 0x115F..=0x1160
                | 0x17B4..=0x17B5
                | 0x180B..=0x180F
                | 0x200B..=0x200F
                | 0x2028..=0x202E
                | 0x2060..=0x206F
                | 0x3164
                | 0xFE00..=0xFE0F
                | 0xFEFF
                | 0xFFF0..=0xFFF8
                | 0xFFA0
                | 0x1BCA0..=0x1BCA3
                | 0x1D173..=0x1D17A
                | 0xE0000..=0xE0FFF
                | 0xFDD0..=0xFDEF
        )
        || scalar & 0xFFFF == 0xFFFE
        || scalar & 0xFFFF == 0xFFFF
}

/// Render untrusted metadata with structural Unicode made explicit.
#[must_use]
pub fn visible_escape_metadata(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if requires_visible_escape(character) {
            push_visible_escape(&mut escaped, character);
        } else {
            escaped.push(character);
        }
    }
    escaped
}

/// Append one visible Unicode scalar escape.
fn push_visible_escape(output: &mut String, character: char) {
    use std::fmt::Write as _;
    let _ = write!(output, "\\u{{{:04X}}}", character as u32);
}
