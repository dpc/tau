#[cfg(test)]
mod tests;
use std::borrow::Cow;
use std::fmt;

/// Harness-selected presentation for authenticated injected text.
///
/// Absence on stored spans retains the historical internal projection. This
/// discriminator controls presentation only, never activation or authority.
/// Family selection, escaping, and carrier rules follow
/// `SPEC-exact-sentinel-prompt-envelopes`; the conventions entry point is
/// `SPEC-tau-proto-provider-data#context-formatting-conventions`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticEnvelope {
    /// Observed agent activity, not a direct request from that agent.
    AgentUpdate,
    /// Same-session agent-authored message.
    AgentMessage,
    /// Cross-session agent-authored message.
    PeerMessage,
    /// Local CLI message with its existing reply capability.
    CliMessage,
    /// Harness-authenticated delegate bootstrap.
    AgentContext,
    /// Tool completion scheduling notification, separate from output.
    ToolCompletion,
    /// Result of a requested compaction.
    CompactionResult,
    /// Context budget advisory.
    ContextSizeAlert,
    /// Loop-guard instruction.
    LoopGuard,
    /// Request to update work status.
    StatusReminder,
    /// Session restore or change notice.
    SessionNotice,
    /// Current tool availability.
    ToolAvailability,
    /// Output-length continuation instruction.
    OutputContinuation,
    /// Availability of archived context.
    CompactionRecall,
    /// Provisional local-summary request.
    SummaryRequest,
    /// Provisional local-summary continuation.
    SummaryContinuation,
}

impl SemanticEnvelope {
    /// Select the registered family from typed harness provenance, not text.
    #[must_use]
    pub const fn family(self) -> RegisteredPayloadEnvelope {
        SEMANTIC_PAYLOAD_ENVELOPES[self as usize]
    }

    /// Escape the complete body and frame it in exactly one semantic family.
    #[must_use]
    pub fn render(self, body: &str) -> String {
        let family = self.family();
        let PayloadEnvelopeOpening::Fixed(open) = family.opening else {
            unreachable!("semantic families have fixed openings");
        };
        format!(
            "{open}{}{close}",
            family.escape_body(body),
            close = family.exact_close
        )
    }

    /// Render semantic metadata and optional data using the shared typed text
    /// serializer before exact-close framing. Headers cannot inject new lines.
    /// The leading LF belongs to envelope layout, not the optional text
    /// separator owned by `SPEC-tau-proto-provider-data`.
    #[must_use]
    pub fn render_fields(self, headers: &[(&str, String)], body: &str) -> String {
        let response = crate::ToolResponse {
            raw: crate::CborValue::Null,
            headers: headers
                .iter()
                .map(|(key, value)| crate::ToolResponseHeader {
                    key: (*key).to_owned(),
                    value: crate::visible_escape_metadata(value),
                })
                .collect(),
            body: body.to_owned(),
        };
        self.render(&format!("\n{}", response.render()))
    }
}

/// Closed semantic families for harness-owned injected-user projections.
pub const SEMANTIC_PAYLOAD_ENVELOPES: [RegisteredPayloadEnvelope; 16] = [
    semantic_family(
        "agent_update",
        "<agent_update>",
        "</agent_update>",
        "&lt;/agent_update&gt;",
    ),
    semantic_family(
        "agent_message",
        "<agent_message>",
        "</agent_message>",
        "&lt;/agent_message&gt;",
    ),
    semantic_family(
        "peer_message",
        "<peer_message>",
        "</peer_message>",
        "&lt;/peer_message&gt;",
    ),
    semantic_family(
        "cli_message",
        "<cli_message>",
        "</cli_message>",
        "&lt;/cli_message&gt;",
    ),
    semantic_family(
        "agent_context",
        "<agent_context>",
        "</agent_context>",
        "&lt;/agent_context&gt;",
    ),
    semantic_family(
        "tool_completion",
        "<tool_completion>",
        "</tool_completion>",
        "&lt;/tool_completion&gt;",
    ),
    semantic_family(
        "compaction_result",
        "<compaction_result>",
        "</compaction_result>",
        "&lt;/compaction_result&gt;",
    ),
    semantic_family(
        "context_size_alert",
        "<context_size_alert>",
        "</context_size_alert>",
        "&lt;/context_size_alert&gt;",
    ),
    semantic_family(
        "loop_guard",
        "<loop_guard>",
        "</loop_guard>",
        "&lt;/loop_guard&gt;",
    ),
    semantic_family(
        "status_reminder",
        "<status_reminder>",
        "</status_reminder>",
        "&lt;/status_reminder&gt;",
    ),
    semantic_family(
        "session_notice",
        "<session_notice>",
        "</session_notice>",
        "&lt;/session_notice&gt;",
    ),
    semantic_family(
        "tool_availability",
        "<tool_availability>",
        "</tool_availability>",
        "&lt;/tool_availability&gt;",
    ),
    semantic_family(
        "output_continuation",
        "<output_continuation>",
        "</output_continuation>",
        "&lt;/output_continuation&gt;",
    ),
    semantic_family(
        "compaction_recall",
        "<compaction_recall>",
        "</compaction_recall>",
        "&lt;/compaction_recall&gt;",
    ),
    semantic_family(
        "summary_request",
        "<summary_request>",
        "</summary_request>",
        "&lt;/summary_request&gt;",
    ),
    semantic_family(
        "summary_continuation",
        "<summary_continuation>",
        "</summary_continuation>",
        "&lt;/summary_continuation&gt;",
    ),
];

/// Construct registry metadata without duplicating the carrier contract.
const fn semantic_family(
    name: &'static str,
    open: &'static str,
    close: &'static str,
    visible_close: &'static str,
) -> RegisteredPayloadEnvelope {
    RegisteredPayloadEnvelope {
        name,
        opening: PayloadEnvelopeOpening::Fixed(open),
        ordered_attributes: &[],
        exact_close: close,
        visible_close,
        carrier: PayloadEnvelopeCarrier::GenericUserText,
    }
}

/// The model-context carrier in which a registered payload envelope appears.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PayloadEnvelopeCarrier {
    /// A complete free-form text part in a user-role message.
    GenericUserText,
    /// Canonical message-fact text projected in user or assistant role
    /// according to the typed event direction.
    GenericUserOrAssistantText,
    /// A protocol-typed tool result whose body carries its own external-data
    /// frame.
    TypedToolResult,
}

/// Opening-sentinel shape for one registered payload-envelope family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PayloadEnvelopeOpening {
    /// The complete opening sentinel has no attributes.
    Fixed(&'static str),
    /// The opening sentinel starts with the tag name followed by registered
    /// attributes.
    Attributed(&'static str),
}

/// Shared metadata for one top-level XML-lite payload-envelope family.
///
/// `SPEC-exact-sentinel-prompt-envelopes` owns this registry contract,
/// including the distinction between carrier provenance and payload-local
/// markup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisteredPayloadEnvelope {
    /// Stable XML-lite tag name.
    pub name: &'static str,
    /// Fixed or attributed opening-sentinel shape.
    pub opening: PayloadEnvelopeOpening,
    /// Dynamic attribute names in deterministic rendering order.
    pub ordered_attributes: &'static [&'static str],
    /// Exact trusted closing sentinel appended by the owning renderer.
    pub exact_close: &'static str,
    /// Visible replacement for exact closing-sentinel collisions in payload
    /// text.
    pub visible_close: &'static str,
    /// Model-context carrier in which the outer family is used.
    pub carrier: PayloadEnvelopeCarrier,
}

impl RegisteredPayloadEnvelope {
    /// Neutralize this family's exact close while preserving every other byte.
    #[must_use]
    pub fn escape_body<'a>(self, body: &'a str) -> Cow<'a, str> {
        escape_exact_sentinel_close(body, self.exact_close, self.visible_close)
    }

    /// Return whether text is exactly one lexically closed instance of this
    /// family.
    #[must_use]
    pub fn matches_whole(self, text: &str) -> bool {
        let body = match self.opening {
            PayloadEnvelopeOpening::Fixed(open) => text.strip_prefix(open),
            PayloadEnvelopeOpening::Attributed(prefix) => text
                .strip_prefix(prefix)
                .and_then(|rest| rest.find('>').map(|end| &rest[end + 1..])),
        };
        body.and_then(|body| body.strip_suffix(self.exact_close))
            .is_some_and(|body| !body.contains(self.exact_close))
    }

    /// Render one attributed envelope while enforcing the registry's attribute
    /// order.
    pub fn render_attributed(
        self,
        attributes: &[(&str, String)],
        body: &str,
    ) -> Result<String, PayloadEnvelopeRenderError> {
        let PayloadEnvelopeOpening::Attributed(opening) = self.opening else {
            return Err(PayloadEnvelopeRenderError::FixedOpening);
        };
        let mut next_index = 0;
        for (name, _) in attributes {
            let Some(relative) = self.ordered_attributes[next_index..]
                .iter()
                .position(|expected| expected == name)
            else {
                return Err(PayloadEnvelopeRenderError::UnknownOrMisorderedAttribute(
                    (*name).to_owned(),
                ));
            };
            next_index += relative + 1;
        }

        let escaped_body = self.escape_body(body);
        let mut output = String::new();
        output.push_str(opening);
        for (index, (name, value)) in attributes.iter().enumerate() {
            if index != 0 {
                output.push(' ');
            }
            output.push_str(name);
            output.push_str("=\"");
            output.push_str(&escape_payload_envelope_attribute(value));
            output.push('"');
        }
        output.push('>');
        output.push_str(&escaped_body);
        output.push_str(self.exact_close);
        Ok(output)
    }
}

/// Why a registered attributed envelope could not be rendered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PayloadEnvelopeRenderError {
    /// The selected family has a fixed opening and accepts no attributes.
    FixedOpening,
    /// An attribute is unknown, duplicated, or appears out of registry order.
    UnknownOrMisorderedAttribute(String),
}

impl fmt::Display for PayloadEnvelopeRenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FixedOpening => formatter.write_str("payload envelope has a fixed opening"),
            Self::UnknownOrMisorderedAttribute(name) => {
                write!(
                    formatter,
                    "unknown or misordered payload attribute `{name}`"
                )
            }
        }
    }
}

/// Registered output from an exact tool invocation, including nonterminal
/// batches.
pub const TAU_TOOL_OUTPUT_PAYLOAD_ENVELOPE: RegisteredPayloadEnvelope = RegisteredPayloadEnvelope {
    name: "tool_output",
    opening: PayloadEnvelopeOpening::Attributed("<tool_output "),
    ordered_attributes: &["call_id", "terminal", "status", "omitted_bytes"],
    exact_close: "</tool_output>",
    visible_close: "&lt;/tool_output&gt;",
    carrier: PayloadEnvelopeCarrier::GenericUserText,
};

/// Registered attributed envelope for direct Tau user-channel input.
pub const USER_PAYLOAD_ENVELOPE: RegisteredPayloadEnvelope = RegisteredPayloadEnvelope {
    name: "user",
    opening: PayloadEnvelopeOpening::Attributed("<user "),
    ordered_attributes: &["source", "delayed"],
    exact_close: "</user>",
    visible_close: "&lt;/user&gt;",
    carrier: PayloadEnvelopeCarrier::GenericUserText,
};

/// Registered attributed output from a configured extension, not direct user
/// input.
pub const TAU_EXTENSION_OUTPUT_PAYLOAD_ENVELOPE: RegisteredPayloadEnvelope =
    RegisteredPayloadEnvelope {
        name: "tau_extension_output",
        opening: PayloadEnvelopeOpening::Attributed("<tau_extension_output "),
        ordered_attributes: &["publisher", "operation_id"],
        exact_close: "</tau_extension_output>",
        visible_close: "&lt;/tau_extension_output&gt;",
        carrier: PayloadEnvelopeCarrier::GenericUserText,
    };

/// New configured-extension deliveries use an unprefixed family. The legacy
/// family remains recognizable for committed text, never selected by new
/// intake.
pub const EXTENSION_OUTPUT_PAYLOAD_ENVELOPE: RegisteredPayloadEnvelope =
    RegisteredPayloadEnvelope {
        name: "extension_output",
        opening: PayloadEnvelopeOpening::Attributed("<extension_output "),
        ordered_attributes: &["publisher", "operation_id"],
        exact_close: "</extension_output>",
        visible_close: "&lt;/extension_output&gt;",
        carrier: PayloadEnvelopeCarrier::GenericUserText,
    };

/// Registered fieldless envelope for harness-authenticated asynchronous input.
pub const TAU_INTERNAL_PAYLOAD_ENVELOPE: RegisteredPayloadEnvelope = RegisteredPayloadEnvelope {
    name: "tau_internal",
    opening: PayloadEnvelopeOpening::Fixed("<tau_internal>"),
    ordered_attributes: &[],
    exact_close: "</tau_internal>",
    visible_close: "&lt;/tau_internal&gt;",
    carrier: PayloadEnvelopeCarrier::GenericUserText,
};

/// Registered attributed envelope for canonical external message facts.
pub const MESSAGE_PAYLOAD_ENVELOPE: RegisteredPayloadEnvelope = RegisteredPayloadEnvelope {
    name: "message",
    opening: PayloadEnvelopeOpening::Attributed("<message "),
    ordered_attributes: &[
        "event",
        "publisher",
        "message_ref_publisher",
        "message_ref",
        "sender_ref",
        "sender_display",
        "sender_auth",
        "sender_trust",
        "recipient_ref",
        "recipient_display",
        "conversation",
        "reaction",
        "mentions",
        "reply_relation",
        "reply_to_publisher",
        "reply_to",
        "content_trust",
    ],
    exact_close: "</message>",
    visible_close: "&lt;/message&gt;",
    carrier: PayloadEnvelopeCarrier::GenericUserOrAssistantText,
};

/// Registered parent quote attached to the preceding canonical message text
/// part.
pub const QUOTED_MESSAGE_PAYLOAD_ENVELOPE: RegisteredPayloadEnvelope = RegisteredPayloadEnvelope {
    name: "quoted_message",
    opening: PayloadEnvelopeOpening::Attributed("<quoted_message "),
    ordered_attributes: &[
        "message_ref_publisher",
        "message_ref",
        "author",
        "excerpt",
        "content_trust",
    ],
    exact_close: "</quoted_message>",
    visible_close: "&lt;/quoted_message&gt;",
    carrier: PayloadEnvelopeCarrier::GenericUserText,
};

/// Registered attributed envelope for typed web-search tool results.
pub const TAU_WEB_CONTENT_PAYLOAD_ENVELOPE: RegisteredPayloadEnvelope = RegisteredPayloadEnvelope {
    name: "tau_web_content",
    opening: PayloadEnvelopeOpening::Attributed("<tau_web_content "),
    ordered_attributes: &["adapter", "operation", "content_trust"],
    exact_close: "</tau_web_content>",
    visible_close: "&lt;/tau_web_content&gt;",
    carrier: PayloadEnvelopeCarrier::TypedToolResult,
};

/// Registered attributed envelope for a committed background-tool preview.
pub const TAU_BACKGROUND_RESULT_PAYLOAD_ENVELOPE: RegisteredPayloadEnvelope =
    RegisteredPayloadEnvelope {
        name: "tau_background_result",
        opening: PayloadEnvelopeOpening::Attributed("<tau_background_result "),
        ordered_attributes: &[
            "call_id",
            "tool",
            "tool_outcome",
            "delivery",
            "rendered_bytes",
            "retrieval",
            "process_outcome",
            "process_source",
            "process_success",
            "termination_reason",
            "exit_code",
            "signal",
            "timed_out",
            "message_bytes",
            "message_truncated",
        ],
        exact_close: "</tau_background_result>",
        visible_close: "&lt;/tau_background_result&gt;",
        carrier: PayloadEnvelopeCarrier::GenericUserText,
    };

/// Registered envelope for lazy discovery guidance and current applicability.
pub const TAU_DISCOVERY_PAYLOAD_ENVELOPE: RegisteredPayloadEnvelope = RegisteredPayloadEnvelope {
    name: "tau_discovery",
    opening: PayloadEnvelopeOpening::Fixed("<tau_discovery>"),
    ordered_attributes: &[],
    exact_close: "</tau_discovery>",
    visible_close: "&lt;/tau_discovery&gt;",
    carrier: PayloadEnvelopeCarrier::GenericUserText,
};

/// Current discovery guidance; stored legacy discovery text remains unchanged.
pub const DISCOVERY_PAYLOAD_ENVELOPE: RegisteredPayloadEnvelope = semantic_family(
    "discovery",
    "<discovery>",
    "</discovery>",
    "&lt;/discovery&gt;",
);

/// Return every registered top-level model-facing payload-envelope family.
#[must_use]
pub const fn registered_payload_envelopes() -> &'static [RegisteredPayloadEnvelope] {
    &[
        EXTENSION_OUTPUT_PAYLOAD_ENVELOPE,
        DISCOVERY_PAYLOAD_ENVELOPE,
        SEMANTIC_PAYLOAD_ENVELOPES[0],
        SEMANTIC_PAYLOAD_ENVELOPES[1],
        SEMANTIC_PAYLOAD_ENVELOPES[2],
        SEMANTIC_PAYLOAD_ENVELOPES[3],
        SEMANTIC_PAYLOAD_ENVELOPES[4],
        SEMANTIC_PAYLOAD_ENVELOPES[5],
        SEMANTIC_PAYLOAD_ENVELOPES[6],
        SEMANTIC_PAYLOAD_ENVELOPES[7],
        SEMANTIC_PAYLOAD_ENVELOPES[8],
        SEMANTIC_PAYLOAD_ENVELOPES[9],
        SEMANTIC_PAYLOAD_ENVELOPES[10],
        SEMANTIC_PAYLOAD_ENVELOPES[11],
        SEMANTIC_PAYLOAD_ENVELOPES[12],
        SEMANTIC_PAYLOAD_ENVELOPES[13],
        SEMANTIC_PAYLOAD_ENVELOPES[14],
        SEMANTIC_PAYLOAD_ENVELOPES[15],
        USER_PAYLOAD_ENVELOPE,
        TAU_EXTENSION_OUTPUT_PAYLOAD_ENVELOPE,
        TAU_INTERNAL_PAYLOAD_ENVELOPE,
        MESSAGE_PAYLOAD_ENVELOPE,
        QUOTED_MESSAGE_PAYLOAD_ENVELOPE,
        TAU_WEB_CONTENT_PAYLOAD_ENVELOPE,
        TAU_BACKGROUND_RESULT_PAYLOAD_ENVELOPE,
        TAU_TOOL_OUTPUT_PAYLOAD_ENVELOPE,
        TAU_DISCOVERY_PAYLOAD_ENVELOPE,
    ]
}

/// Return registered families used by the shared generic user-role text
/// carrier.
pub fn generic_user_payload_envelopes() -> impl Iterator<Item = &'static RegisteredPayloadEnvelope>
{
    registered_payload_envelopes().iter().filter(|family| {
        matches!(
            family.carrier,
            PayloadEnvelopeCarrier::GenericUserText
                | PayloadEnvelopeCarrier::GenericUserOrAssistantText
        )
    })
}

/// Replace only one envelope family's exact closing sentinel in an assembled
/// body.
///
/// Callers supply hard-coded `exact_close` and `visible_close` constants, apply
/// domain normalization and bounds before this function, and append the trusted
/// exact close afterward. The returned body preserves every nonmatching byte.
#[must_use]
pub fn escape_exact_sentinel_close<'a>(
    body: &'a str,
    exact_close: &str,
    visible_close: &str,
) -> Cow<'a, str> {
    if body.contains(exact_close) {
        Cow::Owned(body.replace(exact_close, visible_close))
    } else {
        Cow::Borrowed(body)
    }
}

/// Escape one validated dynamic XML-lite attribute value.
#[must_use]
pub fn escape_payload_envelope_attribute(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}
