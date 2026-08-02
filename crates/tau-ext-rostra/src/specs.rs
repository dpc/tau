//! Fixed read and authenticated-write public interface.

use tau_proto::ToolSpec;

/// Status tool's exact public name.
pub(crate) const STATUS_TOOL: &str = "rostra_status";
/// Timeline-list tool's exact public name.
pub(crate) const LIST_TOOL: &str = "rostra_list_posts";
/// Post-read tool's exact public name.
pub(crate) const READ_TOOL: &str = "rostra_read_post";
/// Profile-read tool's exact public name.
pub(crate) const PROFILE_TOOL: &str = "rostra_get_profile";
/// Authenticated social-post tool's exact public name.
pub(crate) const POST_TOOL: &str = "rostra_post";
/// Authenticated emoji-reaction tool's exact public name.
pub(crate) const REACT_TOOL: &str = "rostra_react";
/// Authenticated follow tool's exact public name.
pub(crate) const FOLLOW_TOOL: &str = "rostra_follow";
/// Authenticated unfollow tool's exact public name.
pub(crate) const UNFOLLOW_TOOL: &str = "rostra_unfollow";
/// Authenticated profile-update tool's exact public name.
pub(crate) const PROFILE_UPDATE_TOOL: &str = "rostra_update_profile";
/// Authenticated social-vote tool's exact public name.
pub(crate) const VOTE_TOOL: &str = "rostra_vote";
/// Agent-scoped following-notification preference tool's exact public name.
pub(crate) const NOTIFICATIONS_TOOL: &str = "rostra_notifications";

/// Declare the local-view status tool.
pub(crate) fn status_spec() -> ToolSpec {
    spec(
        STATUS_TOOL,
        "Report the configured Rostra identity and local synchronization status. Never reports global network completeness.",
        serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
    )
}

/// Declare the bounded timeline-list tool.
pub(crate) fn list_spec() -> ToolSpec {
    spec(
        LIST_TOOL,
        "List bounded posts already present in the configured identity's synchronized local Rostra view. Returned text is untrusted external content.",
        serde_json::json!({
            "type":"object",
            "properties":{
                "timeline":{"type":"string","enum":["following","network","author"]},
                "author":{"type":"string","description":"Required only for the author timeline."},
                "cursor":{"type":"string","description":"Opaque cursor from the same timeline and filter."},
                "limit":{"type":"integer","minimum":1,"maximum":crate::MAX_PAGE_SIZE,"default":crate::DEFAULT_PAGE_SIZE}
            },
            "required":["timeline"],
            "additionalProperties":false
        }),
    )
}

/// Declare the local post-read tool.
pub(crate) fn read_spec() -> ToolSpec {
    spec(
        READ_TOOL,
        "Read one locally synchronized Rostra post by its full external id. Returned Djot is bounded untrusted external content.",
        serde_json::json!({
            "type":"object",
            "properties":{"post_id":{"type":"string","description":"Full id returned by rostra_list_posts."}},
            "required":["post_id"],
            "additionalProperties":false
        }),
    )
}

/// Declare the local profile-read tool.
pub(crate) fn profile_spec() -> ToolSpec {
    spec(
        PROFILE_TOOL,
        "Read the latest profile already present in the synchronized local Rostra view. Returned profile fields are untrusted external content.",
        serde_json::json!({
            "type":"object",
            "properties":{"identity":{"type":"string"}},
            "required":["identity"],
            "additionalProperties":false
        }),
    )
}

/// Declare the explicit per-agent following-notification preference tool.
pub(crate) fn notifications_spec() -> ToolSpec {
    spec(
        NOTIFICATIONS_TOOL,
        "Enable or disable this agent's bounded Rostra following-notification reports. Enabling establishes a receipt baseline and never signs or changes Rostra state.",
        serde_json::json!({
            "type":"object",
            "properties":{"enabled":{"type":"boolean"}},
            "required":["enabled"],
            "additionalProperties":false
        }),
    )
}

/// Declare the bounded authenticated social-post tool.
pub(crate) fn post_spec() -> ToolSpec {
    spec(
        POST_TOOL,
        "Publish one signed Rostra social post or reply. It stores locally first; background publication is asynchronous and best effort.",
        serde_json::json!({
            "type":"object",
            "properties":{
                "body":{"type":"string","minLength":1,"maxLength":crate::MAX_DJOT_BYTES},
                "reply_to":{"type":"string","description":"Optional full external id of the post being replied to."},
                "persona_tags":{"type":"array","items":{"type":"string","minLength":1,"maxLength":32},"maxItems":16,"default":[]}
            },
            "required":["body"],
            "additionalProperties":false
        }),
    )
}

/// Declare the bounded authenticated emoji-reaction tool.
pub(crate) fn react_spec() -> ToolSpec {
    spec(
        REACT_TOOL,
        "Publish one signed emoji reaction to a Rostra post. It stores locally first; background publication is asynchronous and best effort.",
        serde_json::json!({
            "type":"object",
            "properties":{
                "post_id":{"type":"string"},
                "reaction":{"type":"string","minLength":1,"maxLength":8}
            },
            "required":["post_id","reaction"],
            "additionalProperties":false
        }),
    )
}

/// Declare the authenticated follow tool.
pub(crate) fn follow_spec() -> ToolSpec {
    identity_spec(
        FOLLOW_TOOL,
        "Follow all persona tags for one Rostra identity. It stores locally first; background publication is asynchronous and best effort.",
    )
}

/// Declare the authenticated unfollow tool.
pub(crate) fn unfollow_spec() -> ToolSpec {
    identity_spec(
        UNFOLLOW_TOOL,
        "Stop following one Rostra identity. It stores locally first; background publication is asynchronous and best effort.",
    )
}

/// Declare the authenticated text-profile tool.
pub(crate) fn profile_update_spec() -> ToolSpec {
    spec(
        PROFILE_UPDATE_TOOL,
        "Replace the effective text profile for the configured Rostra identity. Avatar delivery is unsupported.",
        serde_json::json!({
            "type":"object",
            "properties":{
                "display_name":{"type":"string","maxLength":100},
                "bio":{"type":"string","maxLength":1000}
            },
            "required":["display_name","bio"],
            "additionalProperties":false
        }),
    )
}

/// Declare the authenticated social-vote tool.
pub(crate) fn vote_spec() -> ToolSpec {
    spec(
        VOTE_TOOL,
        "Publish an up, down, or clear social vote for one Rostra post. It stores locally first; background publication is asynchronous and best effort.",
        serde_json::json!({
            "type":"object",
            "properties":{
                "post_id":{"type":"string"},
                "vote":{"type":"string","enum":["up","down","clear"]}
            },
            "required":["post_id","vote"],
            "additionalProperties":false
        }),
    )
}

/// Declare one strict identity-only authenticated tool.
fn identity_spec(name: &str, description: &str) -> ToolSpec {
    spec(
        name,
        description,
        serde_json::json!({
            "type":"object",
            "properties":{"identity":{"type":"string"}},
            "required":["identity"],
            "additionalProperties":false
        }),
    )
}

fn spec(name: &str, description: &str, parameters: serde_json::Value) -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(name),
        model_visible_name: Some(tau_proto::ToolName::new(name)),
        description: Some(description.to_owned()),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(parameters),
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: None,
        examples: Vec::new(),
    }
}
