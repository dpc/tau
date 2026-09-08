//! Purpose admission before operational provider runtime construction.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::error::Error;
use std::io::{Read, Write};
use std::sync::{Arc, mpsc};

use tau_client::TauExtensionRunner;
use tau_proto::{ClientKind, ProviderName};

use crate::{
    BuiltinProviderProfiles, CancellationState, CodexRuntime, EXTENSION_NAME,
    OAuthRefreshRejectionCache, PrewarmSupervisor, PromptCredentialAdmissionState,
    ProviderDiagnosticsState, ProviderExtension, ProviderRuntime, QuotaCoordinator,
    RuntimeExecutors, RuntimeStartup, WorkerMessage, WorkerQueueState, models_for_profiles,
    run_provider_loop, validate_configure_settings,
};

/// Runs the extension with injected executors and settings.
pub(super) fn run_inner_with_executors_and_clock_with_settings<R, W, F>(
    reader: R,
    writer: W,
    load_prompt_profiles: F,
    prompt_concurrency_limit: usize,
    executors: RuntimeExecutors,
    startup: RuntimeStartup,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
    F: FnMut(Option<&ProviderName>) -> BuiltinProviderProfiles + 'static,
{
    let extension = ProviderExtension::<F>::new(
        Arc::clone(&startup.settings_snapshot),
        (!startup.publish_models_after_configure).then(|| startup.profiles.clone()),
    );
    if startup.publish_models_after_configure {
        let hello = tau_proto::Hello {
            declaration_inspection: false,
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: EXTENSION_NAME.parse()?,
            client_kind: ClientKind::Provider,
            expected_session_id: None,
            capabilities: Vec::new(),
        };
        let Some(connection) =
            tau_client::prepare_inspection(reader, writer, hello, |configure| {
                let profiles = validate_configure_settings(&configure.settings_files)?;
                Ok(tau_proto::InspectionComplete {
                    providers: vec![tau_proto::InspectionProviderModels {
                        models: models_for_profiles(&profiles),
                    }],
                    ..Default::default()
                })
            })?
        else {
            return Ok(());
        };
        return connection.run(extension, |runner, reader, writer| {
            run_admitted_provider(
                runner,
                reader,
                writer,
                load_prompt_profiles,
                prompt_concurrency_limit,
                executors,
                startup,
            )
        })?;
    }
    run_admitted_provider(
        TauExtensionRunner::new(extension),
        reader,
        writer,
        load_prompt_profiles,
        prompt_concurrency_limit,
        executors,
        startup,
    )
}

/// Construct workers, network policy and credentials only on an ordinary path.
fn run_admitted_provider<R, W, F>(
    runner: TauExtensionRunner<ProviderExtension<F>>,
    reader: R,
    writer: W,
    load_prompt_profiles: F,
    prompt_concurrency_limit: usize,
    executors: RuntimeExecutors,
    startup: RuntimeStartup,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
    F: FnMut(Option<&ProviderName>) -> BuiltinProviderProfiles + 'static,
{
    let (worker_tx, worker_rx) = mpsc::channel::<WorkerMessage>();
    let startup_responses_modes = startup.profiles.startup_responses_modes();
    let network = Arc::new(tau_provider::OutboundNetworkPolicy::from_env());
    let codex_runtime = Arc::new(CodexRuntime::new(network));
    if !startup.publish_models_after_configure {
        codex_runtime.initialize_cache_diagnostics(startup.profiles.startup_cache_diagnostics());
    }
    let runtime = ProviderRuntime {
        load_prompt_profiles,
        startup_responses_modes,
        prompt_concurrency_limit,
        prompt_executor: executors.prompt,
        prewarm_executor: executors.prewarm,
        worker_tx,
        worker_rx,
        worker_waker: None,
        retry_scheduler: None,
        credential_admission: PromptCredentialAdmissionState::default(),
        retry_clock: executors.retry_clock,
        shared_cooldowns: BTreeMap::new(),
        shared_cooldown_generation: 0,
        codex_runtime,
        prewarm_supervisor: PrewarmSupervisor::default(),
        provider_profile_identities: BTreeMap::new(),
        prewarm_profile_identities: BTreeMap::new(),
        cancellation: Arc::new(CancellationState::default()),
        prompt_queue: VecDeque::new(),
        active_prompts: 0,
        input_closed: false,
        cancel_generation: 0,
        quota: QuotaCoordinator::default(),
        oauth_refresh_rejections: OAuthRefreshRejectionCache::default(),
        unavailable_compact_identities: HashSet::new(),
        compact_profile_identities: HashMap::new(),
        extension_data_client: None,
        declared_credential_observations: None,
        declared_models: None,
        diagnostics: ProviderDiagnosticsState {
            output_queue: WorkerQueueState::enabled(),
            ..ProviderDiagnosticsState::default()
        },
    };
    let install_extension_data_client = startup.publish_models_after_configure;
    let mut runtime = runner.start_manual_loop_with_extension_data_state(
        reader,
        writer,
        move |_handle, extension_data_client| {
            let mut runtime = runtime;
            if install_extension_data_client {
                runtime.extension_data_client = Some(extension_data_client);
            }
            runtime
        },
    )?;
    let worker_waker = runtime.waker();
    runtime.state_mut().set_worker_waker(worker_waker);
    let handle = runtime.handle();
    #[cfg(not(test))]
    tau_provider::debug_capture_writer::initialize_provider_debug_capture_transport(handle.clone());
    runtime.state_mut().initialize_quota(&handle)?;
    run_provider_loop(runtime)
}
