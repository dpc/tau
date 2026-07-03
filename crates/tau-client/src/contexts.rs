use std::collections::BTreeMap;
use std::path::Path;

use crate::{ClientHandle, ClientResult};

/// Context passed to a live tool dispatch handler.
pub struct ToolContext<'a, State> {
    /// Mutable extension state shared by handlers.
    pub state: &'a mut State,
    /// Live `tool.started` payload selected by tool name.
    pub invoke: &'a tau_proto::ToolStarted,
    /// Cloneable handle for sending frames to the harness.
    pub handle: ClientHandle,
    /// Stop flag checked by the runner after this handler returns.
    pub(crate) stop_requested: &'a mut bool,
}

impl<'a, State> ToolContext<'a, State> {
    /// Returns the live `tool.started` payload selected by tool name.
    #[must_use]
    pub fn invoke(&self) -> &tau_proto::ToolStarted {
        self.invoke
    }

    /// Returns a cloneable handle for sending frames to the harness.
    #[must_use]
    pub fn handle(&self) -> ClientHandle {
        self.handle.clone()
    }

    /// Emits a durable event through the harness.
    pub fn emit(&self, event: tau_proto::Event) -> ClientResult<()> {
        self.handle.emit(event)
    }

    /// Requests that the runner stop after the current message is handled.
    pub fn request_stop(&mut self) {
        *self.stop_requested = true;
    }
}

/// Context passed to a typed configuration handler.
pub struct ConfigureContext<'a, State, Config> {
    /// Mutable extension state shared by handlers.
    pub state: &'a mut State,
    /// Parsed typed configuration value.
    pub config: Config,
    /// Original configure message metadata from the harness.
    pub configure: &'a tau_proto::Configure,
    /// Cloneable handle for sending frames to the harness.
    pub handle: ClientHandle,
}

impl<'a, State, Config> ConfigureContext<'a, State, Config> {
    /// Returns the parsed typed configuration value.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Returns the configured extension instance name, if any.
    #[must_use]
    pub fn instance_name(&self) -> Option<&tau_proto::ExtensionName> {
        self.configure.instance_name.as_ref()
    }

    /// Returns the harness-assigned extension state directory, if any.
    #[must_use]
    pub fn state_dir(&self) -> Option<&Path> {
        self.configure.state_dir.as_deref()
    }

    /// Returns the secrets authorized for this extension instance.
    #[must_use]
    pub fn secrets(&self) -> &BTreeMap<String, tau_proto::SecretValue> {
        &self.configure.secrets
    }

    /// Returns a cloneable handle for sending frames to the harness.
    #[must_use]
    pub fn handle(&self) -> ClientHandle {
        self.handle.clone()
    }
}

/// Context passed to a typed event handler.
pub struct EventContext<'a, State, Payload> {
    /// Mutable extension state shared by handlers.
    pub state: &'a mut State,
    /// Typed event payload selected from the delivery.
    pub event: &'a Payload,
    /// True when the delivery is replaying historical state.
    pub replay: bool,
    /// Runtime or historical append timestamp attached to the delivery.
    pub recorded_at: Option<tau_proto::UnixMicros>,
    /// Cloneable handle for sending frames to the harness.
    pub handle: ClientHandle,
}

impl<'a, State, Payload> EventContext<'a, State, Payload> {
    /// Returns the typed event payload selected from the delivery.
    #[must_use]
    pub fn event(&self) -> &Payload {
        self.event
    }

    /// Returns true when this delivery is replaying historical state.
    #[must_use]
    pub fn is_replay(&self) -> bool {
        self.replay
    }

    /// Returns a cloneable handle for sending frames to the harness.
    #[must_use]
    pub fn handle(&self) -> ClientHandle {
        self.handle.clone()
    }
}

/// Context passed to a raw event delivery handler.
pub struct RawEventContext<'a, State> {
    /// Mutable extension state shared by handlers.
    pub state: &'a mut State,
    /// Full event delivery from the harness.
    pub delivery: &'a tau_proto::EventDelivery,
    /// Cloneable handle for sending frames to the harness.
    pub handle: ClientHandle,
}

impl<'a, State> RawEventContext<'a, State> {
    /// Returns the delivered event.
    #[must_use]
    pub fn event(&self) -> &tau_proto::Event {
        self.delivery.event()
    }

    /// Returns true when this delivery is replaying historical state.
    #[must_use]
    pub fn is_replay(&self) -> bool {
        self.delivery.is_replay()
    }

    /// Returns the runtime or historical append timestamp attached to the
    /// delivery.
    #[must_use]
    pub fn recorded_at(&self) -> Option<tau_proto::UnixMicros> {
        self.delivery.recorded_at
    }

    /// Returns a cloneable handle for sending frames to the harness.
    #[must_use]
    pub fn handle(&self) -> ClientHandle {
        self.handle.clone()
    }
}

/// Context passed to an intercept handler.
pub struct InterceptContext<'a, State> {
    /// Mutable extension state shared by handlers.
    pub state: &'a mut State,
    /// Intercept request from the harness.
    pub request: &'a tau_proto::InterceptRequest,
    /// Cloneable handle for sending frames to the harness.
    pub handle: ClientHandle,
}

impl<'a, State> InterceptContext<'a, State> {
    /// Returns the event offered to this interceptor.
    #[must_use]
    pub fn event(&self) -> &tau_proto::Event {
        self.request.event.as_ref()
    }

    /// Returns true when the original publish request was transient.
    #[must_use]
    pub fn transient(&self) -> bool {
        self.request.transient
    }

    /// Returns a cloneable handle for sending frames to the harness.
    #[must_use]
    pub fn handle(&self) -> ClientHandle {
        self.handle.clone()
    }

    /// Emits a durable event while handling the intercept request.
    pub fn emit(&self, event: tau_proto::Event) -> ClientResult<()> {
        self.handle.emit(event)
    }

    /// Emits a transient event while handling the intercept request.
    pub fn emit_transient(&self, event: tau_proto::Event) -> ClientResult<()> {
        self.handle.emit_transient(event)
    }
}
