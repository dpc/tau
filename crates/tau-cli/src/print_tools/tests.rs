use super::*;

/// Ensures preview serialization changes only the routable name into its
/// provider-visible alias and preserves every model-visible definition field,
/// preventing the diagnostic surface from drifting from provider request
/// serialization.
#[test]
fn model_visible_definition_preserves_complete_shape() {
    let definition = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("internal_tool"),
        model_visible_name: Some(tau_proto::ToolName::new("visible_tool")),
        description: Some("Visible description".to_owned()),
        tool_type: tau_proto::ToolType::Custom,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"],
            "additionalProperties": false
        })),
        format: Some(tau_proto::ToolFormat::Grammar {
            syntax: tau_proto::ToolGrammarSyntax::Regex,
            definition: "[a-z]+".to_owned(),
        }),
    };

    let rendered = serde_json::to_value(ModelVisibleToolDefinition::from(definition))
        .expect("serialize model-visible definition");

    assert_eq!(
        rendered,
        serde_json::json!({
            "name": "visible_tool",
            "description": "Visible description",
            "tool_type": "custom",
            "parameters": {
                "type": "object",
                "properties": {"value": {"type": "string"}},
                "required": ["value"],
                "additionalProperties": false
            },
            "format": {
                "type": "grammar",
                "syntax": "regex",
                "definition": "[a-z]+"
            }
        })
    );
}

/// Provider-hosted search must be explicitly marked native so a developer
/// preview cannot be mistaken for an ordinary Tau-routed function tool.
#[test]
fn hosted_web_search_is_marked_provider_native() {
    let rendered = serde_json::to_value(ProviderVisibleToolDefinition::Hosted(
        ModelVisibleHostedToolDefinition::from(tau_proto::HostedToolDefinition::WebSearch {
            access: tau_proto::ProviderWebSearchAccess::Cached,
            context_size: Some(tau_proto::WebSearchContextSize::High),
            allowed_domains: vec!["docs.rs".to_owned()],
        }),
    ))
    .expect("serialize native hosted tool");

    assert_eq!(
        rendered,
        serde_json::json!({
            "name": "web_search",
            "execution": "provider_native",
            "access": "cached",
            "context_size": "high",
            "allowed_domains": ["docs.rs"]
        })
    );
}
