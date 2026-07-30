use tau_proto::CborValue;

use super::*;

fn ls_args(path: &std::path::Path) -> CborValue {
    CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )])
}

/// Protects bounded mutation/read paths from blocking indefinitely on Unix
/// special files such as FIFOs.
#[cfg(unix)]
#[test]
fn read_file_limited_rejects_fifo_without_blocking() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let fifo_path = tempdir.path().join("pipe");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo_path)
        .status()
        .expect("run mkfifo");
    assert!(status.success(), "mkfifo failed");

    let error = read_file_limited_real(&fifo_path, MAX_SAFE_FILE_READ_BYTES)
        .expect_err("fifo should be rejected without blocking");

    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("not a regular file"));
}

/// Protects real-world writes against truncate-in-place updates by checking
/// the atomic helper updates existing files through a same-directory
/// rename.
#[test]
fn atomic_write_updates_existing_file() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let path = tempdir.path().join("file.txt");
    std::fs::write(&path, "old\n").expect("write old");

    atomic_write_file(&path, b"new\n").expect("atomic write");

    assert_eq!(std::fs::read_to_string(&path).expect("read"), "new\n");
}

/// Protects file creation semantics for mutation tools after switching from
/// direct writes to same-directory atomic renames.
#[test]
fn atomic_write_creates_new_file() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let path = tempdir.path().join("created.txt");

    atomic_write_file(&path, b"created\n").expect("atomic write");

    assert_eq!(std::fs::read_to_string(&path).expect("read"), "created\n");
}

/// Ensures final symlinks keep the previous write-through behavior: editing
/// a symlink updates its target instead of replacing the symlink itself.
#[cfg(unix)]
#[test]
fn atomic_write_preserves_final_symlink() {
    use std::os::unix::fs::symlink;

    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let target = tempdir.path().join("target.txt");
    let link = tempdir.path().join("link.txt");
    std::fs::write(&target, "old\n").expect("write target");
    symlink("target.txt", &link).expect("symlink");

    atomic_write_file(&link, b"new\n").expect("atomic write");

    assert_eq!(
        std::fs::read_to_string(&target).expect("read target"),
        "new\n"
    );
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("link metadata")
            .file_type()
            .is_symlink()
    );
}

/// Protects chained final symlinks: atomic writes should update the real
/// target at the end of the chain instead of replacing an intermediate
/// link.
#[cfg(unix)]
#[test]
fn atomic_write_follows_chained_final_symlink() {
    use std::os::unix::fs::symlink;

    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let target = tempdir.path().join("target.txt");
    let link2 = tempdir.path().join("link2.txt");
    let link1 = tempdir.path().join("link1.txt");
    std::fs::write(&target, "old\n").expect("write target");
    symlink("target.txt", &link2).expect("link2");
    symlink("link2.txt", &link1).expect("link1");

    atomic_write_file(&link1, b"new\n").expect("atomic write");

    assert_eq!(
        std::fs::read_to_string(&target).expect("read target"),
        "new\n"
    );
    assert!(
        std::fs::symlink_metadata(&link1)
            .expect("link1 metadata")
            .file_type()
            .is_symlink()
    );
    assert!(
        std::fs::symlink_metadata(&link2)
            .expect("link2 metadata")
            .file_type()
            .is_symlink()
    );
}

/// Ensures cleanup after atomic write failure does not delete a colliding
/// temp path that this call failed to create.
#[test]
fn atomic_write_temp_collision_preserves_existing_temp() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let target = tempdir.path().join("target.txt");
    let colliding_temp = tempdir.path().join(".target.txt.tmp-collision");
    std::fs::write(&target, "old\n").expect("write target");
    std::fs::write(&colliding_temp, "someone else\n").expect("write temp");

    let err = atomic_write_file_to_temp(&target, &colliding_temp, b"new\n")
        .expect_err("temp collision should fail");

    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read_to_string(&target).expect("read target"),
        "old\n"
    );
    assert_eq!(
        std::fs::read_to_string(&colliding_temp).expect("read temp"),
        "someone else\n"
    );
}

#[test]
fn ls_vcr_records_world_ops_and_replays_through_tool_logic() {
    let real_dir = tempfile::TempDir::new().expect("real dir");
    std::fs::write(real_dir.path().join("beta"), "b").expect("write beta");
    std::fs::create_dir(real_dir.path().join("alpha")).expect("create alpha");
    let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
    let args = ls_args(real_dir.path());

    let mut recording = ShellWorld::for_tool(
        "ls",
        "call_ls",
        &args,
        Some(tau_vcr::VcrConfig::new(
            tau_vcr::VcrMode::RecordIfMissing,
            cassette_dir.path(),
        )),
    )
    .expect("recording world");
    let recorded = crate::tools::ls::run_ls(&args, &mut recording).expect("recorded ls");
    recording.finish().expect("record cassette");
    std::fs::remove_file(real_dir.path().join("beta")).expect("remove live file");
    std::fs::remove_dir(real_dir.path().join("alpha")).expect("remove live dir");

    let mut replay = ShellWorld::for_tool(
        "ls",
        "call_ls",
        &args,
        Some(tau_vcr::VcrConfig::new(
            tau_vcr::VcrMode::ReplayOnly,
            cassette_dir.path(),
        )),
    )
    .expect("replay world");
    let replayed = crate::tools::ls::run_ls(&args, &mut replay).expect("replayed ls");
    replay.finish().expect("consume replay ops");

    assert_eq!(replayed.result, recorded.result);
    let cassette =
        std::fs::read_to_string(cassette_dir.path().join("call_ls.yaml")).expect("read cassette");
    assert!(cassette.contains("op: is_dir"));
    assert!(cassette.contains("op: read_dir"));
    assert!(cassette.contains("name: alpha"));
    assert!(!cassette.contains("kind: utf8"));
    assert!(!cassette.contains("value: alpha"));
    assert!(!cassette.contains("1 alpha/"));
}
#[test]
fn read_vcr_replays_file_bytes_through_read_logic() {
    let real_dir = tempfile::TempDir::new().expect("real dir");
    let file = real_dir.path().join("file.txt");
    std::fs::write(&file, b"alpha\n\xFFbeta").expect("write file");
    let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(file.display().to_string()),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer(2.into()),
        ),
    ]);

    let mut recording = ShellWorld::for_tool(
        "read",
        "call_read",
        &args,
        Some(tau_vcr::VcrConfig::new(
            tau_vcr::VcrMode::RecordIfMissing,
            cassette_dir.path(),
        )),
    )
    .expect("recording world");
    let recorded = crate::tools::read::read_file(&args, &mut recording).expect("recorded read");
    recording.finish().expect("record cassette");
    std::fs::write(&file, b"changed").expect("change live file");

    let mut replay = ShellWorld::for_tool(
        "read",
        "call_read",
        &args,
        Some(tau_vcr::VcrConfig::new(
            tau_vcr::VcrMode::ReplayOnly,
            cassette_dir.path(),
        )),
    )
    .expect("replay world");
    let replayed = crate::tools::read::read_file(&args, &mut replay).expect("replayed read");
    replay.finish().expect("consume replay ops");

    assert_eq!(replayed.result, recorded.result);
    let cassette =
        std::fs::read_to_string(cassette_dir.path().join("call_read.yaml")).expect("read cassette");
    assert!(cassette.contains("op: read_file"));
    assert!(cassette.contains("\\uDCFFbeta"));
    assert!(!cassette.contains("- 255"));
}
#[test]
fn edit_vcr_replay_asserts_write_without_mutating_live_file() {
    let real_dir = tempfile::TempDir::new().expect("real dir");
    let file = real_dir.path().join("file.txt");
    std::fs::write(&file, b"one\ntwo\n").expect("write file");
    let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(file.display().to_string()),
        ),
        (
            CborValue::Text("edits".to_owned()),
            CborValue::Array(vec![CborValue::Map(vec![
                (
                    CborValue::Text("start_line".to_owned()),
                    CborValue::Integer(2.into()),
                ),
                (
                    CborValue::Text("end_line_exclusive".to_owned()),
                    CborValue::Integer(3.into()),
                ),
                (
                    CborValue::Text("newText".to_owned()),
                    CborValue::Text("TWO\n".to_owned()),
                ),
                (
                    CborValue::Text("context_line".to_owned()),
                    CborValue::Text("two".to_owned()),
                ),
            ])]),
        ),
    ]);

    let mut recording = ShellWorld::for_tool(
        "edit",
        "call_edit",
        &args,
        Some(tau_vcr::VcrConfig::new(
            tau_vcr::VcrMode::RecordIfMissing,
            cassette_dir.path(),
        )),
    )
    .expect("recording world");
    let recorded = crate::tools::edit::edit_file(&args, &mut recording).expect("recorded edit");
    recording.finish().expect("record cassette");
    assert_eq!(
        std::fs::read(&file).expect("read recorded file"),
        b"one\nTWO\n"
    );
    std::fs::write(&file, b"live should not change\n").expect("change live file");

    let mut replay = ShellWorld::for_tool(
        "edit",
        "call_edit",
        &args,
        Some(tau_vcr::VcrConfig::new(
            tau_vcr::VcrMode::ReplayOnly,
            cassette_dir.path(),
        )),
    )
    .expect("replay world");
    let replayed = crate::tools::edit::edit_file(&args, &mut replay).expect("replayed edit");
    replay.finish().expect("consume replay ops");

    assert_eq!(replayed.result, recorded.result);
    assert_eq!(
        std::fs::read(&file).expect("read live file"),
        b"live should not change\n"
    );
    let cassette =
        std::fs::read_to_string(cassette_dir.path().join("call_edit.yaml")).expect("read cassette");
    assert!(cassette.contains("op: read_file"));
    assert!(cassette.contains("op: path_exists"));
    assert!(cassette.contains("op: write_file"));
}
#[test]
fn apply_patch_vcr_replay_asserts_move_write_and_remove_without_mutating_live_files() {
    let real_dir = tempfile::TempDir::new().expect("real dir");
    let source = real_dir.path().join("source.txt");
    let dest = real_dir.path().join("dest.txt");
    std::fs::write(&source, "one\ntwo\n").expect("write source");
    let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
    let args = CborValue::Text(format!(
        "*** Begin Patch\n*** Update File: {}\n*** Move to: {}\n@@\n one\n-two\n+TWO\n*** End Patch",
        source.display(),
        dest.display()
    ));

    let mut recording = ShellWorld::for_tool(
        "apply_patch",
        "call_patch",
        &args,
        Some(tau_vcr::VcrConfig::new(
            tau_vcr::VcrMode::RecordIfMissing,
            cassette_dir.path(),
        )),
    )
    .expect("recording world");
    let recorded = crate::tools::apply_patch::apply_patch(&args, &mut recording)
        .expect("recorded apply_patch");
    recording.finish().expect("record cassette");
    assert!(!source.exists());
    assert_eq!(
        std::fs::read_to_string(&dest).expect("read dest"),
        "one\nTWO\n"
    );
    std::fs::write(&source, "live source\n").expect("restore live source");
    std::fs::write(&dest, "live dest\n").expect("change live dest");

    let mut replay = ShellWorld::for_tool(
        "apply_patch",
        "call_patch",
        &args,
        Some(tau_vcr::VcrConfig::new(
            tau_vcr::VcrMode::ReplayOnly,
            cassette_dir.path(),
        )),
    )
    .expect("replay world");
    let replayed =
        crate::tools::apply_patch::apply_patch(&args, &mut replay).expect("replayed apply_patch");
    replay.finish().expect("consume replay ops");

    assert_eq!(replayed.result, recorded.result);
    assert_eq!(
        std::fs::read_to_string(&source).expect("read source"),
        "live source\n"
    );
    assert_eq!(
        std::fs::read_to_string(&dest).expect("read dest"),
        "live dest\n"
    );
    let cassette = std::fs::read_to_string(cassette_dir.path().join("call_patch.yaml"))
        .expect("read cassette");
    assert!(cassette.contains("op: read_file"));
    assert!(cassette.contains("op: create_dir_all"));
    assert!(cassette.contains("op: write_file"));
    assert!(cassette.contains("op: is_dir"));
    assert!(cassette.contains("op: remove_file"));
}

#[test]
fn apply_patch_vcr_relative_paths_do_not_record_cwd_absolute_paths() {
    let cwd = std::env::current_dir().expect("current dir");
    let real_dir = tempfile::Builder::new()
        .prefix("world-relative-")
        .tempdir_in(&cwd)
        .expect("real dir under cwd");
    let source = real_dir.path().join("source.txt");
    let dest = real_dir.path().join("dest.txt");
    std::fs::write(&source, "one\ntwo\n").expect("write source");
    let source_rel = source.strip_prefix(&cwd).expect("relative source");
    let dest_rel = dest.strip_prefix(&cwd).expect("relative dest");
    let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
    let args = CborValue::Text(format!(
        "*** Begin Patch\n*** Update File: {}\n*** Move to: {}\n@@\n one\n-two\n+TWO\n*** End Patch",
        source_rel.display(),
        dest_rel.display()
    ));

    let mut recording = ShellWorld::for_tool(
        "apply_patch",
        "call_relative_patch",
        &args,
        Some(tau_vcr::VcrConfig::new(
            tau_vcr::VcrMode::RecordIfMissing,
            cassette_dir.path(),
        )),
    )
    .expect("recording world");
    crate::tools::apply_patch::apply_patch(&args, &mut recording).expect("recorded apply_patch");
    recording.finish().expect("record cassette");

    let cassette = std::fs::read_to_string(cassette_dir.path().join("call_relative_patch.yaml"))
        .expect("read cassette");
    assert!(cassette.contains(&format!("path: {}", source_rel.display())));
    assert!(cassette.contains(&format!("path: {}", dest_rel.display())));
    assert!(!cassette.contains(cwd.to_str().expect("utf8 cwd")));
}
