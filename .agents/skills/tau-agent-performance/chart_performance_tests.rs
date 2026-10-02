//! Regression oracles for scalar selection, honest chart evidence and private
//! scans.
use std::io::Cursor;

fn chart(rows: &[Row], rate: bool, since: i64, until: i64) -> Result<String> {
    super::chart(rows, rate, since, until, &model_colors(rows)?)
}

use serde_json::json;

use super::*;

fn start() -> i64 {
    instant("2026-10-01T00:00:00Z").unwrap()
}

fn prompt(number: usize) -> Value {
    json!({"record_type":"provider_prompt", "agent_id":"PRIVATE_AGENT",
        "agent_prompt_id":format!("PRIVATE_PROMPT_{number}"), "journal_seq":number,
        "model":"provider/model-v1", "terminal_present":true,
        "terminal_at_us":1_000_000, "recorded_at_wall_elapsed_us":1_000_000,
        "response_received_tokens":10})
}

fn trace(rows: &[Value]) -> String {
    let header = json!({"schema":"tau.agent_performance", "schema_version":0,
        "record_type":"header","content_included":false,
        "origin_recorded_at_unix_micros":start()});
    std::iter::once(header)
        .chain(rows.iter().cloned())
        .map(|v| format!("{v}\n"))
        .collect()
}

fn select(rows: &[Value], since: i64, until: i64) -> (Vec<Row>, Counts) {
    let mut counts = Counts::new();
    let samples = read_trace(Cursor::new(trace(rows)), since, until, &mut counts).unwrap();
    (aggregate(samples, since, until).unwrap(), counts)
}

/// Duplicate prompt identities must not bias medians or merge exact
/// versions/providers.
#[test]
fn medians_exact_versions_and_dedup() {
    let mut a = prompt(1);
    a["response_received_tokens"] = json!(100);
    let mut b = prompt(2);
    b["model"] = json!("provider/model-v2");
    let mut c = prompt(3);
    c["model"] = json!("other/model-v1");
    let (rows, counts) = select(&[prompt(0), prompt(0), a, b, c], start(), start() + DAY);
    assert_eq!(rows.len(), 3);
    let row = rows
        .iter()
        .find(|r| r.provider == "provider" && r.model == "model-v1")
        .unwrap();
    assert_eq!(row.completed_samples, 2);
    assert_eq!(row.median_output_tokens_per_wall_second, Some(55.0));
    assert_eq!(counts["duplicate_rows"], 1);
}

/// Missing clocks, incomplete prompts, zero elapsed and zero usage are distinct
/// evidence.
#[test]
fn missing_zero_and_incomplete_are_distinct() {
    let mut rows: Vec<_> = (0..7).map(prompt).collect();
    rows[0]["terminal_present"] = json!(false);
    rows[1]["terminal_at_us"] = Value::Null;
    rows[2]["recorded_at_wall_elapsed_us"] = Value::Null;
    rows[3]["recorded_at_wall_elapsed_us"] = json!(0);
    rows[4]["response_received_tokens"] = Value::Null;
    rows[5]["response_received_tokens"] = json!(0);
    rows[6]["recorded_at_wall_elapsed_us"] = json!(-1);
    let (rows, counts) = select(&rows, start(), start() + DAY);
    assert_eq!(counts["incomplete_prompts"], 1);
    assert_eq!(counts["missing_terminal_time"], 1);
    assert_eq!(rows[0].completed_samples, 5);
    assert_eq!(rows[0].latency_samples, 3);
    assert_eq!(rows[0].rate_samples, 1);
    assert_eq!(rows[0].median_output_tokens_per_wall_second, Some(0.0));
}

/// The current partial bucket is retained; exclusive endpoints and metric gaps
/// stay honest.
#[test]
fn half_open_partial_current_bucket_and_gaps() {
    let until = start() + 13 * DAY / 24;
    let mut b = prompt(1);
    b["terminal_at_us"] = json!(12 * DAY / 24);
    let mut c = prompt(2);
    c["terminal_at_us"] = json!(13 * DAY / 24);
    let (rows, _) = select(&[prompt(0), b, c], start(), until);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].bucket_end, iso(until).unwrap());
    let program = chart(&rows, false, start(), until).unwrap();
    assert!(program.contains(" 1\n\n")); // Explicit blank line breaks the run.
    let mut b = prompt(1);
    b["terminal_at_us"] = json!(BUCKET);
    let (adjacent, _) = select(&[prompt(0), b], start(), until);
    assert!(
        !chart(&adjacent, false, start(), until)
            .unwrap()
            .contains(" 1\n\n")
    );
    assert!(program.contains(&format!(
        "{}",
        (start() + 12 * DAY / 24 + until) as f64 / 2e6
    )));
}

/// Unsupported/content-bearing traces are rejected and labels cannot inject
/// gnuplot syntax.
#[test]
fn schema_validation_and_escaping() {
    let bad = trace(&[]).replace("\"content_included\":false", "\"content_included\":true");
    assert!(read_trace(Cursor::new(bad), start(), start() + DAY, &mut Counts::new()).is_err());
    let mut row = prompt(0);
    row["model"] = json!("provider/model\" 'literal' `backtick` \n<&>");
    let (rows, _) = select(&[row], start(), start() + DAY);
    let program = chart(&rows, false, start(), start() + DAY).unwrap();
    assert!(program.contains("model\" ''literal'' `backtick`  <&>"));
    assert!(!program.contains("PRIVATE_AGENT"));
    assert!(
        read_trace(
            Cursor::new(trace(&[prompt(0)]) + "{"),
            start(),
            start() + DAY,
            &mut Counts::new()
        )
        .is_err()
    );
}

/// Wide positive data uses log axes, but a genuine zero forces an honest linear
/// axis.
#[test]
fn scale_palette_order_and_markers() {
    let mut a = prompt(0);
    a["model"] = json!("chatgpt/sol-6.10");
    a["recorded_at_wall_elapsed_us"] = json!(100_000_000);
    let mut b = prompt(1);
    b["model"] = json!("chatgpt-fedi/sol-6.2");
    let (rows, _) = select(&[a.clone(), b.clone()], start(), start() + DAY);
    let program = chart(&rows, false, start(), start() + DAY).unwrap();
    assert!(program.contains("set logscale y"));
    assert!(program.find("sol-6.2").unwrap() < program.find("sol-6.10").unwrap());
    assert!(program.contains("point pt 5") && program.contains("point pt 7"));
    b["recorded_at_wall_elapsed_us"] = json!(0);
    let (rows, _) = select(&[a, b], start(), start() + DAY);
    assert!(
        !chart(&rows, false, start(), start() + DAY)
            .unwrap()
            .contains("set logscale y")
    );
    assert_eq!(known_color("sol-5.6"), Some("#15803d"));
    assert_eq!(known_color("terra-5.6"), Some("#a16207"));
}

/// A fixed single endpoint retains today's partial bucket and validates
/// range/CLI bounds.
#[test]
fn endpoint_and_invalid_ranges() {
    let now = instant("2026-10-01T13:25:12.123456Z").unwrap();
    assert_eq!(range(None, None, now).unwrap(), (now - 14 * DAY, now));
    assert_eq!(
        range(None, Some("2026-09-01T13:25:00Z"), now).unwrap().1,
        instant("2026-09-01T13:25:00Z").unwrap()
    );
    for (since, until) in [
        ("2026-10-01", "2026-10-02T00:00:00Z"),
        ("2026-10-02T00:00:00Z", "2026-10-01T00:00:00Z"),
        ("2024-01-01T00:00:00Z", "2026-10-01T00:00:00Z"),
    ] {
        assert!(range(Some(since), Some(until), now).is_err());
    }
}

/// Real subprocesses exercise failure/timeout counting and root-only private
/// trace discovery.
#[test]
fn discovery_failure_timeout_and_private_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("agents");
    fs::create_dir(&root).unwrap();
    for name in ["ok", "old", "slow", "bad"] {
        let agent = root.join(name);
        fs::create_dir(&agent).unwrap();
        fs::write(agent.join("events.cbor"), "").unwrap();
    }
    let executable = temp.path().join("tau");
    fs::write(&executable, format!("#!/bin/sh\ncase \"$3\" in\nold) exit 1;;\nslow) exec sleep 10;;\nbad) echo '{{}}';;\nok) cat <<'EOF'\n{}EOF\n;;\nesac\n",trace(&[prompt(0)]))).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let args = Args {
        agents_dir: root,
        tau: executable,
        since: None,
        until: None,
        timeout: 0.15,
        out: temp.path().join("out"),
    };
    let mut counts = Counts::new();
    let rows = aggregate(
        scan(&args, start(), start() + DAY, &mut counts).unwrap(),
        start(),
        start() + DAY,
    )
    .unwrap();
    assert_eq!(counts["discovered_journals"], 4);
    assert_eq!(counts["failed_journals"], 1);
    assert_eq!(counts["timed_out_journals"], 1);
    assert_eq!(counts["unsupported_or_malformed_traces"], 1);
    assert_eq!(rows.len(), 1);
    write_artifacts(&args, &rows, &counts, start(), start() + DAY).unwrap();
    assert_eq!(
        fs::metadata(&args.out).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for entry in fs::read_dir(&args.out).unwrap() {
        let bytes = fs::read(entry.unwrap().path()).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE"));
    }
    let mut reader = csv::Reader::from_path(args.out.join("performance.csv")).unwrap();
    assert_eq!(reader.records().count(), 1); // No duplicate header from struct serialization.
    for stem in ["latency", "throughput"] {
        assert!(
            fs::read(args.out.join(format!("{stem}.png")))
                .unwrap()
                .starts_with(b"\x89PNG\r\n\x1a\n")
        );
    }
    assert!(run(args).is_err()); // Existing report cannot be overwritten.
}

/// Empty or entirely missing metrics still produce valid SVG and PNG artifacts.
#[test]
fn empty_charts_render() {
    let dir = tempfile::tempdir().unwrap();
    for rate in [false, true] {
        let stem = if rate { "throughput" } else { "latency" };
        render(
            dir.path(),
            stem,
            &chart(&[], rate, start(), start() + DAY).unwrap(),
        )
        .unwrap();
        assert!(
            fs::metadata(dir.path().join(format!("{stem}.svg")))
                .unwrap()
                .len()
                > 100
        );
    }
}

/// Gnuplot labels must preserve literal quote/backslash/backtick text without
/// lexical expansion.
#[test]
fn literal_label_quoting() {
    assert_eq!(quote("a'b\"c`d`"), "'a''b\"c`d`'");
    assert_eq!(quote("a\\U+0041"), "'a' . sprintf('%c',92) . 'U+0041'");
    assert_eq!(quote("a\nb\rc\td"), "'a b c d'");
    let mut row = prompt(0);
    row["model"] = json!("provider/literal 'quote' \"double\" `backtick` \\U+0041");
    let (rows, _) = select(&[row], start(), start() + DAY);
    let dir = tempfile::tempdir().unwrap();
    render(
        dir.path(),
        "latency",
        &chart(&rows, false, start(), start() + DAY).unwrap(),
    )
    .unwrap();
    let svg = fs::read_to_string(dir.path().join("latency.svg")).unwrap();
    assert!(svg.contains("`backtick`"));
    assert!(svg.contains("\\U+0041"));
    assert!(svg.contains("'quote'"));
}

/// Exact models must share one collision-aware map across metrics and recorded
/// providers.
#[test]
fn palette_resolves_family_and_version_collisions() {
    let names = [
        "claude",
        "gemini",
        "sol-6.26",
        "sol-6.30",
        "sol-5.6",
        "sol-6",
        "sol-6.1",
        "astra-6",
        "terra-5.6",
        "luna-5.6",
        "luna-6",
    ];
    let prompts: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let mut row = prompt(index);
            row["model"] = json!(format!("chatgpt/{name}"));
            row
        })
        .collect();
    let (rows, _) = select(&prompts, start(), start() + DAY);
    let colors = model_colors(&rows).unwrap();
    assert_ne!(colors["claude"], colors["gemini"]);
    assert_ne!(colors["sol-6.26"], colors["sol-6.30"]);
    for (name, color) in [
        ("astra-6", "#dc2626"),
        ("sol-5.6", "#15803d"),
        ("sol-6", "#65a30d"),
        ("sol-6.1", "#166534"),
        ("terra-5.6", "#a16207"),
        ("luna-5.6", "#1e40af"),
        ("luna-6", "#0284c7"),
    ] {
        assert_eq!(colors[name], color);
    }
    let mut reversed = rows;
    reversed.reverse();
    assert_eq!(model_colors(&reversed).unwrap(), colors);
    for rate in [false, true] {
        let program = super::chart(&reversed, rate, start(), start() + DAY, &colors).unwrap();
        for color in colors.values() {
            assert!(program.contains(&format!("lc rgb '{color}'")));
        }
    }
}

/// A larger model inventory gets readable 28-pixel legend rows, not compressed
/// fractional spacing.
#[test]
fn twenty_model_legend_spacing() {
    let prompts: Vec<_> = (0..20)
        .map(|index| {
            let mut row = prompt(index);
            row["model"] = json!(format!("provider/sol-6.{index}"));
            row
        })
        .collect();
    let (rows, _) = select(&prompts, start(), start() + DAY);
    let program = chart(&rows, false, start(), start() + DAY).unwrap();
    let height = 750.0 + 20.0 * 28.0;
    let positions: Vec<f64> = program
        .lines()
        .filter(|line| line.starts_with("set label 'sol-"))
        .map(|line| {
            line.split("at screen 0.115,")
                .nth(1)
                .unwrap()
                .split(' ')
                .next()
                .unwrap()
                .parse()
                .unwrap()
        })
        .collect();
    assert_eq!(positions.len(), 20);
    for ys in positions.windows(2) {
        assert!(((ys[0] - ys[1]) * height - 28.0).abs() < 1e-6);
    }
    let dir = tempfile::tempdir().unwrap();
    render(dir.path(), "latency", &program).unwrap();
}
