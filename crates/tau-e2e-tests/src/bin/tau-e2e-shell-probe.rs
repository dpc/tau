//! Hermetic child process used by the deterministic shell-concurrency oracle.

use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use nix::time::{ClockId, clock_gettime};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ident = std::env::args().nth(1).ok_or("missing probe identity")?;
    if ident == "--version" {
        println!("tau-e2e-shell-probe 1");
        return Ok(());
    }

    let ordinal = ident
        .strip_prefix("parallel-")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|ordinal| (1..=4).contains(ordinal))
        .ok_or("invalid probe identity")?;
    let directory = Path::new(".");
    let deadline = Instant::now() + Duration::from_secs(30);
    std::fs::write(format!(".tau-parallel-ready-{ordinal}"), b"ready")?;
    wait_for_marker_count(directory, ".tau-parallel-ready-", 4, deadline)?;

    let start = monotonic_ns()?;
    std::fs::write(format!(".tau-parallel-started-{ordinal}"), b"started")?;
    wait_for_marker_count(directory, ".tau-parallel-started-", 4, deadline)?;
    std::thread::sleep(Duration::from_secs(3));
    if ordinal < 4 {
        wait_for_marker(
            &directory.join(format!(".tau-parallel-release-{ordinal}")),
            deadline,
        )?;
    }

    let end = monotonic_ns()?;
    let mut stdout = std::io::stdout().lock();
    writeln!(
        stdout,
        "id={ident} start_ns={start} end_ns={end} elapsed_ms={:.3}",
        (end - start) as f64 / 1_000_000.0
    )?;
    stdout.flush()?;
    Ok(())
}

fn wait_for_marker_count(
    directory: &Path,
    prefix: &str,
    expected: usize,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let count = std::fs::read_dir(directory)?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(prefix))
            })
            .count();
        if expected <= count {
            return Ok(());
        }
        if deadline <= Instant::now() {
            return Err(format!("parallel marker barrier `{prefix}` timed out").into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_marker(path: &Path, deadline: Instant) -> Result<(), Box<dyn std::error::Error>> {
    while !path.exists() {
        if deadline <= Instant::now() {
            return Err(
                format!("parallel completion marker `{}` timed out", path.display()).into(),
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

fn monotonic_ns() -> Result<u64, nix::Error> {
    let time = clock_gettime(ClockId::CLOCK_MONOTONIC)?;
    Ok((time.tv_sec() as u64) * 1_000_000_000 + time.tv_nsec() as u64)
}
