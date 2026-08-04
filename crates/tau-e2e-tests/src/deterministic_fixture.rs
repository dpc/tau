//! Private-root headless harness fixture for deterministic provider scenarios.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use tau_harness::{EmbeddedOptions, InteractionOutcome, run_embedded_message_with_options};
use tempfile::TempDir;

use crate::{ScenarioV1, ScenarioV2, sanitize_name};

/// Always-on hermetic fixture backed by supervised provider and tool
/// subprocesses.
#[derive(Debug)]
pub struct DeterministicFixture {
    /// Temporary root retained automatically when a test panics.
    tempdir: Option<TempDir>,
    /// Generated harness configuration directory.
    config_dir: PathBuf,
    /// Isolated Tau state directory.
    state_dir: PathBuf,
    /// Durable harness session root.
    harness_state_dir: PathBuf,
    /// Synthetic provider observation trace.
    trace_path: PathBuf,
    /// Expected exact scenario action count.
    expected_actions: usize,
    /// Number of independent lanes that must reach zero remaining actions.
    expected_lanes: usize,
    /// Whether the deterministic dummy subprocess is configured.
    dummy_enabled: bool,
    /// Whether the bundled production core-shell subprocess is configured.
    core_shell_enabled: bool,
    /// Private daemon and extension working directory.
    shell_base: PathBuf,
    /// Outside-target canary whose exact bytes must remain unchanged.
    outside_canary: PathBuf,
    /// Whether an operation failed and artifacts must be retained.
    failed: Cell<bool>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum FixtureMode {
    Standard,
    CoreShell,
    SessionRestore,
    SessionRestoreWatch,
    SessionRestoreMultipleWorkers,
    SessionRestoreInterruptedTool,
    SessionRestoreMixed,
}

impl DeterministicFixture {
    /// Creates a fixture using exact Cargo-built subprocess paths.
    ///
    /// The caller should pass `env!("CARGO_BIN_EXE_tau-e2e-fake-provider")`
    /// and, for tool scenarios, `env!("CARGO_BIN_EXE_tau-e2e-test-dummy")`.
    ///
    /// # Errors
    ///
    /// Returns an error when private directories, exact binaries, generated
    /// configuration, or synthetic artifacts cannot be validated or written.
    pub fn new(
        name: &str,
        scenario: &ScenarioV1,
        fake_provider_bin: impl AsRef<Path>,
        dummy_tool_bin: Option<PathBuf>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_serialized(
            name,
            serde_json::to_value(scenario)?,
            scenario.turns.len(),
            1,
            fake_provider_bin,
            dummy_tool_bin,
            FixtureMode::Standard,
        )
    }

    /// Creates a multi-lane version-two fixture.
    ///
    /// # Errors
    ///
    /// Returns an error when private directories, the exact provider binary,
    /// generated configuration, or synthetic artifacts cannot be prepared.
    pub fn new_v2(
        name: &str,
        scenario: &ScenarioV2,
        fake_provider_bin: impl AsRef<Path>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let expected_actions = scenario.lanes.iter().map(|lane| lane.actions.len()).sum();
        Self::new_serialized(
            name,
            serde_json::to_value(scenario)?,
            expected_actions,
            scenario.lanes.len(),
            fake_provider_bin,
            None,
            FixtureMode::Standard,
        )
    }

    /// Creates the two-role production-`agent_start` cold-resume fixture.
    ///
    /// # Errors
    ///
    /// Returns an error when private directories, the exact provider binary,
    /// generated role configuration, or synthetic artifacts cannot be prepared.
    pub fn new_session_restore(
        name: &str,
        scenario: &ScenarioV2,
        fake_provider_bin: impl AsRef<Path>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let expected_actions = scenario.lanes.iter().map(|lane| lane.actions.len()).sum();
        Self::new_serialized(
            name,
            serde_json::to_value(scenario)?,
            expected_actions,
            scenario.lanes.len(),
            fake_provider_bin,
            None,
            FixtureMode::SessionRestore,
        )
    }

    /// Creates the two-role cold-resume fixture with production `agent_start`
    /// and `agent_watch` enabled only for the main role.
    ///
    /// # Errors
    ///
    /// Returns an error when private directories, exact binaries, generated
    /// configuration, or synthetic artifacts cannot be prepared.
    pub fn new_session_restore_watch(
        name: &str,
        scenario: &ScenarioV2,
        fake_provider_bin: impl AsRef<Path>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let expected_actions = scenario.lanes.iter().map(|lane| lane.actions.len()).sum();
        Self::new_serialized(
            name,
            serde_json::to_value(scenario)?,
            expected_actions,
            scenario.lanes.len(),
            fake_provider_bin,
            None,
            FixtureMode::SessionRestoreWatch,
        )
    }

    /// Creates the three-role S4 cold-resume fixture with production
    /// `agent_start` enabled only for the main role.
    ///
    /// # Errors
    ///
    /// Returns an error when private directories, exact binaries, generated
    /// configuration, or synthetic artifacts cannot be prepared.
    pub fn new_session_restore_multiple_workers(
        name: &str,
        scenario: &ScenarioV2,
        fake_provider_bin: impl AsRef<Path>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let expected_actions = scenario.lanes.iter().map(|lane| lane.actions.len()).sum();
        Self::new_serialized(
            name,
            serde_json::to_value(scenario)?,
            expected_actions,
            scenario.lanes.len(),
            fake_provider_bin,
            None,
            FixtureMode::SessionRestoreMultipleWorkers,
        )
    }

    /// Creates the closed S6 worker-tool interruption fixture.
    ///
    /// # Errors
    ///
    /// Returns an error when private directories, exact provider/tool binaries,
    /// generated role configuration, or synthetic artifacts cannot be prepared.
    pub fn new_session_restore_interrupted_tool(
        name: &str,
        scenario: &ScenarioV2,
        fake_provider_bin: impl AsRef<Path>,
        dummy_tool_bin: impl AsRef<Path>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let expected_actions = scenario.lanes.iter().map(|lane| lane.actions.len()).sum();
        Self::new_serialized(
            name,
            serde_json::to_value(scenario)?,
            expected_actions,
            scenario.lanes.len(),
            fake_provider_bin,
            Some(dummy_tool_bin.as_ref().to_path_buf()),
            FixtureMode::SessionRestoreInterruptedTool,
        )
    }

    /// Creates the closed S7 mixed-state cold-resume fixture.
    ///
    /// # Errors
    ///
    /// Returns an error when private directories, exact provider/tool binaries,
    /// generated role configuration, or synthetic artifacts cannot be prepared.
    pub fn new_session_restore_mixed(
        name: &str,
        scenario: &ScenarioV2,
        fake_provider_bin: impl AsRef<Path>,
        dummy_tool_bin: impl AsRef<Path>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let expected_actions = scenario.lanes.iter().map(|lane| lane.actions.len()).sum();
        Self::new_serialized(
            name,
            serde_json::to_value(scenario)?,
            expected_actions,
            scenario.lanes.len(),
            fake_provider_bin,
            Some(dummy_tool_bin.as_ref().to_path_buf()),
            FixtureMode::SessionRestoreMixed,
        )
    }

    /// Creates the closed production core-shell cold-resume fixture.
    pub fn new_core_shell(
        name: &str,
        scenario: &ScenarioV2,
        fake_provider_bin: impl AsRef<Path>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let expected_actions = scenario.lanes.iter().map(|lane| lane.actions.len()).sum();
        Self::new_serialized(
            name,
            serde_json::to_value(scenario)?,
            expected_actions,
            scenario.lanes.len(),
            fake_provider_bin,
            None,
            FixtureMode::CoreShell,
        )
    }

    fn new_serialized(
        name: &str,
        scenario: serde_json::Value,
        expected_actions: usize,
        expected_lanes: usize,
        fake_provider_bin: impl AsRef<Path>,
        dummy_tool_bin: Option<PathBuf>,
        mode: FixtureMode,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let core_shell_enabled = mode == FixtureMode::CoreShell;
        let tempdir = TempDir::new()?;
        let root = tempdir.path().join(sanitize_name(name));
        let config_dir = root.join("config");
        let state_dir = root.join("state");
        let harness_state_dir = root.join("harness-state");
        let artifacts_dir = root.join("artifacts");
        for private_root in [
            "home",
            "xdg-config",
            "xdg-state",
            "xdg-cache",
            "xdg-runtime",
        ] {
            std::fs::create_dir_all(root.join(private_root))?;
        }
        std::fs::create_dir_all(&config_dir)?;
        std::fs::create_dir_all(&state_dir)?;
        std::fs::create_dir_all(&harness_state_dir)?;
        std::fs::create_dir_all(&artifacts_dir)?;
        let shell_base = root.join("shell-base");
        let project = shell_base.join("project");
        std::fs::create_dir_all(&project)?;
        let shell_base = shell_base.canonicalize()?;
        let project = project.canonicalize()?;
        if shell_base.symlink_metadata()?.file_type().is_symlink()
            || project.symlink_metadata()?.file_type().is_symlink()
        {
            return Err("core-shell scratch layout must not contain symlinks".into());
        }
        let outside_canary = root.join("outside-canary");
        std::fs::write(&outside_canary, b"outside-canary:unchanged\n")?;
        let trace_path = artifacts_dir.join("fake-provider.trace");

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
                "command": [exact_binary(fake_provider_bin.as_ref())?],
                "role": "provider",
                "require": true,
                "cwd": artifacts_dir,
                "config": {
                     "scenario": scenario,
                },
            }),
        );
        let dummy_enabled = dummy_tool_bin.is_some();
        let tools = if let Some(dummy_tool_bin) = dummy_tool_bin {
            extensions.insert(
                "e2e-test-dummy".to_owned(),
                serde_json::json!({
                    "command": [exact_binary(&dummy_tool_bin)?],
                    "role": "tool",
                    "require": true,
                    "config": {
                        "restart_mode": if matches!(
                            mode,
                            FixtureMode::SessionRestoreInterruptedTool
                                | FixtureMode::SessionRestoreMixed
                        ) {
                            "hold_no_side_effect"
                        } else {
                            "success"
                        }
                    },
                }),
            );
            serde_json::json!(["restart_test_dummy"])
        } else {
            serde_json::json!([])
        };
        let tools = if core_shell_enabled {
            let tau_bin = exact_tau_binary()?;
            extensions.insert(
                "core-shell".to_owned(),
                serde_json::json!({
                    "enable": true,
                    "command": [tau_bin],
                    "suffix": ["component", "ext-shell"],
                    "role": "tool",
                    "require": true,
                    "cwd": shell_base,
                    "config": {
                        "working_directory": shell_base,
                        "dir_lock": { "enable": false }
                    }
                }),
            );
            serde_json::json!(["workdir", "edit"])
        } else {
            tools
        };
        let role_config = if matches!(
            mode,
            FixtureMode::SessionRestore
                | FixtureMode::SessionRestoreWatch
                | FixtureMode::SessionRestoreMultipleWorkers
                | FixtureMode::SessionRestoreInterruptedTool
                | FixtureMode::SessionRestoreMixed
        ) {
            let main_tools = if mode == FixtureMode::SessionRestoreWatch {
                serde_json::json!(["agent_start", "agent_watch"])
            } else {
                serde_json::json!(["agent_start"])
            };
            let mut roles = serde_json::json!({
                "deterministic-main": {
                    "model": "fake/test",
                    "tools": main_tools,
                },
                "deterministic-worker": {
                    "model": "fake/test",
                    "tools": if mode == FixtureMode::SessionRestoreInterruptedTool {
                        serde_json::json!(["restart_test_dummy"])
                    } else {
                        serde_json::json!([])
                    },
                }
            });
            if mode == FixtureMode::SessionRestoreMultipleWorkers {
                let roles = roles
                    .as_object_mut()
                    .expect("literal role configuration is an object");
                roles.remove("deterministic-worker");
                for role in ["deterministic-worker-alpha", "deterministic-worker-beta"] {
                    roles.insert(
                        role.to_owned(),
                        serde_json::json!({
                            "model": "fake/test",
                            "tools": [],
                        }),
                    );
                }
            } else if mode == FixtureMode::SessionRestoreMixed {
                let roles = roles
                    .as_object_mut()
                    .expect("literal role configuration is an object");
                roles.remove("deterministic-worker");
                for (role, tools) in [
                    ("deterministic-worker-quiescent", serde_json::json!([])),
                    ("deterministic-worker-uncertain", serde_json::json!([])),
                    (
                        "deterministic-worker-repair",
                        serde_json::json!(["restart_test_dummy"]),
                    ),
                ] {
                    roles.insert(
                        role.to_owned(),
                        serde_json::json!({
                            "model": "fake/test",
                            "tools": tools,
                        }),
                    );
                }
            }
            serde_json::json!({
                "default_role": "deterministic-main",
                "id_template": "main",
                "role_groups": {
                    "e2e": {
                        "roles": roles
                    }
                }
            })
        } else {
            serde_json::json!({
                "default_role": "deterministic-e2e",
                "id_template": "main",
                "role_groups": {
                    "e2e": {
                        "roles": {
                            "deterministic-e2e": {
                                "model": "fake/test",
                                "tools": tools,
                            }
                        }
                    }
                }
            })
        };
        let config = serde_json::json!({
            "agents": role_config,
            "extensions": extensions,
        });
        let config_bytes = serde_json::to_vec_pretty(&config)?;
        std::fs::write(config_dir.join("harness.yaml"), &config_bytes)?;
        std::fs::write(
            artifacts_dir.join("scenario.json"),
            serde_json::to_vec_pretty(&scenario)?,
        )?;
        std::fs::write(artifacts_dir.join("harness-config.json"), config_bytes)?;

        Ok(Self {
            tempdir: Some(tempdir),
            config_dir,
            state_dir,
            harness_state_dir,
            trace_path,
            expected_actions,
            expected_lanes,
            dummy_enabled,
            core_shell_enabled,
            shell_base,
            outside_canary,
            failed: Cell::new(false),
        })
    }

    /// Marks externally orchestrated daemon work incomplete.
    ///
    /// This retains artifacts if the caller exits before a successful
    /// [`Self::assert_consumed`] acknowledges clean completion.
    pub fn mark_daemon_started(&self) {
        self.failed.set(true);
    }

    /// Returns the durable harness state root.
    #[must_use]
    pub fn harness_state_dir(&self) -> &Path {
        &self.harness_state_dir
    }

    /// Returns a private socket path for one daemon boot.
    ///
    /// The socket lives directly under the short temporary-directory path
    /// rather than the descriptive artifact root so Unix `sockaddr_un` limits
    /// cannot be exceeded by long test names.
    #[must_use]
    pub fn socket_path(&self, boot: &str) -> PathBuf {
        self.tempdir
            .as_ref()
            .expect("fixture temporary directory is available before drop")
            .path()
            .join(format!("{}.sock", sanitize_name(boot)))
    }

    /// Runs one synthetic interaction and performs clean embedded shutdown.
    pub fn run_turn(&self, prompt: &str) -> Result<InteractionOutcome, tau_harness::HarnessError> {
        let mut allowed_extensions =
            BTreeSet::from([tau_proto::ExtensionName::parse("e2e-fake-provider")
                .expect("fake provider name must satisfy the extension identifier grammar")]);
        if self.dummy_enabled {
            allowed_extensions.insert(
                tau_proto::ExtensionName::parse("e2e-test-dummy")
                    .expect("dummy extension name must satisfy the extension identifier grammar"),
            );
        }
        let result = run_embedded_message_with_options(
            &self.harness_state_dir,
            "deterministic-e2e-session",
            prompt,
            EmbeddedOptions::builder()
                .dirs(tau_config::settings::TauDirs {
                    config_dir: Some(self.config_dir.clone()),
                    state_dir: Some(self.state_dir.clone()),
                })
                .ignore_startup_environment(true)
                .allowed_extensions(allowed_extensions)
                .build(),
        );
        match result {
            Ok(outcome) => match self.assert_consumed() {
                Ok(()) => Ok(outcome),
                Err(error) => {
                    self.failed.set(true);
                    Err(tau_harness::HarnessError::Participant(error.to_string()))
                }
            },
            Err(error) => {
                self.failed.set(true);
                Err(error)
            }
        }
    }

    /// Returns the private artifact root for failure diagnostics.
    #[must_use]
    pub fn root(&self) -> &Path {
        self.config_dir
            .parent()
            .expect("fixture config directory has a root")
    }

    /// Returns the canonical core-shell base directory.
    pub fn shell_base(&self) -> &Path {
        &self.shell_base
    }

    /// Returns the exact outside-target canary path.
    pub fn outside_canary(&self) -> &Path {
        &self.outside_canary
    }

    /// Returns whether this fixture enables the bundled production core-shell.
    pub fn core_shell_enabled(&self) -> bool {
        self.core_shell_enabled
    }

    /// Returns whether this fixture explicitly enables the dummy extension.
    pub fn dummy_enabled(&self) -> bool {
        self.dummy_enabled
    }

    /// Returns the generated harness configuration directory.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Returns the private extension state directory.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Reads the bounded provider semantic trace.
    pub fn trace(&self) -> Result<String, std::io::Error> {
        std::fs::read_to_string(&self.trace_path)
    }

    /// Verifies the generated S1 main/worker role, model, and tool projection.
    ///
    /// # Errors
    ///
    /// Returns an error when the generated configuration differs from the
    /// closed production-`agent_start` fixture surface.
    pub fn assert_session_restore_roles(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.assert_session_restore_role_tools(&["agent_start"])
    }

    /// Verifies the generated S2 role projection adds only `agent_watch` to the
    /// S1 main role.
    ///
    /// # Errors
    ///
    /// Returns an error when the generated role surface differs from the closed
    /// explicit-watch fixture.
    pub fn assert_session_restore_watch_roles(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.assert_session_restore_role_tools(&["agent_start", "agent_watch"])
    }

    /// Verifies the generated S4 role projection has one closed main and two
    /// distinct tool-free worker roles.
    ///
    /// # Errors
    ///
    /// Returns an error when the generated role surface differs from S4.
    pub fn assert_session_restore_multiple_worker_roles(
        &self,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let config: serde_json::Value = serde_json::from_slice(&std::fs::read(
            self.root().join("artifacts/harness-config.json"),
        )?)?;
        let agents = &config["agents"];
        let roles = &agents["role_groups"]["e2e"]["roles"];
        if agents["default_role"] != "deterministic-main"
            || agents["id_template"] != "main"
            || roles.as_object().is_none_or(|roles| roles.len() != 3)
            || roles["deterministic-main"]["model"] != "fake/test"
            || roles["deterministic-main"]["tools"] != serde_json::json!(["agent_start"])
            || ["deterministic-worker-alpha", "deterministic-worker-beta"]
                .into_iter()
                .any(|role| {
                    roles[role]["model"] != "fake/test"
                        || roles[role]["tools"] != serde_json::json!([])
                })
        {
            return Err("generated S4 role configuration changed".into());
        }
        Ok(())
    }

    /// Verifies the S6 projection exposes the sole closed hold-mode dummy tool
    /// only to the worker.
    ///
    /// # Errors
    ///
    /// Returns an error when the generated role or extension configuration
    /// differs from the interrupted-worker-tool fixture surface.
    pub fn assert_session_restore_interrupted_tool_roles(
        &self,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let config: serde_json::Value = serde_json::from_slice(&std::fs::read(
            self.root().join("artifacts/harness-config.json"),
        )?)?;
        let agents = &config["agents"];
        let roles = &agents["role_groups"]["e2e"]["roles"];
        let extensions = &config["extensions"];
        if agents["default_role"] != "deterministic-main"
            || agents["id_template"] != "main"
            || roles.as_object().is_none_or(|roles| roles.len() != 2)
            || roles["deterministic-main"]["model"] != "fake/test"
            || roles["deterministic-main"]["tools"] != serde_json::json!(["agent_start"])
            || roles["deterministic-worker"]["model"] != "fake/test"
            || roles["deterministic-worker"]["tools"] != serde_json::json!(["restart_test_dummy"])
            || extensions["e2e-test-dummy"]["role"] != "tool"
            || extensions["e2e-test-dummy"]["require"] != true
            || extensions["e2e-test-dummy"]["config"]["restart_mode"] != "hold_no_side_effect"
        {
            return Err("generated S6 role/extension configuration changed".into());
        }
        Ok(())
    }

    /// Verifies the S7 projection exposes production `agent_start` only to the
    /// main and the closed hold-mode dummy only to the repair worker.
    ///
    /// # Errors
    ///
    /// Returns an error when the generated role or extension configuration
    /// differs from the mixed-state fixture surface.
    pub fn assert_session_restore_mixed_roles(&self) -> Result<(), Box<dyn std::error::Error>> {
        let config: serde_json::Value = serde_json::from_slice(&std::fs::read(
            self.root().join("artifacts/harness-config.json"),
        )?)?;
        let agents = &config["agents"];
        let roles = &agents["role_groups"]["e2e"]["roles"];
        let extensions = &config["extensions"];
        if agents["default_role"] != "deterministic-main"
            || agents["id_template"] != "main"
            || roles.as_object().is_none_or(|roles| roles.len() != 4)
            || roles["deterministic-main"]["model"] != "fake/test"
            || roles["deterministic-main"]["tools"] != serde_json::json!(["agent_start"])
            || [
                "deterministic-worker-quiescent",
                "deterministic-worker-uncertain",
            ]
            .into_iter()
            .any(|role| {
                roles[role]["model"] != "fake/test" || roles[role]["tools"] != serde_json::json!([])
            })
            || roles["deterministic-worker-repair"]["model"] != "fake/test"
            || roles["deterministic-worker-repair"]["tools"]
                != serde_json::json!(["restart_test_dummy"])
            || extensions["e2e-test-dummy"]["role"] != "tool"
            || extensions["e2e-test-dummy"]["require"] != true
            || extensions["e2e-test-dummy"]["config"]["restart_mode"] != "hold_no_side_effect"
        {
            return Err("generated S7 role/extension configuration changed".into());
        }
        Ok(())
    }

    fn assert_session_restore_role_tools(
        &self,
        main_tools: &[&str],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let config: serde_json::Value = serde_json::from_slice(&std::fs::read(
            self.root().join("artifacts/harness-config.json"),
        )?)?;
        let agents = &config["agents"];
        let roles = &agents["role_groups"]["e2e"]["roles"];
        if agents["default_role"] != "deterministic-main"
            || agents["id_template"] != "main"
            || roles.as_object().is_none_or(|roles| roles.len() != 2)
            || roles["deterministic-main"]["model"] != "fake/test"
            || roles["deterministic-main"]["tools"] != serde_json::json!(main_tools)
            || roles["deterministic-worker"]["model"] != "fake/test"
            || roles["deterministic-worker"]["tools"] != serde_json::json!([])
        {
            return Err("generated session-restore role configuration changed".into());
        }
        Ok(())
    }

    /// Marks an intentionally asserted negative-case error as handled.
    pub fn acknowledge_expected_failure(&self) {
        self.failed.set(false);
    }

    /// Asserts exact action/lane consumption and clears failure retention on
    /// success.
    pub fn assert_consumed(&self) -> Result<(), Box<dyn std::error::Error>> {
        let trace = self.trace()?;
        let matched = trace
            .lines()
            .filter(|line| line.contains(" matched "))
            .count();
        if matched != self.expected_actions
            || trace
                .lines()
                .filter(|line| line.ends_with("remaining=0"))
                .count()
                != self.expected_lanes
            || trace.contains("mismatch")
        {
            return Err(format!(
                "scenario not exactly consumed: matched {matched}/{}; trace at {}",
                self.expected_actions,
                self.trace_path.display()
            )
            .into());
        }
        self.failed.set(false);
        Ok(())
    }

    /// Reads typed published events from the best-effort harness debug trace.
    pub fn published_trace_events(
        &self,
    ) -> Result<Vec<tau_proto::Event>, Box<dyn std::error::Error>> {
        let path = self
            .harness_state_dir
            .join("sessions")
            .join("deterministic-e2e-session")
            .join("events.jsonl");
        let bytes = std::fs::read_to_string(path)?;
        Ok(tau_test_support::parse_published_trace_events(&bytes)?)
    }
}

fn exact_tau_binary() -> Result<String, Box<dyn std::error::Error>> {
    let candidate = if let Some(path) = std::env::var_os("TAU_E2E_TAU_BIN") {
        PathBuf::from(path)
    } else {
        std::env::current_exe()?
            .ancestors()
            .map(|ancestor| ancestor.join("tau"))
            .find(|candidate| candidate.is_file())
            .ok_or("integration test executable has no ancestor Cargo profile containing `tau`")?
    };
    exact_binary(&candidate)
}

impl Drop for DeterministicFixture {
    fn drop(&mut self) {
        if (std::thread::panicking()
            || self.failed.get()
            || std::env::var("TAU_E2E_KEEP_ARTIFACTS").as_deref() == Ok("1"))
            && let Some(tempdir) = self.tempdir.take()
        {
            let path = tempdir.keep();
            eprintln!("retained deterministic e2e artifacts at {}", path.display());
        }
    }
}

fn exact_binary(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let path = path.canonicalize()?;
    if !path.is_file() {
        return Err(format!("fixture binary is not a file: {}", path.display()).into());
    }
    Ok(path.display().to_string())
}
