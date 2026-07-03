use serde::de::DeserializeOwned;

use crate::contexts::{
    ConfigureContext, ConfigureErrorContext, EventContext, InterceptContext, RawConfigureContext,
    RawEventContext, ToolContext,
};
use crate::event_payload::EventPayload;
use crate::{ClientHandle, ClientResult, InterceptDecision};

/// Runtime handler for one typed configuration declaration.
pub(crate) trait ConfigureHandler<State> {
    /// Parses and applies one configure message, emitting `ConfigError` on
    /// failure.
    fn handle(
        &mut self,
        configure: &tau_proto::Configure,
        state: &mut State,
        handle: &ClientHandle,
    ) -> ClientResult<()>;
}

/// Raw configuration handler implementation.
pub(crate) struct RawConfigureHandler<F> {
    /// User-provided raw configuration handler.
    handler: F,
}

impl<F> RawConfigureHandler<F> {
    /// Creates an untyped configuration handler wrapper.
    #[must_use]
    pub(crate) fn new(handler: F) -> Self {
        Self { handler }
    }
}

impl<State, F> ConfigureHandler<State> for RawConfigureHandler<F>
where
    F: for<'a> FnMut(RawConfigureContext<'a, State>) -> ClientResult<()>,
{
    fn handle(
        &mut self,
        configure: &tau_proto::Configure,
        state: &mut State,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let cx = RawConfigureContext {
            state,
            configure,
            handle: handle.clone(),
        };
        if let Err(error) = (self.handler)(cx) {
            handle.config_error(error.to_string())?;
        }
        Ok(())
    }
}

/// Runtime handler for one delivered event declaration.
pub(crate) trait EventHandler<State> {
    /// Dispatches one event delivery when its type and replay policy match.
    fn handle(
        &mut self,
        delivery: &tau_proto::EventDelivery,
        state: &mut State,
        handle: &ClientHandle,
    ) -> ClientResult<()>;
}

/// Runtime handler for one raw event delivery declaration.
pub(crate) trait RawEventHandler<State> {
    /// Dispatches one event delivery when its selector and replay policy match.
    fn handle(
        &mut self,
        delivery: &tau_proto::EventDelivery,
        state: &mut State,
        handle: &ClientHandle,
    ) -> ClientResult<()>;
}

/// Runtime handler for one live tool declaration.
pub(crate) trait ToolHandler<State> {
    /// Dispatches one live `tool.started` payload when its name matches.
    fn handle(
        &mut self,
        invoke: &tau_proto::ToolStarted,
        state: &mut State,
        handle: &ClientHandle,
        stop_requested: &mut bool,
    ) -> ClientResult<()>;
}

/// Runtime handler for intercepted publish requests.
pub(crate) trait InterceptHandler<State> {
    /// Computes the intercept decision for one request.
    fn handle(
        &mut self,
        request: &tau_proto::InterceptRequest,
        state: &mut State,
        handle: &ClientHandle,
    ) -> ClientResult<InterceptDecision>;
}

/// Optional callback invoked when typed configuration fails.
type ConfigureErrorHandler<State> =
    Box<dyn for<'a> FnMut(ConfigureErrorContext<'a, State>) + 'static>;

/// Typed configuration handler implementation.
pub(crate) struct TypedConfigureHandler<Config, F> {
    /// User-provided configuration handler.
    handler: F,
    /// Marker for the typed config payload.
    _config: std::marker::PhantomData<fn() -> Config>,
}

impl<Config, F> TypedConfigureHandler<Config, F> {
    /// Creates a typed configuration handler wrapper.
    #[must_use]
    pub(crate) fn new(handler: F) -> Self {
        Self {
            handler,
            _config: std::marker::PhantomData,
        }
    }
}

impl<State, Config, F> ConfigureHandler<State> for TypedConfigureHandler<Config, F>
where
    Config: DeserializeOwned,
    F: for<'a> FnMut(ConfigureContext<'a, State, Config>) -> ClientResult<()>,
{
    fn handle(
        &mut self,
        configure: &tau_proto::Configure,
        state: &mut State,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let config = match crate::config::parse_config::<Config>(&configure.config) {
            Ok(config) => config,
            Err(message) => {
                handle.config_error(message)?;
                return Ok(());
            }
        };
        let cx = ConfigureContext {
            state,
            config,
            configure,
            handle: handle.clone(),
        };
        if let Err(error) = (self.handler)(cx) {
            handle.config_error(error.to_string())?;
        }
        Ok(())
    }
}

/// Typed configuration handler implementation with an error hook.
pub(crate) struct TypedConfigureWithErrorHandler<State, Config, F> {
    /// User-provided configuration handler.
    handler: F,
    /// Optional hook run before `ConfigError` is emitted.
    error_handler: ConfigureErrorHandler<State>,
    /// Marker for the typed config payload.
    _config: std::marker::PhantomData<fn() -> Config>,
}

impl<State, Config, F> TypedConfigureWithErrorHandler<State, Config, F> {
    /// Creates a typed configuration handler wrapper.
    #[must_use]
    pub(crate) fn new(handler: F, error_handler: ConfigureErrorHandler<State>) -> Self {
        Self {
            handler,
            error_handler,
            _config: std::marker::PhantomData,
        }
    }

    /// Emits a config error after running any registered error hook.
    fn handle_error(
        &mut self,
        configure: &tau_proto::Configure,
        state: &mut State,
        handle: &ClientHandle,
        message: String,
    ) -> ClientResult<()> {
        (self.error_handler)(ConfigureErrorContext {
            state,
            configure,
            message: &message,
            handle: handle.clone(),
        });
        handle.config_error(message)
    }
}

impl<State, Config, F> ConfigureHandler<State> for TypedConfigureWithErrorHandler<State, Config, F>
where
    Config: DeserializeOwned,
    F: for<'a> FnMut(ConfigureContext<'a, State, Config>) -> ClientResult<()>,
{
    fn handle(
        &mut self,
        configure: &tau_proto::Configure,
        state: &mut State,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let config = match crate::config::parse_config::<Config>(&configure.config) {
            Ok(config) => config,
            Err(message) => {
                self.handle_error(configure, state, handle, message)?;
                return Ok(());
            }
        };
        let cx = ConfigureContext {
            state,
            config,
            configure,
            handle: handle.clone(),
        };
        if let Err(error) = (self.handler)(cx) {
            self.handle_error(configure, state, handle, error.to_string())?;
        }
        Ok(())
    }
}

/// Typed event handler implementation.
pub(crate) struct TypedEventHandler<Payload, F> {
    /// Whether replay-marked deliveries should be skipped.
    live_only: bool,
    /// User-provided event handler.
    handler: F,
    /// Marker for the typed event payload.
    _payload: std::marker::PhantomData<fn() -> Payload>,
}

impl<Payload, F> TypedEventHandler<Payload, F> {
    /// Creates a typed event handler wrapper.
    #[must_use]
    pub(crate) fn new(live_only: bool, handler: F) -> Self {
        Self {
            live_only,
            handler,
            _payload: std::marker::PhantomData,
        }
    }
}

impl<State, Payload, F> EventHandler<State> for TypedEventHandler<Payload, F>
where
    Payload: EventPayload,
    F: for<'a> FnMut(EventContext<'a, State, Payload>) -> ClientResult<()>,
{
    fn handle(
        &mut self,
        delivery: &tau_proto::EventDelivery,
        state: &mut State,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        if self.live_only && delivery.is_replay() {
            return Ok(());
        }
        let Some(event) = Payload::from_event(delivery.event()) else {
            return Ok(());
        };
        let cx = EventContext {
            state,
            event,
            replay: delivery.replay,
            recorded_at: delivery.recorded_at,
            handle: handle.clone(),
        };
        (self.handler)(cx)
    }
}

/// Raw event handler implementation.
pub(crate) struct TypedRawEventHandler<F> {
    /// Selector matched against delivered event names.
    selector: tau_proto::EventSelector,
    /// Whether replay-marked deliveries should be skipped.
    live_only: bool,
    /// User-provided event handler.
    handler: F,
}

impl<F> TypedRawEventHandler<F> {
    /// Creates a raw event handler wrapper.
    #[must_use]
    pub(crate) fn new(selector: tau_proto::EventSelector, live_only: bool, handler: F) -> Self {
        Self {
            selector,
            live_only,
            handler,
        }
    }
}

impl<State, F> RawEventHandler<State> for TypedRawEventHandler<F>
where
    F: for<'a> FnMut(RawEventContext<'a, State>) -> ClientResult<()>,
{
    fn handle(
        &mut self,
        delivery: &tau_proto::EventDelivery,
        state: &mut State,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        if self.live_only && delivery.is_replay() {
            return Ok(());
        }
        let event_name = delivery.event().name();
        let selected = match &self.selector {
            tau_proto::EventSelector::Exact(name) => event_name == *name,
            tau_proto::EventSelector::Prefix(prefix) => event_name.matches_prefix(prefix),
        };
        if !selected {
            return Ok(());
        }
        let cx = RawEventContext {
            state,
            delivery,
            handle: handle.clone(),
        };
        (self.handler)(cx)
    }
}

/// Live tool handler implementation.
pub(crate) struct NamedToolHandler<F> {
    /// Tool name this handler owns.
    tool_name: tau_proto::ToolName,
    /// User-provided tool handler.
    handler: F,
}

impl<F> NamedToolHandler<F> {
    /// Creates a live tool handler wrapper.
    #[must_use]
    pub(crate) fn new(tool_name: tau_proto::ToolName, handler: F) -> Self {
        Self { tool_name, handler }
    }
}

impl<State, F> ToolHandler<State> for NamedToolHandler<F>
where
    F: for<'a> FnMut(ToolContext<'a, State>) -> ClientResult<()>,
{
    fn handle(
        &mut self,
        invoke: &tau_proto::ToolStarted,
        state: &mut State,
        handle: &ClientHandle,
        stop_requested: &mut bool,
    ) -> ClientResult<()> {
        if invoke.tool_name != self.tool_name {
            return Ok(());
        }
        let cx = ToolContext {
            state,
            invoke,
            handle: handle.clone(),
            stop_requested,
        };
        (self.handler)(cx)
    }
}

/// Intercept handler implementation.
pub(crate) struct TypedInterceptHandler<F> {
    /// User-provided intercept handler.
    handler: F,
}

impl<F> TypedInterceptHandler<F> {
    /// Creates an intercept handler wrapper.
    #[must_use]
    pub(crate) fn new(handler: F) -> Self {
        Self { handler }
    }
}

impl<State, F> InterceptHandler<State> for TypedInterceptHandler<F>
where
    F: for<'a> FnMut(InterceptContext<'a, State>) -> ClientResult<InterceptDecision>,
{
    fn handle(
        &mut self,
        request: &tau_proto::InterceptRequest,
        state: &mut State,
        handle: &ClientHandle,
    ) -> ClientResult<InterceptDecision> {
        let cx = InterceptContext {
            state,
            request,
            handle: handle.clone(),
        };
        (self.handler)(cx)
    }
}
