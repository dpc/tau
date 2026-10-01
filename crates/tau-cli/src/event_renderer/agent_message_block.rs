//! Literal message headers and Markdown-lite prose body presentation.

use tau_cli_term::resolve::{convert_color, themed_text};
use tau_proto::Event;
use tau_themes::{SpanTree, StyleName, ThemedText, names};

use super::EventRenderer;
use crate::markdown_render::markdown_block_with_osc8;

impl EventRenderer {
    /// Renders literal headers with bright routing identities and Markdown-lite
    /// prose bodies. Structured watch records retain their literal rendering.
    pub(super) fn submitted_agent_message_block(
        &self,
        event: &Event,
        use_local_names: bool,
        include_body: bool,
    ) -> tau_cli_term::StyledBlock {
        let mut themed = ThemedText::new();
        let body_style = themed.add_style(names::SYSTEM_INFO);
        let marker_style = themed.add_style(names::PROMPT_MARKER_SUBMITTED);
        let identity_style = themed.add_style(names::AGENT_MESSAGE_IDENTITY);
        let mut content = self
            .agent_message_header_parts(event, use_local_names)
            .into_iter()
            .map(|(text, bright)| {
                if bright {
                    SpanTree::span(identity_style, vec![SpanTree::text(text)])
                } else {
                    SpanTree::text(text)
                }
            })
            .collect::<Vec<_>>();
        let kind = match event {
            Event::AgentMessageSent(message) => message.kind,
            Event::AgentMessageReceived(message) => message.kind,
            _ => unreachable!("only agent message events are rendered here"),
        };
        let markdown_body = include_body
            && matches!(
                kind,
                tau_proto::AgentMessageKind::Message
                    | tau_proto::AgentMessageKind::WatchPrompt
                    | tau_proto::AgentMessageKind::WatchResponse
            );
        if include_body {
            content.push(SpanTree::text(":\n"));
            if !markdown_body {
                content.push(SpanTree::text(Self::agent_message_body(event)));
            }
        }
        themed.push_tree(SpanTree::span(
            body_style,
            vec![
                SpanTree::span(
                    marker_style,
                    vec![SpanTree::text(crate::transcript_markers::MESSAGE)],
                ),
                SpanTree::span(body_style, content),
            ],
        ));

        let body_ts = self
            .resources
            .theme
            .resolve_style(&StyleName::new(names::SYSTEM_INFO));
        let mut block = tau_cli_term::StyledBlock::new(themed_text(&self.resources.theme, &themed));
        if markdown_body {
            let body = markdown_block_with_osc8(
                &self.resources.theme,
                names::SYSTEM_INFO,
                &Self::agent_message_body(event),
                self.presentation.osc8_links,
            );
            for span in body.content.spans() {
                block.content.push(span.clone());
            }
        }
        if let Some(bg) = body_ts.bg {
            block = block.bg(convert_color(bg));
        }
        block
    }
}
