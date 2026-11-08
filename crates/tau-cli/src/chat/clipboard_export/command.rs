//! Clipboard intent, validated before reserving terminal authority.

use tau_cli_term::ClipboardFileName;
use tau_proto::ArtifactKey;

const USAGE: &str = "usage: :export-to-clipboard KEY [--raw [--mime TYPE/SUBTYPE[;charset=TOKEN]] | --mime TYPE/SUBTYPE[;charset=TOKEN] | --file [--name BASENAME]]";

/// One validated invocation; raw MIME and file intent are mutually exclusive.
pub(super) struct Command {
    /// Fixed-width public identity of the original artifact.
    pub(super) key: ArtifactKey,
    /// User intent, resolved against verified bytes rather than catalog hints.
    pub(super) mode: Mode,
}

/// Mutually exclusive automatic, raw-MIME, and copied-file intents.
pub(super) enum Mode {
    /// Infer ordinary text, native image, or a copied file.
    Auto,
    /// Force raw bytes, optionally under an explicit MIME declaration.
    Raw(Option<String>),
    /// Force a copied file, optionally with a validated literal basename.
    File(Option<ClipboardFileName>),
}

impl Command {
    /// Parses the fixed grammar, including a single- or double-quoted basename.
    pub(super) fn parse(args: &str) -> Result<Self, &'static str> {
        let (key, rest) = word(args);
        let key = tau_proto::parse_artifact_reference(key)
            .map_err(|_| "Expected a 24-lowercase-hex-digit artifact key, tau-artifact:KEY, or <tau-artifact:KEY>")?;
        let (option, rest) = word(rest);
        let mut command = Self {
            key,
            mode: Mode::Auto,
        };
        match option {
            "" => {}
            "--raw" | "--mime" => {
                let rest = if option == "--raw" {
                    let (option, rest) = word(rest);
                    if option.is_empty() {
                        command.mode = Mode::Raw(None);
                        return Ok(command);
                    }
                    if option != "--mime" {
                        return Err(USAGE);
                    }
                    rest
                } else {
                    rest
                };
                let (mime, tail) = word(rest);
                if mime.is_empty() || !tail.is_empty() {
                    return Err(USAGE);
                }
                tau_client::ArtifactMedia::select(&[], Some(mime))?;
                command.mode = Mode::Raw(Some(mime.to_owned()));
            }
            "--file" => {
                let (option, rest) = word(rest);
                let name = match option {
                    "" => None,
                    "--name" => Some(ClipboardFileName::new(basename(rest)?.to_owned())?),
                    _ => return Err(USAGE),
                };
                command.mode = Mode::File(name);
            }
            _ => return Err(USAGE),
        }
        Ok(command)
    }
}

/// Splits one unquoted syntax token; only the final basename supports quotes.
fn word(input: &str) -> (&str, &str) {
    let input = input.trim();
    input
        .split_once(char::is_whitespace)
        .map_or((input, ""), |(head, tail)| (head, tail.trim()))
}

/// Quotes group the entire basename literally; there is no shell expansion or
/// escape syntax, and trailing tokens must not become part of an argument.
fn basename(input: &str) -> Result<&str, &'static str> {
    let input = input.trim();
    if let Some(quote @ ('\'' | '"')) = input.chars().next() {
        let name = input
            .strip_prefix(quote)
            .and_then(|s| s.strip_suffix(quote))
            .ok_or(USAGE)?;
        if name.contains(quote) {
            return Err(USAGE);
        }
        Ok(name)
    } else if input.is_empty() || input.chars().any(char::is_whitespace) {
        Err(USAGE)
    } else {
        Ok(input)
    }
}

#[cfg(test)]
mod tests;
