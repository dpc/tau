//! Fixtures for Tau's deterministic and VCR multiprocess end-to-end tests.
//!
//! [`DeterministicFixture`] is always-on, uses only exact test subprocesses and
//! synthetic inputs below a private root, and ignores ambient Tau startup
//! overrides. [`ProviderBuiltinRetryFixture`] is a separate hermetic family
//! that runs the exact production provider binary through one closed loopback
//! retry script. [`VcrFixture`] is opt-in and non-hermetic: it uses a trusted
//! local `tau`, normal provider authentication, and the shell extension with
//! user permissions. See `ARCH-tau-e2e-tests` and the crate `SECURITY.md`.

use std::{env as path_std_env, io as path_std_io};

mod deterministic_fixture;
mod durable_session_snapshot;
mod durable_snapshot;
pub mod fake_provider;
mod provider_builtin_retry_fixture;
pub mod scenario;

use std::path::{Path, PathBuf};

pub use deterministic_fixture::DeterministicFixture;
pub use durable_session_snapshot::DurableSessionSnapshot;
pub use durable_snapshot::DurableSnapshot;
pub use provider_builtin_retry_fixture::{
    CapturedChatRequest, PROVIDER_BUILTIN_RETRY_SESSION, ProviderBuiltinRetryFixture,
};
pub use scenario::{
    AgentWatchResultExpectationV2, CANONICAL_OPAQUE_COMPACTION_JSON, FAKE_MODEL_ID,
    InitialStatusOutcome, ScenarioActionV2, ScenarioLaneV2, ScenarioTurnV1, ScenarioV1, ScenarioV2,
    StatusTerminalPhase, StatusToolOrder, WatchNotificationV2,
};
use tau_harness::{EmbeddedOptions, InteractionOutcome, run_embedded_message_with_options};
use tempfile::TempDir;

const DEFAULT_SESSION_ID: &str = "vcr-e2e-session";

/// A real headless Tau run with isolated harness config and state.
///
/// The caller owns VCR mode through normal environment variables such as
/// `TAU_VCR` and `TAU_VCR_DIR`. The fixture isolates Tau config, fixture state,
/// and embedded-harness session state, but intentionally does not rewrite
/// process-wide XDG environment variables so provider extensions can use the
/// user's real auth store.
#[derive(Debug)]
pub struct VcrFixture {
    /// Temporary root that owns all fixture-local directories for the test
    /// turn.
    _tempdir: TempDir,
    /// Isolated Tau config directory containing the generated `harness.yaml`.
    config_dir: PathBuf,
    /// Isolated Tau state directory passed through
    /// [`tau_config::settings::TauDirs`].
    state_dir: PathBuf,
    /// Harness session/event state root used by the embedded harness helper.
    harness_state_dir: PathBuf,
    /// Working directory configured for the shell extension.
    work_dir: PathBuf,
    /// Stable session id used to match VCR cassette traffic across runs.
    session_id: String,
}

impl VcrFixture {
    /// Creates a fixture from the e2e environment.
    ///
    /// Returns `Ok(None)` when the caller did not opt into VCR e2e execution:
    /// `TAU_VCR` is missing/off, or `TAU_E2E_MODEL` is missing. Active VCR
    /// modes (`record-if-missing` and `replay-only`) require `TAU_VCR_DIR`;
    /// invalid or non-Unicode environment values return `Err` so
    /// misconfigured e2e runs fail loudly instead of silently using live
    /// providers outside cassette mode.
    pub fn from_env(name: &str) -> Result<Option<Self>, Box<dyn std::error::Error>> {
        if !vcr_enabled_from_env()? {
            eprintln!(
                "skipping {name}: set TAU_VCR=record-if-missing or replay-only, TAU_VCR_DIR, \
                 and TAU_E2E_MODEL to run VCR e2e"
            );
            return Ok(None);
        }
        let Some(model) = e2e_model_from_env()? else {
            eprintln!(
                "skipping {name}: set TAU_VCR=record-if-missing or replay-only, TAU_VCR_DIR, \
                 and TAU_E2E_MODEL to run VCR e2e"
            );
            return Ok(None);
        };

        let tempdir = TempDir::new()?;
        let root = tempdir.path().join(sanitize_name(name));
        let config_dir = root.join("config");
        let state_dir = root.join("state");
        let harness_state_dir = root.join("harness-state");
        std::fs::create_dir_all(&config_dir)?;
        std::fs::create_dir_all(&state_dir)?;
        let work_dir = root.join("work");
        std::fs::create_dir_all(&harness_state_dir)?;
        std::fs::create_dir_all(&work_dir)?;

        let fixture = Self {
            _tempdir: tempdir,
            config_dir,
            state_dir,
            harness_state_dir,
            work_dir,
            session_id: std::env::var("TAU_E2E_SESSION_ID")
                .unwrap_or_else(|_| DEFAULT_SESSION_ID.to_owned()),
        };
        let tau_bin = std::env::var("TAU_E2E_TAU_BIN").unwrap_or_else(|_| "tau".to_owned());
        fixture.write_harness_config(&model, &canonicalize_command_if_path(&tau_bin))?;
        Ok(Some(fixture))
    }

    /// Override the stable session id used for cassette correlation.
    ///
    /// Non-portable characters are replaced so callers can derive
    /// collision-free ids from descriptive test and trial names.
    #[must_use]
    pub fn with_session_id(mut self, session_id: &str) -> Self {
        self.session_id = sanitize_name(session_id);
        self
    }

    /// Runs one real embedded Tau turn and returns its trace.
    ///
    /// VCR mismatch or missing cassette errors surface as the returned harness
    /// error. Callers should assert embedded `tool_calls` and byte-free
    /// `tool_results` when a test needs to prove a specific invocation and
    /// observed terminal result; progress messages are presentation-only.
    pub fn run_turn(&self, prompt: &str) -> Result<InteractionOutcome, tau_harness::HarnessError> {
        run_embedded_message_with_options(
            &self.harness_state_dir,
            &self.session_id,
            prompt,
            EmbeddedOptions::builder()
                .dirs(tau_config::settings::TauDirs {
                    config_dir: Some(self.config_dir.clone()),
                    state_dir: Some(self.state_dir.clone()),
                })
                .build(),
        )
    }

    /// Write deterministic fixture bytes into the shell extension's working
    /// directory.
    ///
    /// `relative_path` must be one normal filename rather than a path
    /// traversal.
    pub fn write_work_file(
        &self,
        relative_path: impl AsRef<Path>,
        bytes: &[u8],
    ) -> Result<PathBuf, std::io::Error> {
        let path = relative_path.as_ref();
        if path.components().count() != 1 {
            return Err(path_std_io::Error::new(
                path_std_io::ErrorKind::InvalidInput,
                "fixture path must be one filename",
            ));
        }
        let path = self.work_dir.join(path);
        std::fs::write(&path, bytes)?;
        Ok(path)
    }

    fn write_harness_config(
        &self,
        model: &str,
        tau_bin: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let tau_bin = serde_json::to_string(tau_bin)?;
        let model = serde_json::to_string(model)?;
        let work_dir = serde_json::to_string(&self.work_dir.display().to_string())?;
        std::fs::write(
            self.config_dir.join("harness.yaml"),
            format!(
                concat!(
                    "agents:\n",
                    "  default_role: vcr-e2e\n",
                    "  idTemplate: main\n",
                    "  role_groups:\n",
                    "    e2e:\n",
                    "      roles:\n",
                    "        vcr-e2e:\n",
                    "          model: {model}\n",
                    "          tools: [shell]\n",
                    "extensions:\n",
                    "  provider-builtin:\n",
                    "    command: [{tau_bin}]\n",
                    "    suffix: [ext, ext-provider-builtin]\n",
                    "  core-shell:\n",
                    "    command: [{tau_bin}]\n",
                    "    suffix: [ext, ext-shell]\n",
                    "    config:\n",
                    "      working_directory: {work_dir}\n",
                    "  std-notifications:\n",
                    "    enable: false\n",
                    "  std-websearch:\n",
                    "    enable: false\n",
                ),
                model = model,
                tau_bin = tau_bin,
                work_dir = work_dir,
            ),
        )?;
        Ok(())
    }
}

fn vcr_enabled_from_env() -> Result<bool, Box<dyn std::error::Error>> {
    let mode = match std::env::var("TAU_VCR") {
        Ok(value) => Some(value),
        Err(path_std_env::VarError::NotPresent) => None,
        Err(path_std_env::VarError::NotUnicode(_)) => {
            return Err("TAU_VCR is not valid Unicode".into());
        }
    };
    vcr_enabled(mode.as_deref(), std::env::var_os("TAU_VCR_DIR").is_some())
}

fn e2e_model_from_env() -> Result<Option<String>, Box<dyn std::error::Error>> {
    match std::env::var("TAU_E2E_MODEL") {
        Ok(value) => Ok(Some(value)),
        Err(path_std_env::VarError::NotPresent) => Ok(None),
        Err(path_std_env::VarError::NotUnicode(_)) => {
            Err("TAU_E2E_MODEL is not valid Unicode".into())
        }
    }
}

fn vcr_enabled(mode: Option<&str>, has_vcr_dir: bool) -> Result<bool, Box<dyn std::error::Error>> {
    let mode = match mode {
        Some(value) => tau_vcr::VcrMode::parse(value)?,
        None => tau_vcr::VcrMode::Off,
    };
    if mode == tau_vcr::VcrMode::Off {
        return Ok(false);
    }
    if !has_vcr_dir {
        return Err("TAU_VCR_DIR must be set when TAU_VCR is enabled".into());
    }
    Ok(true)
}

fn canonicalize_command_if_path(command: &str) -> String {
    if !command.contains(std::path::MAIN_SEPARATOR) {
        return command.to_owned();
    }
    let path = Path::new(command);
    if let Ok(canonical) = path.canonicalize() {
        return canonical.display().to_string();
    }
    workspace_root()
        .join(path)
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("tau-e2e-tests lives under crates/")
        .to_path_buf()
}

pub(crate) fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '-',
        })
        .collect()
}

#[cfg(test)]
mod tests;
