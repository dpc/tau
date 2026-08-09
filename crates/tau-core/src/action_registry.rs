//! Registry for extension-provided UI actions.
//!
//! The harness owns routing and pending-invocation privacy; this registry keeps
//! the live schema snapshot and resolves `action.invoke` owner tuples to the
//! extension connection that published them.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use tau_proto::{ActionInvoke, CborValue, ConnectionId, ExtensionInstanceId, ExtensionName};

/// One harness-stamped schema provider currently known to the registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionProviderSchema {
    /// Connection id that published this schema.
    pub connection_id: ConnectionId,
    /// Extension name stamped by the harness.
    pub extension_name: ExtensionName,
    /// Extension instance id stamped by the harness.
    pub instance_id: ExtensionInstanceId,
    /// Validated action schema.
    pub schema: tau_actions::ActionSchema,
}

/// Error returned when registering an invalid action schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionRegistryError {
    message: String,
}

impl ActionRegistryError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Human-readable registry failure.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ActionRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ActionRegistryError {}

/// Error returned when an action invocation cannot be routed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActionRouteError {
    /// No live extension owns the requested action tuple.
    NoProvider {
        /// Extension name in the request.
        extension_name: ExtensionName,
        /// Extension instance id in the request.
        instance_id: ExtensionInstanceId,
        /// Stable action id in the request.
        action_id: String,
    },
    /// The invocation payload does not match the registered schema.
    InvalidInvocation {
        /// Human-readable validation failure.
        reason: String,
    },
}

impl fmt::Display for ActionRouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoProvider {
                extension_name,
                instance_id,
                action_id,
            } => write!(
                f,
                "no live provider for action {extension_name}#{instance_id}:{action_id}"
            ),
            Self::InvalidInvocation { reason } => write!(f, "invalid action invocation: {reason}"),
        }
    }
}

impl std::error::Error for ActionRouteError {}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ActionRouteKey {
    extension_name: ExtensionName,
    instance_id: ExtensionInstanceId,
    action_id: String,
}

impl ActionRouteKey {
    fn new(
        extension_name: ExtensionName,
        instance_id: ExtensionInstanceId,
        action_id: String,
    ) -> Self {
        Self {
            extension_name,
            instance_id,
            action_id,
        }
    }

    fn from_invoke(invoke: &ActionInvoke) -> Self {
        Self::new(
            invoke.extension_name.clone(),
            invoke.instance_id,
            invoke.action_id.clone(),
        )
    }
}

/// Live extension action schemas and route table.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ActionRegistry {
    schemas_by_connection: HashMap<ConnectionId, ActionProviderSchema>,
    routes: HashMap<ActionRouteKey, ConnectionId>,
}

impl ActionRegistry {
    /// Create an empty action registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register or replace the schema for one extension connection.
    pub fn register_schema(
        &mut self,
        connection_id: &ConnectionId,
        extension_name: ExtensionName,
        instance_id: ExtensionInstanceId,
        schema: tau_actions::ActionSchema,
    ) -> Result<(), ActionRegistryError> {
        let action_ids = schema
            .executable_action_ids()
            .map_err(|error| ActionRegistryError::new(format!("invalid action schema: {error}")))?;
        let route_keys = action_ids
            .into_iter()
            .map(|action_id| ActionRouteKey::new(extension_name.clone(), instance_id, action_id))
            .collect::<Vec<_>>();
        for key in &route_keys {
            if let Some(owner) = self.routes.get(key)
                && owner != connection_id
            {
                return Err(ActionRegistryError::new(format!(
                    "action route collision for {}#{}:{} already owned by {}",
                    key.extension_name, key.instance_id, key.action_id, owner
                )));
            }
        }
        self.unregister_connection(connection_id);

        for key in route_keys {
            self.routes.insert(key, connection_id.clone());
        }
        self.schemas_by_connection.insert(
            connection_id.clone(),
            ActionProviderSchema {
                connection_id: connection_id.clone(),
                extension_name,
                instance_id,
                schema,
            },
        );
        Ok(())
    }

    /// Remove any schema and actions owned by one connection.
    pub fn unregister_connection(
        &mut self,
        connection_id: &ConnectionId,
    ) -> Option<ActionProviderSchema> {
        let removed = self.schemas_by_connection.remove(connection_id)?;
        self.routes.retain(|_, provider| provider != connection_id);
        Some(removed)
    }

    /// Resolve an action invocation to the owning extension connection.
    pub fn route_action_invoke(
        &self,
        invoke: &ActionInvoke,
    ) -> Result<ConnectionId, ActionRouteError> {
        let key = ActionRouteKey::from_invoke(invoke);
        let provider =
            self.routes
                .get(&key)
                .cloned()
                .ok_or_else(|| ActionRouteError::NoProvider {
                    extension_name: invoke.extension_name.clone(),
                    instance_id: invoke.instance_id,
                    action_id: invoke.action_id.clone(),
                })?;
        let schema = self.schemas_by_connection.get(&provider).ok_or_else(|| {
            ActionRouteError::InvalidInvocation {
                reason: format!("missing schema for provider {provider}"),
            }
        })?;
        validate_invoke_against_schema(invoke, &schema.schema)?;
        Ok(provider)
    }

    /// Return current schemas in deterministic order for late-join replay.
    #[must_use]
    pub fn published_schemas(&self) -> Vec<ActionProviderSchema> {
        let mut by_key = BTreeMap::new();
        for schema in self.schemas_by_connection.values() {
            by_key.insert(
                (
                    schema.extension_name.to_string(),
                    schema.instance_id.get(),
                    schema.connection_id.to_string(),
                ),
                schema.clone(),
            );
        }
        by_key.into_values().collect()
    }

    /// Return true when the connection currently has a published schema.
    #[must_use]
    pub fn has_schema_for_connection(&self, connection_id: &ConnectionId) -> bool {
        self.schemas_by_connection.contains_key(connection_id)
    }

    /// Return the current schema owned by one exact live connection.
    #[must_use]
    pub fn schema_for_connection(
        &self,
        connection_id: &ConnectionId,
    ) -> Option<&ActionProviderSchema> {
        self.schemas_by_connection.get(connection_id)
    }
}

fn validate_invoke_against_schema(
    invoke: &ActionInvoke,
    schema: &tau_actions::ActionSchema,
) -> Result<(), ActionRouteError> {
    let parsed = schema.parse_line(&invoke.raw_line).map_err(|error| {
        ActionRouteError::InvalidInvocation {
            reason: error.to_string(),
        }
    })?;
    if parsed.action_id != invoke.action_id {
        return Err(ActionRouteError::InvalidInvocation {
            reason: format!(
                "raw_line selected action `{}` but invoke requested `{}`",
                parsed.action_id, invoke.action_id
            ),
        });
    }
    if parsed.argv != invoke.argv {
        return Err(ActionRouteError::InvalidInvocation {
            reason: "argv does not match raw_line/schema parse".to_owned(),
        });
    }
    let expected_arguments = parsed_action_arguments(&parsed.named_args);
    if expected_arguments != invoke.arguments {
        return Err(ActionRouteError::InvalidInvocation {
            reason: "typed arguments do not match raw_line/schema parse".to_owned(),
        });
    }
    Ok(())
}

fn parsed_action_arguments(
    args: &std::collections::BTreeMap<String, tau_actions::ParsedArgValue>,
) -> CborValue {
    CborValue::Map(
        args.iter()
            .map(|(name, value)| {
                let value = match value {
                    tau_actions::ParsedArgValue::String(value) => CborValue::Text(value.clone()),
                    tau_actions::ParsedArgValue::Integer(value) => {
                        CborValue::Integer((*value).into())
                    }
                };
                (CborValue::Text(name.clone()), value)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests;
