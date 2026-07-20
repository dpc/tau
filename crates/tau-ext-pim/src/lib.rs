//! Standard personal information management extension.
//!
//! The extension exposes split email and calendar command tools while keeping
//! shared configuration, approval, and runtime boundaries inside one extension.
//! Component, storage, provider, and secret-handling boundaries are summarized
//! in `ARCH-tau-ext-pim`.

use std::error::Error;
use std::io::{Read, Write};
use std::rc::Rc;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use tau_proto::{ActionArg, ActionArgKind, ActionChoice, ActionSchema, CborValue, Event};

pub mod calendar;
pub mod email;
mod google_oauth;
mod opaque_id;
mod storage;

/// Maximum current account names offered at one auth argument position.
///
/// Each inventory appears on both `start` and `finish`, and PIM publishes email
/// plus calendar together, so this is deliberately below the per-list action
/// schema maximum to preserve the aggregate schema text budget.
const AUTH_ACCOUNT_COMPLETION_LIMIT: usize = 64;

/// Maximum account names repeated in a missing-argument diagnostic.
///
/// Completion choices have their separate schema-owned bound. Keeping
/// diagnostics shorter avoids turning a large configured inventory into an
/// oversized notice.
const AUTH_ACCOUNT_DIAGNOSTIC_LIMIT: usize = 6;

/// `tracing` target for extension-level events emitted by the PIM wrapper.
pub const LOG_TARGET: &str = "pim";

/// Run the extension over stdio.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_client::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

/// Run the extension over the supplied reader/writer pair.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let mut runtime = tau_client::TauExtensionRunner::new(PimExtension)
        .start_manual_loop_with_extension_data_state(reader, writer, |_, client| RuntimeState {
            storage: Some(Rc::new(storage::RpcStorage::new(
                tau_proto::ExtensionDataScope::User,
                client,
            ))),
            ..RuntimeState::default()
        })?;
    loop {
        match runtime.recv()? {
            tau_client::ManualRuntimeInput::Message(message) => {
                match runtime.dispatch_one(message)? {
                    tau_client::DispatchOutcome::Continue => {}
                    tau_client::DispatchOutcome::Disconnect(_) => {
                        let _ = runtime.finish_detached();
                        return Ok(());
                    }
                    tau_client::DispatchOutcome::StopRequested => break,
                }
            }
            tau_client::ManualRuntimeInput::Timeout => {}
            tau_client::ManualRuntimeInput::InputClosed => break,
        }
    }
    runtime.finish()?;
    Ok(())
}

/// Combined state for the top-level PIM runtime.
#[derive(Default)]
struct RuntimeState {
    /// Runtime state for email tools and actions.
    email: email::RuntimeState,
    /// Runtime state for calendar tools and actions.
    calendar: calendar::RuntimeState,
    /// Shared storage backend installed after tau-client startup.
    storage: Option<storage::SharedStorage>,
}

/// Top-level configuration shape that can configure email and calendar modules.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PimExtensionConfig {
    /// Email module configuration, or defaults when omitted.
    email: Option<email::EmailExtensionConfig>,
    /// Calendar module configuration, or defaults when omitted.
    calendar: Option<calendar::CalendarExtensionConfig>,
}

impl RuntimeState {
    fn configure(
        &mut self,
        configure: tau_proto::Configure,
        storage: storage::SharedStorage,
    ) -> Result<(), String> {
        let result = match parse_config::<PimExtensionConfig>(&configure.config) {
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

    fn initial_tool_progress(
        &self,
        invoke: &tau_proto::ToolStarted,
        local_tool_name: &tau_proto::ToolName,
    ) -> Option<tau_proto::ToolProgress> {
        match local_tool_name.as_str() {
            name if email::is_tool_name(name) => Some(tau_proto::ToolProgress {
                call_id: invoke.call_id.clone(),
                tool_name: invoke.tool_name.clone(),
                message: None,
                progress: None,
                display: Some(email::initial_display_for_tool(
                    local_tool_name.as_str(),
                    &invoke.arguments,
                )),
            }),
            name if calendar::is_tool_name(name) => {
                let mut local_invoke = invoke.clone();
                local_invoke.tool_name = local_tool_name.clone();
                let mut progress = calendar::initial_progress(&local_invoke);
                progress.tool_name = invoke.tool_name.clone();
                Some(progress)
            }
            _ => None,
        }
    }

    fn dispatch_tool(
        &mut self,
        invoke: tau_proto::ToolStarted,
        local_tool_name: &tau_proto::ToolName,
    ) -> Option<Event> {
        let wire_tool_name = invoke.tool_name.clone();
        let mut local_invoke = invoke;
        local_invoke.tool_name = local_tool_name.clone();
        let event = match local_tool_name.as_str() {
            name if email::is_tool_name(name) => Some(self.email.dispatch(local_invoke)),
            name if calendar::is_tool_name(name) => Some(self.calendar.dispatch(local_invoke)),
            _ => None,
        };
        event.map(|mut event| {
            match &mut event {
                Event::ToolResult(result) => result.tool_name = wire_tool_name,
                Event::ToolError(error) => error.tool_name = wire_tool_name,
                _ => {}
            }
            event
        })
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

    /// Build the action schema from the effective, validated module
    /// inventories.
    fn action_schema(&self) -> ActionSchema {
        action_schema_with_accounts(
            &self.email.google_auth_account_ids(),
            &self.calendar.google_auth_account_ids(),
        )
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

/// Decode harness-provided configuration CBOR into a typed config shape.
///
/// The error text preserves the human-readable serde diagnostic used in
/// `ConfigError` output, matching the former startup-helper parse behavior
/// without keeping that helper crate as a dependency.
pub(crate) fn parse_config<C: DeserializeOwned>(value: &CborValue) -> Result<C, String> {
    value.deserialized().map_err(|e| match e {
        ciborium::value::Error::Custom(message) => message,
    })
}

/// Top-level tau-client extension declaration for the combined PIM runtime.
struct PimExtension;

impl tau_client::TauExtension for PimExtension {
    type State = RuntimeState;

    fn name(&self) -> &'static str {
        "tau-ext-pim"
    }

    fn register(self, builder: &mut tau_client::ExtensionBuilder<Self::State>) {
        register_tools_with_prompt_fragment(
            builder,
            email::email_tool_specs(),
            tau_proto::ToolGroupName::new("email"),
            "email_read",
            email::email_prompt_fragment(),
        );
        register_tools_with_prompt_fragment(
            builder,
            calendar::calendar_tool_specs(),
            tau_proto::ToolGroupName::new("calendar"),
            "calendar_get",
            calendar::calendar_prompt_fragment(),
        );
        builder
            .ready_message("pim extension ready")
            .configure_raw(|cx| {
                let storage = cx
                    .state
                    .storage
                    .as_ref()
                    .cloned()
                    .ok_or_else(|| tau_client::ClientError::handler("pim storage not ready"))?;
                cx.state
                    .configure(cx.configure.clone(), storage)
                    .map_err(tau_client::ClientError::handler)?;
                cx.handle.emit(Event::ActionSchemaPublished(
                    tau_proto::ActionSchemaPublished {
                        extension_name: tau_proto::ExtensionName::default(),
                        instance_id: 0.into(),
                        schema: cx.state.action_schema(),
                    },
                ))
            })
            .on_raw_live(
                tau_proto::EventSelector::Exact(tau_proto::EventName::ACTION_INVOKE),
                |cx| {
                    let Event::ActionInvoke(invoke) = cx.event().clone() else {
                        return Ok(());
                    };
                    let event = cx.state.dispatch_action(invoke);
                    cx.handle.emit(event)
                },
            );
    }
}

fn register_tools_with_prompt_fragment(
    builder: &mut tau_client::ExtensionBuilder<RuntimeState>,
    tools: Vec<tau_proto::ToolSpec>,
    group_name: tau_proto::ToolGroupName,
    prompt_tool_name: &str,
    prompt_fragment: tau_proto::PromptFragment,
) {
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
        builder.tool_with_group_and_prompt_fragment(
            tool,
            Some(tool_group.clone()),
            prompt_fragment,
            |cx| {
                let local_tool_name = cx.local_tool_name().clone();
                if let Some(progress) = cx.state.initial_tool_progress(cx.invoke, &local_tool_name)
                {
                    cx.handle.report_tool_progress(progress)?;
                }
                if let Some(event) = cx.state.dispatch_tool(cx.invoke.clone(), &local_tool_name) {
                    cx.handle.emit(event)?;
                }
                Ok(())
            },
        );
    }
}

/// Build the combined action schema with current interactive Google OAuth
/// account inventories.
fn action_schema_with_accounts(
    email_accounts: &[String],
    calendar_accounts: &[String],
) -> ActionSchema {
    let mut schema = ActionSchema {
        version: tau_proto::ACTION_SCHEMA_VERSION,
        roots: Vec::new(),
    };
    schema
        .roots
        .extend(email::email_action_schema_with_accounts(email_accounts).roots);
    schema
        .roots
        .extend(calendar::calendar_action_schema_with_accounts(calendar_accounts).roots);
    schema
}

/// Build one open-ended account argument with bounded authoritative
/// suggestions.
fn google_auth_account_arg(account_kind: &str, accounts: &[String]) -> ActionArg {
    let mut accounts = accounts.to_vec();
    accounts.sort();
    accounts.dedup();

    let description = if accounts.is_empty() {
        format!(
            "{account_kind} account id; no accounts are available for interactive Google authorization"
        )
    } else {
        let shown = accounts
            .iter()
            .take(AUTH_ACCOUNT_DIAGNOSTIC_LIMIT)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        if accounts.len() <= AUTH_ACCOUNT_DIAGNOSTIC_LIMIT {
            format!("{account_kind} account id; available account names: {shown}")
        } else {
            format!(
                "{account_kind} account id; available account names (showing {} of {}): {shown}",
                AUTH_ACCOUNT_DIAGNOSTIC_LIMIT,
                accounts.len()
            )
        }
    };
    let suggestions = accounts
        .into_iter()
        .take(AUTH_ACCOUNT_COMPLETION_LIMIT)
        .map(|value| ActionChoice {
            value,
            description: format!("{account_kind} account"),
        })
        .collect();
    ActionArg {
        name: "account".to_owned(),
        description,
        required: true,
        suggestions,
        kind: ActionArgKind::String,
    }
}

/// Return whether an account id can be inserted exactly as one bounded, safe
/// slash-action token.
fn is_safe_action_account_id(id: &str) -> bool {
    id.len() <= tau_proto::MAX_ACTION_TOKEN_BYTES && id.chars().all(|ch| ch.is_ascii_graphic())
}

#[cfg(test)]
mod tests;
