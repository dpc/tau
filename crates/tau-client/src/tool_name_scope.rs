use crate::{ClientError, ClientResult};

/// Immutable logical-to-wire mapping established by the first harness
/// configuration for an extension connection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolNameScope {
    /// Optional configured prefix.
    prefix: Option<tau_proto::ToolNamePrefix>,
}

impl ToolNameScope {
    /// Create a scope from the first lifecycle configuration.
    #[must_use]
    pub fn from_configure(configure: &tau_proto::Configure) -> Self {
        Self {
            prefix: configure.tool_prefix.clone(),
        }
    }

    /// Borrow the configured prefix, if any.
    #[must_use]
    pub fn prefix(&self) -> Option<&tau_proto::ToolNamePrefix> {
        self.prefix.as_ref()
    }

    /// Map a logical tool name or model-visible alias to its final wire name.
    ///
    /// # Errors
    ///
    /// Returns an error when prefix composition exceeds the protocol name
    /// limit.
    pub fn wire_tool_name(&self, local: &tau_proto::ToolName) -> ClientResult<tau_proto::ToolName> {
        self.prefix.as_ref().map_or_else(
            || Ok(local.clone()),
            |prefix| {
                prefix
                    .compose_tool_name(local)
                    .map_err(|error| ClientError::name_scope(error.to_string()))
            },
        )
    }

    /// Parse and map a logical tool name.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid local name or overflowing composition.
    pub fn wire_tool(&self, local: &str) -> ClientResult<tau_proto::ToolName> {
        let local = tau_proto::ToolName::try_new(local)
            .ok_or_else(|| ClientError::name_scope(format!("invalid local tool name `{local}`")))?;
        self.wire_tool_name(&local)
    }

    /// Map a logical tool-group name to its final wire name.
    ///
    /// # Errors
    ///
    /// Returns an error when prefix composition exceeds the protocol group
    /// limit.
    pub fn wire_group_name(
        &self,
        local: &tau_proto::ToolGroupName,
    ) -> ClientResult<tau_proto::ToolGroupName> {
        self.prefix.as_ref().map_or_else(
            || Ok(local.clone()),
            |prefix| {
                prefix
                    .compose_group_name(local)
                    .map_err(|error| ClientError::name_scope(error.to_string()))
            },
        )
    }

    /// Structurally map a logical registration while leaving tags, prose,
    /// schemas, grammars, examples, and prompt fragments unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error if any composed structural identifier overflows.
    pub fn scope_registration(
        &self,
        mut registration: tau_proto::ToolRegistrationDeclared,
    ) -> ClientResult<tau_proto::ToolRegistrationDeclared> {
        registration.tool.name = self.wire_tool_name(&registration.tool.name)?;
        registration.tool.model_visible_name = registration
            .tool
            .model_visible_name
            .as_ref()
            .map(|name| self.wire_tool_name(name))
            .transpose()?;
        if let Some(group) = registration.tool_group.as_mut() {
            group.name = self.wire_group_name(&group.name)?;
        }
        Ok(registration)
    }
}
