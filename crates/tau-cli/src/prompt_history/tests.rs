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

/// Queued prompt history should persist in append order, including multiline
/// prompt text, once the asynchronous worker crosses its test ordering point.
#[test]
fn persistence_worker_writes_queued_history_in_order() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = PromptHistoryStore::for_path(tmp.path().join(HISTORY_FILE));

    assert_eq!(store.append("one"), PromptHistoryAdmission::Queued);
    assert_eq!(store.append("two\nlines"), PromptHistoryAdmission::Queued);
    store.wait_for_persistence();

    assert_eq!(store.load().expect("load"), vec!["one", "two\nlines"]);
    assert_eq!(store.queued_bytes.load(Ordering::Acquire), 0);
}

/// A saturated persistence queue must drop its newest request immediately
/// instead of making a prompt submission wait for a worker to make room.
#[test]
fn persistence_admission_drops_when_the_bounded_queue_is_full() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, _persistence_rx) =
        PromptHistoryStore::for_path_without_worker(tmp.path().join(HISTORY_FILE));

    for index in 0..PERSIST_QUEUE_CAPACITY {
        assert_eq!(
            store.append(&format!("queued prompt {index}")),
            PromptHistoryAdmission::Queued
        );
    }
    assert_eq!(
        store.append("dropped prompt"),
        PromptHistoryAdmission::DroppedFull
    );
}

/// The shared byte budget must reject an oversized request before retaining a
/// copy, and it must account for every queued request independently of slots.
#[test]
fn persistence_admission_drops_when_the_byte_budget_is_full() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, _persistence_rx) =
        PromptHistoryStore::for_path_without_worker(tmp.path().join(HISTORY_FILE));
    let oversized = "x".repeat(PERSIST_QUEUE_MAX_BYTES + 1);

    assert_eq!(
        store.append(&oversized),
        PromptHistoryAdmission::DroppedFull
    );
    assert_eq!(store.queued_bytes.load(Ordering::Acquire), 0);
    assert_eq!(
        store.append(&"x".repeat(PERSIST_QUEUE_MAX_BYTES)),
        PromptHistoryAdmission::Queued
    );
    assert_eq!(
        store.append("dropped after byte budget"),
        PromptHistoryAdmission::DroppedFull
    );
}

/// Missing persistence configuration and a stopped worker must both drop
/// requests without blocking prompt submission.
#[test]
fn persistence_admission_reports_unavailable_workers() {
    let unavailable = PromptHistoryStore::for_optional_path(None);
    assert_eq!(
        unavailable.append("no state directory"),
        PromptHistoryAdmission::DroppedUnavailable
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let (disconnected, persistence_rx) =
        PromptHistoryStore::for_path_without_worker(tmp.path().join(HISTORY_FILE));
    drop(persistence_rx);
    assert_eq!(
        disconnected.append("stopped worker"),
        PromptHistoryAdmission::DroppedUnavailable
    );
    assert_eq!(disconnected.queued_bytes.load(Ordering::Acquire), 0);
}

/// Empty prompts must remain a truthful no-op instead of claiming persistence
/// admission or reserving queue memory.
#[test]
fn persistence_admission_ignores_empty_prompts() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, _persistence_rx) =
        PromptHistoryStore::for_path_without_worker(tmp.path().join(HISTORY_FILE));

    assert_eq!(store.append(""), PromptHistoryAdmission::IgnoredEmpty);
    assert_eq!(store.queued_bytes.load(Ordering::Acquire), 0);
}

/// Once an unchanged tail is witnessed, warm validation must not revisit any
/// record in the already-validated prefix.
#[test]
fn warm_tail_validation_skips_unchanged_history_prefix() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(HISTORY_FILE);
    let store = PromptHistoryStore::for_path(path.clone());
    for index in 0..1_000 {
        append_raw_record(
            &path,
            &PromptHistoryRecord {
                version: PROMPT_HISTORY_VERSION,
                recorded_at_micros: index,
                text: format!("prompt {index}"),
            },
        );
    }
    store.load().expect("load seeded history");
    let tail = store
        .validated_tail
        .lock()
        .expect("tail lock")
        .clone()
        .expect("validated tail");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open history");

    assert!(tail.matches(&mut file).expect("match witness"));
    let validation = truncate_corrupt_prompt_history_tail_from_with_limit(
        &path,
        &mut file,
        MAX_HISTORY_FILE_BYTES,
        tail.end_offset,
    )
    .expect("validate unchanged suffix");
    assert_eq!(validation.end_offset, tail.end_offset);
    assert_eq!(
        validation.records_scanned, 0,
        "warm validation must be independent of prefix record count"
    );
    drop(file);

    store.append_and_wait("warm append");
    let appended_tail = store
        .validated_tail
        .lock()
        .expect("tail lock")
        .clone()
        .expect("updated tail");
    assert_eq!(
        appended_tail.records_scanned, 0,
        "production append wiring must use the matched witness"
    );
    assert_eq!(
        store
            .load()
            .expect("load appended history")
            .last()
            .map(String::as_str),
        Some("warm append")
    );
}

/// A second CLI may append while this process is idle; the cached witness must
/// validate that external suffix before appending another reachable record.
#[test]
fn cached_tail_validates_external_append_delta() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(HISTORY_FILE);
    let store = PromptHistoryStore::for_path(path.clone());
    store.append_and_wait("first");

    append_raw_record(
        &path,
        &PromptHistoryRecord {
            version: PROMPT_HISTORY_VERSION,
            recorded_at_micros: 1,
            text: "external".to_owned(),
        },
    );
    store.append_and_wait("last");
    let appended_tail = store
        .validated_tail
        .lock()
        .expect("tail lock")
        .clone()
        .expect("updated tail");
    assert_eq!(
        appended_tail.records_scanned, 1,
        "only the externally appended frame should be validated"
    );

    assert_eq!(
        store.load().expect("load"),
        vec!["first", "external", "last"]
    );
}

/// Same-inode mutation inside the boundary witness must invalidate the cached
/// prefix and trigger bounded full-file validation.
#[test]
fn cached_tail_falls_back_after_boundary_mismatch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(HISTORY_FILE);
    let store = PromptHistoryStore::for_path(path.clone());
    store.append_and_wait("old boundary");
    let tail = store
        .validated_tail
        .lock()
        .expect("tail lock")
        .clone()
        .expect("validated tail");
    let mut file = OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open history");
    file.seek(io::SeekFrom::Start(tail.end_offset - 1))
        .expect("seek boundary");
    file.write_all(b"x").expect("mutate boundary");
    drop(file);

    store.append_and_wait("new");

    let appended_tail = store
        .validated_tail
        .lock()
        .expect("tail lock")
        .clone()
        .expect("updated tail");
    assert_eq!(
        appended_tail.records_scanned, 1,
        "boundary mismatch should validate the complete one-record prefix"
    );
    assert_eq!(
        store
            .load()
            .expect("load repaired history")
            .last()
            .map(String::as_str),
        Some("new")
    );
}

/// Truncation below cached EOF on the same inode must invalidate the witness
/// and validate a replacement prefix from byte zero.
#[test]
fn cached_tail_falls_back_after_same_inode_truncation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(HISTORY_FILE);
    let store = PromptHistoryStore::for_path(path.clone());
    store.append_and_wait(&format!("old {}", "x".repeat(4_096)));
    let tail = store
        .validated_tail
        .lock()
        .expect("tail lock")
        .clone()
        .expect("validated tail");
    let original_inode = path.metadata().expect("old metadata").ino();
    OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open history")
        .set_len(0)
        .expect("truncate history");
    append_raw_record(
        &path,
        &PromptHistoryRecord {
            version: PROMPT_HISTORY_VERSION,
            recorded_at_micros: 1,
            text: "replacement".to_owned(),
        },
    );
    let replacement_metadata = path.metadata().expect("replacement metadata");
    assert_eq!(replacement_metadata.ino(), original_inode);
    assert!(
        replacement_metadata.len() < tail.end_offset,
        "replacement must exercise shorter-than-witness invalidation"
    );

    store.append_and_wait("new");

    let appended_tail = store
        .validated_tail
        .lock()
        .expect("tail lock")
        .clone()
        .expect("updated tail");
    assert_eq!(appended_tail.records_scanned, 1);
    assert_eq!(store.load().expect("load"), vec!["replacement", "new"]);
}

/// File replacement invalidates inode identity and must fall back to validating
/// the replacement from its first frame.
#[test]
fn cached_tail_falls_back_after_file_replacement() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(HISTORY_FILE);
    let store = PromptHistoryStore::for_path(path.clone());
    store.append_and_wait("old");
    fs::remove_file(&path).expect("replace history");
    append_raw_record(
        &path,
        &PromptHistoryRecord {
            version: PROMPT_HISTORY_VERSION,
            recorded_at_micros: 1,
            text: "replacement".to_owned(),
        },
    );

    store.append_and_wait("new");

    assert_eq!(store.load().expect("load"), vec!["replacement", "new"]);
}

/// Loading should honor the history file-size cap before scanning records, so a
/// legacy oversized file cannot block startup.
#[test]
fn load_ignores_history_files_over_size_limit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(HISTORY_FILE);
    let store = PromptHistoryStore::for_path(path.clone());

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
    let store = PromptHistoryStore::for_path(path.clone());

    store.append_and_wait("kept");
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
    let store = PromptHistoryStore::for_path(path.clone());

    store.append_and_wait("before");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open history");
    file.write_all(&4_u64.to_le_bytes()).expect("write length");
    file.write_all(b"junk").expect("write malformed payload");
    drop(file);
    store.append_and_wait("after");

    assert_eq!(store.load().expect("load"), vec!["before", "after"]);
}

/// Appending after a partial length header should truncate the torn header so
/// the newly appended prompt stays reachable.
#[test]
fn append_after_partial_length_header_keeps_new_entry_reachable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(HISTORY_FILE);
    let store = PromptHistoryStore::for_path(path.clone());

    store.append_and_wait("before");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open history");
    file.write_all(&42_u64.to_le_bytes()[..4])
        .expect("write partial length");
    drop(file);

    store.append_and_wait("after");

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

    let store = PromptHistoryStore::for_path(path);

    store.append_and_wait("new");
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

    let store = PromptHistoryStore::for_path(path);
    assert_eq!(store.load().expect("load"), vec!["new"]);
}

/// A single prompt whose framed record cannot fit under the file-size cap
/// should be rejected rather than creating a history file that load will
/// ignore.
#[test]
fn append_rejects_single_entry_over_size_limit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(HISTORY_FILE);

    let error = match append_prompt_history_locked_with_limit(&path, "new", 8) {
        Ok(_) => panic!("oversized single entry should fail"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

/// A crash can leave a partial final record. Appending must repair that tail
/// first; otherwise the new record is hidden behind the stale length prefix.
#[test]
fn append_after_torn_tail_keeps_new_entry_reachable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(HISTORY_FILE);
    let store = PromptHistoryStore::for_path(path.clone());

    store.append_and_wait("before");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open history");
    file.write_all(&8_u64.to_le_bytes()).expect("write length");
    file.write_all(b"torn").expect("write partial payload");
    drop(file);

    store.append_and_wait("after");

    assert_eq!(store.load().expect("load"), vec!["before", "after"]);
}

/// Oversized tail lengths are treated as corruption. Repairing before append
/// prevents one bad prefix from permanently hiding all future prompts.
#[test]
fn append_after_oversized_tail_keeps_new_entry_reachable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(HISTORY_FILE);
    let store = PromptHistoryStore::for_path(path.clone());

    store.append_and_wait("before");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open history");
    file.write_all(&u64::MAX.to_le_bytes())
        .expect("write corrupt prefix");
    drop(file);

    store.append_and_wait("after");

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

/// Diagnostic admission labels must remain bounded classes and must never
/// derive labels from prompt content.
#[test]
fn prompt_history_diagnostic_classes_are_fixed_and_content_free() {
    assert_eq!(PromptHistoryAdmission::Queued.diagnostic_class(), "queued");
    assert_eq!(
        PromptHistoryAdmission::IgnoredEmpty.diagnostic_class(),
        "ignored_empty"
    );
    assert_eq!(
        PromptHistoryAdmission::DroppedFull.diagnostic_class(),
        "dropped_full"
    );
    assert_eq!(
        PromptHistoryAdmission::DroppedUnavailable.diagnostic_class(),
        "dropped_unavailable"
    );
}
