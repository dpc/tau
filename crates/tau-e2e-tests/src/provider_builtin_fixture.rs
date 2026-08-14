//! Private-root fixture and bounded loopback server for production
//! provider-builtin acceptance.

mod scripted_chat_server;

use std::cell::Cell;
use std::path::{Path, PathBuf};

pub use scripted_chat_server::CapturedChatRequest;
use scripted_chat_server::{Script, ScriptedChatServer};
use tempfile::TempDir;

use crate::sanitize_name;

/// Valid-by-construction inputs for each closed production-provider script.
enum FixtureScript<'a> {
    /// Retry script with no tool extension.
    Retry,
    /// Qwen script with its required exact deterministic tool binary.
    Qwen {
        /// Exact executable that serves the script's closed function-tool
        /// calls.
        dummy_bin: &'a Path,
    },
}

/// Durable session used only by provider-builtin subprocess fixtures.
pub const PROVIDER_BUILTIN_SESSION: &str = "provider-builtin-retry-e2e";

/// Hermetic configuration, state, and loopback authority for one closed
/// production-provider script.
#[derive(Debug)]
pub struct ProviderBuiltinFixture {
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
    /// Whether this script also runs the exact deterministic tool extension.
    test_dummy: bool,
}

impl ProviderBuiltinFixture {
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
        Self::new_with_script(name, provider_bin.as_ref(), FixtureScript::Retry)
    }

    /// Creates private configuration for the Qwen compatibility script and its
    /// deterministic tool extension.
    ///
    /// # Errors
    ///
    /// Returns an error if either exact binary, private directories, or
    /// generated configuration cannot be prepared.
    pub fn new_qwen(
        name: &str,
        provider_bin: impl AsRef<Path>,
        dummy_bin: impl AsRef<Path>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_with_script(
            name,
            provider_bin.as_ref(),
            FixtureScript::Qwen {
                dummy_bin: dummy_bin.as_ref(),
            },
        )
    }

    /// Builds one closed production-provider fixture variant.
    fn new_with_script(
        name: &str,
        provider_bin: &Path,
        script: FixtureScript<'_>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let provider_bin = exact_binary(provider_bin)?;
        let (server_script, dummy_bin) = match script {
            FixtureScript::Retry => (Script::Retry, None),
            FixtureScript::Qwen { dummy_bin } => (Script::Qwen, Some(exact_binary(dummy_bin)?)),
        };
        let tempdir = TempDir::new()?;
        let root = tempdir.path().join(sanitize_name(name));
        let config_dir = root.join("config");
        let state_dir = root.join("state");
        let harness_state_dir = root.join("harness-state");
        for path in [&config_dir, &state_dir, &harness_state_dir] {
            std::fs::create_dir_all(path)?;
        }
        let server = ScriptedChatServer::spawn(server_script)?;
        let profile_dir = config_dir.join("providers/provider-builtin");
        std::fs::create_dir_all(&profile_dir)?;
        let qwen = matches!(server_script, Script::Qwen);
        let mut profile = serde_json::json!({
            "kind": "chat_completions",
            "base_url": server.base_url(),
            "models": [{"id": "retry-model"}],
            "credential": {"kind": "none"}
        });
        if qwen {
            profile["extra_body"] = serde_json::json!({
                "chat_template_kwargs": {
                    "enable_thinking": true,
                    "preserve_thinking": true
                },
                "temperature": 1.0,
                "top_p": 0.95,
                "top_k": 20,
                "min_p": 0.0,
                "presence_penalty": 0.0,
                "repetition_penalty": 1.0
            });
            profile["models"] = serde_json::json!([{
                "id": "Qwen/Qwen3.8-27B",
                "context_window": 262144,
                "compat": {
                    "stream_options": true,
                    "reasoning_effort": {
                        "efforts": ["low", "medium", "xhigh"],
                        "wire": "literal"
                    },
                    "reasoning_replay": "both",
                    "single_initial_system_message": true
                }
            }]);
        }
        std::fs::write(
            profile_dir.join("local.json"),
            serde_json::to_vec_pretty(&profile)?,
        )?;
        let role = if qwen {
            "provider-builtin-qwen"
        } else {
            "provider-builtin-retry"
        };
        let model = if qwen {
            "local/Qwen/Qwen3.8-27B"
        } else {
            "local/retry-model"
        };
        let tools = if qwen { "[restart_test_dummy]" } else { "[]" };
        let effort = if qwen {
            "          effort: xhigh\n"
        } else {
            ""
        };
        let dummy_extension = if let Some(dummy_bin) = dummy_bin {
            format!(
                concat!(
                    "  e2e-test-dummy:\n",
                    "    command: [{}]\n",
                    "    role: tool\n",
                    "    require: true\n",
                    "    config:\n",
                    "      restart_mode: success\n",
                ),
                serde_json::to_string(&dummy_bin.display().to_string())?,
            )
        } else {
            String::new()
        };
        std::fs::write(
            config_dir.join("harness.yaml"),
            format!(
                concat!(
                    "agents:\n",
                    "  default_role: {role}\n",
                    "  idTemplate: main\n",
                    "  role_groups:\n",
                    "    e2e:\n",
                    "      roles:\n",
                    "        {role}:\n",
                    "          model: {model}\n",
                    "{effort}",
                    "          tools: {tools}\n",
                    "extensions:\n",
                    "  provider-builtin:\n",
                    "    command: [{}]\n",
                    "    role: provider\n",
                    "    require: true\n",
                    "{dummy_extension}",
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
                role = role,
                model = model,
                tools = tools,
                effort = effort,
                dummy_extension = dummy_extension,
            ),
        )?;
        Ok(Self {
            tempdir: Some(tempdir),
            config_dir,
            state_dir,
            harness_state_dir,
            server,
            failed: Cell::new(false),
            test_dummy: qwen,
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

    /// Reports whether the fixture's closed extension allowlist includes the
    /// deterministic tool binary.
    #[must_use]
    pub fn uses_test_dummy(&self) -> bool {
        self.test_dummy
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

impl Drop for ProviderBuiltinFixture {
    fn drop(&mut self) {
        if (std::thread::panicking()
            || self.failed.get()
            || std::env::var("TAU_E2E_KEEP_ARTIFACTS").as_deref() == Ok("1"))
            && let Some(tempdir) = self.tempdir.take()
        {
            let path = tempdir.keep();
            eprintln!(
                "retained provider-builtin e2e artifacts at {}",
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
