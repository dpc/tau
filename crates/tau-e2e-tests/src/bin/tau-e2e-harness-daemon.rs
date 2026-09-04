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
    let mut args = parse_daemon_args()?;
    install_output_length_cut(args.output_length_cut.take())?;
    launch_daemon(args)
}

/// Fully parsed closed command line for one deterministic daemon process.
struct DaemonArgs {
    /// Daemon listener socket path.
    socket: path_std_ffi::OsString,
    /// Harness-private durable state path.
    harness_state: path_std_ffi::OsString,
    /// Fixture-owned configuration path.
    config_dir: path_std_ffi::OsString,
    /// Fixture-owned Tau state path.
    state_dir: path_std_ffi::OsString,
    /// Requested new or resumed session status.
    status: path_std_ffi::OsString,
    /// Optional core-shell working directory.
    core_shell_cwd: Option<path_std_ffi::OsString>,
    /// Selected exact provider topology.
    provider_mode: ProviderMode,
    /// Whether the exact test-dummy extension is allowed.
    test_dummy: bool,
    /// Optional fixture-only output-length persistence cut.
    output_length_cut: Option<(OutputLengthCommitCut, PathBuf)>,
}

/// Parses the closed deterministic-daemon command line before mutating process
/// state.
fn parse_daemon_args() -> Result<DaemonArgs, Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let output_length_cut = parse_output_length_cut(&mut args)?;
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
    let output_length_cut = output_length_cut
        .map(|(cut, reached)| parse_output_length_cut_value(cut, reached))
        .transpose()?;
    if provider_mode == ProviderMode::Builtin && core_shell_cwd.is_some() {
        return Err("provider-builtin daemon cannot enable core-shell".into());
    }
    if output_length_cut.is_some() && provider_mode != ProviderMode::Fake {
        return Err("output-length cut requires the fake provider".into());
    }
    Ok(DaemonArgs {
        socket: socket.to_owned(),
        harness_state: harness_state.to_owned(),
        config_dir: config_dir.to_owned(),
        state_dir: state_dir.to_owned(),
        status: status.to_owned(),
        core_shell_cwd: core_shell_cwd.cloned(),
        provider_mode,
        test_dummy,
        output_length_cut,
    })
}

/// Removes and returns the sole optional output-length-cut argument pair.
fn parse_output_length_cut(
    args: &mut Vec<path_std_ffi::OsString>,
) -> Result<Option<(path_std_ffi::OsString, path_std_ffi::OsString)>, &'static str> {
    args.iter()
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
        .transpose()
}

/// Parses fixture-only output-length-cut values into the harness test hook
/// type.
fn parse_output_length_cut_value(
    cut: path_std_ffi::OsString,
    reached: path_std_ffi::OsString,
) -> Result<(OutputLengthCommitCut, PathBuf), Box<dyn std::error::Error>> {
    let cut = match cut.to_str() {
        Some("planned-response") => OutputLengthCommitCut::AfterPlannedResponse,
        Some("continuation-steer") => OutputLengthCommitCut::AfterContinuationSteer,
        Some("typed-receipt-sender-terminal") => OutputLengthCommitCut::AfterTypedReceiptSenderTerminal,
        Some("next-provider-response") => OutputLengthCommitCut::AfterNextProviderResponse,
        _ => return Err("test persistence cut must be planned-response, continuation-steer, typed-receipt-sender-terminal, or next-provider-response".into()),
    };
    let reached = PathBuf::from(reached);
    if reached.parent().is_none() {
        return Err("test barrier socket must have a parent".into());
    }
    Ok((cut, reached))
}

/// Installs the optional fixture-only persistence cut after command-line
/// validation.
fn install_output_length_cut(
    output_length_cut: Option<(OutputLengthCommitCut, PathBuf)>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some((cut, reached)) = output_length_cut {
        tau_harness::output_length_test_barrier::install(cut, reached)?;
    }
    Ok(())
}

/// Starts the daemon with only the exact extensions selected by its closed
/// arguments.
fn launch_daemon(args: DaemonArgs) -> Result<(), Box<dyn std::error::Error>> {
    let DaemonArgs {
        socket,
        harness_state,
        config_dir,
        state_dir,
        status,
        core_shell_cwd,
        provider_mode,
        test_dummy,
        output_length_cut: _,
    } = args;
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
        std::env::set_current_dir(&cwd)?;
        allowed_extensions.insert(tau_proto::ExtensionName::parse("core-shell")?);
    }
    tau_harness::run_daemon_with_internal_tools(
        PathBuf::from(socket),
        PathBuf::from(harness_state),
        provider_mode.session_id(),
        tau_harness::ServeOptions::builder()
            .max_clients(1usize)
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
