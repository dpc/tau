use super::*;

fn append_raw_record(path: &Path, record: &PromptHistoryRecord) {
    let mut encoded = Vec::new();
    ciborium::into_writer(record, &mut encoded).expect("encode record");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open history");
    file.write_all(&(encoded.len() as u64).to_le_bytes())
        .expect("write length");
    file.write_all(&encoded).expect("write payload");
}

/// Prompt history should round-trip non-empty prompts in append order,
/// including multiline prompt text.
#[test]
fn appends_and_loads_prompt_history_in_order() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = PromptHistoryStore {
        path: Some(tmp.path().join(HISTORY_FILE)),
    };

    store.append("one").expect("append one");
    store.append("two\nlines").expect("append two");

    assert_eq!(store.load().expect("load"), vec!["one", "two\nlines"]);
}

/// Loading should honor the history file-size cap before scanning records, so a
/// legacy oversized file cannot block startup.
#[test]
fn load_ignores_history_files_over_size_limit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(HISTORY_FILE);
    let store = PromptHistoryStore {
        path: Some(path.clone()),
    };

    append_raw_record(
        &path,
        &PromptHistoryRecord {
            version: PROMPT_HISTORY_VERSION,
            recorded_at_micros: 1,
            text: "old".to_owned(),
        },
    );
    let file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open history");
    file.set_len(MAX_HISTORY_FILE_BYTES + 1)
        .expect("grow over cap");

    assert_eq!(store.load().expect("load"), Vec::<String>::new());
}

/// Loading should ignore a crash-torn final record while preserving complete
/// records before it.
#[test]
fn ignores_torn_tail_record() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(HISTORY_FILE);
    let store = PromptHistoryStore {
        path: Some(path.clone()),
    };

    store.append("kept").expect("append kept");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open history");
    file.write_all(&8_u64.to_le_bytes()).expect("write length");
    file.write_all(b"torn").expect("write partial payload");

    assert_eq!(store.load().expect("load"), vec!["kept"]);
}

/// Complete malformed records should not stop later complete records from
/// loading, because their length still keeps the stream aligned.
#[test]
fn ignores_malformed_record_and_keeps_reading() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(HISTORY_FILE);
    let store = PromptHistoryStore {
        path: Some(path.clone()),
    };

    store.append("before").expect("append before");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open history");
    file.write_all(&4_u64.to_le_bytes()).expect("write length");
    file.write_all(b"junk").expect("write malformed payload");
    drop(file);
    store.append("after").expect("append after");

    assert_eq!(store.load().expect("load"), vec!["before", "after"]);
}

/// Appending after a partial length header should truncate the torn header so
/// the newly appended prompt stays reachable.
#[test]
fn append_after_partial_length_header_keeps_new_entry_reachable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(HISTORY_FILE);
    let store = PromptHistoryStore {
        path: Some(path.clone()),
    };

    store.append("before").expect("append before");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open history");
    file.write_all(&42_u64.to_le_bytes()[..4])
        .expect("write partial length");
    drop(file);

    store
        .append("after")
        .expect("append repairs partial length header");

    assert_eq!(store.load().expect("load"), vec!["before", "after"]);
}

/// Corrupt-tail repair must keep append work bounded; files over the configured
/// cap are discarded before writing the next reachable prompt.
#[test]
fn append_repair_truncates_history_files_over_size_limit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(HISTORY_FILE);
    append_raw_record(
        &path,
        &PromptHistoryRecord {
            version: PROMPT_HISTORY_VERSION,
            recorded_at_micros: 1,
            text: "old".to_owned(),
        },
    );

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open history");
    truncate_corrupt_prompt_history_tail_with_limit(&path, &mut file, 16)
        .expect("truncate over limit");
    assert_eq!(file.metadata().expect("metadata").len(), 0);
    file.seek(io::SeekFrom::End(0)).expect("seek end");
    drop(file);

    let store = PromptHistoryStore { path: Some(path) };

    store.append("new").expect("append after bounded repair");
    assert_eq!(store.load().expect("load"), vec!["new"]);
}

/// Appending near the file-size cap should reset old history before writing the
/// new prompt, preserving the invariant that a file produced by append can be
/// loaded again.
#[test]
fn append_resets_history_when_new_entry_would_exceed_size_limit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(HISTORY_FILE);

    append_prompt_history_locked_with_limit(&path, "old", 256).expect("append old");
    let old_len = path.metadata().expect("metadata").len();
    append_prompt_history_locked_with_limit(&path, "new", old_len + 1)
        .expect("append new with reset");

    let store = PromptHistoryStore { path: Some(path) };
    assert_eq!(store.load().expect("load"), vec!["new"]);
}

/// A single prompt whose framed record cannot fit under the file-size cap
/// should be rejected rather than creating a history file that load will
/// ignore.
#[test]
fn append_rejects_single_entry_over_size_limit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(HISTORY_FILE);

    let error = append_prompt_history_locked_with_limit(&path, "new", 8)
        .expect_err("oversized single entry should fail");

    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

/// A crash can leave a partial final record. Appending must repair that tail
/// first; otherwise the new record is hidden behind the stale length prefix.
#[test]
fn append_after_torn_tail_keeps_new_entry_reachable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(HISTORY_FILE);
    let store = PromptHistoryStore {
        path: Some(path.clone()),
    };

    store.append("before").expect("append before");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open history");
    file.write_all(&8_u64.to_le_bytes()).expect("write length");
    file.write_all(b"torn").expect("write partial payload");
    drop(file);

    store.append("after").expect("append after torn tail");

    assert_eq!(store.load().expect("load"), vec!["before", "after"]);
}

/// Oversized tail lengths are treated as corruption. Repairing before append
/// prevents one bad prefix from permanently hiding all future prompts.
#[test]
fn append_after_oversized_tail_keeps_new_entry_reachable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(HISTORY_FILE);
    let store = PromptHistoryStore {
        path: Some(path.clone()),
    };

    store.append("before").expect("append before");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open history");
    file.write_all(&u64::MAX.to_le_bytes())
        .expect("write corrupt prefix");
    drop(file);

    store.append("after").expect("append after oversized tail");

    assert_eq!(store.load().expect("load"), vec!["before", "after"]);
}

/// Prompt history records use an internal versioned format.
/// Extra fields indicate a schema mismatch, so the record should be skipped.
#[test]
fn prompt_history_record_rejects_unknown_fields() {
    let error = serde_json::from_value::<PromptHistoryRecord>(serde_json::json!({
        "version": PROMPT_HISTORY_VERSION,
        "recorded_at_micros": 42,
        "text": "prompt",
        "extra": true,
    }))
    .expect_err("prompt history record should reject unknown fields");

    assert!(error.to_string().contains("unknown field"), "got: {error}");
}
