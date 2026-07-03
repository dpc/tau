//! Reusable extension bootstrap helper.
//!
//! Every extension process opens its session with the same prelude:
//! `Hello` → optional `Subscribe` → optional `Intercept` → zero or more startup
//! `Emit` event requests → `Ready`, then flushes. The exact mix varies (some
//! extensions register tools, some intercept, some subscribe to several events,
//! some announce model state) but the order and the surrounding message-shaping
//! is fixed. Copy-pasting that sequence into every crate is mechanical and
//! drifts out of sync; this helper writes it once and lets each extension
//! declare only what differs.
//!
//! `Subscribe` semantics are uniform across UI clients and extensions: the
//! harness installs live routing, then catches the subscriber up with
//! current-state announcements plus selector-matched durable facts delivered
//! as replay-marked frames. Side-effecting extensions must check
//! [`tau_proto::EventDelivery::is_replay`] and skip historical frames instead
//! of relying on history being withheld.
//!
//! ```ignore
//! tau_extension::Handshake::tool("tau-ext-example")
//!     .subscribe([EventName::TOOL_STARTED])
//!     .register_tool(tool_spec())
//!     .ready_message("example tools ready")
//!     .run(&mut writer)?;
//! ```
//!
//! `Handshake` only writes peer-to-harness startup frames. It does not read or
//! apply harness-to-extension [`tau_proto::HarnessOutputMessage::Configure`]
//! messages. Extensions that accept configuration must parse and apply
//! `Configure.config` themselves (for example with [`crate::parse_config`]) and
//! send [`tau_proto::HarnessInputMessage::ConfigError`] for parse or apply
//! failures.
//!
//! `client_kind` defaults to [`ClientKind::Tool`] and
//! `protocol_version` to [`PROTOCOL_VERSION`] — every extension in
//! this workspace uses both, and they belong on the handshake, not
//! at each call site.

use std::fmt;
use std::io::Write;

use tau_proto::{
    ActionSchema, ActionSchemaPublished, ClientKind, EncodeError, Event, EventName, EventSelector,
    ExtensionName, HarnessInputMessage, Hello, Intercept, InterceptionPriority, PROTOCOL_VERSION,
    PeerOutputWriter, PromptFragment, Ready, Subscribe, ToolGroup, ToolRegister, ToolSpec,
};

/// Error returned while building or writing an extension handshake.
#[derive(Debug)]
pub enum HandshakeError {
    /// Encoding or flushing a startup frame failed.
    Encode(EncodeError),
    /// Multiple intercept selectors used different priorities.
    ///
    /// The harness stores one interceptor registration per extension
    /// connection, so one handshake can only send one priority.
    MixedInterceptPriorities {
        /// Priority already attached to this handshake's interceptor.
        existing: InterceptionPriority,
        /// Priority requested by the later selector.
        requested: InterceptionPriority,
    },
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode(error) => write!(f, "{error}"),
            Self::MixedInterceptPriorities {
                existing,
                requested,
            } => write!(
                f,
                "one extension handshake cannot register mixed interception priorities \
                 (existing {}, requested {})",
                existing.get(),
                requested.get()
            ),
        }
    }
}

impl std::error::Error for HandshakeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Encode(error) => Some(error),
            Self::MixedInterceptPriorities { .. } => None,
        }
    }
}

impl From<EncodeError> for HandshakeError {
    fn from(error: EncodeError) -> Self {
        Self::Encode(error)
    }
}

/// Builder for the opening message sequence an extension sends to the
/// harness. See the module-level documentation for a worked example.
#[must_use = "Handshake does nothing until `.run()` is called"]
pub struct Handshake {
    client_name: ExtensionName,
    client_kind: ClientKind,
    selectors: Vec<EventSelector>,
    intercept: Option<Intercept>,
    error: Option<HandshakeError>,
    tools: Vec<ToolRegister>,
    events: Vec<Event>,
    ready_message: Option<String>,
}

impl Handshake {
    /// Start a handshake for a tool-kind extension. The vast majority
    /// of extensions in this workspace are tools; use
    /// [`Handshake::with_kind`] for the rare exception.
    pub fn tool(client_name: impl Into<ExtensionName>) -> Self {
        Self::with_kind(client_name, ClientKind::Tool)
    }

    /// Start a handshake with an explicit `client_kind`.
    pub fn with_kind(client_name: impl Into<ExtensionName>, client_kind: ClientKind) -> Self {
        Self {
            client_name: client_name.into(),
            client_kind,
            selectors: Vec::new(),
            intercept: None,
            error: None,
            tools: Vec::new(),
            events: Vec::new(),
            ready_message: None,
        }
    }

    /// Subscribe to a set of events by exact name. Equivalent to extending
    /// the existing selectors with one `EventSelector::Exact` per item.
    ///
    /// Subscribing also requests subscribe-time catch-up: the harness sends
    /// current-state announcements and selector-matched durable facts (as
    /// replay-marked frames) before live delivery begins. Check
    /// [`tau_proto::EventDelivery::is_replay`] before performing side effects.
    pub fn subscribe(mut self, names: impl IntoIterator<Item = EventName>) -> Self {
        self.selectors
            .extend(names.into_iter().map(EventSelector::Exact));
        self
    }

    /// Append a pre-built `EventSelector` (e.g. `Prefix`).
    ///
    /// Like [`Handshake::subscribe`], this requests subscribe-time catch-up:
    /// the harness may send replay-marked frames before live delivery. Check
    /// [`tau_proto::EventDelivery::is_replay`] before performing side effects.
    pub fn subscribe_selector(mut self, selector: EventSelector) -> Self {
        self.selectors.push(selector);
        self
    }

    /// Intercept events matching `selector` at the given priority.
    ///
    /// Repeated calls with the same priority accumulate selectors into one wire
    /// `Intercept` message.
    ///
    /// If a later call uses a different priority, the handshake records a
    /// [`HandshakeError::MixedInterceptPriorities`] and [`Handshake::run`]
    /// returns it instead of panicking. Use [`Handshake::try_intercept`] to
    /// receive that error immediately while building the handshake.
    pub fn intercept(mut self, selector: EventSelector, priority: InterceptionPriority) -> Self {
        if self.error.is_none()
            && let Err(error) = self.add_intercept(selector, priority)
        {
            self.error = Some(error);
        }
        self
    }

    /// Try to intercept events matching `selector` at the given priority.
    ///
    /// Repeated same-priority calls accumulate selectors into one wire
    /// `Intercept` message. Mixed priorities return
    /// [`HandshakeError::MixedInterceptPriorities`] because the harness stores
    /// a single interceptor registration per extension connection.
    ///
    /// # Errors
    ///
    /// Returns [`HandshakeError::MixedInterceptPriorities`] when this handshake
    /// already has an interceptor priority and `priority` differs.
    pub fn try_intercept(
        mut self,
        selector: EventSelector,
        priority: InterceptionPriority,
    ) -> Result<Self, HandshakeError> {
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        self.add_intercept(selector, priority)?;
        Ok(self)
    }

    fn add_intercept(
        &mut self,
        selector: EventSelector,
        priority: InterceptionPriority,
    ) -> Result<(), HandshakeError> {
        match &mut self.intercept {
            Some(intercept) => {
                if intercept.priority != priority {
                    return Err(HandshakeError::MixedInterceptPriorities {
                        existing: intercept.priority,
                        requested: priority,
                    });
                }
                intercept.selectors.push(selector);
            }
            None => {
                self.intercept = Some(Intercept {
                    selectors: vec![selector],
                    priority,
                });
            }
        }
        Ok(())
    }

    /// Register a single tool without adding a prompt fragment.
    pub fn register_tool(self, tool: ToolSpec) -> Self {
        self.register_tool_with_prompt_fragment(tool, None)
    }

    /// Register a single tool and optionally attach a prompt fragment that
    /// the harness includes whenever the tool is enabled for the current role.
    pub fn register_tool_with_prompt_fragment(
        self,
        tool: ToolSpec,
        prompt_fragment: Option<PromptFragment>,
    ) -> Self {
        self.register_tool_with_group_and_prompt_fragment(tool, None, prompt_fragment)
    }

    /// Register a single grouped tool and optionally attach a tool-specific
    /// prompt fragment.
    pub fn register_tool_with_group_and_prompt_fragment(
        mut self,
        tool: ToolSpec,
        tool_group: Option<ToolGroup>,
        prompt_fragment: Option<PromptFragment>,
    ) -> Self {
        self.tools.push(ToolRegister {
            tool,
            tool_group,
            prompt_fragment,
        });
        self
    }

    /// Register multiple tools at once without adding prompt fragments.
    pub fn register_tools(mut self, tools: impl IntoIterator<Item = ToolSpec>) -> Self {
        self.tools
            .extend(tools.into_iter().map(|tool| ToolRegister {
                tool,
                tool_group: None,
                prompt_fragment: None,
            }));
        self
    }

    /// Announce an extension-provided action schema before `Ready`.
    ///
    /// The owner fields in the wire event are placeholders; the harness stamps
    /// the real extension name and instance id before broadcasting the schema.
    pub fn publish_actions(self, schema: ActionSchema) -> Self {
        self.announce_event(Event::ActionSchemaPublished(ActionSchemaPublished {
            extension_name: ExtensionName::default(),
            instance_id: 0.into(),
            schema,
        }))
    }

    /// Announce one startup event before the terminal `Ready` message.
    ///
    /// Use this for extension-owned state that the harness should activate when
    /// the handshake reaches `Ready`, such as `provider.models_updated`. Tool
    /// registrations should continue to use [`Handshake::register_tool`] so
    /// their intent stays clear.
    pub fn announce_event(mut self, event: Event) -> Self {
        self.events.push(event);
        self
    }

    /// Announce multiple startup events before the terminal `Ready` message.
    pub fn announce_events(mut self, events: impl IntoIterator<Item = Event>) -> Self {
        self.events.extend(events);
        self
    }

    /// Attach a human-readable message to the terminal `Ready` message.
    pub fn ready_message(mut self, message: impl Into<String>) -> Self {
        self.ready_message = Some(message.into());
        self
    }

    /// Write the full sequence (`Hello`, optional `Subscribe`, optional
    /// `Intercept`, startup event `Emit`s, `Ready`) and flush. Subscribe is
    /// omitted when no selectors have been added — sending an empty
    /// subscription would still be valid but adds noise on the wire.
    ///
    /// For extensions, `Subscribe` starts live delivery and may also send
    /// replay-marked catch-up frames; side-effecting extensions must check
    /// [`tau_proto::EventDelivery::is_replay`].
    ///
    /// # Errors
    ///
    /// Returns [`HandshakeError::MixedInterceptPriorities`] if chained
    /// [`Handshake::intercept`] calls used different priorities. Returns
    /// [`HandshakeError::Encode`] if writing or flushing the protocol frames
    /// fails.
    pub fn run<W: Write>(self, writer: &mut PeerOutputWriter<W>) -> Result<(), HandshakeError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        writer.write_message(&HarnessInputMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: self.client_name,
            client_kind: self.client_kind,
        }))?;
        if !self.selectors.is_empty() {
            writer.write_message(&HarnessInputMessage::Subscribe(Subscribe {
                selectors: self.selectors,
            }))?;
        }
        if let Some(intercept) = self.intercept {
            writer.write_message(&HarnessInputMessage::Intercept(intercept))?;
        }
        for tool in self.tools {
            writer.write_message(&HarnessInputMessage::emit(Event::ToolRegister(tool)))?;
        }
        for event in self.events {
            writer.write_message(&HarnessInputMessage::emit(event))?;
        }
        writer.write_message(&HarnessInputMessage::Ready(Ready {
            message: self.ready_message,
        }))?;
        writer.flush().map_err(EncodeError::Io)?;
        Ok(())
    }
}
