//! Hermetic child process used by the deterministic shell-concurrency oracle.

use std::path::Path;
use std::time::{Duration, Instant};

use nix::time::{ClockId, clock_gettime};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ident = std::env::args().nth(1).ok_or("missing probe identity")?;
    if ident == "--version" {
        println!("tau-e2e-shell-probe 1");
        return Ok(());
    }

    let marker = format!(".tau-parallel-{ident}");
    std::fs::write(&marker, b"ready")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while marker_count(Path::new("."))? < 4 {
        if deadline <= Instant::now() {
            return Err("parallel barrier timed out".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let start = monotonic_ns()?;
    std::thread::sleep(Duration::from_secs(3));
    let end = monotonic_ns()?;
    println!(
        "id={ident} start_ns={start} end_ns={end} elapsed_ms={:.3}",
        (end - start) as f64 / 1_000_000.0
    );
    Ok(())
}

fn marker_count(directory: &Path) -> Result<usize, std::io::Error> {
    let mut count = 0;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(".tau-parallel-parallel-"))
        {
            count += 1;
        }
    }
    Ok(count)
}

fn monotonic_ns() -> Result<u64, nix::Error> {
    let time = clock_gettime(ClockId::CLOCK_MONOTONIC)?;
    Ok((time.tv_sec() as u64) * 1_000_000_000 + time.tv_nsec() as u64)
}
