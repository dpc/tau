use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tau_summary_eval::{Corpus, evaluate};

/// Maximum bytes accepted for either evaluation input.
const MAXIMUM_INPUT_BYTES: usize = 4 * 1024 * 1024;

/// Deterministic offline summary-quality corpus tooling.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Operation to perform without network access.
    #[command(subcommand)]
    command: Command,
}

/// Supported offline corpus operations.
#[derive(Debug, Subcommand)]
enum Command {
    /// Validate a corpus's schema, bounds, and public-synthetic safeguards.
    ValidateCorpus {
        /// JSON corpus to validate.
        corpus: PathBuf,
    },
    /// Score candidate summaries and emit a stable privacy-minimized JSON
    /// record.
    Score {
        /// JSON corpus containing synthetic inputs and deterministic
        /// assertions.
        #[arg(long)]
        corpus: PathBuf,
        /// JSON candidate set containing summaries and complete run provenance.
        #[arg(long)]
        candidates: PathBuf,
        /// Destination for the result record; refuses to overwrite.
        #[arg(long)]
        output: PathBuf,
    },
}

/// Executes the selected offline operation and reports a concise failure.
fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("tau-summary-eval: {error}");
        std::process::exit(1);
    }
}

/// Implements CLI operations without ambient provider or network configuration.
fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::ValidateCorpus { corpus } => {
            let bytes = read(&corpus)?;
            parse_corpus(&bytes)?.validate()
        }
        Command::Score {
            corpus,
            candidates,
            output,
        } => {
            let corpus_bytes = read(&corpus)?;
            let candidate_bytes = read(&candidates)?;
            let result = evaluate(&corpus_bytes, &candidate_bytes)?;
            let mut encoded = serde_json::to_vec_pretty(&result)
                .map_err(|error| format!("cannot encode result: {error}"))?;
            encoded.push(b'\n');
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&output)
                .and_then(|mut file| file.write_all(&encoded))
                .map_err(|error| format!("cannot create {}: {error}", output.display()))
        }
    }
}

/// Reads a bounded evaluation input file.
fn read(path: &PathBuf) -> Result<Vec<u8>, String> {
    let file =
        fs::File::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    read_bounded(file).map_err(|error| format!("cannot read {}: {error}", path.display()))
}

/// Reads at most one byte beyond the limit so streams and changing files stay
/// bounded.
fn read_bounded(reader: impl Read) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take((MAXIMUM_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAXIMUM_INPUT_BYTES {
        return Err("input exceeds the 4 MiB limit".into());
    }
    Ok(bytes)
}

/// Parses one strict corpus document.
fn parse_corpus(bytes: &[u8]) -> Result<Corpus, String> {
    serde_json::from_slice(bytes).map_err(|error| format!("cannot parse corpus: {error}"))
}

#[cfg(test)]
mod main_tests;
