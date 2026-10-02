#!/usr/bin/env -S nix shell .#diagnostics -c tau-diagnostics-cargo
---cargo
[package]
edition = "2024"
[dependencies]
anyhow = "=1.0.104"
chrono = "=0.4.44"
clap = { version = "=4.6.1", features = ["derive"] }
csv = "=1.4.0"
serde_json = { version = "=1.0.149", features = ["raw_value"] }
[dev-dependencies]
tempfile = "=3.27.0"
---
//! Offline canonical quota/usage diagnostics, with structurally skipped
//! provider content.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::Parser;
use serde_json::value::RawValue;
use serde_json::{Value, json};

const DAY: i64 = 86_400_000;
const HOUR: i64 = DAY / 24;
const SIX: i64 = 6 * HOUR;
const QUOTA_EVENT: &str = "harness.provider_quota_changed";
const TOKEN_EVENT: &str = "provider.response_finished";
const COLORS: [&str; 6] = [
    "#2563eb", "#dc2626", "#059669", "#7c3aed", "#ea580c", "#0891b2",
];
const QUOTA_FIELDS: [&str; 23] = [
    "observed_at",
    "observed_at_unix_ms",
    "profile_label",
    "provider",
    "profile_epoch",
    "sequence",
    "limit_id",
    "window_id",
    "used_basis_points",
    "used_percent",
    "remaining_basis_points",
    "remaining_percent",
    "window_seconds",
    "reset_at",
    "reset_at_unix_seconds",
    "remaining_seconds_at_timing_anchor",
    "timing_anchor_observed_at_unix_ms",
    "server_offset_ms",
    "server_offset_observed_at_unix_ms",
    "route_models_json",
    "route_provenances_json",
    "route_observed_at_unix_ms_json",
    "omitted_unchanged_before",
];
const TOKEN_FIELDS: [&str; 16] = [
    "hour_start",
    "hour_start_unix_ms",
    "hour_end",
    "profile_label",
    "provider",
    "interval_start",
    "interval_end",
    "elapsed_seconds",
    "partial_bucket",
    "cached_input_tokens",
    "uncached_input_tokens",
    "output_tokens",
    "cached_input_tokens_per_second",
    "uncached_input_tokens_per_second",
    "output_tokens_per_second",
    "accepted_terminal_observations",
];
const METRICS: [&str; 3] = [
    "cached_input_tokens",
    "uncached_input_tokens",
    "output_tokens",
];
type Row = BTreeMap<String, Value>;
type Counts = BTreeMap<&'static str, u64>;
type Profiles = Vec<(String, String)>;
type Identity = (String, String, String, u64);

/// Offline selection arguments; labels are presentation, not account evidence.
#[derive(Parser)]
struct Args {
    /// Directory of immediate session directories (symlinks are followed).
    #[arg(long, default_value_os_t = default_sessions())]
    sessions_root: PathBuf,
    /// Repeatable label and exact provider prefix.
    #[arg(long)]
    profile: Vec<String>,
    /// Compatibility selector, equivalent to NAME=NAME.
    #[arg(long, hide = true)]
    provider: Option<String>,
    /// Inclusive RFC3339 lower bound, default endpoint minus fourteen days.
    #[arg(long)]
    since: Option<String>,
    /// Exclusive RFC3339 upper bound, default captured current UTC instant.
    #[arg(long)]
    until: Option<String>,
    /// Artifact directory, created private if new.
    #[arg(long)]
    out: PathBuf,
}

/// Identity evidence used for deduplication before the time filter.
#[derive(Clone)]
struct Observation {
    /// Selected presentation label.
    label: String,
    /// Exact provider selected from canonical model.
    provider: String,
    /// Canonical time at microsecond precision for earliest-copy selection.
    micros: i64,
    /// Label, agent ID, prompt ID and attempt; never exported.
    identity: Identity,
    /// Complete cache-hit, cache-miss, output usage; absent means unknown.
    usage: Option<[u64; 3]>,
}

/// A projected record decodes only selected scalar/usage fields.
enum Projection {
    /// Accepted canonical quota snapshot.
    Quota(Value),
    /// Accepted terminal without content.
    Token(Value),
    /// A different event (including provider-reported quota).
    Other,
}

/// Retained evidence and coverage counters for one finite scan.
struct Scan {
    /// Collapsed quota CSV evidence, separated by process epoch/pool/window.
    quota: Vec<Row>,
    /// All in-range quota evidence, before display reduction or collapse.
    quota_observations: Vec<Row>,
    /// One-hour token rows after identity deduplication.
    tokens: Vec<Row>,
    /// Scalar scan/coverage counters with no identifiers.
    counts: Counts,
}

fn default_sessions() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state/tau/sessions")
}

fn instant(text: &str) -> Result<i64> {
    Ok(DateTime::parse_from_rfc3339(text)?.timestamp_millis())
}

fn iso(value: i64) -> Result<String> {
    Ok(DateTime::<Utc>::from_timestamp_millis(value)
        .context("timestamp outside representable range")?
        .to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn range(since: Option<&str>, until: Option<&str>, now: i64) -> Result<(i64, i64)> {
    let until = until.map(instant).transpose()?.unwrap_or(now);
    let since = since
        .map(instant)
        .transpose()?
        .unwrap_or(until.checked_sub(14 * DAY).context("range overflow")?);
    let span = until.checked_sub(since).context("range overflow")?;
    ensure!(
        span > 0 && span <= 366 * DAY,
        "range must be positive and at most 366 days"
    );
    // Validate microsecond time-filter arithmetic before scanning.
    since.checked_mul(1000).context("range overflow")?;
    until.checked_mul(1000).context("range overflow")?;
    Ok((since, until))
}

fn profiles(args: &Args) -> Result<Profiles> {
    ensure!(
        args.provider.is_none() || args.profile.is_empty(),
        "--provider cannot be combined with --profile"
    );
    let values = if let Some(provider) = &args.provider {
        vec![format!("{provider}={provider}")]
    } else if args.profile.is_empty() {
        vec!["chatgpt=chatgpt".into()]
    } else {
        args.profile.clone()
    };
    let mut labels = BTreeSet::new();
    let mut providers = BTreeSet::new();
    values
        .into_iter()
        .map(|value| {
            let (label, provider) = value
                .split_once('=')
                .context("--profile must be LABEL=PROVIDER")?;
            ensure!(
                !label.is_empty() && !provider.is_empty(),
                "--profile must be LABEL=PROVIDER"
            );
            ensure!(
                labels.insert(label.to_owned()) && providers.insert(provider.to_owned()),
                "each --profile label and provider must be unique"
            );
            Ok((label.to_owned(), provider.to_owned()))
        })
        .collect()
}

fn string(value: &Value) -> Result<&str> {
    value
        .as_str()
        .filter(|v| !v.is_empty())
        .context("expected nonempty string")
}

fn integer(value: &Value) -> Result<i64> {
    value.as_i64().context("expected integer")
}

fn optional_integer(value: &Value) -> Result<Value> {
    if value.is_null() {
        Ok(json!(""))
    } else {
        Ok(json!(integer(value)?))
    }
}

fn checked_time(value: &Value, scale: i64) -> Result<Value> {
    let result = optional_integer(value)?;
    if let Some(time) = result.as_i64() {
        iso(time.checked_mul(scale).context("timestamp overflow")?)?;
    }
    Ok(result)
}

fn object(value: &Value) -> Result<&serde_json::Map<String, Value>> {
    value.as_object().context("expected object")
}

fn array(value: &Value) -> Result<&Vec<Value>> {
    value.as_array().context("expected array")
}

fn bump(counts: &mut Counts, key: &'static str) {
    *counts.entry(key).or_default() += 1;
}

/// Borrow spans rather than materializing arbitrary nested JSON
/// strings/objects.
fn raw_object(text: &str) -> Result<BTreeMap<String, &RawValue>> {
    Ok(serde_json::from_str(text)?)
}

fn decode(fields: &BTreeMap<String, &RawValue>, key: &str) -> Result<Value> {
    Ok(serde_json::from_str(
        fields.get(key).context("missing selected field")?.get(),
    )?)
}

fn project(raw: &str) -> Result<Projection> {
    let record = raw_object(raw)?;
    let event = raw_object(record.get("event").context("missing event")?.get())?;
    let name = decode(&event, "event")?;
    if name != QUOTA_EVENT && name != TOKEN_EVENT {
        return Ok(Projection::Other);
    }
    let record_type = decode(&record, "type")?;
    ensure!(
        record_type == "published",
        "canonical record type must be published"
    );
    let recorded = decode(&record, "recorded_at_micros")?;
    let micros = integer(&recorded)?;
    iso(micros.div_euclid(1000))?;
    let payload = event.get("payload").context("missing payload")?.get();
    if name == QUOTA_EVENT {
        return Ok(Projection::Quota(serde_json::from_str(payload)?));
    }
    let fields = raw_object(payload)?;
    let mut selected = serde_json::Map::new();
    for key in ["agent_id", "agent_prompt_id", "provider_attempt"] {
        if fields.contains_key(key) {
            selected.insert(key.into(), decode(&fields, key)?);
        }
    }
    if let Some(raw_usage) = fields.get("usage") {
        if raw_usage.get() == "null" {
            selected.insert("usage".into(), Value::Null);
        } else {
            let usage = raw_object(raw_usage.get())?;
            let mut selected_usage = serde_json::Map::new();
            for key in [
                "model",
                "prompt_sent_tokens",
                "prompt_cached_tokens",
                "response_received_tokens",
            ] {
                if usage.contains_key(key) {
                    selected_usage.insert(key.into(), decode(&usage, key)?);
                }
            }
            selected.insert("usage".into(), Value::Object(selected_usage));
        }
    }
    selected.insert("recorded_at_micros".into(), recorded);
    Ok(Projection::Token(Value::Object(selected)))
}

fn selected_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(root)? {
        let session = entry?.path();
        let candidate = session.join("events.jsonl");
        if session.is_dir() && candidate.is_file() {
            files.push(candidate);
        }
    }
    files.sort();
    Ok(files)
}

fn quota_rows(
    payload: &Value,
    label: &str,
    provider: &str,
    since: i64,
    until: i64,
) -> Result<Option<Vec<Row>>> {
    object(payload)?;
    let actual_provider = string(&payload["provider"])?;
    if actual_provider != provider {
        return Ok(None);
    }
    let epoch = string(&payload["profile_epoch"])?;
    let sequence = integer(&payload["sequence"])?;
    let bindings = array(&payload["route_bindings"])?;
    let windows = array(&payload["windows"])?;
    let mut result = Vec::new();
    for window in windows {
        object(window)?;
        object(&window["key"])?;
        let limit = string(&window["key"]["limit_id"])?;
        let id = string(&window["key"]["window_id"])?;
        let observed = integer(&window["usage_observed_at_unix_ms"])?;
        let observed_at = iso(observed)?;
        let used = integer(&window["used_basis_points"])?;
        let duration = integer(&window["window_seconds"])?;
        ensure!(
            (0..=10000).contains(&used) && duration >= 0,
            "invalid quota usage or duration"
        );
        let reset = checked_time(&window["reset_at_unix_seconds"], 1000)?;
        let anchor = checked_time(&window["timing_anchor_observed_at_unix_ms"], 1)?;
        let offset_at = checked_time(&window["server_offset_observed_at_unix_ms"], 1)?;
        let mut routes = Vec::new();
        for binding in bindings {
            object(binding)?;
            let limits = array(&binding["limit_ids"])?;
            ensure!(
                limits.iter().all(Value::is_string),
                "route limit_ids must contain strings"
            );
            let model = string(&binding["model"])?;
            let provenance = if binding.get("provenance").is_none() {
                ""
            } else {
                binding["provenance"]
                    .as_str()
                    .context("invalid route provenance")?
            };
            let time = checked_time(&binding["observed_at_unix_ms"], 1)?;
            if limits.iter().any(|v| v.as_str() == Some(limit)) {
                routes.push((model, provenance, time));
            }
        }
        routes.sort_by(|a, b| (&a.0, &a.1, a.2.as_i64()).cmp(&(&b.0, &b.1, b.2.as_i64())));
        if observed < since || until <= observed {
            continue;
        }
        let mut row: Row = serde_json::from_value(json!({
            "observed_at": observed_at, "observed_at_unix_ms": observed,
            "profile_label": label, "provider": provider, "profile_epoch": epoch, "sequence": sequence,
            "limit_id": limit, "window_id": id, "used_basis_points": used, "used_percent": used as f64 / 100.0,
            "remaining_basis_points": 10000 - used, "remaining_percent": 100.0 - used as f64 / 100.0,
            "window_seconds": duration, "reset_at": reset.as_i64().map(|v| iso(v * 1000)).transpose()?.unwrap_or_default(),
            "reset_at_unix_seconds": reset, "timing_anchor_observed_at_unix_ms": anchor,
            "server_offset_observed_at_unix_ms": offset_at,
            "route_models_json": serde_json::to_string(&routes.iter().map(|r| r.0).collect::<Vec<_>>())?,
            "route_provenances_json": serde_json::to_string(&routes.iter().map(|r| r.1).collect::<Vec<_>>())?,
            "route_observed_at_unix_ms_json": serde_json::to_string(&routes.iter().map(|r| &r.2).collect::<Vec<_>>())?,
            "omitted_unchanged_before": 0
        }))?;
        row.insert(
            "remaining_seconds_at_timing_anchor".into(),
            optional_integer(&window["remaining_seconds_at_timing_anchor"])?,
        );
        row.insert(
            "server_offset_ms".into(),
            optional_integer(&window["server_offset_ms"])?,
        );
        result.push(row);
    }
    Ok(Some(result))
}

fn token(payload: &Value, profiles: &Profiles, counts: &mut Counts) -> Result<Option<Observation>> {
    object(payload)?;
    let usage = &payload["usage"];
    if usage.is_null() {
        bump(counts, "token_events_missing_usage");
        return Ok(None);
    }
    object(usage)?;
    let model = &usage["model"];
    if model.is_null() {
        bump(counts, "token_events_unselected_model");
        return Ok(None);
    }
    let model = string(model)?;
    let profile = profiles
        .iter()
        .find(|(_, provider)| model.starts_with(&format!("{provider}/")));
    let Some((label, provider)) = profile else {
        bump(counts, "token_events_unselected_model");
        return Ok(None);
    };
    let agent = string(&payload["agent_id"])?;
    let prompt = string(&payload["agent_prompt_id"])?;
    let attempt = payload
        .get("provider_attempt")
        .map(|v| {
            v.as_u64()
                .filter(|v| *v > 0)
                .context("invalid provider attempt")
        })
        .transpose()?
        .unwrap_or(1);
    let complete = if usage.get("prompt_cached_tokens").is_none() {
        None
    } else {
        let sent = usage["prompt_sent_tokens"]
            .as_u64()
            .context("invalid prompt tokens")?;
        let cached = usage["prompt_cached_tokens"]
            .as_u64()
            .context("invalid cache tokens")?;
        let output = usage["response_received_tokens"]
            .as_u64()
            .context("invalid output tokens")?;
        ensure!(cached <= sent, "cached tokens exceed prompt tokens");
        Some([cached, sent - cached, output])
    };
    Ok(Some(Observation {
        label: label.clone(),
        provider: provider.clone(),
        micros: integer(&payload["recorded_at_micros"])?,
        identity: (label.clone(), agent.into(), prompt.into(), attempt),
        usage: complete,
    }))
}

fn signature(row: &Row) -> String {
    let values: BTreeMap<_, _> = row
        .iter()
        .filter(|(key, _)| {
            ![
                "observed_at",
                "observed_at_unix_ms",
                "sequence",
                "omitted_unchanged_before",
            ]
            .contains(&key.as_str())
        })
        .collect();
    serde_json::to_string(&values).expect("scalar CSV values serialize")
}

fn collapse(mut rows: Vec<Row>) -> Vec<Row> {
    rows.sort_by_key(|r| (r["observed_at_unix_ms"].as_i64(), r["sequence"].as_i64()));
    let mut kept = Vec::new();
    let mut index = 0;
    while index < rows.len() {
        let mut end = index + 1;
        let first = signature(&rows[index]);
        while end < rows.len() && signature(&rows[end]) == first {
            end += 1;
        }
        kept.push(rows[index].clone());
        if end > index + 1 {
            let mut last = rows[end - 1].clone();
            last.insert("omitted_unchanged_before".into(), json!(end - index - 2));
            kept.push(last);
        }
        index = end;
    }
    kept
}

fn interval(start: i64, duration: i64, since: i64, until: i64) -> Result<Row> {
    let lower = start.max(since);
    let upper = (start + duration).min(until);
    ensure!(upper > lower, "bucket does not intersect range");
    Ok(serde_json::from_value(json!({
        "interval_start": iso(lower)?, "interval_end": iso(upper)?,
        "elapsed_seconds": (upper - lower) as f64 / 1000.0,
        "partial_bucket": lower != start || upper != start + duration
    }))?)
}

fn aggregate(
    observations: &[Observation],
    duration: i64,
    since: i64,
    until: i64,
) -> Result<Vec<Row>> {
    let mut groups = BTreeMap::<_, ([u64; 3], u64)>::new();
    for observation in observations {
        let start = observation.micros.div_euclid(1000).div_euclid(duration) * duration;
        let group = groups
            .entry((start, &observation.label, &observation.provider))
            .or_default();
        let usage = observation
            .usage
            .context("incomplete observation in aggregation")?;
        for (total, value) in group.0.iter_mut().zip(usage) {
            *total = total.checked_add(value).context("token total overflow")?;
        }
        group.1 += 1;
    }
    groups
        .into_iter()
        .map(|((start, label, provider), (usage, count))| {
            let mut row = interval(start, duration, since, until)?;
            let seconds = row["elapsed_seconds"].as_f64().unwrap();
            row.insert("hour_start".into(), json!(iso(start)?));
            row.insert("hour_start_unix_ms".into(), json!(start));
            row.insert("hour_end".into(), json!(iso(start + duration)?));
            row.insert("profile_label".into(), json!(label));
            row.insert("provider".into(), json!(provider));
            row.insert("accepted_terminal_observations".into(), json!(count));
            for (field, total) in METRICS.into_iter().zip(usage) {
                row.insert(field.into(), json!(total));
                row.insert(format!("{field}_per_second"), json!(total as f64 / seconds));
            }
            Ok(row)
        })
        .collect()
}

fn six_hour(rows: &[Row], since: i64, until: i64) -> Result<Vec<Row>> {
    let observations: Vec<_> = rows
        .iter()
        .map(|r| Observation {
            label: r["profile_label"].as_str().unwrap().into(),
            provider: r["provider"].as_str().unwrap().into(),
            micros: r["hour_start_unix_ms"].as_i64().unwrap() * 1000,
            identity: Default::default(),
            usage: Some(METRICS.map(|m| r[m].as_u64().unwrap())),
        })
        .collect();
    let mut buckets = aggregate(&observations, SIX, since, until)?;
    for bucket in &mut buckets {
        let start = bucket["hour_start_unix_ms"].as_i64().unwrap();
        let count: u64 = rows
            .iter()
            .filter(|row| {
                row["profile_label"] == bucket["profile_label"]
                    && row["provider"] == bucket["provider"]
                    && row["hour_start_unix_ms"].as_i64().unwrap().div_euclid(SIX) * SIX == start
            })
            .map(|row| row["accepted_terminal_observations"].as_u64().unwrap())
            .sum();
        bucket.insert("accepted_terminal_observations".into(), json!(count));
    }
    Ok(buckets)
}

fn scan(files: &[PathBuf], profiles: &Profiles, since: i64, until: i64) -> Result<Scan> {
    let mut counts = Counts::from([("files", files.len() as u64)]);
    let mut quota_groups = BTreeMap::<_, Vec<Row>>::new();
    let mut all_quota = Vec::new();
    let mut sessions = BTreeSet::new();
    let mut identities = BTreeMap::<Identity, ((i64, PathBuf, usize), Observation)>::new();
    let mut variants = BTreeMap::<Identity, BTreeSet<[u64; 3]>>::new();
    for path in files {
        *counts.entry("bytes").or_default() += fs::metadata(path)?.len();
        let reader = BufReader::new(fs::File::open(path)?);
        for (line_number, raw) in reader.split(b'\n').enumerate() {
            let bytes = raw?;
            let raw = String::from_utf8_lossy(&bytes);
            if !raw.contains(QUOTA_EVENT) && !raw.contains(TOKEN_EVENT) {
                continue;
            }
            match project(&raw) {
                Err(_) => {
                    for (name, candidate, malformed) in [
                        (
                            QUOTA_EVENT,
                            "quota_candidate_lines",
                            "quota_malformed_candidates",
                        ),
                        (
                            TOKEN_EVENT,
                            "token_candidate_lines",
                            "token_malformed_candidates",
                        ),
                    ] {
                        if raw.contains(name) {
                            bump(&mut counts, candidate);
                            bump(&mut counts, malformed);
                        }
                    }
                }
                Ok(Projection::Other) => {}
                Ok(Projection::Quota(payload)) => {
                    bump(&mut counts, "quota_candidate_lines");
                    for (label, provider) in profiles {
                        match quota_rows(&payload, label, provider, since, until) {
                            Err(_) => {
                                bump(&mut counts, "quota_malformed_candidates");
                                break;
                            }
                            Ok(None) => continue,
                            Ok(Some(rows)) => {
                                bump(&mut counts, "quota_validated_events");
                                if !rows.is_empty() {
                                    sessions.insert(path.parent().unwrap());
                                }
                                for row in rows {
                                    bump(&mut counts, "quota_window_observations");
                                    all_quota.push(row.clone());
                                    let key = [
                                        "profile_label",
                                        "provider",
                                        "profile_epoch",
                                        "limit_id",
                                        "window_id",
                                    ]
                                    .map(|field| row[field].as_str().unwrap().to_owned());
                                    quota_groups.entry(key).or_default().push(row);
                                }
                                break;
                            }
                        }
                    }
                }
                Ok(Projection::Token(payload)) => {
                    bump(&mut counts, "token_candidate_lines");
                    match token(&payload, profiles, &mut counts) {
                        Err(_) => bump(&mut counts, "token_malformed_candidates"),
                        Ok(None) => {}
                        Ok(Some(observation)) => {
                            if let Some(usage) = observation.usage {
                                bump(&mut counts, "token_validated_events");
                                variants
                                    .entry(observation.identity.clone())
                                    .or_default()
                                    .insert(usage);
                            }
                            let tie = (observation.micros, path.clone(), line_number);
                            if let Some((previous, kept)) =
                                identities.get_mut(&observation.identity)
                            {
                                bump(&mut counts, "token_duplicate_observations");
                                if tie < *previous {
                                    *previous = tie;
                                    *kept = observation;
                                }
                            } else {
                                identities.insert(observation.identity.clone(), (tie, observation));
                            }
                        }
                    }
                }
            }
        }
    }
    let mut quota: Vec<_> = quota_groups.into_values().flat_map(collapse).collect();
    quota.sort_by_key(|r| {
        (
            r["observed_at_unix_ms"].as_i64(),
            r["profile_label"].to_string(),
            r["provider"].to_string(),
            r["profile_epoch"].to_string(),
            r["limit_id"].to_string(),
            r["window_id"].to_string(),
            r["sequence"].as_i64(),
        )
    });
    let mut selected = Vec::new();
    for (_, observation) in identities.into_values() {
        if observation.usage.is_none() {
            bump(&mut counts, "token_events_missing_cached_tokens");
        } else if observation.micros < since * 1000 || observation.micros >= until * 1000 {
            bump(&mut counts, "token_events_out_of_range");
        } else {
            selected.push(observation);
        }
    }
    counts.insert(
        "token_conflicting_duplicates",
        variants.values().filter(|v| v.len() > 1).count() as u64,
    );
    counts.insert("sessions_with_quota_observations", sessions.len() as u64);
    counts.insert("quota_emitted_rows", quota.len() as u64);
    counts.insert(
        "quota_omitted_unchanged_rows",
        quota
            .iter()
            .map(|r| r["omitted_unchanged_before"].as_u64().unwrap())
            .sum(),
    );
    counts.insert("token_unique_terminal_observations", selected.len() as u64);
    let tokens = aggregate(&selected, HOUR, since, until)?;
    counts.insert("token_hour_rows", tokens.len() as u64);
    Ok(Scan {
        quota,
        quota_observations: all_quota,
        tokens,
        counts,
    })
}

fn display_rows(rows: &[Row]) -> Vec<Row> {
    let mut selected = BTreeMap::<_, Row>::new();
    for row in rows {
        if row["limit_id"] != "codex" || row["window_id"] != "primary" {
            continue;
        }
        let hour = row["observed_at_unix_ms"]
            .as_i64()
            .unwrap()
            .div_euclid(HOUR)
            * HOUR;
        let key = (
            row["profile_label"].to_string(),
            row["provider"].to_string(),
            hour,
        );
        let rank = |r: &Row| {
            (
                r["remaining_basis_points"].as_i64(),
                r["observed_at_unix_ms"].as_i64(),
                r["sequence"].as_i64(),
                r["profile_epoch"].to_string(),
            )
        };
        if selected
            .get(&key)
            .is_none_or(|previous| rank(row) > rank(previous))
        {
            let mut row = row.clone();
            row.insert("display_hour_start_unix_ms".into(), json!(hour));
            selected.insert(key, row);
        }
    }
    selected.into_values().collect()
}

fn csv(path: &Path, rows: &[Row], fields: &[&str]) -> Result<()> {
    let mut writer = csv::Writer::from_path(path)?;
    writer.write_record(fields)?;
    for row in rows {
        writer.write_record(fields.iter().map(|field| match &row[*field] {
            Value::String(text) => text.clone(),
            Value::Bool(value) => if *value { "True" } else { "False" }.into(),
            value => value.to_string(),
        }))?;
    }
    writer.flush()?;
    Ok(())
}

fn quote(text: &str) -> String {
    // Single quotes disable gnuplot command substitution. Runtime backslashes
    // also avoid interpreting a literal provider label as a Unicode escape.
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

fn chart(
    rows: &[Row],
    profiles: &Profiles,
    tokens: bool,
    since: i64,
    until: i64,
) -> Result<String> {
    let height = 700 + profiles.len() * 25;
    let unit = if tokens {
        "six-hour total / elapsed seconds: tokens/s (log1p; 0 preserved)"
    } else {
        "Remaining (%)"
    };
    let title = if tokens {
        "Accepted token usage"
    } else {
        "Quota remaining"
    };
    let mut program = format!(
        "set terminal svg size 1600,{height} noenhanced font 'sans,14'\n\
        set encoding default\nunset key\nset grid ytics\nset border 3\nset xdata time\nset timefmt '%s'\n\
        set xrange [{}:{}]\nset xlabel 'UTC; missing buckets are gaps'\nset ylabel {}\nset title {}\n\
        set bmargin at screen 0.30\nset tmargin at screen 0.88\n",
        since as f64 / 1000.0,
        until as f64 / 1000.0,
        quote(unit),
        quote(&format!("{title}\nUTC [{}, {})", iso(since)?, iso(until)?))
    );
    let mut ticks = Vec::new();
    let mut day = since.div_euclid(DAY) * DAY;
    while day < until {
        let lower = day.max(since);
        let upper = (day + DAY).min(until);
        let label = DateTime::<Utc>::from_timestamp_millis(day)
            .context("day outside representable range")?
            .format("%m-%d")
            .to_string();
        ticks.push(format!(
            "{} {}",
            quote(&label),
            (lower as f64 + upper as f64) / 2000.0
        ));
        if since <= day {
            let _ = writeln!(
                program,
                "set arrow from first {}, graph 0 to first {}, graph 1 nohead dt 3 lc rgb '#dddddd' back",
                day as f64 / 1000.0,
                day as f64 / 1000.0
            );
        }
        day += DAY;
    }
    program.push_str(&format!("set xtics ({}) rotate by -45\n", ticks.join(", ")));
    if tokens {
        let maximum = rows
            .iter()
            .flat_map(|row| METRICS.map(|m| row[&format!("{m}_per_second")].as_f64().unwrap()))
            .fold(0.0, f64::max);
        let ceiling = if 0.0 < maximum { maximum } else { 1.0 };
        let mut ticks = vec!["'0' 0".to_owned()];
        if maximum > 0.0 {
            let first = (maximum.log10().floor() as i32).min(0);
            let last = maximum.log10().floor() as i32;
            for exponent in first..=last {
                let value = 10_f64.powi(exponent);
                ticks.push(format!("{} {}", quote(&format!("{value}")), value.ln_1p()));
            }
            ticks.push(format!(
                "{} {}",
                quote(&format!("{maximum:.3}")),
                maximum.ln_1p()
            ));
        }
        program.push_str(&format!(
            "set yrange [0:{}]\nset ytics ({})\n",
            ceiling.ln_1p() * 1.05,
            ticks.join(", ")
        ));
        program.push_str("set label 'Partial boundary buckets use range-overlap seconds (full bucket: 21,600 s).' at screen 0.08,0.26 front\n");
    } else {
        program.push_str("set yrange [0:100]\n");
    }
    program.push_str("set label 'Subscription' at screen 0.08,0.22 front\n");
    let step = 0.16 / profiles.len().max(1) as f64;
    for (index, (label, _)) in profiles.iter().enumerate() {
        let y = 0.19 - index as f64 * step;
        let _ = write!(
            program,
            "set arrow from screen 0.08,{y} to screen 0.105,{y} nohead lw 2 lc rgb '{}'\nset label {} at screen 0.115,{y} front\n",
            COLORS[index % COLORS.len()],
            quote(label)
        );
    }
    if tokens {
        program.push_str("set label 'Metric' at screen 0.65,0.22 front\n");
        for (index, label) in ["Cache hits", "Cache misses", "Output tokens"]
            .iter()
            .enumerate()
        {
            let y = 0.19 - index as f64 * 0.05;
            let _ = write!(
                program,
                "set arrow from screen 0.65,{y} to screen 0.69,{y} nohead lw 2 dt {} lc rgb '#444444'\nset label {} at screen 0.70,{y} front\n",
                index + 1,
                quote(label)
            );
        }
    }
    let mut plots = Vec::new();
    let categories = if tokens { 3 } else { 1 };
    for category in 0..categories {
        for (index, (label, _)) in profiles.iter().enumerate() {
            let selected: Vec<_> = rows
                .iter()
                .filter(|r| r["profile_label"].as_str() == Some(label))
                .collect();
            if selected.is_empty() {
                continue;
            }
            let id = format!("s{category}_{index}");
            let _ = writeln!(program, "${id} << EOD");
            let duration = if tokens { SIX } else { HOUR };
            let field = if tokens {
                "hour_start_unix_ms"
            } else {
                "display_hour_start_unix_ms"
            };
            let mut run: Vec<(i64, f64)> = Vec::new();
            // Isolated observations get a horizontal segment over their clipped
            // bucket, so metric line styles stay distinguishable
            // without invented observations.
            let append = |program: &mut String, run: &[(i64, f64)]| {
                if run.len() == 1 {
                    let (start, value) = run[0];
                    for t in [start.max(since), (start + duration).min(until)] {
                        let _ = writeln!(program, "{} {value}", t as f64 / 1000.0);
                    }
                } else {
                    for (start, value) in run {
                        let midpoint = (start.max(&since).to_owned() as f64
                            + (start + duration).min(until) as f64)
                            / 2000.0;
                        let _ = writeln!(program, "{midpoint} {value}");
                    }
                }
                program.push('\n');
            };
            for row in &selected {
                let start = row[field].as_i64().unwrap();
                if run
                    .last()
                    .is_some_and(|(previous, _)| start != previous + duration)
                {
                    append(&mut program, &run);
                    run.clear();
                }
                let value = if tokens {
                    row[&format!("{}_per_second", METRICS[category])]
                        .as_f64()
                        .unwrap()
                } else {
                    row["remaining_percent"].as_f64().unwrap()
                };
                run.push((start, value));
            }
            append(&mut program, &run);
            program.push_str("EOD\n");
            let _ = writeln!(program, "${id}_points << EOD");
            for row in selected {
                let start = row[field].as_i64().unwrap();
                let midpoint =
                    (start.max(since) as f64 + (start + duration).min(until) as f64) / 2000.0;
                let value = if tokens {
                    row[&format!("{}_per_second", METRICS[category])]
                        .as_f64()
                        .unwrap()
                } else {
                    row["remaining_percent"].as_f64().unwrap()
                };
                let _ = writeln!(program, "{midpoint} {value}");
            }
            program.push_str("EOD\n");
            let using = if tokens { "1:(log(1+$2))" } else { "1:2" };
            plots.push(format!(
                "${id} using {using} with lines lw 2 dt {} lc rgb '{}' notitle",
                category + 1,
                COLORS[index % COLORS.len()]
            ));
            plots.push(format!(
                "${id}_points using {using} with points pt 7 ps 0.5 lc rgb '{}' notitle",
                COLORS[index % COLORS.len()]
            ));
        }
    }
    if plots.is_empty() {
        program.push_str("set label 'No selected canonical evidence' at graph 0.35,0.5 front\n");
        plots.push("NaN notitle".into());
    }
    let stem = if tokens { "tokens" } else { "quota" };
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
        // Use the terminals' UTF-8 locale default, without gnuplot's explicit
        // utf8 escape conversion changing literal labels such as \U+0041.
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

fn artifacts(
    out: &Path,
    profiles: &Profiles,
    scan: &Scan,
    since: i64,
    until: i64,
    started: Instant,
) -> Result<()> {
    if !out.try_exists()? {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(out)?;
    }
    csv(&out.join("quota.csv"), &scan.quota, &QUOTA_FIELDS)?;
    csv(&out.join("tokens.csv"), &scan.tokens, &TOKEN_FIELDS)?;
    let display = display_rows(&scan.quota_observations);
    let buckets = six_hour(&scan.tokens, since, until)?;
    render(
        out,
        "quota",
        &chart(&display, profiles, false, since, until)?,
    )?;
    render(
        out,
        "tokens",
        &chart(&buckets, profiles, true, since, until)?,
    )?;
    let mut summary = format!(
        "selected profiles: {}\ntime filter: [{}, {})\n",
        profiles
            .iter()
            .map(|(l, p)| format!("{l}={p}"))
            .collect::<Vec<_>>()
            .join(", "),
        iso(since)?,
        iso(until)?
    );
    summary.push_str("quota SVG display selection: canonical default codex/primary; maximum actual remaining percent per subscription and UTC hour; missing hours break lines\n\
        quota SVG guides: every UTC day boundary\n\
        token SVG display selection: UTC six-hour Cache hits, Cache misses, Output tokens; totals divide by range-overlap seconds (full bucket: 21,600)\n\
        token SVG encoding: shared log1p Y axis, actual-rate labels and zero at baseline; subscription color, metric line style; missing buckets break lines\n");
    for (key, label) in [
        ("files", "selected canonical files"),
        ("bytes", "selected file bytes"),
        ("quota_candidate_lines", "quota candidate lines"),
        ("quota_validated_events", "validated selected quota events"),
        ("quota_malformed_candidates", "malformed quota candidates"),
        (
            "sessions_with_quota_observations",
            "sessions with selected quota observations",
        ),
        (
            "quota_window_observations",
            "quota window observations in range",
        ),
        ("quota_emitted_rows", "retained quota CSV evidence rows"),
        (
            "quota_omitted_unchanged_rows",
            "omitted unchanged quota rows",
        ),
        ("token_candidate_lines", "token candidate lines"),
        (
            "token_validated_events",
            "validated selected token event copies with complete usage",
        ),
        ("token_malformed_candidates", "malformed token candidates"),
        (
            "token_events_missing_usage",
            "token events without canonical usage",
        ),
        (
            "token_events_missing_cached_tokens",
            "token events with missing Cache hits field",
        ),
        (
            "token_events_unselected_model",
            "token events for unselected or missing model",
        ),
        ("token_events_out_of_range", "token events outside range"),
        (
            "token_duplicate_observations",
            "duplicate token terminal observations discarded",
        ),
        (
            "token_conflicting_duplicates",
            "conflicting duplicate token identities",
        ),
        (
            "token_unique_terminal_observations",
            "unique token terminal observations",
        ),
        ("token_hour_rows", "UTC one-hour token rows"),
    ] {
        let _ = writeln!(summary, "{label}: {}", scan.counts.get(key).unwrap_or(&0));
    }
    summary.push_str(&format!("rendered quota hourly default-series observations: {}\nUTC six-hour token display rows: {}\n\
        rendered six-hour token values across three categories: {}\npartial hourly token rows: {}\npartial six-hour token display rows: {}\nelapsed seconds: {:.3}\n",
        display.len(), buckets.len(), buckets.len() * 3,
        scan.tokens.iter().filter(|r| r["partial_bucket"] == true).count(),
        buckets.iter().filter(|r| r["partial_bucket"] == true).count(), started.elapsed().as_secs_f64()));
    let mut latest = BTreeMap::<&str, &Row>::new();
    for row in &display {
        let label = row["profile_label"].as_str().unwrap();
        if latest
            .get(label)
            .is_none_or(|r| row["observed_at_unix_ms"].as_i64() > r["observed_at_unix_ms"].as_i64())
        {
            latest.insert(label, row);
        }
    }
    summary.push_str("latest quota observation by configured subscription:\n");
    for (label, row) in latest {
        let _ = writeln!(
            summary,
            "- {label} subscription, latest displayed codex/primary quota: {}% remaining at {}",
            row["remaining_percent"],
            row["observed_at"].as_str().unwrap()
        );
    }
    summary.push_str("Coverage is sparse evidence, not predicted quota or account identity. Missing usage/buckets are not zero.\n\
        IDs used internally for earliest-copy deduplication never enter artifacts. No content or raw records exported.\n");
    fs::write(out.join("summary.txt"), &summary)?;
    fs::write(
        out.join("README.md"),
        format!(
            "# Tau quota and token-usage evidence\n\nRange: [{}, {}).\n\n\
        Keep CSV, SVG, PNG, gnuplot programs and summary together. Re-render with `gnuplot quota.gnuplot` and `gnuplot tokens.gnuplot` here.\n\
        Quota CSV retains every selected pool/window/process epoch; collapse keeps first and last of unchanged runs.\n\
        Quota display selects maximum actual remaining percent in each UTC hour for canonical codex/primary only.\n\
        It never predicts/interpolates or joins missing hours. Process epochs do not identify accounts.\n\
        Token CSV uses UTC hours; display uses UTC six-hour Cache hits (cached input), Cache misses (sent minus cached), and Output tokens.\n\
        Rates divide by range-overlap seconds: full hour 3,600; full six hours 21,600. Zero stays on shared log1p baseline.\n\
        Missing cache evidence is unknown, not zero. Deduplication uses the earliest canonical terminal copy before time filtering.\n\
        Labels/provider prefixes select configured subscriptions, not proven account identities. Only normalized route evidence is exported.\n\
        Programs contain aggregate chart evidence only. Artifacts reveal activity/quota/models; inspect before sharing and never commit them.\n",
            iso(since)?,
            iso(until)?
        ),
    )?;
    println!("{summary}Artifacts: {}", out.display());
    Ok(())
}

fn run(args: Args) -> Result<()> {
    let now = Utc::now().timestamp_millis(); // One endpoint before discovery/scanning.
    let (since, until) = range(args.since.as_deref(), args.until.as_deref(), now)?;
    let profiles = profiles(&args)?;
    let files = selected_files(&args.sessions_root)?;
    let started = Instant::now();
    let scan = scan(&files, &profiles, since, until)?;
    artifacts(&args.out, &profiles, &scan, since, until, started)
}

fn main() {
    if let Err(error) = run(Args::parse()) {
        eprintln!("error: {error}");
        std::process::exit(2);
    }
}

#[cfg(test)]
#[path = "extract_quota_tests.rs"]
mod tests;
