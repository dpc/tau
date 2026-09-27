//! Shared active-history validation and presentation for the CLI and tools.

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::{PapercutRecord, PapercutRecordParseError};

/// Parses one complete reporter-owned JSONL snapshot, rejecting records that
/// could not be safely rendered by the operator CLI before archival.
///
/// # Errors
///
/// Returns a line-specific error for invalid or unsupported records.
pub fn parse_records(contents: &[u8]) -> Result<Vec<PapercutRecord>, String> {
    let contents = std::str::from_utf8(contents)
        .map_err(|_| "papercut records are not valid UTF-8".to_owned())?;
    let mut records = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        let record = match PapercutRecord::parse_json_line(line) {
            Ok(record) => record,
            Err(PapercutRecordParseError::Invalid) => {
                return Err(format!("invalid papercut record at line {}", index + 1));
            }
            Err(PapercutRecordParseError::UnsupportedSchema) => {
                return Err(format!(
                    "unsupported papercut record schema at line {}",
                    index + 1
                ));
            }
        };
        let _ = format_timestamp(record.timestamp_us())?;
        records.push(record);
    }
    records.sort_unstable_by(|left, right| {
        left.timestamp_us()
            .cmp(&right.timestamp_us())
            .then_with(|| left.agent_id().cmp(right.agent_id()))
            .then_with(|| left.session_id().cmp(right.session_id()))
            .then_with(|| left.report().cmp(right.report()))
    });
    Ok(records)
}

/// Renders active reports in the CLI's concise line-safe format.
///
/// # Errors
///
/// Returns an error if a timestamp cannot be formatted.
pub fn format_plain(records: &[PapercutRecord]) -> Result<String, String> {
    if records.is_empty() {
        return Ok("no papercut reports\n".to_owned());
    }
    let mut output = String::new();
    for record in records {
        output.push_str(&format_timestamp(record.timestamp_us())?);
        output.push(' ');
        output.push_str(record.agent_id().as_str());
        output.push_str(" [");
        output.push_str(record.session_id().as_str());
        output.push_str("] ");
        for character in record.report().chars() {
            match character {
                '\\' => output.push_str("\\\\"),
                '\n' => output.push_str("\\n"),
                '\r' => output.push_str("\\r"),
                '\t' => output.push_str("\\t"),
                character if character.is_control() => {
                    use std::fmt::Write as _;
                    let _ = write!(output, "\\u{{{:x}}}", character as u32);
                }
                character => output.push(character),
            }
        }
        output.push('\n');
    }
    Ok(output)
}

/// Renders active reports in the CLI's literal Markdown format.
///
/// # Errors
///
/// Returns an error if a timestamp cannot be formatted.
pub fn format_markdown(records: &[PapercutRecord]) -> Result<String, String> {
    let mut output = String::from("# Papercuts\n\n");
    if records.is_empty() {
        output.push_str("No papercut reports.\n");
        return Ok(output);
    }
    for record in records {
        output.push_str("## ");
        output.push_str(&format_timestamp(record.timestamp_us())?);
        output.push_str("\n\n- Agent: `");
        output.push_str(record.agent_id().as_str());
        output.push_str("`\n- Session: `");
        output.push_str(record.session_id().as_str());
        output.push_str("`\n\n");
        let fence = "`".repeat(
            3.max(
                record
                    .report()
                    .split(|character| character != '`')
                    .map(str::len)
                    .max()
                    .unwrap_or_default()
                    + 1,
            ),
        );
        output.push_str(&fence);
        output.push_str("text\n");
        for character in record.report().chars() {
            match character {
                '\n' => output.push('\n'),
                '\r' => output.push_str("\\r"),
                '\t' => output.push_str("\\t"),
                character if character.is_control() => {
                    use std::fmt::Write as _;
                    let _ = write!(output, "\\u{{{:x}}}", character as u32);
                }
                character => output.push(character),
            }
        }
        if !record.report().ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&fence);
        output.push_str("\n\n");
    }
    Ok(output)
}

/// Formats a stored report timestamp as RFC 3339 UTC.
///
/// # Errors
///
/// Returns an error when the timestamp falls outside the formatter's range.
pub fn format_timestamp(timestamp_us: tau_proto::UnixMicros) -> Result<String, String> {
    let timestamp =
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp_us.get()) * 1_000)
            .map_err(|_| "papercut record has an invalid timestamp".to_owned())?;
    timestamp
        .format(&Rfc3339)
        .map_err(|_| "could not format papercut timestamp".to_owned())
}
