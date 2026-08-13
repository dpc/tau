//! Private-root fixture and bounded loopback server for provider-builtin retry
//! acceptance.

mod scripted_chat_server;

use std::cell::Cell;
use std::path::{Path, PathBuf};

pub use scripted_chat_server::CapturedChatRequest;
use scripted_chat_server::ScriptedChatServer;
use tempfile::TempDir;

use crate::sanitize_name;

/// Durable session used only by the provider-builtin retry fixture.
pub const PROVIDER_BUILTIN_RETRY_SESSION: &str = "provider-builtin-retry-e2e";

/// Hermetic configuration, state, and loopback authority for one retry test.
#[derive(Debug)]
pub struct ProviderBuiltinRetryFixture {
    /// Temporary root retained on test failure.
    tempdir: Option<TempDir>,
    /// Generated Tau configuration directory.
    config_dir: PathBuf,
    /// Private extension state directory.
    state_dir: PathBuf,
    /// Private durable harness state directory.
    harness_state_dir: PathBuf,
    /// Bounded loopback Chat Completions server.
    server: ScriptedChatServer,
    /// Whether a failed orchestration must retain the root.
    failed: Cell<bool>,
}

impl ProviderBuiltinRetryFixture {
    /// Creates private configuration for one keyless local Chat Completions
    /// provider.
    ///
    /// # Errors
    ///
    /// Returns an error if the exact provider binary, private directories, or
    /// generated configuration cannot be prepared.
    pub fn new(
        name: &str,
        provider_bin: impl AsRef<Path>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let provider_bin = exact_binary(provider_bin.as_ref())?;
        let tempdir = TempDir::new()?;
        let root = tempdir.path().join(sanitize_name(name));
        let config_dir = root.join("config");
        let state_dir = root.join("state");
        let harness_state_dir = root.join("harness-state");
        for path in [&config_dir, &state_dir, &harness_state_dir] {
            std::fs::create_dir_all(path)?;
        }
        let server = ScriptedChatServer::spawn()?;
        let profile_dir = config_dir.join("providers/provider-builtin");
        std::fs::create_dir_all(&profile_dir)?;
        std::fs::write(
            profile_dir.join("local.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "kind": "chat_completions",
                "base_url": server.base_url(),
                "models": [{"id": "retry-model"}],
                "credential": {"kind": "none"}
            }))?,
        )?;
        std::fs::write(
            config_dir.join("harness.yaml"),
            format!(
                concat!(
                    "agents:\n",
                    "  default_role: provider-builtin-retry\n",
                    "  idTemplate: main\n",
                    "  role_groups:\n",
                    "    e2e:\n",
                    "      roles:\n",
                    "        provider-builtin-retry:\n",
                    "          model: local/retry-model\n",
                    "          tools: []\n",
                    "extensions:\n",
                    "  provider-builtin:\n",
                    "    command: [{}]\n",
                    "    role: provider\n",
                    "    require: true\n",
                    "  core-shell:\n",
                    "    enable: false\n",
                    "  test-dummy:\n",
                    "    enable: false\n",
                    "  std-rhai:\n",
                    "    enable: false\n",
                    "  std-rostra:\n",
                    "    enable: false\n",
                    "  std-notifications:\n",
                    "    enable: false\n",
                    "  std-slack:\n",
                    "    enable: false\n",
                    "  std-telegram:\n",
                    "    enable: false\n",
                    "  std-zulip:\n",
                    "    enable: false\n",
                    "  std-xmpp:\n",
                    "    enable: false\n",
                    "  std-utils:\n",
                    "    enable: false\n",
                    "  std-websearch:\n",
                    "    enable: false\n",
                    "  std-pim:\n",
                    "    enable: false\n",
                    "  std-email:\n",
                    "    enable: false\n",
                ),
                serde_json::to_string(&provider_bin.display().to_string())?,
            ),
        )?;
        Ok(Self {
            tempdir: Some(tempdir),
            config_dir,
            state_dir,
            harness_state_dir,
            server,
            failed: Cell::new(false),
        })
    }

    /// Returns the private artifact root for failure diagnostics.
    #[must_use]
    pub fn root(&self) -> &Path {
        self.config_dir
            .parent()
            .expect("fixture config directory has a parent")
    }

    /// Returns the generated Tau configuration directory.
    #[must_use]
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Returns the private extension state directory.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Returns the private durable harness state directory.
    #[must_use]
    pub fn harness_state_dir(&self) -> &Path {
        &self.harness_state_dir
    }

    /// Returns a short private daemon socket path.
    #[must_use]
    pub fn socket_path(&self) -> PathBuf {
        self.tempdir
            .as_ref()
            .expect("fixture temporary directory is available before drop")
            .path()
            .join("provider-builtin-retry.sock")
    }

    /// Receives the next captured upstream request under the fixture watchdog.
    ///
    /// # Errors
    ///
    /// Returns an error when the bounded server did not observe a request in
    /// time.
    pub fn recv_request(&self) -> Result<CapturedChatRequest, Box<dyn std::error::Error>> {
        self.server.recv_request()
    }

    /// Fails if the server already accepted an unexpected additional request.
    ///
    /// # Errors
    ///
    /// Returns an error when an unexpected request arrived.
    pub fn require_no_ready_request(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.server.require_no_ready_request()
    }

    /// Opens the server's second-attempt phase only after the UI observes the
    /// accepted manual retry result.
    ///
    /// # Errors
    ///
    /// Returns an error if the server cannot enter that phase.
    pub fn release_accepted_retry(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.server.release_accepted_retry()
    }

    /// Marks daemon orchestration incomplete until all exact assertions finish.
    pub fn mark_daemon_started(&self) {
        self.failed.set(true);
    }

    /// Joins the bounded server after its exact three-request script completed.
    ///
    /// # Errors
    ///
    /// Returns an error if the script was not exactly consumed.
    pub fn finish(mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.server.finish()?;
        self.failed.set(false);
        Ok(())
    }
}

impl Drop for ProviderBuiltinRetryFixture {
    fn drop(&mut self) {
        if (std::thread::panicking()
            || self.failed.get()
            || std::env::var("TAU_E2E_KEEP_ARTIFACTS").as_deref() == Ok("1"))
            && let Some(tempdir) = self.tempdir.take()
        {
            let path = tempdir.keep();
            eprintln!(
                "retained provider-builtin retry e2e artifacts at {}",
                path.display()
            );
        }
    }
}

/// Canonicalizes and validates one exact fixture executable path.
fn exact_binary(path: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;

    let path = path.canonicalize()?;
    if !path.is_file() || path.metadata()?.permissions().mode() & 0o111 == 0 {
        return Err(format!(
            "fixture provider binary is not an executable file: {}",
            path.display()
        )
        .into());
    }
    Ok(path)
}
