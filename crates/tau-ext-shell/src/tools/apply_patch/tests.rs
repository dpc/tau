use super::*;

#[test]
fn parse_update_hunk() {
    let patch = "*** Begin Patch\n*** Update File: hello.txt\n@@\n-old\n+new\n*** End Patch";
    let hunks = parse_patch(patch).expect("patch should parse");
    assert_eq!(hunks.len(), 1);
}

/// Ensures the parser keeps the Codex add-file format strict: every add-file
/// payload line must be prefixed with `+` so accidental malformed content
/// cannot be interpreted as valid file data.
#[test]
fn parse_add_file_rejects_unprefixed_content() {
    let patch = "*** Begin Patch\n*** Add File: hello.txt\nplain\n*** End Patch";
    let err = parse_patch(patch).expect_err("unprefixed add-file content should fail");

    assert_eq!(err, "invalid add-file line: plain");
}

/// Ensures `*** Move to` remains tied to update hunks and preserves the
/// destination path in the parsed hunk, because later application and lock
/// selection rely on this metadata.
#[test]
fn parse_update_hunk_with_move_destination() {
    let patch = "*** Begin Patch\n*** Update File: old.txt\n*** Move to: new.txt\n@@\n-old\n+new\n*** End Patch";
    let hunks = parse_patch(patch).expect("move update should parse");

    assert_eq!(
        hunks,
        vec![Hunk::Update {
            path: PathBuf::from("old.txt"),
            move_path: Some(PathBuf::from("new.txt")),
            chunks: vec![UpdateChunk {
                change_context: None,
                old_lines: vec!["old".to_owned()],
                new_lines: vec!["new".to_owned()],
                is_end_of_file: false,
            }],
        }]
    );
}

/// Ensures the grammar-declared `*** End of File` marker is parsed as chunk
/// metadata instead of being mistaken for the start of another patch operation.
#[test]
fn parse_update_hunk_end_of_file_marker() {
    let patch = "*** Begin Patch\n*** Update File: file.txt\n@@\n-old\n+new\n*** End of File\n*** End Patch";
    let hunks = parse_patch(patch).expect("end-of-file update should parse");
    let [Hunk::Update { chunks, .. }] = hunks.as_slice() else {
        panic!("expected one update hunk");
    };

    assert!(chunks[0].is_end_of_file);
}

/// Ensures delete hunks still parse as single-line operations, preventing the
/// parser refactor from requiring chunk content for file deletion.
#[test]
fn parse_delete_hunk() {
    let patch = "*** Begin Patch\n*** Delete File: old.txt\n*** End Patch";
    let hunks = parse_patch(patch).expect("delete should parse");

    assert_eq!(
        hunks,
        vec![Hunk::Delete {
            path: PathBuf::from("old.txt")
        }]
    );
}

#[test]
fn compute_replacements_with_context() {
    let original = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
    let chunks = vec![UpdateChunk {
        change_context: Some("a".to_owned()),
        old_lines: vec!["b".to_owned()],
        new_lines: vec!["B".to_owned()],
        is_end_of_file: false,
    }];
    let replacements = compute_replacements(&original, Path::new("file.txt"), &chunks)
        .expect("replacement plan should compute");
    assert_eq!(replacements, vec![(1, 1, vec!["B".to_owned()])]);
}

#[test]
fn context_only_chunk_can_position_later_update_chunk() {
    // Codex-style patches sometimes use an initial context-only chunk as a
    // cursor before a later chunk performs the real edit. Accept that shape so
    // Tau can apply patches generated for the same apply_patch format.
    let patch = "*** Begin Patch\n*** Update File: file.txt\n@@\n fn anchor() {\n@@\n }\n\n+#[test]\n+fn inserted() {}\n+\n #[test]\n fn next() {}\n*** End Patch";
    let hunks = parse_patch(patch).expect("context-only chunk should parse");
    let [Hunk::Update { chunks, .. }] = hunks.as_slice() else {
        panic!("expected one update hunk");
    };

    let original = "fn before() {}\n\nfn anchor() {\n}\n\n#[test]\nfn next() {}\n";
    let new_contents = derive_new_contents_from_chunks(Path::new("file.txt"), original, chunks)
        .expect("context-only chunk should guide the later insertion");

    assert_eq!(
        new_contents,
        "fn before() {}\n\nfn anchor() {\n}\n\n#[test]\nfn inserted() {}\n\n#[test]\nfn next() {}\n"
    );
}

#[test]
fn format_single_file_diff_payload() {
    let summary = format_summary(&[AppliedChange {
        display_path: "file.txt".to_owned(),
        path: PathBuf::from("file.txt"),
        status: ChangeStatus::Modify,
        old_content: "before\n".to_owned(),
        new_content: Some("after\n".to_owned()),
    }]);
    assert!(matches!(
        display_payload_for_changes(
            &[AppliedChange {
                display_path: "file.txt".to_owned(),
                path: PathBuf::from("file.txt"),
                status: ChangeStatus::Modify,
                old_content: "before\n".to_owned(),
                new_content: Some("after\n".to_owned()),
            }],
            &summary,
        ),
        Some(ToolUsePayload::Diff(_))
    ));
}

/// Ensures `*** Add File` cannot silently clobber an existing path; callers
/// must use an update hunk when they intend to overwrite content.
#[test]
fn add_file_rejects_existing_target() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("exists.txt");
    std::fs::write(&path, "original\n").expect("write original");

    let mut world = ShellWorld::real();
    let err = apply_hunks(
        &[Hunk::Add {
            path: path.clone(),
            contents: "replacement\n".to_owned(),
        }],
        &mut world,
    )
    .expect_err("add file should reject existing target");

    assert!(
        err.message.contains("Add File target already exists"),
        "unexpected error: {}",
        err.message
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("read original"),
        "original\n"
    );
}
