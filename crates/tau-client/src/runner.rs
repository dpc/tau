use std::io::{Read, Write};
use std::sync::mpsc;

use crate::builder::ExtensionBuilder;
use crate::writer_thread::{WriterCommand, run_writer};
use crate::{ClientError, ClientHandle, ClientResult, TauExtension};

/// Runtime that performs the Tau protocol lifecycle for one extension.
pub struct TauExtensionRunner<Extension> {
    /// Extension declaration consumed when the runner starts.
    extension: Extension,
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
        let mut builder = ExtensionBuilder::new(self.extension.name(), self.extension.kind());
        self.extension.register(&mut builder);
        builder.validate()?;

        let (sender, receiver) = mpsc::channel::<WriterCommand>();
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
                (Ok(state), Ok(()), Ok(())) => Ok(state),
                (Err(error), _, _) => Err(error),
                (_, Err(error), _) => Err(error),
                (_, _, Err(error)) => Err(error),
            }
        })
    }
}

/// Runs startup and harness message dispatch on the reader thread.
fn run_client_loop<R, State>(
    reader: R,
    mut state: State,
    mut builder: ExtensionBuilder<State>,
    handle: ClientHandle,
) -> ClientResult<State>
where
    R: Read,
{
    write_startup(&builder, &handle)?;

    let mut reader = tau_proto::PeerInputReader::new(reader);
    while let Some(message) = reader.read_message()? {
        let stop = dispatch_message(message, &mut state, &mut builder, &handle)?;
        if stop {
            break;
        }
    }
    Ok(state)
}

/// Writes the startup prelude in harness-defined order.
fn write_startup<State>(
    builder: &ExtensionBuilder<State>,
    handle: &ClientHandle,
) -> ClientResult<()> {
    handle.send(tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: builder.name.clone(),
        client_kind: builder.kind.clone(),
    }))?;
    if builder.force_subscribe || !builder.selectors.is_empty() {
        handle.send(tau_proto::HarnessInputMessage::Subscribe(
            tau_proto::Subscribe {
                selectors: builder.selectors.clone(),
            },
        ))?;
    }
    if let Some(intercept) = &builder.intercept {
        handle.send(tau_proto::HarnessInputMessage::Intercept(intercept.clone()))?;
    }
    for event in &builder.startup_events {
        handle.emit(event.clone())?;
    }
    handle.send(tau_proto::HarnessInputMessage::Ready(tau_proto::Ready {
        message: builder.ready_message.clone(),
    }))?;
    Ok(())
}

/// Dispatches one harness-to-peer message and returns whether the loop should
/// stop.
fn dispatch_message<State>(
    message: tau_proto::HarnessOutputMessage,
    state: &mut State,
    builder: &mut ExtensionBuilder<State>,
    handle: &ClientHandle,
) -> ClientResult<bool> {
    match message {
        tau_proto::HarnessOutputMessage::Configure(configure) => {
            for handler in &mut builder.configure_handlers {
                handler.handle(&configure, state, handle)?;
            }
            Ok(false)
        }
        tau_proto::HarnessOutputMessage::Deliver(delivery) => {
            dispatch_delivery(&delivery, state, builder, handle)
        }
        tau_proto::HarnessOutputMessage::InterceptRequest(request) => {
            dispatch_intercept(&request, state, builder, handle)?;
            Ok(false)
        }
        tau_proto::HarnessOutputMessage::Disconnect(_) => Ok(true),
        tau_proto::HarnessOutputMessage::AgentPromptCreatedResult(_)
        | tau_proto::HarnessOutputMessage::RenderedSystemPromptResult(_)
        | tau_proto::HarnessOutputMessage::RenderedPromptResult(_)
        | tau_proto::HarnessOutputMessage::RenderedToolDefinitionsResult(_)
        | tau_proto::HarnessOutputMessage::ExtensionDataResult(_)
        | tau_proto::HarnessOutputMessage::ExternalAgentMessageResult(_) => Ok(false),
    }
}

/// Dispatches one event delivery to matching live tool, raw event, and typed
/// event handlers.
fn dispatch_delivery<State>(
    delivery: &tau_proto::EventDelivery,
    state: &mut State,
    builder: &mut ExtensionBuilder<State>,
    handle: &ClientHandle,
) -> ClientResult<bool> {
    let mut stop_requested = false;
    if !delivery.is_replay()
        && let tau_proto::Event::ToolStarted(invoke) = delivery.event.as_ref()
    {
        for handler in &mut builder.tool_handlers {
            handler.handle(invoke, state, handle, &mut stop_requested)?;
        }
    }
    for handler in &mut builder.raw_event_handlers {
        handler.handle(delivery, state, handle)?;
    }
    for handler in &mut builder.event_handlers {
        handler.handle(delivery, state, handle)?;
    }
    Ok(stop_requested)
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
