use serde::de::DeserializeOwned;

use crate::contexts::{
    ConfigureContext, ConfigureErrorContext, EventContext, InterceptContext, RawConfigureContext,
    RawEventContext, ToolContext,
};
use crate::event_payload::EventPayload;
use crate::handler::{
    ConfigureHandler, EventHandler, InterceptHandler, NamedToolHandler, RawConfigureHandler,
    RawEventHandler, ToolHandler, TypedConfigureHandler, TypedConfigureWithErrorHandler,
    TypedEventHandler, TypedInterceptHandler, TypedRawEventHandler,
};
use crate::{ClientError, ClientResult, ExtensionPlugin, InterceptDecision};

/// Builder used by a [`crate::TauExtension`] to declare startup frames and
/// handlers.
pub struct ExtensionBuilder<State> {
    /// Extension name used in the startup `Hello` frame.
    pub(crate) name: tau_proto::ExtensionName,
    /// Peer kind used in the startup `Hello` frame.
    pub(crate) kind: tau_proto::ClientKind,
    /// Event selectors sent in the optional startup `Subscribe` frame.
    pub(crate) selectors: Vec<tau_proto::EventSelector>,
    /// Whether to send `Subscribe` even when `selectors` is empty.
    pub(crate) force_subscribe: bool,
    /// Intercept declaration sent during startup.
    pub(crate) intercept: Option<tau_proto::Intercept>,
    /// Startup events emitted before `Ready`.
    pub(crate) startup_events: Vec<tau_proto::Event>,
    /// Human-readable message attached to `Ready`.
    pub(crate) ready_message: Option<String>,
    /// Typed configuration handlers.
    pub(crate) configure_handlers: Vec<Box<dyn ConfigureHandler<State>>>,
    /// Typed event handlers.
    pub(crate) event_handlers: Vec<Box<dyn EventHandler<State>>>,
    /// Raw event delivery handlers.
    pub(crate) raw_event_handlers: Vec<Box<dyn RawEventHandler<State>>>,
    /// Live tool handlers.
    pub(crate) tool_handlers: Vec<Box<dyn ToolHandler<State>>>,
    /// Intercept handler used for every intercept request.
    pub(crate) intercept_handler: Option<Box<dyn InterceptHandler<State>>>,
    /// Builder validation error detected during declaration.
    pub(crate) error: Option<ClientError>,
}

impl<State> ExtensionBuilder<State> {
    /// Creates an empty builder for one extension peer.
    #[must_use]
    pub fn new(name: impl Into<tau_proto::ExtensionName>, kind: tau_proto::ClientKind) -> Self {
        Self {
            name: name.into(),
            kind,
            selectors: Vec::new(),
            force_subscribe: false,
            intercept: None,
            startup_events: Vec::new(),
            ready_message: None,
            configure_handlers: Vec::new(),
            event_handlers: Vec::new(),
            raw_event_handlers: Vec::new(),
            tool_handlers: Vec::new(),
            intercept_handler: None,
            error: None,
        }
    }

    /// Adds exact event-name subscriptions to the startup `Subscribe` frame.
    pub fn subscribe(
        &mut self,
        names: impl IntoIterator<Item = tau_proto::EventName>,
    ) -> &mut Self {
        for name in names {
            self.add_selector(tau_proto::EventSelector::Exact(name));
        }
        self
    }

    /// Adds one custom event selector to the startup `Subscribe` frame.
    pub fn subscribe_selector(&mut self, selector: tau_proto::EventSelector) -> &mut Self {
        self.add_selector(selector);
        self
    }

    /// Forces an empty startup `Subscribe` frame when no selectors are
    /// registered.
    pub fn subscribe_empty(&mut self) -> &mut Self {
        self.force_subscribe = true;
        self
    }

    /// Emits one startup event before the terminal `Ready` frame.
    pub fn startup_event(&mut self, event: tau_proto::Event) -> &mut Self {
        self.startup_events.push(event);
        self
    }

    /// Attaches a human-readable message to the terminal `Ready` frame.
    pub fn ready_message(&mut self, message: impl Into<String>) -> &mut Self {
        self.ready_message = Some(message.into());
        self
    }

    /// Installs a reusable plugin into this builder.
    pub fn install<Plugin>(&mut self, plugin: Plugin) -> &mut Self
    where
        Plugin: ExtensionPlugin<State>,
    {
        plugin.register(self);
        self
    }

    /// Registers a typed configuration handler.
    ///
    /// Decode failures and handler application errors are reported to the
    /// harness as `ConfigError` frames. The runner then continues processing
    /// subsequent messages so an operator can correct configuration without
    /// restarting the extension.
    pub fn configure<Config>(
        &mut self,
        handler: impl for<'a> FnMut(ConfigureContext<'a, State, Config>) -> ClientResult<()> + 'static,
    ) -> &mut Self
    where
        Config: DeserializeOwned + 'static,
    {
        self.configure_handlers
            .push(Box::new(TypedConfigureHandler::<Config, _>::new(handler)));
        self
    }

    /// Registers an untyped configuration handler.
    ///
    /// Use this when extension lifecycle policy must inspect state before typed
    /// decoding. Returning an error emits exactly one `ConfigError` frame and
    /// the runner continues processing later messages, matching typed
    /// configuration application-error behavior.
    pub fn configure_raw(
        &mut self,
        handler: impl for<'a> FnMut(RawConfigureContext<'a, State>) -> ClientResult<()> + 'static,
    ) -> &mut Self {
        self.configure_handlers
            .push(Box::new(RawConfigureHandler::new(handler)));
        self
    }

    /// Registers a typed configuration handler with an error hook.
    ///
    /// The error hook runs before the runner emits `ConfigError` for either
    /// typed decode failures or handler application failures. Use it for
    /// fail-closed extensions that must clear active runtime state whenever a
    /// new configuration cannot be parsed or applied.
    pub fn configure_with_error<Config>(
        &mut self,
        handler: impl for<'a> FnMut(ConfigureContext<'a, State, Config>) -> ClientResult<()> + 'static,
        error_handler: impl for<'a> FnMut(ConfigureErrorContext<'a, State>) + 'static,
    ) -> &mut Self
    where
        Config: DeserializeOwned + 'static,
        State: 'static,
    {
        self.configure_handlers.push(Box::new(
            TypedConfigureWithErrorHandler::<State, Config, _>::new(
                handler,
                Box::new(error_handler),
            ),
        ));
        self
    }

    /// Registers a replay-aware typed event handler and subscribes to its
    /// event.
    pub fn on<Payload>(
        &mut self,
        handler: impl for<'a> FnMut(EventContext<'a, State, Payload>) -> ClientResult<()> + 'static,
    ) -> &mut Self
    where
        Payload: EventPayload + 'static,
    {
        self.subscribe([Payload::NAME]);
        self.event_handlers
            .push(Box::new(TypedEventHandler::<Payload, _>::new(
                false, handler,
            )));
        self
    }

    /// Registers a live-only typed event handler and subscribes to its event.
    pub fn on_live<Payload>(
        &mut self,
        handler: impl for<'a> FnMut(EventContext<'a, State, Payload>) -> ClientResult<()> + 'static,
    ) -> &mut Self
    where
        Payload: EventPayload + 'static,
    {
        self.subscribe([Payload::NAME]);
        self.event_handlers
            .push(Box::new(TypedEventHandler::<Payload, _>::new(
                true, handler,
            )));
        self
    }

    /// Registers a replay-aware raw event handler and subscribes with
    /// `selector`.
    ///
    /// Use this for event variants that do not yet have a built-in
    /// [`EventPayload`] implementation or when the handler needs the complete
    /// [`tau_proto::EventDelivery`] metadata.
    pub fn on_raw(
        &mut self,
        selector: tau_proto::EventSelector,
        handler: impl for<'a> FnMut(RawEventContext<'a, State>) -> ClientResult<()> + 'static,
    ) -> &mut Self {
        self.subscribe_selector(selector.clone());
        self.raw_event_handlers
            .push(Box::new(TypedRawEventHandler::new(
                selector, false, handler,
            )));
        self
    }

    /// Registers a live-only raw event handler and subscribes with `selector`.
    ///
    /// Replay-marked deliveries are skipped before the selector is evaluated.
    pub fn on_raw_live(
        &mut self,
        selector: tau_proto::EventSelector,
        handler: impl for<'a> FnMut(RawEventContext<'a, State>) -> ClientResult<()> + 'static,
    ) -> &mut Self {
        self.subscribe_selector(selector.clone());
        self.raw_event_handlers
            .push(Box::new(TypedRawEventHandler::new(selector, true, handler)));
        self
    }

    /// Registers one tool and a live dispatch handler for matching
    /// `tool.started` events.
    pub fn tool(
        &mut self,
        tool: tau_proto::ToolSpec,
        handler: impl for<'a> FnMut(ToolContext<'a, State>) -> ClientResult<()> + 'static,
    ) -> &mut Self {
        self.tool_with_group_and_prompt_fragment(tool, None, None, handler)
    }

    /// Registers one grouped tool and a live dispatch handler for matching tool
    /// calls.
    pub fn tool_with_group_and_prompt_fragment(
        &mut self,
        tool: tau_proto::ToolSpec,
        tool_group: Option<tau_proto::ToolGroup>,
        prompt_fragment: Option<tau_proto::PromptFragment>,
        handler: impl for<'a> FnMut(ToolContext<'a, State>) -> ClientResult<()> + 'static,
    ) -> &mut Self {
        self.subscribe([tau_proto::EventName::TOOL_STARTED]);
        self.tool_handlers
            .push(Box::new(NamedToolHandler::new(tool.name.clone(), handler)));
        self.startup_event(tau_proto::Event::ToolRegister(tau_proto::ToolRegister {
            tool,
            tool_group,
            prompt_fragment,
        }))
    }

    /// Registers one intercept selector and the handler for incoming intercept
    /// requests.
    ///
    /// If the handler returns an error, the runner first sends a no-op
    /// `InterceptReply` so the harness is not left waiting, then returns the
    /// handler error and stops this extension run.
    pub fn intercept(
        &mut self,
        selector: tau_proto::EventSelector,
        priority: tau_proto::InterceptionPriority,
        handler: impl for<'a> FnMut(InterceptContext<'a, State>) -> ClientResult<InterceptDecision>
        + 'static,
    ) -> &mut Self {
        match &mut self.intercept {
            Some(intercept) if intercept.priority == priority => intercept.selectors.push(selector),
            Some(intercept) => {
                if self.error.is_none() {
                    self.error = Some(ClientError::builder(format!(
                        "one extension cannot register mixed interception priorities (existing {}, requested {})",
                        intercept.priority.get(),
                        priority.get()
                    )));
                }
            }
            None => {
                self.intercept = Some(tau_proto::Intercept {
                    selectors: vec![selector],
                    priority,
                });
            }
        }
        if self.intercept_handler.is_some() {
            if self.error.is_none() {
                self.error = Some(ClientError::builder(
                    "only one intercept handler can be registered in this tau-client slice",
                ));
            }
        } else {
            self.intercept_handler = Some(Box::new(TypedInterceptHandler::new(handler)));
        }
        self
    }

    /// Converts any accumulated builder error into a result.
    pub(crate) fn validate(&mut self) -> ClientResult<()> {
        match self.error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Adds one startup subscription selector unless it is already present.
    fn add_selector(&mut self, selector: tau_proto::EventSelector) {
        if !self.selectors.contains(&selector) {
            self.selectors.push(selector);
        }
    }
}
