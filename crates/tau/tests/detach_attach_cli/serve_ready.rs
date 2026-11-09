//! Public foreground allocator and stdio identity handoff regressions.

use std::os::unix::fs::symlink;

use super::*;

/// Real foreground child whose cleanup also runs if a readiness assertion
/// fails.
struct ReadyServer {
    /// Child retained until explicit signal-and-reap or fallback cleanup.
    child: Child,
    /// Private stdout capture, outside the daemon's configuration/state trees.
    stdout_path: PathBuf,
}

impl ReadyServer {
    /// Start native allocation from the requested project directory.
    fn start(environment: &TestEnvironment, cwd: &Path, index: usize) -> Self {
        let stdout_path = environment.temp.path().join(format!("ready-{index}.json"));
        let child = environment
            .command()
            .current_dir(cwd)
            .args(["serve", "--create", "--ready-json"])
            .stdin(Stdio::null())
            .stdout(File::create(&stdout_path).expect("create readiness capture"))
            .stderr(Stdio::null())
            .spawn()
            .expect("start native foreground allocation");
        Self { child, stdout_path }
    }

    /// Wait for a flushed, complete bounded line, never a partial JSON prefix.
    fn identity(&mut self) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let bytes = std::fs::read(&self.stdout_path).expect("read readiness capture");
            assert!(bytes.len() <= 64 * 1024);
            if bytes.last() == Some(&b'\n') {
                assert_eq!(bytes.iter().filter(|b| **b == b'\n').count(), 1);
                return serde_json::from_slice(&bytes).expect("one complete identity object");
            }
            assert!(self.child.try_wait().expect("check server").is_none());
            assert!(Instant::now() < deadline, "ready record was not flushed");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Terminate normally and ensure no output followed the readiness record.
    fn stop(&mut self, identity: &serde_json::Value) {
        assert!(
            Command::new("kill")
                .args(["-TERM", &self.child.id().to_string()])
                .status()
                .expect("signal ready server")
                .success()
        );
        assert!(self.child.wait().expect("wait ready server").success());
        let bytes = std::fs::read(&self.stdout_path).expect("read final stdout");
        assert_eq!(bytes.iter().filter(|b| **b == b'\n').count(), 1);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).expect("final identity"),
            *identity
        );
    }
}

impl Drop for ReadyServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Native allocation uses the ordinary sanitized basename and independent IDs
/// at one canonical root; readiness identifies the exact routable durable
/// owner.
#[test]
fn native_serve_ready_identity_matches_catalog_and_exclusive_session() {
    let environment = TestEnvironment::new();
    let project = environment.temp.path().join("serve project\n");
    std::fs::create_dir(&project).expect("create project");
    let alias = environment.temp.path().join("alias");
    symlink(&project, &alias).expect("symlink project");
    let mut first = ReadyServer::start(&environment, &alias, 1);
    let identity = first.identity();
    let mut second = ReadyServer::start(&environment, &project, 2);
    let second_identity = second.identity();
    assert_ne!(identity["session_id"], second_identity["session_id"]);
    for ready in [&identity, &second_identity] {
        assert_eq!(ready.as_object().expect("identity object").len(), 2);
        assert_eq!(
            ready["project_root"],
            project.to_str().expect("UTF-8 project")
        );
        let id = ready["session_id"].as_str().expect("session id");
        let suffix = id.strip_prefix("serve_project_-").expect("native basename");
        assert_eq!(suffix.len(), 6);
        assert!(
            suffix
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        );
        assert!(
            environment
                .state_home
                .join(format!("tau/sessions/{id}/meta.json"))
                .is_file()
        );
    }
    let listed = environment
        .command()
        .args(["session", "list", "--json"])
        .output()
        .expect("query live catalog");
    assert!(listed.status.success());
    let listed: Vec<serde_json::Value> =
        serde_json::from_slice(&listed.stdout).expect("catalog JSON");
    assert_eq!(listed.len(), 2);
    assert!(listed.contains(&identity));
    assert!(listed.contains(&second_identity));
    second.stop(&second_identity);
    first.stop(&identity);
    environment.assert_runtime_discovery_empty();
}

/// Failed exclusive creation and a failed stdout write must never leave a
/// running foreground service or claim that the requested identity is ready.
#[test]
fn serve_ready_fails_closed_on_creation_and_output_errors() {
    let environment = TestEnvironment::new();
    let session_dir = environment.state_home.join("tau/sessions/occupied");
    std::fs::create_dir_all(&session_dir).expect("create occupied state");
    std::fs::write(session_dir.join("canary"), b"preserve").expect("write canary");
    let rejected = environment
        .command()
        .args(["serve", "--create", "--session", "occupied", "--ready-json"])
        .output()
        .expect("reject occupied session");
    assert!(!rejected.status.success());
    assert!(rejected.stdout.is_empty());
    assert_eq!(
        std::fs::read(session_dir.join("canary")).expect("read preserved canary"),
        b"preserve"
    );
    environment.assert_runtime_discovery_empty();

    let failed_write = environment
        .command()
        .args(["serve", "--create", "--ready-json"])
        .stdout(
            File::options()
                .write(true)
                .open("/dev/full")
                .expect("open full device"),
        )
        .output()
        .expect("run failed readiness writer");
    assert!(!failed_write.status.success());
    environment.assert_runtime_discovery_empty();
}
