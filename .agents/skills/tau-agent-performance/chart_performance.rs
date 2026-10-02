#!/usr/bin/env -S nix shell .#diagnostics -c tau-diagnostics-cargo
---cargo
[package]
edition = "2024"
[dependencies]
anyhow = "=1.0.104"
chrono = "=0.4.44"
clap = { version = "=4.6.1", features = ["derive"] }
csv = "=1.4.0"
serde = { version = "=1.0.228", features = ["derive"] }
serde_json = "=1.0.149"
tempfile = "=3.27.0"
---
//! Content-free performance diagnostics; run with the locked Nix diagnostics
//! runner.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::Parser;
use serde::Serialize;
use serde_json::Value;

const DAY: i64 = 86_400_000_000;
const BUCKET: i64 = DAY / 4;
type Counts = BTreeMap<&'static str, usize>;

/// Command-line selection; paths and journal identities never enter artifacts.
#[derive(Parser)]
struct Args {
    /// Root of immediate agent directories.
    #[arg(long, default_value_os_t = default_agents())]
    agents_dir: PathBuf,
    /// Executable implementing the validated performance projection.
    #[arg(long, default_value = "tau")]
    tau: PathBuf,
    /// Inclusive RFC3339 lower bound.
    #[arg(long)]
    since: Option<String>,
    /// Exclusive RFC3339 upper bound.
    #[arg(long)]
    until: Option<String>,
    /// Positive finite timeout, in seconds, for each trace subprocess.
    #[arg(long, default_value_t = 120.0)]
    timeout: f64,
    /// New, private artifact directory.
    #[arg(long)]
    out: PathBuf,
}

/// Selected scalar prompt evidence with no retained journal identities.
struct Sample {
    /// UTC-aligned six-hour bucket.
    bucket: i64,
    /// Exact provider prefix.
    provider: String,
    /// Exact recorded model suffix.
    model: String,
    /// Valid wall elapsed seconds, including genuine zero.
    latency: Option<f64>,
    /// Valid output tokens per positive wall second, including zero tokens.
    rate: Option<f64>,
}

/// A single CSV bucket/model row, keeping missing measurements empty.
#[derive(Serialize)]
struct Row {
    /// Clipped inclusive bucket start.
    bucket_start: String,
    /// Clipped exclusive bucket end.
    bucket_end: String,
    /// Exact provider prefix.
    provider: String,
    /// Exact model suffix.
    model: String,
    /// Number of ordinary completed prompt samples.
    completed_samples: usize,
    /// Samples with valid elapsed time.
    latency_samples: usize,
    /// Samples with valid usage and positive elapsed time.
    rate_samples: usize,
    /// Median wall interval in seconds.
    median_wall_latency_seconds: Option<f64>,
    /// Median per-prompt output/wall-time rate.
    median_output_tokens_per_wall_second: Option<f64>,
}

fn default_agents() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        })
        .join("tau/agents")
}

fn instant(text: &str) -> Result<i64> {
    Ok(DateTime::parse_from_rfc3339(text)?.timestamp_micros())
}

fn iso(value: i64) -> Result<String> {
    Ok(DateTime::<Utc>::from_timestamp_micros(value)
        .context("timestamp outside representable range")?
        .to_rfc3339_opts(SecondsFormat::AutoSi, true))
}

fn range(since: Option<&str>, until: Option<&str>, now: i64) -> Result<(i64, i64)> {
    let until = until.map(instant).transpose()?.unwrap_or(now);
    let since = since
        .map(instant)
        .transpose()?
        .unwrap_or(until.checked_sub(14 * DAY).context("range overflow")?);
    ensure!(
        since >= 0 && until > since && until - since <= 366 * DAY,
        "range must be positive, after Unix epoch, and at most 366 days"
    );
    Ok((since, until))
}

fn bump(counts: &mut Counts, key: &'static str) {
    *counts.entry(key).or_default() += 1;
}

fn nonnegative(value: &Value) -> Option<i64> {
    value.as_i64().filter(|value| *value >= 0)
}

fn read_trace(
    reader: impl BufRead,
    since: i64,
    until: i64,
    counts: &mut Counts,
) -> Result<Vec<Sample>> {
    let mut lines = reader.lines();
    let header: Value = serde_json::from_str(&lines.next().context("missing trace header")??)?;
    ensure!(
        header["schema"] == "tau.agent_performance"
            && header["schema_version"] == 0
            && header["record_type"] == "header"
            && header["content_included"] == false,
        "unsupported performance trace"
    );
    let origin = nonnegative(&header["origin_recorded_at_unix_micros"]);
    let mut seen = BTreeSet::new();
    let mut samples = Vec::new();
    for line in lines {
        let row: Value = serde_json::from_str(&line?)?;
        if row["record_type"] != "provider_prompt" {
            continue;
        }
        bump(counts, "prompt_rows");
        let identity = (
            row.get("agent_id")
                .context("missing agent identity")?
                .to_string(),
            row.get("agent_prompt_id")
                .context("missing prompt identity")?
                .to_string(),
            row.get("journal_seq")
                .context("missing journal sequence")?
                .to_string(),
        );
        if !seen.insert(identity) {
            bump(counts, "duplicate_rows");
            continue;
        }
        if row["terminal_present"] != true {
            bump(counts, "incomplete_prompts");
            continue;
        }
        let timestamp = origin
            .zip(nonnegative(&row["terminal_at_us"]))
            .and_then(|(origin, terminal)| origin.checked_add(terminal));
        let Some(timestamp) = timestamp else {
            bump(counts, "missing_terminal_time");
            continue;
        };
        if timestamp < since || until <= timestamp {
            bump(counts, "out_of_range");
            continue;
        }
        let model = row["model"].as_str().and_then(|s| s.split_once('/'));
        let Some((provider, model)) = model.filter(|(p, m)| !p.is_empty() && !m.is_empty()) else {
            bump(counts, "missing_provider_model");
            continue;
        };
        let latency = nonnegative(&row["recorded_at_wall_elapsed_us"]).map(|v| v as f64 / 1e6);
        let rate = row["response_received_tokens"]
            .as_u64()
            .zip(latency)
            .filter(|(_, elapsed)| *elapsed > 0.0)
            .map(|(tokens, elapsed)| tokens as f64 / elapsed);
        bump(counts, "selected_completed");
        if latency.is_none() {
            bump(counts, "missing_latency");
        }
        if rate.is_none() {
            bump(counts, "missing_rate");
        }
        samples.push(Sample {
            bucket: timestamp / BUCKET * BUCKET,
            provider: provider.into(),
            model: model.into(),
            latency,
            rate,
        });
    }
    Ok(samples)
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let n = values.len();
    Some((values[(n - 1) / 2] + values[n / 2]) / 2.0)
}

fn aggregate(samples: Vec<Sample>, since: i64, until: i64) -> Result<Vec<Row>> {
    let mut groups = BTreeMap::<_, Vec<_>>::new();
    for sample in samples {
        groups
            .entry((sample.bucket, sample.provider, sample.model))
            .or_default()
            .push((sample.latency, sample.rate));
    }
    groups
        .into_iter()
        .map(|((bucket, provider, model), values)| {
            let mut latencies: Vec<_> = values.iter().filter_map(|v| v.0).collect();
            let mut rates: Vec<_> = values.iter().filter_map(|v| v.1).collect();
            Ok(Row {
                bucket_start: iso(bucket.max(since))?,
                bucket_end: iso((bucket + BUCKET).min(until))?,
                provider,
                model,
                completed_samples: values.len(),
                latency_samples: latencies.len(),
                rate_samples: rates.len(),
                median_wall_latency_seconds: median(&mut latencies),
                median_output_tokens_per_wall_second: median(&mut rates),
            })
        })
        .collect()
}

fn trace_command(
    args: &Args,
    agent: &Path,
    file: File,
) -> Result<Option<std::process::ExitStatus>> {
    let mut child = Command::new(&args.tau)
        .args(["agent", "trace"])
        .arg(agent.file_name().context("invalid agent directory")?)
        .arg("--agents-dir")
        .arg(&args.agents_dir)
        .args(["--format", "agent-performance-jsonl"])
        .stdout(file)
        .stderr(Stdio::null())
        .spawn()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) if start.elapsed().as_secs_f64() < args.timeout => {
                std::thread::sleep(Duration::from_millis(10))
            }
            result => {
                // Always reap even a failed/timed-out child; never leave trace
                // writers running.
                let _ = child.kill();
                let _ = child.wait();
                return match result {
                    Err(error) => Err(error.into()),
                    _ => Ok(None),
                };
            }
        }
    }
}

fn scan(args: &Args, since: i64, until: i64, counts: &mut Counts) -> Result<Vec<Sample>> {
    let mut agents = Vec::new();
    for entry in fs::read_dir(&args.agents_dir)? {
        let path = entry?.path();
        if path.is_dir() && path.join("events.cbor").is_file() {
            agents.push(path);
        }
    }
    agents.sort();
    counts.insert("discovered_journals", agents.len());
    let mut samples = Vec::new();
    for agent in agents {
        let mut trace = tempfile::tempfile()?;
        match trace_command(args, &agent, trace.try_clone()?)? {
            None => {
                bump(counts, "timed_out_journals");
                continue;
            }
            Some(status) if !status.success() => {
                bump(counts, "failed_journals");
                continue;
            }
            _ => {}
        }
        trace.rewind()?;
        let mut local = Counts::new();
        match read_trace(BufReader::new(trace), since, until, &mut local) {
            Ok(selected) => {
                for (key, value) in local {
                    *counts.entry(key).or_default() += value;
                }
                bump(counts, "successful_journals");
                samples.extend(selected);
            }
            Err(_) => bump(counts, "unsupported_or_malformed_traces"),
        }
    }
    Ok(samples)
}

/// Quote untrusted presentation labels as literal gnuplot strings.
fn quote(text: &str) -> String {
    // Gnuplot expands backquoted commands inside double quotes, not single
    // quotes. Build backslashes at runtime so they remain literal string data.
    let mut result = String::from("'");
    for c in text.chars() {
        match c {
            '\\' => result.push_str("' . sprintf('%c',92) . '"),
            '\'' => result.push_str("''"),
            '\n' | '\r' | '\t' => result.push(' '),
            c if c.is_control() => result.push('?'),
            c => result.push(c),
        }
    }
    result.push('\'');
    result
}

fn natural(text: &str) -> Vec<(String, u64)> {
    // Numeric runs compare numerically, so 6.10 follows 6.2, not 6.1.
    let mut parts = Vec::new();
    let mut chunk = String::new();
    let mut digits = false;
    for c in text.to_lowercase().chars() {
        if !chunk.is_empty() && c.is_ascii_digit() != digits {
            parts.push((
                if digits { String::new() } else { chunk.clone() },
                if digits {
                    chunk.parse().unwrap_or(u64::MAX)
                } else {
                    0
                },
            ));
            chunk.clear();
        }
        digits = c.is_ascii_digit();
        chunk.push(c);
    }
    if !chunk.is_empty() {
        parts.push((
            if digits { String::new() } else { chunk.clone() },
            if digits {
                chunk.parse().unwrap_or(u64::MAX)
            } else {
                0
            },
        ));
    }
    parts
}

fn family(model: &str) -> (usize, String) {
    let lower = model.to_lowercase();
    for (index, name) in ["astra", "sol", "terra", "luna"].iter().enumerate() {
        if lower.starts_with(name) {
            return (index, (*name).into());
        }
    }
    (
        4,
        lower
            .split(|c: char| !c.is_alphabetic())
            .next()
            .unwrap_or(&lower)
            .into(),
    )
}

fn known_color(model: &str) -> Option<&'static str> {
    let version: Vec<_> = natural(model)
        .into_iter()
        .filter_map(|(text, number)| text.is_empty().then_some(number))
        .collect();
    let name = family(model).1;
    match (name.as_str(), version.as_slice()) {
        ("astra", [6]) => Some("#dc2626"),
        ("sol", [5, 6]) => Some("#15803d"),
        ("sol", [6]) => Some("#65a30d"),
        ("sol", [6, 1]) => Some("#166534"),
        ("luna", [5, 6]) => Some("#1e40af"),
        ("luna", [6]) => Some("#0284c7"),
        ("terra", [5, 6]) => Some("#a16207"),
        ("grok", []) => Some("#be185d"),
        ("qwen", []) => Some("#7c3aed"),
        _ => None,
    }
}

fn rgb(hue: f64, saturation: f64, lightness: f64) -> [u8; 3] {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let x = chroma * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let offset = lightness - chroma / 2.0;
    let channels = match hue as u16 / 60 {
        0 => [chroma, x, 0.0],
        1 => [x, chroma, 0.0],
        2 => [0.0, chroma, x],
        3 => [0.0, x, chroma],
        4 => [x, 0.0, chroma],
        _ => [chroma, 0.0, x],
    };
    channels.map(|c| ((c + offset) * 255.0).round() as u8)
}

fn hex([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn ordered_models(rows: &[Row]) -> Vec<&str> {
    let mut models: Vec<_> = rows
        .iter()
        .map(|r| r.model.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    models.sort_by_key(|m| (family(m), natural(m), *m));
    models
}

/// Build one deterministic, collision-aware map over the report's exact models.
fn model_colors(rows: &[Row]) -> Result<BTreeMap<String, String>> {
    let models = ordered_models(rows);
    let mut hues: BTreeMap<String, f64> = [
        ("astra", 0.0),
        ("sol", 125.0),
        ("terra", 45.0),
        ("luna", 215.0),
        ("grok", 335.0),
        ("qwen", 275.0),
    ]
    .into_iter()
    .map(|(name, hue)| (name.into(), hue))
    .collect();
    let names: BTreeSet<_> = models.iter().map(|m| family(m).1).collect();
    for name in &names {
        if hues.contains_key(name) {
            continue;
        }
        let hash = name.bytes().fold(2166136261_u32, |n, b| {
            (n ^ u32::from(b)).wrapping_mul(16777619)
        });
        let mut hue = f64::from(hash % 3600) / 10.0;
        let close = hues.values().any(|other| {
            let distance = (hue - other).abs();
            distance.min(360.0 - distance) < 20.0
        });
        if close {
            // Prefer the largest unused hue gap instead of cycling a short
            // palette.
            let mut occupied: Vec<_> = hues.values().copied().collect();
            occupied.sort_by(f64::total_cmp);
            let (start, gap) = occupied
                .iter()
                .enumerate()
                .map(|(index, start)| {
                    let end = occupied
                        .get(index + 1)
                        .copied()
                        .unwrap_or(occupied[0] + 360.0);
                    (*start, end - start)
                })
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .unwrap();
            hue = (start + gap / 2.0) % 360.0;
        }
        hues.insert(name.clone(), hue);
    }
    let mut colors = BTreeMap::new();
    let mut used = BTreeMap::<String, BTreeSet<[u8; 3]>>::new();
    // Reserve the required fixed colors first, independent of model iteration.
    for model in &models {
        if let Some(color) = known_color(model) {
            let channels =
                [1, 3, 5].map(|start| u8::from_str_radix(&color[start..start + 2], 16).unwrap());
            used.entry(family(model).1).or_default().insert(channels);
            colors.insert((*model).into(), color.into());
        }
    }
    for model in models {
        if colors.contains_key(model) {
            continue;
        }
        let name = family(model).1;
        let family_colors = used.entry(name.clone()).or_default();
        // Choose a well-separated saturation/lightness combination, not a
        // modulo hash shade that can silently coincide with another version.
        let hue = hues[&name];
        let candidates = (0..31)
            .flat_map(|s| {
                (0..65).map(move |l| {
                    rgb(
                        hue,
                        0.45 + s as f64 * 0.4 / 30.0,
                        0.22 + l as f64 * 0.32 / 64.0,
                    )
                })
            })
            .collect::<BTreeSet<_>>();
        let distance = |color: &[u8; 3]| {
            family_colors
                .iter()
                .map(|other| {
                    color
                        .iter()
                        .zip(other)
                        .map(|(a, b)| (i32::from(*a) - i32::from(*b)).pow(2) as u32)
                        .sum::<u32>()
                })
                .min()
                .unwrap_or(u32::MAX)
        };
        let color = candidates
            .into_iter()
            .filter(|color| !family_colors.contains(color))
            .max_by_key(distance)
            .context("too many model versions for a distinct family palette")?;
        family_colors.insert(color);
        colors.insert(model.into(), hex(color));
    }
    Ok(colors)
}

fn provider_marker(provider: &str, index: usize) -> usize {
    match provider {
        "chatgpt" => 7,
        "chatgpt-fedi" => 5,
        "grok" => 13,
        "ren" => 9,
        _ => [11, 3, 1, 2, 4, 6, 8, 10, 12][index % 9],
    }
}

fn chart(
    rows: &[Row],
    rate: bool,
    since: i64,
    until: i64,
    colors: &BTreeMap<String, String>,
) -> Result<String> {
    let models = ordered_models(rows);
    let providers: Vec<_> = rows
        .iter()
        .map(|r| r.provider.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let series: BTreeSet<_> = rows.iter().map(|r| (&r.provider, &r.model)).collect();
    let metric = |row: &Row| {
        if rate {
            row.median_output_tokens_per_wall_second
        } else {
            row.median_wall_latency_seconds
        }
    };
    let values: Vec<_> = rows.iter().filter_map(metric).collect();
    let maximum = values.iter().copied().fold(0.0, f64::max);
    let minimum = values.iter().copied().fold(f64::INFINITY, f64::min);
    let logarithmic = minimum > 0.0 && maximum / minimum >= 20.0;
    let unit = if rate {
        "tokens/wall-second"
    } else {
        "seconds"
    };
    let title = if rate {
        "Output tokens / prompt-to-terminal wall time (not decoder speed)"
    } else {
        "Prompt-to-terminal wall latency"
    };
    let legend_rows = models.len().max(providers.len()).max(1);
    let height = 750 + legend_rows * 28;
    let step = 28.0 / height as f64;
    let legend_top = (legend_rows + 1) as f64 * step;
    let plot_bottom = legend_top + 80.0 / height as f64;
    let plot_top = 1.0 - 110.0 / height as f64;
    let mut program = format!(
        "set terminal svg size 1600,{height} noenhanced font 'sans,14'\n\
         set encoding default\nset datafile missing 'NaN'\nunset key\nset border 3\nset grid xtics ytics\n\
         set xdata time\nset timefmt '%s'\nset format x '%m-%d\\n%H:%M'\n\
         set xrange [{}:{}]\nset xlabel 'UTC; six-hour medians, clipped bucket midpoints; missing buckets are gaps'\n\
         set ylabel {}\nset title {}\nset bmargin at screen {plot_bottom}\nset tmargin at screen {plot_top}\n",
        since as f64 / 1e6,
        until as f64 / 1e6,
        quote(&format!(
            "{unit}{}",
            if logarithmic { " (log scale)" } else { "" }
        )),
        quote(&format!("{title}\nUTC [{}, {})", iso(since)?, iso(until)?))
    );
    if logarithmic {
        program.push_str(&format!(
            "set logscale y\nset yrange [{}:{}]\n",
            minimum / 1.1,
            maximum * 1.1
        ));
    } else {
        program.push_str(&format!("set yrange [0:{}]\n", maximum.max(1.0) * 1.05));
    }
    let _ = write!(
        program,
        "set label 'Model' at screen 0.08,{legend_top} front\nset label 'Provider' at screen 0.65,{legend_top} front\n"
    );
    for (index, model) in models.iter().enumerate() {
        let y = legend_top - (index + 1) as f64 * step;
        let _ = write!(
            program,
            "set arrow from screen 0.08,{y} to screen 0.105,{y} nohead lw 2 lc rgb '{}'\nset label {} at screen 0.115,{y} front\n",
            colors[*model],
            quote(model)
        );
    }
    for (index, provider) in providers.iter().enumerate() {
        let y = legend_top - (index + 1) as f64 * step;
        let _ = writeln!(
            program,
            "set label {} at screen 0.65,{y} point pt {} ps 1 lc rgb '#444444' offset 2,0 front",
            quote(provider),
            provider_marker(provider, index)
        );
    }
    let mut plots = Vec::new();
    for (index, (provider, model)) in series.iter().enumerate() {
        let _ = writeln!(program, "$s{index} << EOD");
        let mut previous = None;
        for row in rows
            .iter()
            .filter(|r| &r.provider == *provider && &r.model == *model)
        {
            let start = instant(&row.bucket_start)?;
            let end = instant(&row.bucket_end)?;
            let bucket = start / BUCKET;
            let Some(value) = metric(row) else {
                continue;
            };
            if previous.is_some_and(|p| bucket != p + 1) {
                program.push('\n');
            }
            let _ = writeln!(program, "{} {value}", (start as f64 + end as f64) / 2e6);
            previous = Some(bucket);
        }
        program.push_str("EOD\n");
        let marker_index = providers
            .iter()
            .position(|p| *p == provider.as_str())
            .unwrap();
        plots.push(format!(
            "$s{index} using 1:2 with linespoints lw 2 pt {} ps 0.7 lc rgb '{}' notitle",
            provider_marker(provider, marker_index),
            colors[model.as_str()]
        ));
    }
    if values.is_empty() {
        program.push_str(
            "set label 'No selected completed prompt metric evidence' at graph 0.3,0.5 front\n",
        );
        // Explicit ranges allow an empty plot without gnuplot's all-undefined
        // error.
        program.push_str("set yrange [0:1]\n");
        plots = vec!["NaN notitle".into()];
    }
    let stem = if rate { "throughput" } else { "latency" };
    program.push_str(&format!("set terminal svg size 1600,{height} noenhanced font 'sans,14'\nset output '{stem}.svg'\nplot {}\n\
        set terminal pngcairo size 1600,{height} noenhanced font 'sans,14'\nset output '{stem}.png'\nreplot\nunset output\n",
        plots.join(", ")));
    Ok(program)
}

fn render(out: &Path, stem: &str, program: &str) -> Result<()> {
    fs::write(out.join(format!("{stem}.gnuplot")), program)?;
    let status = Command::new("gnuplot")
        .arg(format!("{stem}.gnuplot"))
        .current_dir(out)
        // These terminals accept UTF-8 by default. Do not enable gnuplot's utf8
        // escape pass: it would reinterpret a literal model suffix like \U+0041.
        .env("LC_CTYPE", "C.UTF-8")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("gnuplot unavailable; use nix shell .#diagnostics")?;
    ensure!(
        status.success(),
        "gnuplot failed; inspect private {stem}.gnuplot"
    );
    Ok(())
}

fn write_artifacts(
    args: &Args,
    rows: &[Row],
    counts: &Counts,
    since: i64,
    until: i64,
) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&args.out)?;
    fs::set_permissions(&args.out, fs::Permissions::from_mode(0o700))?;
    let mut writer = csv::WriterBuilder::new()
        .has_headers(false)
        .from_path(args.out.join("performance.csv"))?;
    writer.write_record([
        "bucket_start",
        "bucket_end",
        "provider",
        "model",
        "completed_samples",
        "latency_samples",
        "rate_samples",
        "median_wall_latency_seconds",
        "median_output_tokens_per_wall_second",
    ])?;
    for row in rows {
        writer.serialize(row)?;
    }
    writer.flush()?;
    let colors = model_colors(rows)?;
    render(
        &args.out,
        "latency",
        &chart(rows, false, since, until, &colors)?,
    )?;
    render(
        &args.out,
        "throughput",
        &chart(rows, true, since, until, &colors)?,
    )?;
    let mut summary = format!(
        "Range: [{}, {})\nBuckets: UTC six-hour; median per prompt\nProvider/model series: {}\nBucket/model rows: {}\n",
        iso(since)?,
        iso(until)?,
        rows.iter()
            .map(|r| (&r.provider, &r.model))
            .collect::<BTreeSet<_>>()
            .len(),
        rows.len()
    );
    for key in [
        "discovered_journals",
        "successful_journals",
        "failed_journals",
        "timed_out_journals",
        "unsupported_or_malformed_traces",
        "prompt_rows",
        "duplicate_rows",
        "incomplete_prompts",
        "missing_terminal_time",
        "out_of_range",
        "missing_provider_model",
        "selected_completed",
        "missing_latency",
        "missing_rate",
    ] {
        let _ = writeln!(summary, "{key}: {}", counts.get(key).unwrap_or(&0));
    }
    summary.push_str("Incomplete/missing-time counts cover all scanned history, not only the selected range.\n\
        Failed journals may be old/unsupported, corrupt, or busy without a checkpoint.\n\
        Ordinary prompts only; compaction excluded; retries share the overall elapsed interval.\n\
        Wall time includes harness/provider work and retries; not wire latency or pure decoder speed.\n\
        No first-output timestamps. Missing evidence is not zero. Sparse medians are sparse evidence.\n\
        Read CSV for completed/latency/rate sample counts. No content, IDs, paths or raw traces exported.\n");
    fs::write(args.out.join("summary.txt"), &summary)?;
    fs::write(
        args.out.join("README.md"),
        "# Agent performance charts\n\n\
        Keep CSV, SVG, PNG, gnuplot programs and summary together. Programs contain only aggregate chart evidence.\n\
        Re-render with `gnuplot latency.gnuplot` and `gnuplot throughput.gnuplot` in this directory.\n\
        Exact recorded provider/model versions remain separate; family colors are presentation only.\n\
        Markers identify recorded providers, not proven account identities. Missing buckets break lines.\n\
        Partial buckets use only terminals within the half-open range, at clipped midpoints.\n\
        Output/wall-time throughput is not decoder speed. Missing time/usage never becomes zero.\n\
        Log axes are used only for positive wide-ranging data; genuine zeros keep the scale linear.\n\
        Skipped/lagging checkpoint journals can bias comparisons; inspect summary and sample counts.\n\
        Artifacts expose aggregate work patterns and model names. Treat as private; do not commit them.\n",
    )?;
    println!("{summary}Artifacts: {}", args.out.display());
    Ok(())
}

fn run(args: Args) -> Result<()> {
    let now = Utc::now().timestamp_micros(); // Capture before discovery, exactly once.
    let (since, until) = range(args.since.as_deref(), args.until.as_deref(), now)?;
    ensure!(
        args.timeout.is_finite() && args.timeout > 0.0,
        "timeout must be positive and finite"
    );
    ensure!(
        !args.out.try_exists()?,
        "output directory must not already exist"
    );
    let mut counts = Counts::new();
    let rows = aggregate(scan(&args, since, until, &mut counts)?, since, until)?;
    write_artifacts(&args, &rows, &counts, since, until)
}

fn main() {
    if let Err(error) = run(Args::parse()) {
        eprintln!("error: {error}");
        std::process::exit(2);
    }
}

#[cfg(test)]
#[path = "chart_performance_tests.rs"]
mod tests;
