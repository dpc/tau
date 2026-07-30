//! Test-only process wrapper for a hermetic deterministic harness daemon.

use std::collections::BTreeSet;
use std::ffi as path_std_ffi;
use std::path::PathBuf;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let test_dummy = args
        .last()
        .is_some_and(|arg| arg == path_std_ffi::OsStr::new("--test-dummy"));
    if test_dummy {
        args.pop();
    }
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
                 [CORE_SHELL_CWD] [--test-dummy]"
                        .into(),
                );
            }
        };
    let status = match status.to_str() {
        Some("new") => tau_harness::SessionLaunchStatus::New,
        Some("resumed") => tau_harness::SessionLaunchStatus::Resumed,
        _ => return Err("status must be new or resumed".into()),
    };
    let mut allowed_extensions =
        BTreeSet::from([tau_proto::ExtensionName::parse("e2e-fake-provider")?]);
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
        "deterministic-e2e-session",
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
