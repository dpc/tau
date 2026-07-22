use serde::de::DeserializeOwned;

use crate::contexts::{
    ActionContext, ConfigureContext, ConfigureErrorContext, EventContext, InterceptContext,
    RawConfigureContext, RawEventContext, ToolContext,
};
use crate::event_payload::EventPayload;
use crate::handler::{
    ActionHandler, ConfigureHandler, DeliveryPolicy, EventHandler, InterceptHandler,
    NamedActionHandler, NamedToolHandler, OutputMessageHandler, RawConfigureHandler,
    RawEventHandler, RawOutputMessageHandler, ToolHandler, TypedConfigureHandler,
    TypedConfigureWithErrorHandler, TypedEventHandler, TypedInterceptHandler, TypedRawEventHandler,
};
use crate::{ClientError, ClientResult, ExtensionPlugin, InterceptDecision};

/// Builder used by a [`crate::TauExtension`] to declare startup frames and
/// handlers.
pub struct ExtensionBuilder<State> {
    /// Extension name used in the startup `Hello` frame.
    pub(crate) name: tau_proto::ExtensionName,
    /// Peer kind used in the startup `Hello` frame.
    pub(crate) kind: tau_proto::ClientKind,
    /// Optional protocol authorities declared in the startup `Hello` frame.
    pub(crate) peer_capabilities: Vec<tau_proto::PeerCapability>,
    /// Historical event selectors sent in the optional startup `Subscribe`.
    pub(crate) historical_selectors: Vec<tau_proto::EventSelector>,
    /// Live event selectors sent in the optional startup `Subscribe` frame.
    pub(crate) live_selectors: Vec<tau_proto::EventSelector>,
    /// Whether to send `Subscribe` even when both selector sets are empty.
    pub(crate) force_subscribe: bool,
    /// Intercept declaration sent during startup.
    pub(crate) intercept: Option<tau_proto::Intercept>,
    /// Ordered startup declarations emitted before `Ready`.
    pub(crate) startup_events: Vec<StartupDeclaration>,
    /// Human-readable message attached to `Ready`.
    pub(crate) ready_message: Option<String>,
    /// Typed and raw configuration handlers.
    pub(crate) configure_handlers: Vec<Box<dyn ConfigureHandler<State>>>,
    /// Raw output handlers, including correlated non-event RPC results.
    pub(crate) output_message_handlers: Vec<Box<dyn OutputMessageHandler<State>>>,
    /// Typed event handlers.
    pub(crate) event_handlers: Vec<Box<dyn EventHandler<State>>>,
    /// Raw event delivery handlers.
    pub(crate) raw_event_handlers: Vec<Box<dyn RawEventHandler<State>>>,
    /// Live tool handlers.
    pub(crate) tool_handlers: Vec<Box<dyn ToolHandler<State>>>,
    /// Live action handlers.
    pub(crate) action_handlers: Vec<Box<dyn ActionHandler<State>>>,
    /// Intercept handler used for every intercept request.
    pub(crate) intercept_handler: Option<Box<dyn InterceptHandler<State>>>,
    /// Builder validation error detected during declaration.
    pub(crate) error: Option<ClientError>,
}

impl<State> ExtensionBuilder<State> {
    /// Observes every raw harness output, including correlated non-event RPC
    /// results.
    pub fn on_output_message<F>(&mut self, handler: F) -> &mut Self
    where
        F: FnMut(
                &tau_proto::HarnessOutputMessage,
                &mut State,
                &crate::ClientHandle,
            ) -> ClientResult<()>
            + 'static,
    {
        self.output_message_handlers
            .push(Box::new(RawOutputMessageHandler::new(handler)));
        self
    }

    /// Creates an empty builder for one extension peer.
    #[must_use]
    pub fn new(name: impl Into<tau_proto::ExtensionName>, kind: tau_proto::ClientKind) -> Self {
        Self {
            name: name.into(),
            kind,
            peer_capabilities: Vec::new(),
            historical_selectors: Vec::new(),
            live_selectors: Vec::new(),
            force_subscribe: false,
            intercept: None,
            startup_events: Vec::new(),
            ready_message: None,
            configure_handlers: Vec::new(),
            output_message_handlers: Vec::new(),
            event_handlers: Vec::new(),
            raw_event_handlers: Vec::new(),
            tool_handlers: Vec::new(),
            action_handlers: Vec::new(),
            intercept_handler: None,
            error: None,
        }
    }

    /// Declares that this extension publishes external-message reports.
    pub fn message_bridge(&mut self) -> &mut Self {
        if !self
            .peer_capabilities
            .contains(&tau_proto::PeerCapability::MessageBridge)
        {
            self.peer_capabilities
                .push(tau_proto::PeerCapability::MessageBridge);
        }
        self
    }

    /// Adds exact event-name subscriptions to the startup `Subscribe` frame.
    pub fn subscribe(
        &mut self,
        names: impl IntoIterator<Item = tau_proto::EventName>,
    ) -> &mut Self {
        for name in names {
            self.add_live_selector(tau_proto::EventSelector::Exact(name));
        }
        self
    }

    /// Adds one custom event selector to the startup `Subscribe` frame.
    pub fn subscribe_selector(&mut self, selector: tau_proto::EventSelector) -> &mut Self {
        self.add_live_selector(selector);
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
        self.startup_events
            .push(StartupDeclaration::Emit(tau_proto::Emit::new(event)));
        self
    }

    /// Emits one transient startup event before the terminal `Ready` frame.
    pub fn startup_transient_event(&mut self, event: tau_proto::Event) -> &mut Self {
        self.startup_events
            .push(StartupDeclaration::Emit(tau_proto::Emit::with_persist(
                event, false,
            )));
        self
    }

    /// Publishes an extension-provided action schema during startup.
    ///
    /// The owner fields in the startup event are placeholders; the harness
    /// stamps the real extension name and instance id before broadcasting the
    /// schema.
    pub fn publish_actions(&mut self, schema: tau_proto::ActionSchema) -> &mut Self {
        self.startup_event(tau_proto::Event::ActionSchemaPublished(
            tau_proto::ActionSchemaPublished {
                extension_name: tau_proto::ExtensionName::default(),
                instance_id: 0.into(),
                schema,
            },
        ))
    }

    /// Declares that this extension will publish per-agent context and later
    /// emit `extension.context_ready` at runtime.
    ///
    /// This is a startup publication helper only. The extension remains
    /// responsible for subscribing to lifecycle events, publishing context, and
    /// emitting runtime readiness events. The registration uses `persist=false`
    /// wire metadata.
    pub fn register_context_provider(&mut self) -> &mut Self {
        self.startup_transient_event(tau_proto::Event::ExtensionContextProviderRegister(
            tau_proto::ExtensionContextProviderRegister {},
        ))
    }

    /// Declares that this extension will publish session-wide context and later
    /// emit `extension.session_context_ready` at runtime.
    ///
    /// This is a startup publication helper only. Runtime session folding,
    /// context publication, and readiness events remain extension-owned. The
    /// registration uses `persist=false` wire metadata.
    pub fn register_session_context_provider(&mut self) -> &mut Self {
        self.startup_transient_event(tau_proto::Event::ExtensionSessionContextProviderRegister(
            tau_proto::ExtensionSessionContextProviderRegister {},
        ))
    }

    /// Publishes or replaces one extension-level prompt fragment during
    /// startup.
    ///
    /// This helper preserves normal tau-client startup staging: the fragment is
    /// emitted transiently before `Ready`, alongside other startup events.
    pub fn publish_prompt_fragment(&mut self, fragment: tau_proto::PromptFragment) -> &mut Self {
        self.startup_transient_event(tau_proto::Event::ExtPromptFragmentPublish(
            tau_proto::ExtPromptFragmentPublish { fragment },
        ))
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

    /// Registers a live typed event handler and subscribes to its event.
    ///
    /// The payload is added to `live_selectors`. Use [`Self::on_restore`] to
    /// receive historical replay for the same payload.
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
                DeliveryPolicy::LiveOnly,
                handler,
            )));
        self
    }

    /// Registers a typed restore handler and subscribes to historical catch-up.
    ///
    /// The handler runs only for replay-marked deliveries selected by
    /// `historical_selectors`, including durable restore facts and
    /// current-state catch-up snapshots. It does not run for live deliveries.
    pub fn on_restore<Payload>(
        &mut self,
        handler: impl for<'a> FnMut(EventContext<'a, State, Payload>) -> ClientResult<()> + 'static,
    ) -> &mut Self
    where
        Payload: EventPayload + 'static,
    {
        self.add_historical_selector(tau_proto::EventSelector::Exact(Payload::NAME));
        self.event_handlers
            .push(Box::new(TypedEventHandler::<Payload, _>::new(
                DeliveryPolicy::RestoreOnly,
                handler,
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
                DeliveryPolicy::LiveOnly,
                handler,
            )));
        self
    }

    /// Registers a live raw event handler and subscribes with `selector`.
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
                selector,
                DeliveryPolicy::LiveOnly,
                handler,
            )));
        self
    }

    /// Registers a raw restore handler and subscribes historically.
    ///
    /// The handler runs only for replay-marked deliveries selected by
    /// `historical_selectors`, including durable restore facts and
    /// current-state catch-up snapshots. It does not run for live deliveries.
    pub fn on_raw_restore(
        &mut self,
        selector: tau_proto::EventSelector,
        handler: impl for<'a> FnMut(RawEventContext<'a, State>) -> ClientResult<()> + 'static,
    ) -> &mut Self {
        self.add_historical_selector(selector.clone());
        self.raw_event_handlers
            .push(Box::new(TypedRawEventHandler::new(
                selector,
                DeliveryPolicy::RestoreOnly,
                handler,
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
        self.on_raw_routed_live(selector, handler)
    }

    /// Registers a replay-aware raw event handler without adding a startup
    /// subscription selector.
    ///
    /// Use this only for deliveries the harness routes to this peer because of
    /// some other protocol contract, such as provider-owned prompt deliveries.
    /// This avoids broadening startup subscriptions while still reusing
    /// tau-client's dispatch/replay machinery for routed events.
    pub fn on_raw_routed(
        &mut self,
        selector: tau_proto::EventSelector,
        handler: impl for<'a> FnMut(RawEventContext<'a, State>) -> ClientResult<()> + 'static,
    ) -> &mut Self {
        self.raw_event_handlers
            .push(Box::new(TypedRawEventHandler::new(
                selector,
                DeliveryPolicy::Any,
                handler,
            )));
        self
    }

    /// Registers a live-only raw event handler without adding a startup
    /// subscription selector.
    ///
    /// Replay-marked routed deliveries are skipped before the selector is
    /// evaluated. Use this for direct/routed effectful events where subscribing
    /// would request extra replay or broadcast traffic.
    pub fn on_raw_routed_live(
        &mut self,
        selector: tau_proto::EventSelector,
        handler: impl for<'a> FnMut(RawEventContext<'a, State>) -> ClientResult<()> + 'static,
    ) -> &mut Self {
        self.raw_event_handlers
            .push(Box::new(TypedRawEventHandler::new(
                selector,
                DeliveryPolicy::LiveOnly,
                handler,
            )));
        self
    }

    /// Declares one logical/local tool and registers a live dispatch handler
    /// for matching final `tool.started` events.
    ///
    /// The transient startup declaration is not an acceptance acknowledgement;
    /// if accepted, the harness publishes canonical `tool.register` after
    /// activation. Structural names are scoped after Configure.
    pub fn tool(
        &mut self,
        tool: tau_proto::ToolSpec,
        handler: impl for<'a> FnMut(ToolContext<'a, State>) -> ClientResult<()> + 'static,
    ) -> &mut Self {
        self.tool_with_group_and_prompt_fragment(tool, None, None, handler)
    }

    /// Declares one grouped logical/local tool and registers a live dispatch
    /// handler for matching final tool calls.
    ///
    /// The transient startup declaration is not an acceptance acknowledgement;
    /// if accepted, the harness publishes canonical `tool.register` after
    /// activation. Tool, alias, and group names are scoped after Configure.
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
        self.startup_transient_event(tau_proto::Event::ToolRegistrationDeclared(
            tau_proto::ToolRegistrationDeclared {
                tool,
                tool_group,
                prompt_fragment,
            },
        ))
    }

    /// Declares a tool whose arbitrary metadata needs explicit access to the
    /// immutable name scope.
    ///
    /// The factory returns a registration expressed in logical structural
    /// names; tau-client maps its internal name, visible alias, and group
    /// exactly once. The scope is intended for explicit wire references
    /// embedded in descriptions, schemas, or typed capability metadata.
    pub fn scoped_tool(
        &mut self,
        local_tool_name: tau_proto::ToolName,
        factory: impl FnOnce(&crate::ToolNameScope) -> ClientResult<tau_proto::ToolRegistrationDeclared>
        + 'static,
        handler: impl for<'a> FnMut(ToolContext<'a, State>) -> ClientResult<()> + 'static,
    ) -> &mut Self {
        self.subscribe([tau_proto::EventName::TOOL_STARTED]);
        self.tool_handlers.push(Box::new(NamedToolHandler::new(
            local_tool_name.clone(),
            handler,
        )));
        self.startup_events
            .push(StartupDeclaration::ScopedTool(ScopedToolDeclaration {
                local_tool_name,
                factory: Box::new(factory),
            }));
        self
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

    /// Registers one live action handler and subscribes to `action.invoke`.
    ///
    /// Replay-marked action deliveries are skipped before dispatch. The handler
    /// only runs when the action id matches this declaration; the harness owns
    /// extension/instance-level routing for action invocations.
    ///
    /// If the handler returns an error, the runner treats it as a fatal
    /// extension error. Action-domain failures should emit `ActionError` or
    /// `ActionResult` and return `Ok(())`.
    pub fn action(
        &mut self,
        action_id: impl Into<String>,
        handler: impl for<'a> FnMut(ActionContext<'a, State>) -> ClientResult<()> + 'static,
    ) -> &mut Self {
        self.subscribe([tau_proto::EventName::ACTION_INVOKE]);
        self.action_handlers
            .push(Box::new(NamedActionHandler::new(action_id.into(), handler)));
        self
    }

    /// Converts any accumulated builder error into a result.
    pub(crate) fn validate(&mut self) -> ClientResult<()> {
        match self.error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Validates that this builder can be used with deferred manual startup.
    pub(crate) fn validate_deferred_startup(&self) -> ClientResult<()> {
        if self.force_subscribe
            || !self.historical_selectors.is_empty()
            || !self.live_selectors.is_empty()
            || self.intercept.is_some()
            || !self.startup_events.is_empty()
            || self.ready_message.is_some()
        {
            return Err(ClientError::builder(
                "deferred manual startup cannot use static startup declarations; send dynamic startup frames explicitly before Ready",
            ));
        }
        Ok(())
    }

    /// Apply the immutable name scope to every structural startup declaration
    /// and tool dispatch key.
    pub(crate) fn apply_tool_name_scope(
        &mut self,
        scope: &crate::ToolNameScope,
    ) -> ClientResult<()> {
        for handler in &mut self.tool_handlers {
            handler.apply_name_scope(scope)?;
        }
        let declarations = std::mem::take(&mut self.startup_events);
        for declaration in declarations {
            let emit = match declaration {
                StartupDeclaration::Emit(mut emit) => {
                    if let tau_proto::Event::ToolRegistrationDeclared(registration) =
                        emit.event.as_mut()
                    {
                        *registration = scope.scope_registration(registration.clone())?;
                    }
                    emit
                }
                StartupDeclaration::ScopedTool(declaration) => {
                    let registration = (declaration.factory)(scope)?;
                    if registration.tool.name != declaration.local_tool_name {
                        return Err(ClientError::builder(format!(
                            "scoped tool factory declared `{}` but returned `{}`",
                            declaration.local_tool_name, registration.tool.name
                        )));
                    }
                    tau_proto::Emit::with_persist(
                        tau_proto::Event::ToolRegistrationDeclared(
                            scope.scope_registration(registration)?,
                        ),
                        false,
                    )
                }
            };
            self.startup_events.push(StartupDeclaration::Emit(emit));
        }
        Ok(())
    }

    /// Adds one startup subscription selector unless it is already present.
    fn add_live_selector(&mut self, selector: tau_proto::EventSelector) {
        if !self.live_selectors.contains(&selector) {
            self.live_selectors.push(selector);
        }
    }

    /// Adds one startup historical selector unless it is already present.
    fn add_historical_selector(&mut self, selector: tau_proto::EventSelector) {
        if !self.historical_selectors.contains(&selector) {
            self.historical_selectors.push(selector);
        }
    }
}

/// Delayed local registration factory used when arbitrary metadata embeds wire
/// tool references.
type ScopedToolFactory = dyn FnOnce(&crate::ToolNameScope) -> ClientResult<tau_proto::ToolRegistrationDeclared>
    + 'static;

/// Delayed logical registration paired with its handler's dispatch name.
pub(crate) struct ScopedToolDeclaration {
    /// Logical name used by the dispatch handler.
    local_tool_name: tau_proto::ToolName,
    /// Factory evaluated after the immutable scope is established.
    factory: Box<ScopedToolFactory>,
}

/// One startup event or delayed tool declaration in public call order.
pub(crate) enum StartupDeclaration {
    Emit(tau_proto::Emit),
    ScopedTool(ScopedToolDeclaration),
}
