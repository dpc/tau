//! Canonical selection, privacy, deduplication and chart evidence regression
//! oracles.
use std::os::unix::fs::{PermissionsExt, symlink};

use super::*;

fn selected() -> Profiles {
    vec![
        ("personal".into(), "chatgpt".into()),
        ("work".into(), "chatgpt-fedi".into()),
    ]
}

fn canonical(observed: i64) -> Value {
    json!({"type":"published","recorded_at_micros":observed*1000,
    "event":{"event":QUOTA_EVENT,"payload":{
        "provider":"chatgpt","profile_epoch":"epoch-a","sequence":1,
        "route_bindings":[{"model":"chatgpt/a","limit_ids":["codex"],
            "provenance":"turn","observed_at_unix_ms":observed}],
        "windows":[{"key":{"limit_id":"codex","window_id":"primary"},
            "used_basis_points":2500,"usage_observed_at_unix_ms":observed,
            "window_seconds":604800,"reset_at_unix_seconds":1_800_000_000,
            "remaining_seconds_at_timing_anchor":42,"timing_anchor_observed_at_unix_ms":observed,
            "server_offset_ms":-10,"server_offset_observed_at_unix_ms":observed}]
    }}})
}

fn terminal(recorded: i64, prompt: &str) -> Value {
    json!({"type":"published","recorded_at_micros":recorded*1000,
        "event":{"event":TOKEN_EVENT,"payload":{"agent_id":"PRIVATE_AGENT",
            "agent_prompt_id":prompt,
            "output_items":[{"content":"PRIVATE_CONTENT"}],
            "usage":{"model":"chatgpt/gpt-5","prompt_sent_tokens":1000,
                "prompt_cached_tokens":800,"response_received_tokens":42}}}})
}

fn session(root: &Path, name: &str, records: &[Value]) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("events.jsonl");
    fs::write(
        &file,
        records.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
    file
}

fn fixture(records: &[Value], since: i64, until: i64) -> Scan {
    let root = tempfile::tempdir().unwrap();
    let file = session(root.path(), "session", records);
    scan(&[file], &selected(), since, until).unwrap()
}

fn count(scan: &Scan, key: &str) -> u64 {
    *scan.counts.get(key).unwrap_or(&0)
}

/// Follow immediate session symlinks, but never recursively scan descendant
/// directories.
#[test]
fn selection_follows_session_symlink_root_only() {
    let root = tempfile::tempdir().unwrap();
    let target = session(root.path(), "target", &[canonical(1_700_000_000_000)]);
    let selected = root.path().join("selected");
    fs::create_dir(&selected).unwrap();
    symlink(target.parent().unwrap(), selected.join("linked")).unwrap();
    assert_eq!(
        selected_files(&selected).unwrap(),
        vec![selected.join("linked/events.jsonl")]
    );
    session(&selected, "nested/child", &[]);
    assert_eq!(selected_files(&selected).unwrap().len(), 1);
}

/// Quota time selection is half-open and retains normalized routes across all
/// pools/windows.
#[test]
fn quota_selection_labels_routes_and_half_open_range() {
    let base = 1_700_000_000_000;
    let records = [canonical(base - 1), canonical(base), canonical(base + 100)];
    let scan = fixture(&records, base, base + 100);
    assert_eq!(scan.quota.len(), 1);
    let row = &scan.quota[0];
    assert_eq!(row["profile_label"], "personal");
    assert_eq!(row["remaining_basis_points"], 7500);
    assert_eq!(row["route_models_json"], "[\"chatgpt/a\"]");
    assert_eq!(row["route_provenances_json"], "[\"turn\"]");
    assert_eq!(row["route_observed_at_unix_ms_json"], format!("[{base}]"));
    let mut other = canonical(base);
    other["event"]["payload"]["windows"][0]["key"]["limit_id"] = json!("other");
    let scan = fixture(&[other], base, base + 100);
    assert_eq!(scan.quota.len(), 1);
    assert!(display_rows(&scan.quota_observations).is_empty());
}

/// Collapse only identical consecutive evidence, retain run endpoints and
/// separate epochs.
#[test]
fn collapse_keeps_endpoints_and_separate_process_epochs() {
    let base = 1_700_000_000_000;
    let mut records = Vec::new();
    for n in 0..4 {
        let mut r = canonical(base + n);
        r["event"]["payload"]["sequence"] = json!(n);
        // Keep timing/route evidence unchanged; changing it must prevent
        // collapse.
        r["event"]["payload"]["route_bindings"][0]["observed_at_unix_ms"] = json!(base);
        r["event"]["payload"]["windows"][0]["timing_anchor_observed_at_unix_ms"] = json!(base);
        r["event"]["payload"]["windows"][0]["server_offset_observed_at_unix_ms"] = json!(base);
        records.push(r);
    }
    let mut epoch = records[0].clone();
    epoch["event"]["payload"]["profile_epoch"] = json!("epoch-b");
    records.push(epoch);
    let scan = fixture(&records, base, base + 10);
    assert_eq!(scan.quota.len(), 3);
    assert_eq!(count(&scan, "quota_omitted_unchanged_rows"), 2);
    assert_eq!(scan.quota_observations.len(), 5);
    let changed = fixture(&[canonical(base), canonical(base + 1)], base, base + 10);
    assert_eq!(changed.quota.len(), 2); // Anchor/route times are evidence, not ignored noise.
}

/// Canonical usage keeps cache hits/misses/output separate in UTC-aligned
/// hourly rows.
#[test]
fn canonical_tokens_and_utc_hours() {
    let base = 1_700_000_001_234;
    let a = terminal(base, "one");
    let mut b = terminal(base + 1, "two");
    b["event"]["payload"]["usage"] = json!({"model":"chatgpt/gpt-5",
        "prompt_sent_tokens":300,"prompt_cached_tokens":0,"response_received_tokens":9});
    let mut c = terminal(base + HOUR, "three");
    c["event"]["payload"]["usage"]["model"] = json!("chatgpt-fedi/gpt-5");
    let scan = fixture(&[a, b, c], base.div_euclid(HOUR) * HOUR, base + 2 * HOUR);
    assert_eq!(scan.tokens.len(), 2);
    let row = &scan.tokens[0];
    assert_eq!(row["cached_input_tokens"], 800);
    assert_eq!(row["uncached_input_tokens"], 500);
    assert_eq!(row["output_tokens"], 51);
    assert_eq!(row["cached_input_tokens_per_second"], json!(800.0 / 3600.0));
    assert_eq!(count(&scan, "token_unique_terminal_observations"), 3);
}

/// Missing cache evidence is unknown, and the earliest unknown copy suppresses
/// later zero usage.
#[test]
fn missing_cache_is_unknown_not_zero() {
    let base = 1_700_000_000_000;
    let mut missing = terminal(base, "same");
    missing["event"]["payload"]["usage"]
        .as_object_mut()
        .unwrap()
        .remove("prompt_cached_tokens");
    let mut later = terminal(base + 1, "same");
    later["event"]["payload"]["usage"]["prompt_cached_tokens"] = json!(0);
    let mut no_usage = terminal(base, "absent");
    no_usage["event"]["payload"]
        .as_object_mut()
        .unwrap()
        .remove("usage");
    let scan = fixture(&[later, missing, no_usage], base, base + 10);
    assert!(scan.tokens.is_empty());
    assert_eq!(count(&scan, "token_events_missing_cached_tokens"), 1);
    assert_eq!(count(&scan, "token_events_missing_usage"), 1);
    assert_eq!(count(&scan, "token_duplicate_observations"), 1);
}

/// Deduplicate across files by identity before range filtering, with
/// microsecond tie-breaking.
#[test]
fn dedup_before_range_with_microsecond_tiebreak_and_conflicts() {
    let base = 1_700_000_000_000;
    let mut first = terminal(base, "same");
    first["recorded_at_micros"] = json!(base * 1000 + 100);
    let mut second = terminal(base, "same");
    second["recorded_at_micros"] = json!(base * 1000 + 900);
    second["event"]["payload"]["usage"]["response_received_tokens"] = json!(99);
    let root = tempfile::tempdir().unwrap();
    let late = session(root.path(), "a", &[second.clone(), second]);
    let early = session(root.path(), "z", &[first.clone()]);
    let scan = scan(&[late, early], &selected(), base, base + 1).unwrap();
    assert_eq!(scan.tokens[0]["output_tokens"], 42);
    assert_eq!(count(&scan, "token_duplicate_observations"), 2);
    assert_eq!(count(&scan, "token_conflicting_duplicates"), 1);
    let scan = fixture(&[first, terminal(base + 2, "same")], base + 1, base + 10);
    assert!(scan.tokens.is_empty());
    assert_eq!(count(&scan, "token_events_out_of_range"), 1);
}

/// Provider attempts and agent identities are independent terminals, not
/// model-wide duplicates.
#[test]
fn distinct_terminal_identities() {
    let base = 1_700_000_000_000;
    let a = terminal(base, "same");
    let mut b = a.clone();
    b["event"]["payload"]["provider_attempt"] = json!(2);
    let mut c = a.clone();
    c["event"]["payload"]["agent_id"] = json!("OTHER_PRIVATE_AGENT");
    let scan = fixture(&[a, b, c], base, base + 10);
    assert_eq!(count(&scan, "token_unique_terminal_observations"), 3);
    assert_eq!(count(&scan, "token_duplicate_observations"), 0);
}

/// Validate selected canonical fields, while reported/nonselected events stay
/// outside evidence.
#[test]
fn malformed_unselected_and_reported_events_are_accounted() {
    let base = 1_700_000_000_000;
    let mut bad = terminal(base, "bad");
    bad["event"]["payload"]["usage"]["prompt_cached_tokens"] = json!(1001);
    let mut unrelated = terminal(base, "other");
    unrelated["event"]["payload"]["usage"]["model"] = json!("other/gpt-5");
    let mut reported = canonical(base);
    reported["event"]["event"] = json!("provider.quota_reported");
    let mut unpublished = terminal(base, "badtype");
    unpublished["type"] = json!("pending");
    let mut wrong = canonical(base);
    wrong["event"]["payload"]["windows"][0]["used_basis_points"] = json!(true);
    let scan = fixture(
        &[bad, unrelated, reported, unpublished, wrong],
        base,
        base + 10,
    );
    assert_eq!(count(&scan, "token_malformed_candidates"), 2);
    assert_eq!(count(&scan, "token_events_unselected_model"), 1);
    assert_eq!(count(&scan, "quota_malformed_candidates"), 1);
    assert!(scan.tokens.is_empty() && scan.quota.is_empty());
}

/// Borrowed RawValue spans validate skipped JSON without decoding nested
/// provider content.
#[test]
fn projection_skips_output_and_validates_skipped_json() {
    let mut row = terminal(1_700_000_000_000, "PRIVATE_PROMPT");
    row["event"]["payload"]["output_items"] =
        json!([{"content":"PRIVATE_CONTENT","large":[{"nested":"PRIVATE"}]}]);
    let raw = row.to_string();
    let Projection::Token(projected) = project(&raw).unwrap() else {
        panic!("not a terminal")
    };
    assert!(projected.get("output_items").is_none());
    assert!(!projected.to_string().contains("PRIVATE_CONTENT"));
    for raw in [
        raw.replace(
            "\"prompt_cached_tokens\":800",
            "\"prompt_cached_tokens\":true",
        ),
        raw.replace("\"output_items\":[", "\"output_items\":[,"),
        raw.replace("\"output_items\":[", "\"output_items\":[\u{00a0}"),
        format!("{raw} trailing"),
    ] {
        let result = project(&raw).and_then(|p| match p {
            Projection::Token(v) => token(&v, &selected(), &mut Counts::new()).map(|_| ()),
            _ => Ok(()),
        });
        assert!(result.is_err(), "{raw}");
    }
    // Deep provider output must not allocate its strings into the projected
    // record.
    assert_eq!(projected["usage"]["prompt_cached_tokens"], 800);
}

/// Display maximum actual remainder in each hour, with latest
/// time/sequence/epoch ties.
#[test]
fn hourly_quota_maximum_default_pool_and_ties() {
    let base = 1_700_006_400_000;
    let mut records = Vec::new();
    for (n, used) in [5000, 1000, 9000].into_iter().enumerate() {
        let mut r = canonical(base + n as i64);
        r["event"]["payload"]["windows"][0]["used_basis_points"] = json!(used);
        records.push(r);
    }
    let mut tied = records[1].clone();
    tied["event"]["payload"]["sequence"] = json!(99);
    tied["event"]["payload"]["profile_epoch"] = json!("epoch-z");
    records.push(tied);
    let mut other = canonical(base);
    other["event"]["payload"]["windows"][0]["key"]["limit_id"] = json!("other");
    records.push(other);
    let scan = fixture(&records, base, base + HOUR);
    let rows = display_rows(&scan.quota_observations);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["remaining_percent"], 90.0);
    assert_eq!(rows[0]["sequence"], 99);
    assert_eq!(rows[0]["profile_epoch"], "epoch-z");
    assert_eq!(scan.quota_observations.len(), 5);
}

/// Hourly and six-hour rates divide by clipped range overlap, not observation
/// span.
#[test]
fn partial_boundary_rates_and_six_hour_alignment() {
    let base = instant("2026-10-01T00:00:00Z").unwrap();
    let since = base + 30 * 60 * 1000;
    let until = base + 13 * HOUR;
    let scan = fixture(
        &[
            terminal(since, "one"),
            terminal(base + HOUR, "two"),
            terminal(base + 12 * HOUR, "three"),
            terminal(until, "exclusive"),
        ],
        since,
        until,
    );
    assert_eq!(scan.tokens.len(), 3);
    assert_eq!(scan.tokens[0]["elapsed_seconds"], 1800.0);
    assert_eq!(
        scan.tokens[0]["cached_input_tokens_per_second"],
        json!(800.0 / 1800.0)
    );
    assert_eq!(scan.tokens[0]["partial_bucket"], true);
    let rows = six_hour(&scan.tokens, since, until).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["hour_start_unix_ms"], base);
    assert_eq!(rows[0]["elapsed_seconds"], 19800.0);
    assert_eq!(rows[0]["cached_input_tokens"], 1600);
    assert_eq!(rows[0]["accepted_terminal_observations"], 2);
    assert_eq!(rows[1]["hour_start_unix_ms"], base + 12 * HOUR);
    assert_eq!(rows[1]["elapsed_seconds"], 3600.0);
    let program = chart(&rows, &selected(), true, since, until).unwrap();
    assert!(program.contains("log(1+$2)"));
    assert!(program.contains("log1p; 0 preserved"));
    assert!(program.contains("range-overlap seconds"));
    assert!(
        program.contains("with lines lw 2 dt 1")
            && program.contains("with lines lw 2 dt 2")
            && program.contains("with lines lw 2 dt 3")
    );
    assert!(!program.contains("PRIVATE"));
}

/// Charts retain explicit gaps, all UTC day guides, and all three metrics per
/// subscription.
#[test]
fn chart_series_gaps_daily_guides_and_log1p_zero() {
    let base = instant("2026-10-01T00:00:00Z").unwrap();
    let mut zero = terminal(base, "zero");
    zero["event"]["payload"]["usage"] = json!({
        "model":"chatgpt/gpt-5","prompt_sent_tokens":0,"prompt_cached_tokens":0,"response_received_tokens":0});
    let mut other = terminal(base + 12 * HOUR, "work");
    other["event"]["payload"]["usage"]["model"] = json!("chatgpt-fedi/gpt-5");
    let scan = fixture(
        &[zero, terminal(base + 12 * HOUR, "later"), other],
        base,
        base + 14 * DAY,
    );
    let rows = six_hour(&scan.tokens, base, base + 14 * DAY).unwrap();
    let program = chart(&rows, &selected(), true, base, base + 14 * DAY).unwrap();
    assert_eq!(program.matches("using 1:(log(1+$2)) with lines").count(), 6);
    assert_eq!(program.matches("set arrow from first ").count(), 14);
    assert_eq!(program.matches("rotate by -45").count(), 1);
    assert!(program.contains("'0' 0"));
    assert!(program.contains(" 0\n\n")); // Zero remains evidence, then a gap.
    assert!(
        program.contains("Cache hits")
            && program.contains("Cache misses")
            && program.contains("Output tokens")
    );
    assert!(!program.contains("epoch-a") && !program.contains("PRIVATE_AGENT"));
}

/// A single captured endpoint keeps the current day/partial bucket; explicit
/// historical bounds win.
#[test]
fn endpoint_profiles_and_invalid_inputs() {
    let now = instant("2026-10-01T13:25:12.123Z").unwrap();
    assert_eq!(range(None, None, now).unwrap(), (now - 14 * DAY, now));
    let historical = instant("2026-09-01T13:25:00Z").unwrap();
    assert_eq!(
        range(None, Some("2026-09-01T13:25:00Z"), now).unwrap(),
        (historical - 14 * DAY, historical)
    );
    assert!(
        range(
            Some("2026-10-02T00:00:00Z"),
            Some("2026-10-01T00:00:00Z"),
            now
        )
        .is_err()
    );
    assert!(range(Some("2024-01-01T00:00:00Z"), None, now).is_err());
    assert!(range(Some("2026-10-01"), None, now).is_err());
    let args = |values: &[&str]| {
        Args::try_parse_from(std::iter::once("quota").chain(values.iter().copied())).unwrap()
    };
    assert!(
        profiles(&args(&[
            "--out",
            "out",
            "--profile",
            "x=chatgpt",
            "--profile",
            "x=other"
        ]))
        .is_err()
    );
    assert!(
        profiles(&args(&[
            "--out",
            "out",
            "--profile",
            "x=chatgpt",
            "--profile",
            "y=chatgpt"
        ]))
        .is_err()
    );
    assert!(profiles(&args(&["--out", "out", "--profile", "bad"])).is_err());
    assert!(
        profiles(&args(&[
            "--out",
            "out",
            "--profile",
            "x=chatgpt",
            "--provider",
            "chatgpt"
        ]))
        .is_err()
    );
    assert_eq!(
        profiles(&args(&["--out", "out", "--provider", "name"])).unwrap(),
        vec![("name".into(), "name".into())]
    );
}

/// Artifact generation renders both formats, exports only allowlisted fields
/// and quotes hostile labels.
#[test]
fn artifacts_are_private_redacted_and_reproducible() {
    let root = tempfile::tempdir().unwrap();
    let base = instant("2026-10-01T00:00:00Z").unwrap();
    let scan = fixture(
        &[canonical(base), terminal(base, "PRIVATE_PROMPT")],
        base,
        base + DAY,
    );
    let out = root.path().join("out");
    artifacts(&out, &selected(), &scan, base, base + DAY, Instant::now()).unwrap();
    assert_eq!(
        fs::metadata(&out).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for entry in fs::read_dir(&out).unwrap() {
        let bytes = fs::read(entry.unwrap().path()).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE"));
    }
    for stem in ["quota", "tokens"] {
        assert!(
            fs::read(out.join(format!("{stem}.png")))
                .unwrap()
                .starts_with(b"\x89PNG\r\n\x1a\n")
        );
        assert!(
            fs::read_to_string(out.join(format!("{stem}.svg")))
                .unwrap()
                .contains("personal")
        );
    }
    let mut reader = csv::Reader::from_path(out.join("tokens.csv")).unwrap();
    assert_eq!(
        reader.headers().unwrap().iter().collect::<Vec<_>>(),
        TOKEN_FIELDS
    );
    assert_eq!(reader.records().count(), 1);
    let hostile = vec![(
        "label\" 'literal' `backtick` \n<&>".into(),
        "chatgpt".into(),
    )];
    let program = chart(&[], &hostile, false, base, base + DAY).unwrap();
    assert!(program.contains("label\" ''literal'' `backtick`  <&>"));
    render(&out, "quota", &program).unwrap();
}

/// Empty selections still emit schema headers, coverage and valid SVG/PNG
/// without fabricated zeros.
#[test]
fn empty_artifacts_and_singleton_line_styles_render() {
    let dir = tempfile::tempdir().unwrap();
    let base = instant("2026-10-01T00:00:00Z").unwrap();
    let scan = fixture(&[], base, base + DAY);
    artifacts(
        dir.path(),
        &selected(),
        &scan,
        base,
        base + DAY,
        Instant::now(),
    )
    .unwrap();
    let mut reader = csv::Reader::from_path(dir.path().join("quota.csv")).unwrap();
    assert_eq!(
        reader.headers().unwrap().iter().collect::<Vec<_>>(),
        QUOTA_FIELDS
    );
    assert_eq!(reader.records().count(), 0);
    let scan = fixture(&[terminal(base + HOUR, "singleton")], base, base + DAY);
    let rows = six_hour(&scan.tokens, base, base + DAY).unwrap();
    let program = chart(&rows, &selected(), true, base, base + DAY).unwrap();
    assert!(program.contains(&format!("{} ", base as f64 / 1000.0)));
    assert!(program.contains(&format!("{} ", (base + SIX) as f64 / 1000.0)));
    render(dir.path(), "tokens", &program).unwrap();
}

/// Subscription labels use literal strings, not gnuplot command or
/// Unicode-escape substitution.
#[test]
fn literal_label_quoting() {
    assert_eq!(quote("a'b\"c`d`"), "'a''b\"c`d`'");
    assert_eq!(quote("a\\U+0041"), "'a' . sprintf('%c',92) . 'U+0041'");
    assert_eq!(quote("a\nb\rc\td"), "'a b c d'");
    let dir = tempfile::tempdir().unwrap();
    let base = instant("2026-10-01T00:00:00Z").unwrap();
    let profiles = vec![(
        "literal 'quote' \"double\" `backtick` \\U+0041".into(),
        "chatgpt".into(),
    )];
    render(
        dir.path(),
        "quota",
        &chart(&[], &profiles, false, base, base + DAY).unwrap(),
    )
    .unwrap();
    let svg = fs::read_to_string(dir.path().join("quota.svg")).unwrap();
    assert!(svg.contains("`backtick`"));
    assert!(svg.contains("\\U+0041"));
    assert!(svg.contains("'quote'"));
}

/// Positive sub-one rates fill a data-driven log1p axis rather than an
/// arbitrary one-token ceiling.
#[test]
fn sub_one_token_axis_uses_observed_maximum() {
    let base = instant("2026-10-01T00:00:00Z").unwrap();
    let mut observation = terminal(base, "slow");
    observation["event"]["payload"]["usage"] = json!({
        "model":"chatgpt/gpt-5", "prompt_sent_tokens":216,
        "prompt_cached_tokens":216, "response_received_tokens":108});
    let scan = fixture(&[observation], base, base + DAY);
    let rows = six_hour(&scan.tokens, base, base + DAY).unwrap();
    assert_eq!(rows[0]["cached_input_tokens_per_second"], 0.01);
    let program = chart(&rows, &selected(), true, base, base + DAY).unwrap();
    assert!(program.contains(&format!("set yrange [0:{}]", 0.01_f64.ln_1p() * 1.05)));
    assert!(!program.contains(&format!("set yrange [0:{}]", 1.0_f64.ln_1p() * 1.05)));
    let dir = tempfile::tempdir().unwrap();
    render(dir.path(), "tokens", &program).unwrap();
}
