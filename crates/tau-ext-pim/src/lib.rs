//! Standard personal information management extension.
//!
//! The extension exposes split email and calendar command tools while keeping
//! shared configuration, approval, and runtime boundaries inside one extension.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};
use std::rc::Rc;

use serde::Deserialize;
use tau_proto::{
    ActionSchema, CborValue, ConfigError, Event, HarnessInputMessage, HarnessOutputMessage,
    PeerInputReader, PeerOutputWriter,
};

pub mod calendar;
pub mod email;
mod google_oauth;
mod opaque_id;
mod storage;

/// `tracing` target for extension-level events emitted by the PIM wrapper.
pub const LOG_TARGET: &str = "pim";

/// Run the extension over stdio.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_extension::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

/// Run the extension over the supplied reader/writer pair.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read + 'static,
    W: Write + 'static,
{
    let reader = Rc::new(RefCell::new(PeerInputReader::new(BufReader::new(reader))));
    let writer = Rc::new(RefCell::new(PeerOutputWriter::new(BufWriter::new(writer))));
    let pending = Rc::new(RefCell::new(VecDeque::new()));
    let storage: storage::SharedStorage = Rc::new(storage::RpcStorage::new(
        tau_proto::ExtensionDataScope::User,
        Rc::clone(&reader),
        Rc::clone(&writer),
        Rc::clone(&pending),
    ));
    let mut runtime = RuntimeState::default();

    let handshake = tau_extension::Handshake::tool("tau-ext-pim").subscribe([
        tau_proto::EventName::TOOL_STARTED,
        tau_proto::EventName::ACTION_INVOKE,
    ]);
    let handshake = register_tools_with_prompt_fragment(
        handshake,
        email::email_tool_specs(),
        tau_proto::ToolGroupName::new("email"),
        "email_read",
        email::email_prompt_fragment(),
    );
    let handshake = register_tools_with_prompt_fragment(
        handshake,
        calendar::calendar_tool_specs(),
        tau_proto::ToolGroupName::new("calendar"),
        "calendar_get",
        calendar::calendar_prompt_fragment(),
    );

    handshake
        .publish_actions(action_schema())
        .ready_message("pim extension ready")
        .run(&mut writer.borrow_mut())?;

    loop {
        let message = if let Some(message) = pending.borrow_mut().pop_front() {
            Some(message)
        } else {
            reader.borrow_mut().read_message()?
        };
        let Some(message) = message else { break };
        if !handle_message(&mut runtime, Rc::clone(&storage), message, &writer)? {
            break;
        }
    }
    Ok(())
}

#[derive(Default)]
struct RuntimeState {
    email: email::RuntimeState,
    calendar: calendar::RuntimeState,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PimExtensionConfig {
    email: Option<email::EmailExtensionConfig>,
    calendar: Option<calendar::CalendarExtensionConfig>,
}

impl RuntimeState {
    fn configure(
        &mut self,
        configure: tau_proto::Configure,
        storage: storage::SharedStorage,
    ) -> Result<(), String> {
        let result = match tau_extension::parse_config::<PimExtensionConfig>(&configure.config) {
            Ok(pim) => self.configure_pim(pim, configure, storage),
            Err(message) if has_pim_module_keys(&configure.config) => Err(message),
            Err(_) => {
                let calendar_secrets = configure.secrets.clone();
                let calendar_state_dir = configure.state_dir.clone();
                match self.email.configure(configure, Rc::clone(&storage)) {
                    Ok(()) => self.calendar.configure_with_config(
                        calendar::CalendarExtensionConfig::default(),
                        calendar_state_dir,
                        calendar_secrets,
                        storage,
                    ),
                    Err(message) => Err(message),
                }
            }
        };
        if let Err(message) = &result {
            self.reject_modules(message.clone());
        }
        result
    }

    fn configure_pim(
        &mut self,
        pim: PimExtensionConfig,
        configure: tau_proto::Configure,
        storage: storage::SharedStorage,
    ) -> Result<(), String> {
        let email_config = pim.email.unwrap_or_default();
        let calendar_config = pim.calendar.unwrap_or_default();
        calendar_config.clone().validate()?;
        self.email.configure_with_config(
            email_config,
            configure.state_dir.clone(),
            configure.secrets.clone(),
            Rc::clone(&storage),
        )?;
        self.calendar.configure_with_config(
            calendar_config,
            configure.state_dir,
            configure.secrets,
            storage,
        )
    }

    fn reject_modules(&mut self, reason: String) {
        self.email.reject(reason.clone());
        self.calendar.reject(reason);
    }

    fn initial_tool_progress(&self, invoke: &tau_proto::ToolStarted) -> Option<Event> {
        match invoke.tool_name.as_str() {
            name if email::is_tool_name(name) => {
                Some(Event::ToolProgress(tau_proto::ToolProgress {
                    call_id: invoke.call_id.clone(),
                    tool_name: invoke.tool_name.clone(),
                    message: None,
                    progress: None,
                    display: Some(email::initial_display_for_tool(
                        invoke.tool_name.as_str(),
                        &invoke.arguments,
                    )),
                }))
            }
            name if calendar::is_tool_name(name) => Some(calendar::initial_progress(invoke)),
            _ => None,
        }
    }

    fn dispatch_tool(&mut self, invoke: tau_proto::ToolStarted) -> Option<Event> {
        match invoke.tool_name.as_str() {
            name if email::is_tool_name(name) => Some(self.email.dispatch(invoke)),
            name if calendar::is_tool_name(name) => Some(self.calendar.dispatch(invoke)),
            _ => None,
        }
    }

    fn dispatch_action(&mut self, invoke: tau_proto::ActionInvoke) -> Event {
        if invoke.action_id.starts_with("email.") {
            self.email.dispatch_action(invoke)
        } else if invoke.action_id.starts_with("calendar.") {
            self.calendar.dispatch_action(invoke)
        } else {
            Event::ActionError(tau_proto::ActionError {
                invocation_id: invoke.invocation_id,
                action_id: invoke.action_id,
                message: "unknown pim action".to_owned(),
                details: None,
            })
        }
    }
}

fn has_pim_module_keys(config: &CborValue) -> bool {
    let CborValue::Map(entries) = config else {
        return false;
    };
    entries.iter().any(|(key, _)| match key {
        CborValue::Text(key) => key == "email" || key == "calendar",
        _ => false,
    })
}
fn register_tools_with_prompt_fragment(
    mut handshake: tau_extension::Handshake,
    tools: Vec<tau_proto::ToolSpec>,
    group_name: tau_proto::ToolGroupName,
    prompt_tool_name: &str,
    prompt_fragment: tau_proto::PromptFragment,
) -> tau_extension::Handshake {
    let tool_group = tau_proto::ToolGroup {
        name: group_name,
        prompt_fragment: Some(prompt_fragment.clone()),
    };
    for tool in tools {
        let prompt_fragment = if tool.name.as_str() == prompt_tool_name {
            Some(prompt_fragment.clone())
        } else {
            None
        };
        handshake = handshake.register_tool_with_group_and_prompt_fragment(
            tool,
            Some(tool_group.clone()),
            prompt_fragment,
        );
    }
    handshake
}

fn action_schema() -> ActionSchema {
    let mut schema = ActionSchema {
        version: tau_proto::ACTION_SCHEMA_VERSION,
        roots: Vec::new(),
    };
    schema.roots.extend(email::email_action_schema().roots);
    schema
        .roots
        .extend(calendar::calendar_action_schema().roots);
    schema
}

fn handle_message<W: Write>(
    runtime: &mut RuntimeState,
    storage: storage::SharedStorage,
    message: HarnessOutputMessage,
    writer: &Rc<RefCell<PeerOutputWriter<BufWriter<W>>>>,
) -> Result<bool, Box<dyn Error>> {
    match message {
        HarnessOutputMessage::Configure(configure) => {
            if let Err(message) = runtime.configure(configure, storage) {
                writer
                    .borrow_mut()
                    .write_message(&HarnessInputMessage::ConfigError(ConfigError { message }))?;
                writer.borrow_mut().flush()?;
            }
        }
        HarnessOutputMessage::Deliver(delivery) => {
            // Tool/action invocations are execution triggers; replay-marked
            // frames re-send history and must not re-run them.
            if delivery.is_replay() {
                return Ok(true);
            }
            match delivery.into_event() {
                Event::ToolStarted(invoke) => handle_tool_started(runtime, invoke, writer)?,
                Event::ActionInvoke(invoke) => {
                    let event = runtime.dispatch_action(invoke);
                    writer
                        .borrow_mut()
                        .write_message(&HarnessInputMessage::emit(event))?;
                    writer.borrow_mut().flush()?;
                }
                _ => {}
            }
        }
        HarnessOutputMessage::Disconnect(_) => return Ok(false),
        _ => {}
    }
    Ok(true)
}

fn handle_tool_started<W: Write>(
    runtime: &mut RuntimeState,
    invoke: tau_proto::ToolStarted,
    writer: &Rc<RefCell<PeerOutputWriter<BufWriter<W>>>>,
) -> Result<(), Box<dyn Error>> {
    if let Some(progress) = runtime.initial_tool_progress(&invoke) {
        writer
            .borrow_mut()
            .write_message(&HarnessInputMessage::emit(progress))?;
        writer.borrow_mut().flush()?;
    }
    if let Some(event) = runtime.dispatch_tool(invoke) {
        writer
            .borrow_mut()
            .write_message(&HarnessInputMessage::emit(event))?;
        writer.borrow_mut().flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
