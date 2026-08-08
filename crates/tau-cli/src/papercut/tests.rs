use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs as path_unix_fs;
use std::sync::{Arc, Barrier};
use std::{fs as path_std_fs, thread};

use fs2::FileExt;

use super::*;

fn record(timestamp_us: u64, agent_id: &str, session_id: &str, report: &str) -> PapercutRecord {
    PapercutRecord::new(
        tau_proto::AgentId::parse(agent_id).expect("valid test agent"),
        tau_proto::SessionId::parse(session_id).expect("valid test session"),
        tau_proto::UnixMicros::new(timestamp_us),
        report.to_owned(),
    )
}

fn store(tempdir: &tempfile::TempDir) -> PapercutStore {
    PapercutStore::new(tempdir.path())
}

fn write_records(store: &PapercutStore, records: &[PapercutRecord]) {
    path_std_fs::create_dir_all(&store.root).expect("create reporter root");
    let contents = records
        .iter()
        .map(|record| serde_json::to_string(record).expect("serialize record"))
        .collect::<Vec<_>>()
        .join("\n");
    path_std_fs::write(store.file(), format!("{contents}\n")).expect("write records");
}

fn write_raw_records(store: &PapercutStore, contents: &[u8]) {
    path_std_fs::create_dir_all(&store.root).expect("create reporter root");
    path_std_fs::write(store.file(), contents).expect("write raw records");
}

fn append_report(store: &PapercutStore, record: &PapercutRecord) {
    let root = open_existing_directory_no_follow(&store.root).expect("open reporter root");
    root.lock_exclusive().expect("lock reporter root");
    let mut file = path_std_fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(store.file())
        .expect("open papercut file");
    serde_json::to_writer(&mut file, record).expect("serialize appended record");
    file.write_all(b"\n").expect("append newline");
    file.sync_all().expect("sync appended record");
    root.unlock().expect("unlock reporter root");
}

/// Ensures ordinary output orders canonical records by timestamp and escapes
/// report controls so one model report cannot forge terminal output lines.
#[test]
fn plain_list_is_timestamp_ordered_and_line_safe() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = store(&tempdir);
    write_records(
        &store,
        &[
            record(2_000_000, "agent-a", "session-a", "second\nline"),
            record(1_000_000, "agent-b", "session-b", "first"),
        ],
    );

    let output = format_plain(&store.list().expect("list records")).expect("format plain list");

    assert_eq!(
        output,
        "1970-01-01T00:00:01Z agent-b [session-b] first\n\
1970-01-01T00:00:02Z agent-a [session-a] second\\nline\n"
    );
}

/// Ensures Markdown retains report text as copyable literal content and selects
/// a longer fence when a report itself contains a normal triple-backtick fence.
#[test]
fn markdown_list_uses_safe_fences_for_same_records() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = store(&tempdir);
    write_records(
        &store,
        &[record(
            1_000_000,
            "agent-a",
            "session-a",
            "```rust\nlet answer = 42;\n```",
        )],
    );

    let output =
        format_markdown(&store.list().expect("list records")).expect("format Markdown list");

    assert_eq!(
        output,
        "# Papercuts\n\n\
## 1970-01-01T00:00:01Z\n\n\
- Agent: `agent-a`\n\
- Session: `session-a`\n\n\
````text\n\
```rust\n\
let answer = 42;\n\
```\n\
````\n\n"
    );
}

/// Ensures both renderers report an absent canonical file as an empty history
/// without creating a reporter directory that no agent has used.
#[test]
fn empty_storage_formats_as_an_empty_history() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = store(&tempdir);

    let records = store.list().expect("list absent records");

    assert!(records.is_empty());
    assert_eq!(
        format_plain(&records).expect("format plain"),
        "no papercut reports\n"
    );
    assert_eq!(
        format_markdown(&records).expect("format Markdown"),
        "# Papercuts\n\nNo papercut reports.\n"
    );
    assert_eq!(store.clear().expect("clear absent records"), 0);
    assert!(!store.root.exists());
}

/// Ensures clear removes every record in its canonical locked snapshot and
/// reports how many records it removed.
#[test]
fn clear_removes_listed_records() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = store(&tempdir);
    write_records(
        &store,
        &[
            record(1_000_000, "agent-a", "session-a", "first"),
            record(2_000_000, "agent-b", "session-b", "second"),
        ],
    );

    assert_eq!(store.clear().expect("clear records"), 2);
    assert!(store.list().expect("list cleared records").is_empty());
    assert!(!store.file().exists());
}

/// Ensures an already-cleared history remains a successful no-op, so scripts
/// can safely repeat cleanup after a prior successful clear.
#[test]
fn repeated_clear_is_a_successful_no_op() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = store(&tempdir);
    write_records(
        &store,
        &[record(1_000_000, "agent-a", "session-a", "first")],
    );

    assert_eq!(store.clear().expect("first clear"), 1);
    assert_eq!(store.clear().expect("second clear"), 0);
    assert!(store.list().expect("list cleared records").is_empty());
}

/// Ensures malformed, unsupported, invalid-identity, and unrenderable-timestamp
/// input fails closed and clear leaves each rejected canonical file untouched.
#[test]
fn rejected_records_are_never_cleared() {
    let cases = [
        b"{not json}\n".as_slice(),
        br#"{"schema":2,"agent_id":"agent-a","session_id":"session-a","timestamp_us":1,"report":"future"}"#,
        br#"{"schema":1,"agent_id":"agent\ninjected","session_id":"session-a","timestamp_us":1,"report":"bad"}"#,
        br#"{"schema":1,"agent_id":"agent-a","session_id":"session-a","timestamp_us":18446744073709551615,"report":"bad"}"#,
    ];
    for contents in cases {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let store = store(&tempdir);
        write_raw_records(&store, contents);

        assert!(store.list().is_err(), "list must reject {contents:?}");
        assert!(store.clear().is_err(), "clear must reject {contents:?}");
        assert_eq!(
            path_std_fs::read(store.file()).expect("read rejected record"),
            contents,
            "clear must preserve rejected input"
        );
    }
}

/// Ensures storage inspection rejects oversized, symlinked, and non-regular
/// records rather than following or deleting data outside the reporter
/// contract.
#[test]
#[cfg(unix)]
fn unsafe_or_oversized_record_files_fail_closed() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = store(&tempdir);
    path_std_fs::create_dir_all(&store.root).expect("create reporter root");
    path_std_fs::File::create(store.file())
        .expect("create oversized record")
        .set_len(tau_harness::EXTENSION_DATA_MAX_FILE_BYTES + 1)
        .expect("extend oversized record");
    assert!(store.list().is_err());
    assert!(store.clear().is_err());
    assert!(store.file().exists());

    path_std_fs::remove_file(store.file()).expect("remove oversized record");
    path_std_fs::create_dir(store.file()).expect("create record directory");
    assert!(store.list().is_err());
    assert!(store.clear().is_err());
    path_std_fs::remove_dir(store.file()).expect("remove record directory");

    let target = tempdir.path().join("outside.jsonl");
    path_std_fs::write(&target, b"outside\n").expect("write symlink target");
    path_unix_fs::symlink(&target, store.file()).expect("create records symlink");
    assert!(store.list().is_err());
    assert!(store.clear().is_err());
    assert_eq!(
        path_std_fs::read(target).expect("read symlink target"),
        b"outside\n"
    );
}

/// Ensures a reporter that waits behind clear's shared directory lock appends
/// to the newly empty file, preserving reports accepted after the clear
/// boundary.
#[test]
fn clear_preserves_reports_appended_after_its_lock_boundary() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = store(&tempdir);
    write_records(
        &store,
        &[record(
            1_000_000,
            "agent-before",
            "session-before",
            "before",
        )],
    );
    let midpoint = Arc::new(Barrier::new(2));
    let clear_store = store.clone().with_clear_midpoint(Arc::clone(&midpoint));
    let clear_thread = thread::spawn(move || clear_store.clear().expect("clear records"));
    midpoint.wait();
    let reporter_store = PapercutStore::new(tempdir.path());
    let reporter_thread = thread::spawn(move || {
        append_report(
            &reporter_store,
            &record(2_000_000, "agent-after", "session-after", "after"),
        );
    });

    midpoint.wait();
    assert_eq!(clear_thread.join().expect("clear thread"), 1);
    reporter_thread.join().expect("reporter thread");

    assert_eq!(
        store.list().expect("list post-boundary record"),
        vec![record(2_000_000, "agent-after", "session-after", "after")]
    );
}
