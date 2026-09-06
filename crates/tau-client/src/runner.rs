use std::io::{Read, Write};

use crate::builder::ExtensionBuilder;
use crate::manual_runtime::DispatchOutcome;
use crate::writer_thread::{run_writer, writer_channel};
use crate::{ClientError, ClientHandle, ClientResult, TauExtension, builder as path_crate_builder};

/// Runtime that performs the Tau protocol lifecycle for one extension.
pub struct TauExtensionRunner<Extension> {
    /// Extension declaration consumed when the runner starts.
    pub(crate) extension: Extension,
}

impl<Extension> TauExtensionRunner<Extension>
where
    Extension: TauExtension,
{
    /// Creates a runner for one extension declaration.
    #[must_use]
    pub fn new(extension: Extension) -> Self {
        Self { extension }
    }

    /// Runs the extension over the supplied protocol streams and returns final
    /// state.
    ///
    /// Startup is `Hello` → initial `Configure` → scoped declarations →
    /// `Ready`.
    ///
    /// # Errors
    ///
    /// Returns an error when builder validation fails, protocol input cannot be
    /// decoded, protocol output cannot be encoded or flushed, a handler returns
    /// an error that should stop the extension, or the writer thread closes or
    /// panics before shutdown completes.
    pub fn run<R, W>(
        self,
        reader: R,
        writer: W,
        state: Extension::State,
    ) -> ClientResult<Extension::State>
    where
        R: Read,
        W: Write + Send,
    {
        let mut builder = ExtensionBuilder::new(self.extension.name(), self.extension.kind())?;
        self.extension.register(&mut builder);
        builder.validate()?;

        let (sender, receiver) = writer_channel();
        let handle = ClientHandle::new(sender);

        std::thread::scope(|scope| {
            let writer_thread = scope.spawn(move || run_writer(writer, receiver));
            let run_result = run_client_loop(reader, state, builder, handle.clone());
            let shutdown_result = handle.shutdown();
            let writer_result = writer_thread
                .join()
                .map_err(|_| ClientError::WriterPanicked)
                .and_then(|result| result);

            match (run_result, shutdown_result, writer_result) {
                (Ok((state, _)), Ok(()), Ok(())) => Ok(state),
                (Err(error), _, _) => Err(error),
                (_, Err(error), _) => Err(error),
                (_, _, Err(error)) => Err(error),
            }
        })
    }

    /// Runs the extension without joining the writer after harness disconnect.
    ///
    /// This is intended for extensions that intentionally keep detached
    /// background workers alive after a harness `Disconnect`, where joining the
    /// writer would make disconnect latency depend on queued background output
    /// or pipe backpressure. Non-disconnect exits still flush and join the
    /// writer so ordinary EOF and handler-stop paths keep normal error
    /// reporting.
    ///
    /// # Errors
    ///
    /// Returns an error when builder validation fails, protocol input cannot be
    /// decoded, startup output cannot be encoded or flushed, or a handler
    /// returns an error that should stop the extension.
    pub fn run_detached_writer<R, W>(
        self,
        reader: R,
        writer: W,
        state: Extension::State,
    ) -> ClientResult<Extension::State>
    where
        R: Read,
        W: Write + Send + 'static,
    {
        self.run_detached_writer_with_state(reader, writer, |_| state)
    }

    /// Runs the extension without joining the writer after harness disconnect,
    /// constructing state after initial Configure establishes the name scope.
    ///
    /// The supplied factory receives a cloneable [`ClientHandle`] that can be
    /// stored in runtime state for background workers. The runner writes
    /// `Hello`, waits for initial `Configure`, constructs state, dispatches
    /// configuration, writes static declarations followed by accepted
    /// configuration-derived declarations, then writes `Ready`. Public handle
    /// output remains gated until that prelude completes.
    ///
    /// # Errors
    ///
    /// Returns an error when builder validation fails, startup output cannot be
    /// encoded or flushed, protocol input cannot be decoded, a handler returns
    /// an error that should stop the extension, or non-disconnect writer
    /// shutdown fails.
    pub fn run_detached_writer_with_state<R, W, MakeState>(
        self,
        reader: R,
        writer: W,
        make_state: MakeState,
    ) -> ClientResult<Extension::State>
    where
        R: Read,
        W: Write + Send + 'static,
        MakeState: FnOnce(ClientHandle) -> Extension::State,
    {
        let mut builder = ExtensionBuilder::new(self.extension.name(), self.extension.kind())?;
        self.extension.register(&mut builder);
        builder.validate()?;

        let (sender, receiver) = writer_channel();
        let handle = ClientHandle::new(sender);

        let writer_thread = std::thread::spawn(move || run_writer(writer, receiver));
        let run_result =
            run_client_loop_with_state_factory(reader, builder, handle.clone(), make_state);
        if let Ok((state, LoopExit::Disconnect)) = run_result {
            return Ok(state);
        }

        let shutdown_result = handle.shutdown();
        let writer_result = writer_thread
            .join()
            .map_err(|_| ClientError::WriterPanicked)
            .and_then(|result| result);
        match (run_result, shutdown_result, writer_result) {
            (Ok((state, _)), Ok(()), Ok(())) => Ok(state),
            (Err(error), _, _) => Err(error),
            (_, Err(error), _) => Err(error),
            (_, _, Err(error)) => Err(error),
        }
    }
}

/// Reason the reader loop stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoopExit {
    /// Harness input reached EOF.
    InputClosed,
    /// Harness sent an explicit disconnect frame.
    Disconnect,
    /// A handler requested the extension stop.
    StopRequested,
}

/// Runs startup and harness message dispatch on the reader thread.
fn run_client_loop<R, State>(
    reader: R,
    state: State,
    mut builder: ExtensionBuilder<State>,
    handle: ClientHandle,
) -> ClientResult<(State, LoopExit)>
where
    R: Read,
{
    write_hello(&builder, &handle)?;
    let mut reader = tau_proto::PeerInputReader::new(reader);
    let Some(configure) = read_initial_configure(&mut reader)? else {
        return Ok((state, LoopExit::Disconnect));
    };
    install_scope(&mut builder, &handle, &configure)?;
    let mut state = state;
    if !dispatch_initial_configure(&configure, &mut state, &mut builder, &handle)? {
        handle.discard_configure_outputs();
        return Ok((state, LoopExit::StopRequested));
    }
    write_startup_after_configure(&builder, &handle)?;
    run_message_loop_reader(&mut reader, state, builder, handle)
}

/// Runs the initial gate while constructing state before Configure dispatch.
fn run_client_loop_with_state_factory<R, State, MakeState>(
    reader: R,
    mut builder: ExtensionBuilder<State>,
    handle: ClientHandle,
    make_state: MakeState,
) -> ClientResult<(State, LoopExit)>
where
    R: Read,
    MakeState: FnOnce(ClientHandle) -> State,
{
    write_hello(&builder, &handle)?;
    let mut reader = tau_proto::PeerInputReader::new(reader);
    let Some(configure) = read_initial_configure(&mut reader)? else {
        return Err(ClientError::handler(
            "harness disconnected before detached state initialization",
        ));
    };
    install_scope(&mut builder, &handle, &configure)?;
    let mut state = make_state(handle.clone());
    if !dispatch_initial_configure(&configure, &mut state, &mut builder, &handle)? {
        handle.discard_configure_outputs();
        return Ok((state, LoopExit::StopRequested));
    }
    write_startup_after_configure(&builder, &handle)?;
    run_message_loop_reader(&mut reader, state, builder, handle)
}

/// Runs harness message dispatch after startup frames have been written.
fn run_message_loop_reader<R, State>(
    reader: &mut tau_proto::PeerInputReader<R>,
    mut state: State,
    mut builder: ExtensionBuilder<State>,
    handle: ClientHandle,
) -> ClientResult<(State, LoopExit)>
where
    R: Read,
{
    while let Some(message) = reader.read_message()? {
        match dispatch_message(message, &mut state, &mut builder, &handle)? {
            DispatchOutcome::Continue => {}
            DispatchOutcome::Disconnect(_) => return Ok((state, LoopExit::Disconnect)),
            DispatchOutcome::StopRequested => return Ok((state, LoopExit::StopRequested)),
        }
    }
    Ok((state, LoopExit::InputClosed))
}

/// Require the first harness response after `Hello` to be `Configure`.
fn read_initial_configure<R: Read>(
    reader: &mut tau_proto::PeerInputReader<R>,
) -> ClientResult<Option<tau_proto::Configure>> {
    match reader.read_message()? {
        Some(tau_proto::HarnessOutputMessage::Configure(configure)) => Ok(Some(configure)),
        Some(tau_proto::HarnessOutputMessage::Disconnect(_)) | None => Ok(None),
        Some(message) => Err(ClientError::handler(format!(
            "expected initial Configure after Hello, received {message:?}"
        ))),
    }
}

/// Install the immutable scope before configuration handlers run.
fn install_scope<State>(
    builder: &mut ExtensionBuilder<State>,
    handle: &ClientHandle,
    configure: &tau_proto::Configure,
) -> ClientResult<()> {
    let scope = crate::ToolNameScope::from_configure(configure);
    handle.install_tool_name_scope(scope.clone())?;
    if let Err(error) = builder.apply_tool_name_scope(&scope) {
        handle.config_error(error.to_string())?;
        return Err(error);
    }
    Ok(())
}

/// Deliver the buffered initial configuration before declarations and `Ready`.
pub(crate) fn dispatch_initial_configure<State>(
    configure: &tau_proto::Configure,
    state: &mut State,
    builder: &mut ExtensionBuilder<State>,
    handle: &ClientHandle,
) -> ClientResult<bool> {
    handle.set_configuring(true);
    let result = dispatch_message(
        tau_proto::HarnessOutputMessage::Configure(configure.clone()),
        state,
        builder,
        handle,
    );
    handle.set_configuring(false);
    result?;
    Ok(!handle.startup_rejected())
}

/// Writes startup declarations after the initial configuration gate.
pub(crate) fn write_startup_after_configure<State>(
    builder: &ExtensionBuilder<State>,
    handle: &ClientHandle,
) -> ClientResult<()> {
    if builder.force_subscribe
        || !builder.historical_selectors.is_empty()
        || !builder.live_selectors.is_empty()
    {
        handle.send_startup(tau_proto::HarnessInputMessage::Subscribe(
            tau_proto::Subscribe {
                historical_selectors: builder.historical_selectors.clone(),
                live_selectors: builder.live_selectors.clone(),
            },
        ))?;
    }
    if let Some(intercept) = &builder.intercept {
        handle.send_startup(tau_proto::HarnessInputMessage::Intercept(intercept.clone()))?;
    }
    for declaration in &builder.startup_events {
        let path_crate_builder::StartupDeclaration::Emit(emit) = declaration else {
            return Err(ClientError::builder(
                "startup declaration was not resolved after initial Configure",
            ));
        };
        handle.send_startup(tau_proto::HarnessInputMessage::Emit(emit.clone()))?;
    }
    handle.flush_configure_outputs()?;
    write_ready(handle, builder.ready_message.clone())?;
    Ok(())
}

/// Writes the initial `Hello` frame for one client connection.
pub(crate) fn write_hello<State>(
    builder: &ExtensionBuilder<State>,
    handle: &ClientHandle,
) -> ClientResult<()> {
    handle.send_startup(tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: builder.name.clone(),
        client_kind: builder.kind.clone(),
        expected_session_id: None,
        capabilities: builder.peer_capabilities.clone(),
    }))
}

/// Writes the terminal startup `Ready` frame.
pub(crate) fn write_ready(handle: &ClientHandle, message: Option<String>) -> ClientResult<()> {
    handle.send_ready(message)
}

/// Dispatches one harness-to-peer message and reports whether the caller should
/// continue or stop its loop.
pub(crate) fn dispatch_message<State>(
    message: tau_proto::HarnessOutputMessage,
    state: &mut State,
    builder: &mut ExtensionBuilder<State>,
    handle: &ClientHandle,
) -> ClientResult<DispatchOutcome> {
    if let tau_proto::HarnessOutputMessage::Configure(configure) = &message
        && let Err(error) =
            handle.install_tool_name_scope(crate::ToolNameScope::from_configure(configure))
    {
        handle.config_error(error.to_string())?;
        return Ok(DispatchOutcome::Continue);
    }
    for handler in &mut builder.output_message_handlers {
        handler.handle(&message, state, handle)?;
    }
    match message {
        tau_proto::HarnessOutputMessage::Configure(configure) => {
            for handler in &mut builder.configure_handlers {
                handler.handle(&configure, state, handle)?;
            }
            Ok(DispatchOutcome::Continue)
        }
        tau_proto::HarnessOutputMessage::Deliver(delivery) => {
            dispatch_delivery(&delivery, state, builder, handle)
        }
        tau_proto::HarnessOutputMessage::InterceptRequest(request) => {
            dispatch_intercept(&request, state, builder, handle)?;
            Ok(DispatchOutcome::Continue)
        }
        tau_proto::HarnessOutputMessage::Disconnect(disconnect) => {
            Ok(DispatchOutcome::Disconnect(disconnect))
        }
        tau_proto::HarnessOutputMessage::AgentPromptCreatedResult(_)
        | tau_proto::HarnessOutputMessage::SessionAccepted(_)
        | tau_proto::HarnessOutputMessage::UiQuitResult(_)
        | tau_proto::HarnessOutputMessage::RenderedSystemPromptResult(_)
        | tau_proto::HarnessOutputMessage::RenderedPromptResult(_)
        | tau_proto::HarnessOutputMessage::RenderedToolDefinitionsResult(_)
        | tau_proto::HarnessOutputMessage::CurrentSessionResult(_)
        | tau_proto::HarnessOutputMessage::SessionAgentListResult(_)
        | tau_proto::HarnessOutputMessage::UnloadSessionAgentResult(_)
        | tau_proto::HarnessOutputMessage::ExtensionDataResult(_)
        | tau_proto::HarnessOutputMessage::ExternalAgentMessageResult(_)
        | tau_proto::HarnessOutputMessage::ExternalAgentMessageAuthResult(_)
        | tau_proto::HarnessOutputMessage::PeerSessionProbeResult(_) => {
            Ok(DispatchOutcome::Continue)
        }
    }
}

/// Dispatches one event delivery to matching live tool/action, raw event, and
/// typed event handlers.
fn dispatch_delivery<State>(
    delivery: &tau_proto::EventDelivery,
    state: &mut State,
    builder: &mut ExtensionBuilder<State>,
    handle: &ClientHandle,
) -> ClientResult<DispatchOutcome> {
    let mut stop_requested = false;
    if !delivery.is_replay()
        && let tau_proto::Event::ToolStarted(invoke) = delivery.event.as_ref()
    {
        for handler in &mut builder.tool_handlers {
            handler.handle(invoke, state, handle, &mut stop_requested)?;
        }
    }
    if !delivery.is_replay()
        && let tau_proto::Event::ActionInvoke(invoke) = delivery.event.as_ref()
    {
        for handler in &mut builder.action_handlers {
            handler.handle(invoke, state, handle)?;
        }
    }
    for handler in &mut builder.raw_event_handlers {
        handler.handle(delivery, state, handle)?;
    }
    for handler in &mut builder.event_handlers {
        handler.handle(delivery, state, handle)?;
    }
    if stop_requested {
        Ok(DispatchOutcome::StopRequested)
    } else {
        Ok(DispatchOutcome::Continue)
    }
}

/// Dispatches one intercept request and sends exactly one protocol reply.
fn dispatch_intercept<State>(
    request: &tau_proto::InterceptRequest,
    state: &mut State,
    builder: &mut ExtensionBuilder<State>,
    handle: &ClientHandle,
) -> ClientResult<()> {
    let decision_result = match &mut builder.intercept_handler {
        Some(handler) => handler.handle(request, state, handle),
        None => Ok(crate::InterceptDecision::Pass),
    };
    let (decision, handler_error) = match decision_result {
        Ok(decision) => (decision, None),
        Err(error) => (crate::InterceptDecision::Pass, Some(error)),
    };
    handle.send(tau_proto::HarnessInputMessage::InterceptReply(
        tau_proto::InterceptReply {
            action: decision.into_action(),
        },
    ))?;
    if let Some(error) = handler_error {
        return Err(error);
    }
    Ok(())
}
