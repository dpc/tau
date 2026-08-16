//! Test-only process wrapper for a hermetic deterministic harness daemon.

use std::collections::BTreeSet;
use std::ffi as path_std_ffi;
use std::path::PathBuf;

use tau_harness::output_length_test_barrier::OutputLengthCommitCut;

/// Selects the closed provider and session topology for one daemon invocation.
#[derive(Clone, Copy, Eq, PartialEq)]
enum ProviderMode {
    /// Test-only deterministic fake provider.
    Fake,
    /// Exact packaged built-in provider.
    Builtin,
}

impl ProviderMode {
    /// Returns the sole allowed provider extension name.
    const fn extension_name(self) -> &'static str {
        match self {
            Self::Fake => "e2e-fake-provider",
            Self::Builtin => "provider-builtin",
        }
    }

    /// Returns the topology's fixed durable session identity.
    const fn session_id(self) -> &'static str {
        match self {
            Self::Fake => "deterministic-e2e-session",
            Self::Builtin => "provider-builtin-retry-e2e",
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let output_length_cut = args
        .iter()
        .position(|arg| arg == path_std_ffi::OsStr::new("--output-length-cut"))
        .map(|index| {
            if args.len() < index + 3 {
                return Err("--output-length-cut requires CUT REACHED_PATH");
            }
            let cut = args.remove(index + 1);
            let reached = args.remove(index + 1);
            args.remove(index);
            Ok((cut, reached))
        })
        .transpose()?;
    let test_dummy = args
        .last()
        .is_some_and(|arg| arg == path_std_ffi::OsStr::new("--test-dummy"));
    if test_dummy {
        args.pop();
    }
    let provider_mode = if args
        .last()
        .is_some_and(|arg| arg == path_std_ffi::OsStr::new("--provider-builtin"))
    {
        args.pop();
        ProviderMode::Builtin
    } else {
        ProviderMode::Fake
    };
    let (socket, harness_state, config_dir, state_dir, status, core_shell_cwd) =
        match args.as_slice() {
            [socket, harness_state, config_dir, state_dir, status] => {
                (socket, harness_state, config_dir, state_dir, status, None)
            }
            [socket, harness_state, config_dir, state_dir, status, cwd] => (
                socket,
                harness_state,
                config_dir,
                state_dir,
                status,
                Some(cwd),
            ),
            _ => {
                return Err(
                    "expected SOCKET HARNESS_STATE CONFIG_DIR STATE_DIR {new|resumed} \
                     [CORE_SHELL_CWD] [--provider-builtin] [--test-dummy] \
                     [--output-length-cut {planned-response|continuation-steer} REACHED_PATH]"
                        .into(),
                );
            }
        };
    if provider_mode == ProviderMode::Builtin && core_shell_cwd.is_some() {
        return Err("provider-builtin daemon cannot enable core-shell".into());
    }
    if let Some((cut, reached)) = output_length_cut {
        if provider_mode != ProviderMode::Fake {
            return Err("output-length cut requires the fake provider".into());
        }
        let cut = match cut.to_str() {
            Some("planned-response") => OutputLengthCommitCut::AfterPlannedResponse,
            Some("continuation-steer") => OutputLengthCommitCut::AfterContinuationSteer,
            _ => {
                return Err(
                    "output-length cut must be planned-response or continuation-steer".into(),
                );
            }
        };
        let harness_state = PathBuf::from(harness_state).canonicalize()?;
        let reached = PathBuf::from(reached);
        let reached_parent = reached
            .parent()
            .ok_or("output-length reached path must have a parent")?
            .canonicalize()?;
        if reached.exists() || reached_parent != harness_state {
            return Err(
                "output-length reached path must be an absent direct child of harness state".into(),
            );
        }
        tau_harness::output_length_test_barrier::install(cut, reached)?;
    }
    let status = match status.to_str() {
        Some("new") => tau_harness::SessionLaunchStatus::New,
        Some("resumed") => tau_harness::SessionLaunchStatus::Resumed,
        _ => return Err("status must be new or resumed".into()),
    };
    let mut allowed_extensions = BTreeSet::from([tau_proto::ExtensionName::parse(
        provider_mode.extension_name(),
    )?]);
    if test_dummy {
        allowed_extensions.insert(tau_proto::ExtensionName::parse("e2e-test-dummy")?);
    }
    if let Some(cwd) = core_shell_cwd {
        std::env::set_current_dir(cwd)?;
        allowed_extensions.insert(tau_proto::ExtensionName::parse("core-shell")?);
    }
    tau_harness::run_daemon_with_internal_tools(
        PathBuf::from(socket),
        PathBuf::from(harness_state),
        provider_mode.session_id(),
        tau_harness::ServeOptions::builder()
            .max_clients(1usize)
            .exit_on_disconnect(true)
            .session_status(status)
            .dirs(tau_config::settings::TauDirs {
                config_dir: Some(PathBuf::from(config_dir)),
                state_dir: Some(PathBuf::from(state_dir)),
            })
            .ignore_startup_environment(true)
            .allowed_extensions(allowed_extensions)
            .build(),
        tau_harness_tools::builtin_handlers(),
    )?;
    Ok(())
}
