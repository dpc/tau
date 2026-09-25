//! Scope replacement oracles independent of process HOME and startup cwd.

use std::fs;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use super::DiscoverySource;

/// Write one fixture with deterministic collision precedence inputs.
fn write_skill(path: &Path, description: &str, modified: u64) {
    fs::create_dir_all(path.parent().expect("skill parent")).expect("skill root");
    fs::write(
        path,
        format!("---\nname: shared\ndescription: {description}\n---\n"),
    )
    .expect("skill");
    fs::File::options()
        .write(true)
        .open(path)
        .expect("open skill")
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(modified)))
        .expect("mtime");
}

/// Project replacement restores complete user candidates and preserves sampled
/// user instructions, paths, timestamps and catalog metadata rather than
/// rereads.
#[test]
fn replacing_project_preserves_user_snapshot_and_collision_fallback() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let a = home.join("a");
    let b = home.join("b");
    fs::create_dir_all(&b).expect("project b");
    let project_skill = a.join(".agents/skills/shared.md");
    let xdg_skill = home.join(".config/agents/skills/shared.md");
    let legacy_skill = home.join(".agents/skills/shared.md");
    write_skill(&project_skill, "project-a", 200);
    write_skill(&xdg_skill, "xdg-user", 100);
    write_skill(&legacy_skill, "legacy-user", 300);
    let user_agents = home.join(".config/agents/AGENTS.md");
    fs::write(&user_agents, "USER ORIGINAL").expect("user instructions");
    fs::write(a.join("AGENTS.md"), "PROJECT A").expect("project instructions");
    fs::write(b.join("AGENTS.md"), "PROJECT B").expect("project instructions");
    let source = DiscoverySource::new(Some(home.clone()));
    let first = source.scan_project("session".parse().expect("session id"), Some(&a));
    assert_eq!(first.snapshot.skills[0].description, "legacy-user");
    assert_eq!(
        first.snapshot.skills[0]
            .sampled_modified
            .expect("sampled mtime")
            .get(),
        300_000_000,
    );
    assert!(
        first
            .snapshot
            .agents_files
            .iter()
            .any(|file| file.content == "PROJECT A")
    );

    let retained = source.retained_user_state();
    drop(source);
    write_skill(&xdg_skill, "edited-user", 1000);
    fs::remove_file(&legacy_skill).expect("remove legacy skill");
    fs::write(&user_agents, "USER EDITED").expect("edit user instructions");
    let source =
        DiscoverySource::from_retained_user_state(&retained).expect("restore original user");
    assert_eq!(source.user_candidates().len(), 2);
    let second = source.scan_project("session".parse().expect("session id"), Some(&b));
    assert_eq!(second.snapshot.skills.len(), 1);
    let user = &second.snapshot.skills[0];
    assert_eq!(user.description, "xdg-user");
    assert_eq!(
        user.sampled_modified.expect("sampled mtime").get(),
        100_000_000
    );
    assert!(!user.add_to_prompt);
    assert_eq!(
        user.file_path,
        xdg_skill.canonicalize().expect("canonical user skill")
    );
    let bodies = second
        .snapshot
        .agents_files
        .iter()
        .map(|file| file.content.as_str())
        .collect::<Vec<_>>();
    assert!(bodies.contains(&"USER ORIGINAL"));
    assert!(bodies.contains(&"PROJECT B"));
    assert!(!bodies.contains(&"USER EDITED"));
    assert!(!bodies.contains(&"PROJECT A"));

    // An explicit same-path scan sees project edits and deletions without
    // refreshing users, and clearing the project retains user contributions.
    fs::remove_file(a.join("AGENTS.md")).expect("delete project instructions");
    fs::remove_file(&project_skill).expect("delete project skill");
    let third = source.scan_project("session".parse().expect("session id"), Some(&a));
    assert_eq!(third.snapshot.skills, second.snapshot.skills);
    assert!(
        third
            .snapshot
            .agents_files
            .iter()
            .all(|file| file.content != "PROJECT A")
    );
    let cleared = source.scan_project("session".parse().expect("session id"), None);
    assert_eq!(cleared.snapshot.skills, second.snapshot.skills);
    assert_eq!(cleared.snapshot.agents_files.len(), 1);
    assert_eq!(cleared.snapshot.agents_files[0].content, "USER ORIGINAL");
}

/// A user symlink's captured target must not drift when only project discovery
/// changes; overlapping AGENTS paths still appear once, with user scope first.
#[cfg(unix)]
#[test]
fn replacing_project_preserves_user_symlink_targets_and_deduplication() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let project = home.join("project");
    let user_root = home.join(".config/agents");
    fs::create_dir_all(&project).expect("project");
    fs::create_dir_all(user_root.join("skills")).expect("user skills");
    let first_target = temp.path().join("first.md");
    let next_target = temp.path().join("next.md");
    write_skill(&first_target, "original-user", 100);
    write_skill(&next_target, "changed-user", 200);
    let link = user_root.join("skills/shared.md");
    symlink(&first_target, &link).expect("user skill link");
    let shared_agents = user_root.join("AGENTS.md");
    fs::write(&shared_agents, "SHARED INSTRUCTIONS").expect("agents");
    symlink(&shared_agents, project.join("AGENTS.md")).expect("project link");
    let source = DiscoverySource::new(Some(home));
    let retained = source.retained_user_state();
    drop(source);
    fs::remove_file(&link).expect("unlink");
    symlink(&next_target, &link).expect("retarget");
    let source =
        DiscoverySource::from_retained_user_state(&retained).expect("restore symlink sample");
    let scan = source.scan_project("session".parse().expect("session id"), Some(&project));
    assert_eq!(scan.snapshot.skills[0].description, "original-user");
    assert_eq!(
        scan.snapshot.skills[0].file_path,
        first_target.canonicalize().expect("canonical target")
    );
    assert_eq!(
        scan.snapshot
            .agents_files
            .iter()
            .filter(|file| file.content == "SHARED INSTRUCTIONS")
            .count(),
        1
    );
}
