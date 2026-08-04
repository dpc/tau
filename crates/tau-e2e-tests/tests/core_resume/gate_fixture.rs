//! Private-root configuration and artifact ownership for the Gate 1 PTY test.

use std::cell::Cell;
use std::fs as path_std_fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;

use fs2::FileExt;
use tau_e2e_tests::ScenarioV2;
use tempfile::TempDir;

/// Hermetic filesystem and executable configuration for one Gate 1 run.
pub(super) struct GateFixture {
    /// Temporary private root retained only on failure or explicit opt-in.
    tempdir: Option<TempDir>,
    /// Exact canonical universal Tau executable.
    tau_bin: PathBuf,
    /// Private HOME.
    home: PathBuf,
    /// Private XDG configuration root.
    config_home: PathBuf,
    /// Private XDG state root.
    state_home: PathBuf,
    /// Private XDG cache root.
    cache_home: PathBuf,
    /// Private XDG runtime root.
    runtime_home: PathBuf,
    /// Fixed child working directory.
    cwd: PathBuf,
    /// Bounded retained diagnostic directory.
    artifacts: PathBuf,
    /// Whether test completion was acknowledged.
    completed: Cell<bool>,
    /// Exact extension names enabled at the universal CLI boundary.
    enabled_extensions: &'static [&'static str],
    /// Fixture-private release socket and nonce for the releasable dummy mode.
    dummy_release: Option<(PathBuf, String)>,
}

impl GateFixture {
    /// Creates private XDG roots and the exact fake/dummy-only configuration.
    pub(super) fn new(
        scenario: &ScenarioV2,
        fake_provider_bin: &Path,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_with_mode(scenario, fake_provider_bin, FixtureMode::DummyTool)
    }

    /// Creates the dummy-tool configuration whose invocation completes only
    /// after an exact fixture-private release frame.
    pub(super) fn new_with_dummy_release(
        scenario: &ScenarioV2,
        fake_provider_bin: &Path,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_with_mode(
            scenario,
            fake_provider_bin,
            FixtureMode::ReleasableDummyTool,
        )
    }

    /// Creates the closed S8 production-main/worker configuration.
    pub(super) fn new_multi_agent(
        scenario: &ScenarioV2,
        fake_provider_bin: &Path,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_with_mode(scenario, fake_provider_bin, FixtureMode::MultiAgent)
    }

    /// Creates the closed peer-entrypoint configuration with no initial agent.
    pub(super) fn new_peer_entrypoint(
        scenario: &ScenarioV2,
        fake_provider_bin: &Path,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_with_mode(scenario, fake_provider_bin, FixtureMode::PeerEntrypoint)
    }

    fn new_with_mode(
        scenario: &ScenarioV2,
        fake_provider_bin: &Path,
        mode: FixtureMode,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let tempdir = TempDir::new()?;
        std::fs::set_permissions(tempdir.path(), path_std_fs::Permissions::from_mode(0o700))?;
        let root = tempdir.path();
        let home = root.join("home");
        let config_home = root.join("xdg-config");
        let state_home = root.join("xdg-state");
        let cache_home = root.join("xdg-cache");
        let runtime_home = root.join("xdg-runtime");
        let cwd = root.join("cwd");
        let artifacts = root.join("artifacts");
        for directory in [
            &home,
            &config_home,
            &state_home,
            &cache_home,
            &runtime_home,
            &cwd,
            &artifacts,
        ] {
            std::fs::create_dir_all(directory)?;
            std::fs::set_permissions(directory, path_std_fs::Permissions::from_mode(0o700))?;
        }
        let tau_bin = exact_tau_binary()?;
        let fake_provider_bin = fake_provider_bin.canonicalize()?;
        if !fake_provider_bin.is_file() {
            return Err("fake-provider executable is not a file".into());
        }
        let tau_config = config_home.join("tau");
        std::fs::create_dir_all(&tau_config)?;
        let mut extensions = serde_json::Map::new();
        for name in [
            "provider-builtin",
            "core-shell",
            "test-dummy",
            "std-rhai",
            "std-rostra",
            "std-notifications",
            "std-slack",
            "std-telegram",
            "std-zulip",
            "std-xmpp",
            "std-utils",
            "std-websearch",
            "std-pim",
            "std-email",
        ] {
            extensions.insert(name.to_owned(), serde_json::json!({ "enable": false }));
        }
        extensions.insert(
            "e2e-fake-provider".to_owned(),
            serde_json::json!({
                "command": [fake_provider_bin],
                "role": "provider",
                "require": true,
                "cwd": artifacts,
                "config": { "scenario": scenario },
            }),
        );
        let dummy_release = (mode == FixtureMode::ReleasableDummyTool).then(|| {
            (
                root.join("dummy-release.sock"),
                format!("core-resume-release-{}", std::process::id()),
            )
        });
        if matches!(
            mode,
            FixtureMode::DummyTool | FixtureMode::ReleasableDummyTool
        ) {
            let config = if let Some((socket, nonce)) = &dummy_release {
                serde_json::json!({
                    "restart_mode": "hold_until_success_release",
                    "release_socket_path": socket,
                    "release_nonce": nonce,
                })
            } else {
                serde_json::json!({ "restart_mode": "success" })
            };
            extensions.insert(
                "test-dummy".to_owned(),
                serde_json::json!({
                    "enable": true,
                    "require": true,
                    "command": [tau_bin],
                    "suffix": ["component", "ext-test-dummy"],
                    "role": "tool",
                    "config": config,
                }),
            );
        }
        let (default_role, roles) = match mode {
            FixtureMode::DummyTool | FixtureMode::ReleasableDummyTool => (
                "deterministic-e2e",
                serde_json::json!({
                    "deterministic-e2e": {
                        "model": "fake/test",
                        "tools": ["restart_test_dummy"],
                    }
                }),
            ),
            FixtureMode::MultiAgent => (
                "deterministic-main",
                serde_json::json!({
                    "deterministic-main": {
                        "model": "fake/test",
                        "tools": ["agent_start"],
                    },
                    "deterministic-worker": {
                        "model": "fake/test",
                        "tools": [],
                    }
                }),
            ),
            FixtureMode::PeerEntrypoint => (
                "deterministic-peer",
                serde_json::json!({
                    "deterministic-peer": {
                        "model": "fake/test",
                        "tools": [],
                        "inter_session_receiver": true,
                        "inter_session_auto_start": true,
                    }
                }),
            ),
        };
        let harness = serde_json::json!({
            "agents": {
                "default_role": default_role,
                "id_template": "main",
                "role_groups": {
                    "e2e": {
                        "roles": roles
                    }
                }
            },
            "extensions": extensions,
        });
        std::fs::write(
            tau_config.join("harness.yaml"),
            serde_json::to_vec_pretty(&harness)?,
        )?;
        std::fs::write(
            tau_config.join("cli.yaml"),
            b"greeting: false\nshow_tools: full\nshow_thinking: false\nshow_turn_stats: false\n",
        )?;
        let tau_state = state_home.join("tau");
        std::fs::create_dir_all(&tau_state)?;
        std::fs::write(tau_state.join("cli.json"), br#"{"show_tools":"full"}"#)?;
        std::fs::write(
            artifacts.join("scenario.json"),
            serde_json::to_vec_pretty(scenario)?,
        )?;
        std::fs::write(
            artifacts.join("harness-config.redacted.json"),
            serde_json::to_vec_pretty(&harness)?,
        )?;
        std::fs::write(
            artifacts.join("environment-keys.txt"),
            "HOME\nXDG_CONFIG_HOME\nXDG_STATE_HOME\nXDG_CACHE_HOME\nXDG_RUNTIME_DIR\nTERM\nLANG\n",
        )?;
        Ok(Self {
            tempdir: Some(tempdir),
            tau_bin,
            home,
            config_home,
            state_home,
            cache_home,
            runtime_home,
            cwd,
            artifacts,
            completed: Cell::new(false),
            enabled_extensions: match mode {
                FixtureMode::DummyTool | FixtureMode::ReleasableDummyTool => {
                    &["e2e-fake-provider", "test-dummy"]
                }
                FixtureMode::MultiAgent | FixtureMode::PeerEntrypoint => &["e2e-fake-provider"],
            },
            dummy_release,
        })
    }

    /// Enables prompt-draft content only for a test that needs exact draft
    /// payloads as a synchronization oracle.
    pub(super) fn enable_prompt_draft_content(&self) -> Result<(), std::io::Error> {
        let drop_in = self.config_home.join("tau").join("cli.d");
        std::fs::create_dir_all(&drop_in)?;
        std::fs::write(
            drop_in.join("10-send-prompt-draft-content.yaml"),
            "send_prompt_draft_content: true\n",
        )
    }

    /// Builds a fully scrubbed exact-Tau command for a fresh or resumed boot.
    pub(super) fn command(&self, resume: Option<&str>) -> Command {
        let mut command = Command::new(&self.tau_bin);
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("XDG_CACHE_HOME", &self.cache_home)
            .env("XDG_RUNTIME_DIR", &self.runtime_home)
            .env("TERM", "xterm-256color")
            .env("LANG", "C.UTF-8")
            .arg("--disable-extensions-all")
            .current_dir(&self.cwd);
        for extension in self.enabled_extensions {
            command.arg("--enable-extension").arg(extension);
        }
        if let Some(session_id) = resume {
            command.arg("resume").arg(session_id);
        }
        command
    }

    /// Builds a scrubbed exact-Tau command that attaches to one explicit live
    /// session without attempting to reconfigure its daemon.
    pub(super) fn attach_command(&self, session_id: &str) -> Command {
        let mut command = Command::new(&self.tau_bin);
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("XDG_CACHE_HOME", &self.cache_home)
            .env("XDG_RUNTIME_DIR", &self.runtime_home)
            .env("TERM", "xterm-256color")
            .env("LANG", "C.UTF-8")
            .arg("attach")
            .arg(session_id)
            .current_dir(&self.cwd);
        command
    }

    /// Builds the scrubbed S8 headless-daemon command over these same config,
    /// state, runtime, checkpoint, and artifact roots.
    pub(super) fn headless_command(&self, daemon_bin: &Path, socket: &Path) -> Command {
        let mut command = Command::new(daemon_bin);
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("XDG_CACHE_HOME", &self.cache_home)
            .env("XDG_RUNTIME_DIR", &self.runtime_home)
            .env("LANG", "C.UTF-8")
            .arg(socket)
            .arg(self.tau_state())
            .arg(self.config_home.join("tau"))
            .arg(self.tau_state())
            .arg("new")
            .current_dir(&self.cwd);
        command
    }

    /// Returns a short private socket path for headless Boot A.
    pub(super) fn headless_socket(&self) -> PathBuf {
        self.tempdir
            .as_ref()
            .expect("fixture root remains available")
            .path()
            .join("boot-a.sock")
    }

    /// Returns the private runtime root containing daemon discovery files.
    pub(super) fn runtime_home(&self) -> &Path {
        &self.runtime_home
    }

    /// Returns the authoritative Tau state root containing session and agent
    /// CBOR stores.
    pub(super) fn tau_state(&self) -> PathBuf {
        self.state_home.join("tau")
    }

    /// Proves the prior boot released runtime discovery and the durable session
    /// lock before another process resumes it.
    pub(super) fn require_boot_gone(
        &self,
        session_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let harnesses = self.runtime_home.join("tau/harnesses");
        if harnesses.exists()
            && std::fs::read_dir(&harnesses)?
                .filter_map(Result::ok)
                .any(|entry| {
                    matches!(
                        entry.path().extension().and_then(|value| value.to_str()),
                        Some("sock" | "json")
                    )
                })
        {
            return Err("prior Tau runtime socket metadata survived cleanup".into());
        }
        let lock_path = self
            .tau_state()
            .join("sessions")
            .join(session_id)
            .join("lock");
        let lock = path_std_fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)?;
        lock.try_lock_exclusive()
            .map_err(|_| "prior Tau session lock is still held")?;
        FileExt::unlock(&lock)?;
        Ok(())
    }

    /// Writes one bounded diagnostic artifact.
    pub(super) fn write_artifact(&self, name: &str, bytes: &[u8]) -> Result<(), std::io::Error> {
        let bounded = &bytes[bytes.len().saturating_sub(256 * 1024)..];
        std::fs::write(self.artifacts.join(name), bounded)
    }

    /// Returns one fixture-owned bounded artifact path for continuous writers.
    pub(super) fn artifact_path(&self, name: &str) -> PathBuf {
        self.artifacts.join(name)
    }

    /// Reads the fake provider's bounded semantic trace.
    pub(super) fn trace(&self) -> Result<String, std::io::Error> {
        std::fs::read_to_string(self.artifacts.join("fake-provider.trace"))
    }

    /// Sends one bounded, typed release frame for the active dummy invocation.
    pub(super) fn release_dummy(
        &self,
        call_id: &tau_proto::ToolCallId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (socket, nonce) = self
            .dummy_release
            .as_ref()
            .ok_or("fixture does not use releasable dummy mode")?;
        let mut frame = serde_json::to_vec(&serde_json::json!({
            "call_id": call_id,
            "release_nonce": nonce,
        }))?;
        frame.push(b'\n');
        if 4096 < frame.len() {
            return Err("dummy release frame exceeds protocol bound".into());
        }
        let mut stream = UnixStream::connect(socket)?;
        stream.write_all(&frame)?;
        Ok(())
    }

    /// Marks all exact assertions and cleanup complete.
    pub(super) fn complete(&self) {
        self.completed.set(true);
    }
}

/// Closed role/extension surface selected for one core-resume scenario.
#[derive(Clone, Copy, Eq, PartialEq)]
enum FixtureMode {
    /// Original single-agent gate with the restart dummy tool.
    DummyTool,
    /// Dummy tool held until the fixture sends one authenticated release.
    ReleasableDummyTool,
    /// S8 main/worker gate with only harness-owned `agent_start`.
    MultiAgent,
    /// Auto-starting peer receiver without any initial local agent.
    PeerEntrypoint,
}

impl Drop for GateFixture {
    fn drop(&mut self) {
        if (std::thread::panicking()
            || !self.completed.get()
            || std::env::var("TAU_E2E_KEEP_ARTIFACTS").as_deref() == Ok("1"))
            && let Some(tempdir) = self.tempdir.take()
        {
            let path = tempdir.keep();
            eprintln!("retained core-resume E2E artifacts at {}", path.display());
        }
    }
}

fn exact_tau_binary() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let candidate = if let Some(path) = std::env::var_os("TAU_E2E_TAU_BIN") {
        PathBuf::from(path)
    } else {
        let current = std::env::current_exe()?;
        current
            .ancestors()
            .map(|ancestor| ancestor.join("tau"))
            .find(|candidate| candidate.is_file())
            .ok_or("integration test executable has no ancestor Cargo profile containing `tau`")?
    };
    let candidate = candidate.canonicalize().map_err(|error| {
        format!(
            "exact Tau binary unavailable at {} ({error}); build `cargo build -p tau --bin tau` \
             or set TAU_E2E_TAU_BIN",
            candidate.display()
        )
    })?;
    if !candidate.is_file() {
        return Err(format!("exact Tau binary is not a file: {}", candidate.display()).into());
    }
    Ok(candidate)
}
