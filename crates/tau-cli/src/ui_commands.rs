//! Shared parsing for UI slash-command subsets.

#[cfg(test)]
mod tests;

use tau_proto::Event;

fn is_reset_value(value: &str) -> bool {
    value == "reset"
}

pub(crate) fn parse_tree_navigation_target(
    arg: &str,
) -> Result<tau_proto::UiTreeNavigationTarget, ()> {
    let arg = arg.trim();
    if arg == "root" || arg == "0" {
        return Ok(tau_proto::UiTreeNavigationTarget::Root);
    }
    if let Some(rest) = arg.strip_prefix("node ") {
        let node_id = rest.trim().parse::<u64>().map_err(|_| ())?;
        return Ok(tau_proto::UiTreeNavigationTarget::Node(
            tau_proto::NodeId::new(node_id),
        ));
    }
    let anchor = arg.parse::<u64>().map_err(|_| ())?;
    if anchor == 0 {
        return Ok(tau_proto::UiTreeNavigationTarget::Root);
    }
    Ok(tau_proto::UiTreeNavigationTarget::PromptAnchor(anchor))
}

fn parse_service_tier_update(value: &str) -> Result<Option<tau_proto::ServiceTier>, String> {
    match value {
        "fast" => Ok(Some(tau_proto::ServiceTier::Fast)),
        "flex" => Ok(Some(tau_proto::ServiceTier::Flex)),
        "reset" => Ok(None),
        other => Err(format!(
            "unknown service tier `{other}`; expected fast/flex/reset"
        )),
    }
}

pub(crate) fn parse_tool_list_update(
    value: &str,
) -> Result<Option<Vec<tau_proto::ToolName>>, String> {
    if is_reset_value(value) {
        return Ok(None);
    }
    value
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| {
            tau_proto::ToolName::try_new(name).ok_or_else(|| format!("invalid tool name: {name}"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn parse_disable_tool_list_update(value: &str) -> Result<Vec<tau_proto::ToolName>, String> {
    Ok(parse_tool_list_update(value)?.unwrap_or_default())
}

fn parse_tool_group_list_update(value: &str) -> Result<Vec<tau_proto::ToolGroupName>, String> {
    if is_reset_value(value) {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| {
            tau_proto::ToolGroupName::try_new(name)
                .ok_or_else(|| format!("invalid tool group name: {name}"))
        })
        .collect()
}

fn parse_compaction_threshold_update(value: &str) -> Result<Option<u64>, String> {
    if is_reset_value(value) {
        return Ok(None);
    }
    let threshold = value
        .parse::<u64>()
        .map_err(|_| "compaction-threshold must be a token count of at least 1000".to_owned())?;
    if threshold < 1000 {
        return Err("compaction-threshold must be a token count of at least 1000".to_owned());
    }
    Ok(Some(threshold))
}

pub(crate) fn parse_role_setting_update(
    setting: &str,
    value: &str,
) -> Result<tau_proto::UiRoleUpdateAction, String> {
    match setting {
        "model" => Ok(tau_proto::UiRoleUpdateAction::SetModel {
            model: if is_reset_value(value) {
                None
            } else {
                Some(
                    value
                        .parse::<tau_proto::ModelId>()
                        .map_err(|err| err.to_string())?,
                )
            },
        }),
        "effort" => Ok(tau_proto::UiRoleUpdateAction::SetEffort {
            effort: if is_reset_value(value) {
                None
            } else {
                Some(
                    value
                        .parse::<tau_proto::Effort>()
                        .map_err(|err| err.to_string())?,
                )
            },
        }),
        "verbosity" => Ok(tau_proto::UiRoleUpdateAction::SetVerbosity {
            verbosity: if is_reset_value(value) {
                None
            } else {
                Some(
                    value
                        .parse::<tau_proto::Verbosity>()
                        .map_err(|err| err.to_string())?,
                )
            },
        }),
        "thinking-summary" => Ok(tau_proto::UiRoleUpdateAction::SetThinkingSummary {
            thinking_summary: if is_reset_value(value) {
                None
            } else {
                Some(
                    value
                        .parse::<tau_proto::ThinkingSummary>()
                        .map_err(|err| err.to_string())?,
                )
            },
        }),
        "service-tier" => Ok(tau_proto::UiRoleUpdateAction::SetServiceTier {
            service_tier: parse_service_tier_update(value)?,
        }),
        "compaction-threshold" => Ok(tau_proto::UiRoleUpdateAction::SetCompactionThreshold {
            compaction_threshold: parse_compaction_threshold_update(value)?,
        }),
        "tools" => Ok(tau_proto::UiRoleUpdateAction::SetTools {
            tools: parse_tool_list_update(value)?,
        }),
        "enable-tool-groups" => Ok(tau_proto::UiRoleUpdateAction::SetEnableToolGroups {
            enable_tool_groups: parse_tool_group_list_update(value)?,
        }),
        "disable-tool-groups" => Ok(tau_proto::UiRoleUpdateAction::SetDisableToolGroups {
            disable_tool_groups: parse_tool_group_list_update(value)?,
        }),
        "enable-tools" => Ok(tau_proto::UiRoleUpdateAction::SetEnableTools {
            enable_tools: parse_disable_tool_list_update(value)?,
        }),
        "disable-tools" => Ok(tau_proto::UiRoleUpdateAction::SetDisableTools {
            disable_tools: parse_disable_tool_list_update(value)?,
        }),
        _ => Err("unknown setting".to_owned()),
    }
}

pub(crate) fn parse_role_command(rest: &str) -> Result<Option<Event>, String> {
    let mut parts = rest.split_whitespace();
    let role = parts.next();
    let command = parts.next();
    let value = parts.next();
    let extra = parts.next();

    let Some(role) = role else {
        return Ok(None);
    };
    let Some(command) = command else {
        return Ok(Some(crate::ui_events::role_select(role)));
    };
    if command == "delete" {
        if value.is_some() {
            return Err("/role <role> delete takes no value".to_owned());
        }
        return Ok(Some(crate::ui_events::role_update(
            role,
            tau_proto::UiRoleUpdateAction::Delete,
        )));
    }
    let Some(value) = value else {
        return Err("/role <role> <setting> <value>".to_owned());
    };
    if extra.is_some() {
        return Err("/role: too many arguments".to_owned());
    }
    let action =
        parse_role_setting_update(command, value).map_err(|error| format!("/role: {error}"))?;
    Ok(Some(crate::ui_events::role_update(role, action)))
}
