//! Integration-test-only child binary coupled to `tests/supervisor.rs`.

use std::error::Error;
use std::io::{BufReader, BufWriter, Write};
use std::{io as path_std_io, time as path_std_time};

use tau_proto::{
    HarnessInputMessage, HarnessOutputMessage, PeerInputReader, PeerOutputWriter, Ready,
};

const EXIT_IMMEDIATELY_ARG: &str = "--exit-immediately";
const PARTIAL_FRAME_ARG: &str = "--partial-frame";
const FLOOD_ARG: &str = "--flood";
const REPORT_SECRET_ENV_ARG: &str = "--report-secret-env";
const SLEEP_ARG: &str = "--sleep";
const REPORT_CWD_ARG: &str = "--report-cwd";
const ROUND_TRIP_ARG: &str = "--round-trip";
const STDERR_MARKER_ARG: &str = "--stderr-marker";

fn write_ready(message: impl Into<String>) -> Result<(), Box<dyn Error>> {
    let stdout = std::io::stdout();
    let mut writer = PeerOutputWriter::new(BufWriter::new(stdout.lock()));
    writer.write_message(&HarnessInputMessage::Ready(Ready {
        message: Some(message.into()),
    }))?;
    writer.flush()?;
    Ok(())
}

fn write_partial_frame() -> Result<(), Box<dyn Error>> {
    std::io::stdout().write_all(&[0x81])?;
    Ok(())
}

fn write_flood_messages(message_count: usize) -> Result<(), Box<dyn Error>> {
    let stdout = std::io::stdout();
    let mut writer = PeerOutputWriter::new(BufWriter::new(stdout.lock()));
    for index in 0..message_count {
        writer.write_message(&HarnessInputMessage::Ready(Ready {
            message: Some(index.to_string()),
        }))?;
    }
    writer.flush()?;
    Ok(())
}

fn report_secret_env() -> Result<(), Box<dyn Error>> {
    let secret_visible =
        std::env::vars_os().any(|(key, _)| key.to_string_lossy().starts_with("TAU_SECRET_"));
    write_ready(if secret_visible { "present" } else { "absent" })
}

fn run_test_mode(
    first_arg: Option<String>,
    mut remaining_args: impl Iterator<Item = String>,
) -> Result<bool, Box<dyn Error>> {
    match first_arg.as_deref() {
        Some(EXIT_IMMEDIATELY_ARG) => Ok(true),
        Some(PARTIAL_FRAME_ARG) => {
            write_partial_frame()?;
            Ok(true)
        }
        Some(FLOOD_ARG) => {
            let message_count = remaining_args
                .next()
                .ok_or_else(|| {
                    path_std_io::Error::new(
                        path_std_io::ErrorKind::InvalidInput,
                        "missing flood count",
                    )
                })?
                .parse::<usize>()?;
            write_flood_messages(message_count)?;
            Ok(true)
        }
        Some(REPORT_SECRET_ENV_ARG) => {
            report_secret_env()?;
            Ok(true)
        }
        Some(SLEEP_ARG) => {
            std::thread::sleep(path_std_time::Duration::from_secs(60));
            Ok(true)
        }
        Some(REPORT_CWD_ARG) => {
            write_ready(std::env::current_dir()?.display().to_string())?;
            Ok(true)
        }
        Some(STDERR_MARKER_ARG) => {
            eprintln!("tau-supervisor-stderr-marker");
            write_ready("stderr-written")?;
            Ok(true)
        }
        Some(ROUND_TRIP_ARG) => {
            run_round_trip_mode()?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn run_protocol_mode() -> Result<(), Box<dyn Error>> {
    let stdin = std::io::stdin();
    let mut reader = PeerInputReader::new(BufReader::new(stdin.lock()));
    write_ready("ready")?;

    loop {
        let Some(message) = reader.read_message()? else {
            return Ok(());
        };
        if let HarnessOutputMessage::Disconnect(_) = message {
            return Ok(());
        }
    }
}

fn run_round_trip_mode() -> Result<(), Box<dyn Error>> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = PeerInputReader::new(BufReader::new(stdin.lock()));
    let mut writer = PeerOutputWriter::new(BufWriter::new(stdout.lock()));

    let Some(HarnessOutputMessage::Disconnect(disconnect)) = reader.read_message()? else {
        return Err("round-trip fixture expected one disconnect message".into());
    };
    writer.write_message(&HarnessInputMessage::Disconnect(disconnect))?;
    writer.flush()?;
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let first_arg = args.next();
    if run_test_mode(first_arg, args)? {
        return Ok(());
    }
    run_protocol_mode()
}
