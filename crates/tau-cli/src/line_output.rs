//! Safe line-oriented CLI output.

use std::io;

use crate::CliError;

/// Escapes one field so C0/ANSI controls cannot split records or control a
/// terminal.
pub(crate) fn escape_field(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            character if character.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(escaped, "\\u{{{:x}}}", character as u32);
            }
            character => escaped.push(character),
        }
    }
    escaped
}

/// Writes line-oriented output while treating a closed pipeline as success.
pub(crate) fn write_stdout(output: &str) -> Result<(), CliError> {
    let stdout = io::stdout();
    write_output(&mut stdout.lock(), output)
}

fn write_output(writer: &mut impl io::Write, output: &str) -> Result<(), CliError> {
    match writer.write_all(output.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(CliError::Io(error)),
    }
}

#[cfg(test)]
mod tests;
