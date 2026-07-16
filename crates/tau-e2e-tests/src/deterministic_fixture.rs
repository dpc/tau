//! Private-root headless harness fixture for deterministic provider scenarios.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use tau_harness::{EmbeddedOptions, InteractionOutcome, run_embedded_message_with_options};
use tempfile::TempDir;

use crate::{ScenarioV1, sanitize_name};

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
    /// Expected exact scenario turn count.
    expected_turns: usize,
    /// Whether the deterministic dummy subprocess is configured.
    dummy_enabled: bool,
    /// Whether an operation failed and artifacts must be retained.
    failed: Cell<bool>,
}

impl DeterministicFixture {
    /// Creates a fixture using exact Cargo-built subprocess paths.
    ///
    /// The caller should pass `env!("CARGO_BIN_EXE_tau-e2e-fake-provider")`
    /// and, for tool scenarios, `env!("CARGO_BIN_EXE_tau-e2e-test-dummy")`.
    pub fn new(
        name: &str,
        scenario: &ScenarioV1,
        fake_provider_bin: impl AsRef<Path>,
        dummy_tool_bin: Option<PathBuf>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let tempdir = TempDir::new()?;
        let root = tempdir.path().join(sanitize_name(name));
        let config_dir = root.join("config");
        let state_dir = root.join("state");
        let harness_state_dir = root.join("harness-state");
        let artifacts_dir = root.join("artifacts");
        std::fs::create_dir_all(&config_dir)?;
        std::fs::create_dir_all(&state_dir)?;
        std::fs::create_dir_all(&harness_state_dir)?;
        std::fs::create_dir_all(&artifacts_dir)?;
        let trace_path = artifacts_dir.join("fake-provider.trace");

        let mut extensions = serde_json::Map::new();
        for name in [
            "provider-builtin",
            "core-shell",
            "test-dummy",
            "std-rhai",
            "std-notifications",
            "std-slack",
            "std-telegram",
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
                    "config": { "restart_mode": "success" },
                }),
            );
            serde_json::json!(["restart_test_dummy"])
        } else {
            serde_json::json!([])
        };
        let config = serde_json::json!({
            "agents": {
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
            },
            "extensions": extensions,
        });
        let config_bytes = serde_json::to_vec_pretty(&config)?;
        std::fs::write(config_dir.join("harness.yaml"), &config_bytes)?;
        std::fs::write(
            artifacts_dir.join("scenario.json"),
            serde_json::to_vec_pretty(scenario)?,
        )?;
        std::fs::write(artifacts_dir.join("harness-config.json"), config_bytes)?;

        Ok(Self {
            tempdir: Some(tempdir),
            config_dir,
            state_dir,
            harness_state_dir,
            trace_path,
            expected_turns: scenario.turns.len(),
            dummy_enabled,
            failed: Cell::new(false),
        })
    }

    /// Runs one synthetic interaction and performs clean embedded shutdown.
    pub fn run_turn(&self, prompt: &str) -> Result<InteractionOutcome, tau_harness::HarnessError> {
        let mut allowed_extensions = BTreeSet::from(["e2e-fake-provider".into()]);
        if self.dummy_enabled {
            allowed_extensions.insert("e2e-test-dummy".into());
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

    /// Reads the bounded provider semantic trace.
    pub fn trace(&self) -> Result<String, std::io::Error> {
        std::fs::read_to_string(&self.trace_path)
    }

    /// Marks an intentionally asserted negative-case error as handled.
    pub fn acknowledge_expected_failure(&self) {
        self.failed.set(false);
    }

    /// Asserts that every exact scenario turn was consumed.
    pub fn assert_consumed(&self) -> Result<(), Box<dyn std::error::Error>> {
        let trace = self.trace()?;
        let matched = trace
            .lines()
            .filter(|line| line.contains(" matched "))
            .count();
        if matched != self.expected_turns
            || !trace
                .lines()
                .last()
                .is_some_and(|line| line.ends_with("remaining=0"))
        {
            return Err(format!(
                "scenario not exactly consumed: matched {matched}/{}; trace at {}",
                self.expected_turns,
                self.trace_path.display()
            )
            .into());
        }
        Ok(())
    }

    /// Reads typed published events from the durable harness JSONL projection.
    pub fn durable_events(&self) -> Result<Vec<tau_proto::Event>, Box<dyn std::error::Error>> {
        let path = self
            .harness_state_dir
            .join("sessions")
            .join("deterministic-e2e-session")
            .join("events.jsonl");
        let bytes = std::fs::read_to_string(path)?;
        let mut events = Vec::new();
        for line in bytes.lines() {
            let record: serde_json::Value = serde_json::from_str(line)?;
            if record.get("type").and_then(serde_json::Value::as_str) != Some("published") {
                continue;
            }
            if let Some(event) = record.get("event") {
                events.push(serde_json::from_value(event.clone())?);
            }
        }
        Ok(events)
    }
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
