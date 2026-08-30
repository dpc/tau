//! Stages and atomically publishes selected transcript provider finals.

use super::*;

/// Expensive, immutable presentation work prepared before a final is published.
pub(super) struct FinishedResponseProjection {
    /// Final reasoning text and its settled history block.
    thinking: Option<(String, tau_cli_term::StyledBlock)>,
    /// Complete user assistant text for transcript retention and external
    /// editor publication.
    editor_response: Option<(String, Option<String>)>,
    /// Settled durable items in provider output order.
    items: Vec<FinishedContextProjection>,
    /// Number of declared calls, including length-truncated calls.
    declared_tool_calls: usize,
    /// Number of calls admitted for placeholder execution.
    admitted_tool_calls: usize,
    /// Optional settled response placeholder.
    placeholder: Option<tau_cli_term::StyledBlock>,
    /// Turn latency sampled before the short publication commit.
    turn_latency: Option<Duration>,
    /// Optional usage record and its already-rendered settled block.
    turn_stats: Option<(tau_proto::ProviderTokenUsage, tau_cli_term::StyledBlock)>,
    /// Settled status block built from the projected final logical state.
    status_block: Option<tau_cli_term::StyledBlock>,
}

/// One classified, already-rendered durable provider output item.
enum FinishedContextProjection {
    /// Settled assistant message block.
    Message(tau_cli_term::StyledBlock),
    /// Tool placeholder retained until its lifecycle starts.
    ToolCall {
        /// Call id retained for placeholder publication and lifecycle
        /// correlation.
        call_id: tau_proto::ToolCallId,
        /// Tool name retained for generic placeholder classification.
        name: tau_proto::ToolName,
        /// Whether the stop reason permits an executable placeholder.
        admitted: bool,
    },
    /// Settled provider compaction marker.
    Compaction(tau_cli_term::StyledBlock),
}

impl EventRenderer {
    /// Returns placeholder history identities in caller-supplied call order.
    #[cfg(test)]
    pub(crate) fn tool_placeholder_ids_for_test(&self, call_ids: &[&str]) -> Vec<u64> {
        call_ids
            .iter()
            .map(|call_id| {
                self.transcript.runtime.tool_calls[*call_id]
                    .history_block_id
                    .expect("tool placeholder history")
                    .0
            })
            .collect()
    }

    pub(super) fn handle_provider_response_finished(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
        terminal_tool_calls: &TerminalToolCalls,
    ) {
        let is_standalone = self
            .transcript
            .runtime
            .prompts
            .get(&finished.agent_prompt_id)
            .is_some_and(|state| state.is_standalone_compaction);
        if is_standalone {
            self.finish_standalone_compaction_prompt(&finished.agent_prompt_id, None, false);
            #[cfg(test)]
            if let Some(hook) = &self.finished_commit_hook {
                hook();
            }
            if let Some(status_block) = self.staged_finished_status.take() {
                self.publish_model_status_block(status_block);
            }
            #[cfg(test)]
            if let Some(hook) = &self.finished_published_hook {
                hook();
            }
            return;
        }
        let projection = self
            .staged_finished_response
            .take()
            .unwrap_or_else(|| self.stage_finished_response(finished, terminal_tool_calls));
        self.commit_finished_response(finished, projection);
    }

    /// Prepares settled blocks and classifications without changing visible
    /// state.
    pub(super) fn stage_finished_response(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
        terminal_tool_calls: &TerminalToolCalls,
    ) -> FinishedResponseProjection {
        use tau_themes::names;

        let prompt_state = self
            .transcript
            .runtime
            .prompts
            .get(&finished.agent_prompt_id);
        let turn_latency = prompt_state
            .and_then(|state| state.started_at)
            .map(|started_at| started_at.elapsed());
        let retain_thinking = self.presentation.show_thinking || !self.presentation.verbose_mode;
        #[cfg(test)]
        let mut reasoning_concat_allocations = 0;
        let thinking = retain_thinking
            .then(|| {
                reasoning_text_from_output_items(
                    &finished.output_items,
                    #[cfg(test)]
                    &mut reasoning_concat_allocations,
                )
                .or_else(|| {
                    prompt_state
                        .and_then(|state| state.thinking_text.as_deref())
                        .map(Cow::Borrowed)
                })
            })
            .flatten()
            .map(|text| {
                #[cfg(test)]
                {
                    self.editor
                        .final_semantic_projection
                        .reasoning_materializations += 1;
                    self.editor
                        .final_semantic_projection
                        .reasoning_concat_allocations += reasoning_concat_allocations;
                }
                let display = if self.presentation.verbose_mode && self.presentation.show_thinking {
                    text.as_ref()
                } else {
                    ""
                };
                let block = markdown_block_with_osc8(
                    &self.resources.theme,
                    names::AGENT_THINKING,
                    display,
                    self.presentation.osc8_links,
                );
                (text.into_owned(), block)
            });
        #[cfg(test)]
        let mut assistant_concat_allocations = 0;
        let editor_response = finished
            .originator
            .is_user()
            .then(|| {
                assistant_text_from_output_items(
                    &finished.output_items,
                    #[cfg(test)]
                    &mut assistant_concat_allocations,
                )
                .map(|text| {
                    #[cfg(test)]
                    {
                        self.editor
                            .final_semantic_projection
                            .assistant_materializations += 1;
                        self.editor
                            .final_semantic_projection
                            .assistant_concat_allocations += assistant_concat_allocations;
                    }
                    let retained = text.into_owned();
                    let published = (!self.editor.suppress_editor_context_publish).then(|| {
                        #[cfg(test)]
                        {
                            self.editor
                                .final_semantic_projection
                                .editor_publication_clones += 1;
                            self.editor
                                .final_semantic_projection
                                .editor_publication_clone_bytes += retained.len() as u64;
                        }
                        retained.clone()
                    });
                    (retained, published)
                })
            })
            .flatten();
        let items = self.stage_finished_context_items(finished, terminal_tool_calls);
        let placeholder = self.stage_finished_placeholder(finished, terminal_tool_calls);
        let turn_stats = finished.usage.clone().map(|usage| {
            let mut cumulative = self.transcript.status.cumulative_agent_token_usage;
            Self::add_finished_token_usage(&mut cumulative, &usage);
            let previous = self
                .transcript
                .history
                .turn_stats_history
                .last()
                .map(|entry| entry.usage.clone());
            let block = if self.presentation.verbose_mode && self.presentation.show_turn_stats {
                render_turn_stats_block_with_cumulative_usage(
                    &self.resources.theme,
                    &usage,
                    &cumulative,
                    previous.as_ref(),
                    turn_latency,
                    Some(
                        self.transcript.status.cumulative_agent_latency
                            + turn_latency.unwrap_or_default(),
                    ),
                )
            } else {
                Self::empty_block()
            };
            (usage, block)
        });
        let declared_tool_calls = terminal_tool_calls.len();
        let admitted_tool_calls = terminal_tool_calls.admitted_len();
        let status_block = self.stage_finished_status_block(
            finished,
            terminal_tool_calls,
            admitted_tool_calls,
            turn_latency,
        );
        #[cfg(test)]
        if let Some(hook) = &self.finished_staging_hook {
            hook();
        }
        FinishedResponseProjection {
            thinking,
            editor_response,
            items,
            declared_tool_calls,
            admitted_tool_calls,
            placeholder,
            turn_latency,
            turn_stats,
            status_block: Some(status_block),
        }
    }

    /// Builds status from the final logical values without publishing them.
    pub(super) fn stage_finished_status_block(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
        terminal_tool_calls: &TerminalToolCalls,
        admitted_tool_calls: usize,
        turn_latency: Option<Duration>,
    ) -> tau_cli_term::StyledBlock {
        let original_status = self.transcript.status.clone();
        let original_active_prompts = self.watches.active_agent_prompts.clone();

        self.transcript
            .status
            .agent_activity
            .finish_prompt_with_tool_call_ids(
                &finished.agent_prompt_id,
                terminal_tool_calls.call_ids(),
            );
        self.watches.active_agent_prompts.retain(|_, prompts| {
            prompts.remove(&finished.agent_prompt_id);
            !prompts.is_empty()
        });
        self.transcript.status.main_agent_turn_active = !terminal_tool_calls.is_empty()
            || !self.transcript.status.main_backgrounded_tools.is_empty();
        self.transcript.status.main_tools_total += admitted_tool_calls as u64;
        self.transcript.status.main_tools_visible = admitted_tool_calls != 0;
        if let Some(latency) = turn_latency {
            self.transcript.status.cumulative_agent_latency += latency;
        }
        if let Some(usage) = &finished.usage {
            Self::add_finished_token_usage(
                &mut self.transcript.status.cumulative_agent_token_usage,
                usage,
            );
        }
        let block = self.build_model_status_block();

        self.transcript.status = original_status;
        self.watches.active_agent_prompts = original_active_prompts;
        block
    }

    /// Builds settled standalone lifecycle status without folding private
    /// provider content, usage, or latency into ordinary turn accounting.
    pub(super) fn stage_standalone_finished_status_block(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
        terminal_tool_calls: &TerminalToolCalls,
    ) -> tau_cli_term::StyledBlock {
        let original_status = self.transcript.status.clone();
        let original_active_prompts = self.watches.active_agent_prompts.clone();

        self.transcript
            .status
            .agent_activity
            .finish_prompt_with_tool_call_ids(
                &finished.agent_prompt_id,
                terminal_tool_calls.call_ids(),
            );
        self.watches.active_agent_prompts.retain(|_, prompts| {
            prompts.remove(&finished.agent_prompt_id);
            !prompts.is_empty()
        });
        self.transcript.status.main_agent_turn_active = false;
        self.transcript.status.main_tools_visible = false;
        let block = self.build_model_status_block();

        self.transcript.status = original_status;
        self.watches.active_agent_prompts = original_active_prompts;
        block
    }

    /// Applies one selected transcript's complete final projection.
    fn commit_finished_response(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
        mut projection: FinishedResponseProjection,
    ) {
        let has_output_tool_calls = projection.declared_tool_calls != 0;
        if finished.originator.is_user() && !has_output_tool_calls {
            self.clear_main_agent_turn_active_everywhere();
        }
        self.watches
            .finished_provider_prompts
            .insert(finished.agent_prompt_id.clone());
        let prompt_state = self.take_finished_prompt_state(finished, projection.turn_latency);
        self.finalize_finished_thinking_block(
            prompt_state.thinking_block_id,
            projection.thinking.take(),
        );
        self.finalize_finished_compaction_block(prompt_state.compaction_block_id);
        self.finalize_finished_response_block(prompt_state.response_block_id);
        #[cfg(test)]
        if let Some(hook) = &self.finished_commit_hook {
            hook();
        }

        self.record_finished_assistant_context(projection.editor_response.take());
        self.record_finished_turn_stats(projection.turn_stats.take(), projection.turn_latency);
        let status_block = projection
            .status_block
            .take()
            .expect("finished response status was staged");
        self.render_user_provider_response_items(finished, projection);
        self.publish_model_status_block(status_block);
        #[cfg(test)]
        if let Some(hook) = &self.finished_published_hook {
            hook();
        }
    }

    fn take_finished_prompt_state(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
        turn_latency: Option<Duration>,
    ) -> PromptState {
        // Drain the whole per-prompt state in one shot — every field tracked
        // through the stream is consumed here.
        let prompt_state = self
            .transcript
            .runtime
            .prompts
            .remove(&finished.agent_prompt_id)
            .unwrap_or_default();
        if let Some(latency) = turn_latency {
            self.transcript.status.cumulative_agent_latency += latency;
        }
        prompt_state
    }

    fn finalize_finished_thinking_block(
        &mut self,
        thinking_block_id: Option<tau_cli_term::BlockId>,
        thinking: Option<(String, tau_cli_term::StyledBlock)>,
    ) {
        // Finalize the thinking block above the response, using the final
        // item-model reasoning text or the latest streamed snapshot if one was
        // captured.
        if let Some(tbid) = thinking_block_id {
            self.resources.handle.remove_block(tbid);
        }
        if let Some((thinking, block)) = thinking {
            let bid = self.resources.handle.print_output("agent-thinking", block);
            self.transcript
                .history
                .thinking_history
                .push(ThinkingBlockEntry {
                    block_id: bid,
                    text: thinking,
                });
        }
    }

    fn finalize_finished_compaction_block(
        &mut self,
        compaction_block_id: Option<tau_cli_term::BlockId>,
    ) {
        if let Some(block_id) = compaction_block_id {
            self.resources.handle.remove_block(block_id);
        }
    }

    fn finalize_finished_response_block(
        &mut self,
        response_block_id: Option<tau_cli_term::BlockId>,
    ) {
        if let Some(bid) = response_block_id {
            self.resources.handle.remove_block(bid);
        }
    }

    fn record_finished_assistant_context(
        &mut self,
        editor_response: Option<(String, Option<String>)>,
    ) {
        let Some((retained, published)) = editor_response else {
            return;
        };
        self.transcript
            .runtime
            .editor_conversation_context
            .last_response = Some(retained);
        self.transcript
            .runtime
            .editor_conversation_context
            .current_response = None;
        if let Some(published) = published
            && let Ok(mut context) = self.editor.editor_context.lock()
        {
            context.last_response = Some(published);
            context.current_response = None;
        }
    }

    fn record_finished_turn_stats(
        &mut self,
        staged: Option<(tau_proto::ProviderTokenUsage, tau_cli_term::StyledBlock)>,
        turn_latency: Option<Duration>,
    ) {
        let Some((usage, block)) = staged else {
            return;
        };
        Self::add_finished_token_usage(
            &mut self.transcript.status.cumulative_agent_token_usage,
            &usage,
        );
        let cumulative_usage = self.transcript.status.cumulative_agent_token_usage;
        let previous_usage = self
            .transcript
            .history
            .turn_stats_history
            .last()
            .map(|entry| entry.usage.clone());
        let bid = self.resources.handle.print_output("turn-stats", block);
        self.transcript
            .history
            .turn_stats_history
            .push(TurnStatsBlockEntry {
                block_id: bid,
                usage,
                cumulative_usage,
                previous_usage,
                turn_latency,
                total_latency: Some(self.transcript.status.cumulative_agent_latency),
            });
    }

    /// Adds a durable terminal response's token delta to one UI-owned total.
    pub(super) fn add_finished_token_usage(
        total: &mut tau_proto::TokenUsageCounts,
        usage: &tau_proto::ProviderTokenUsage,
    ) {
        total.sent_tokens = total.sent_tokens.saturating_add(usage.prompt_sent_tokens);
        total.cached_tokens = total
            .cached_tokens
            .saturating_add(usage.prompt_cached_tokens);
        total.received_tokens = total
            .received_tokens
            .saturating_add(usage.response_received_tokens);
    }

    fn render_user_provider_response_items(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
        projection: FinishedResponseProjection,
    ) {
        // The event has already been routed into the owning agent transcript.
        // Only the main agent's tool calls land in the UI as their own blocks.
        // Sub-agent activity is summarized through generic watched-agent stats,
        // so the user sees one activity line per watched agent rather than a
        // flood of nested invocations.
        if finished.output_items.is_empty() {
            self.finish_prompt_tool_summary();
            if let Some(block) = projection.placeholder {
                self.render_provider_response_placeholder(block);
            }
            return;
        }
        let tool_call_count = projection.admitted_tool_calls;
        let has_declared_tool_call = projection.declared_tool_calls != 0;
        self.transcript.status.main_agent_turn_active =
            has_declared_tool_call || !self.transcript.status.main_backgrounded_tools.is_empty();
        self.transcript.status.main_tools_total += tool_call_count as u64;
        self.transcript.status.main_tools_visible = tool_call_count != 0;
        let summary_block_id = self.prepare_tool_summary_for_finished_calls(tool_call_count);
        for item in projection.items {
            self.render_finished_context_item(item, summary_block_id);
        }
        if let Some(block) = projection.placeholder {
            self.render_provider_response_placeholder(block);
        }
        self.resources.handle.redraw();
    }

    fn render_provider_response_placeholder(&mut self, block: tau_cli_term::StyledBlock) {
        self.resources
            .handle
            .print_output("agent-response-placeholder", block);
    }

    /// Classifies and renders durable provider items outside the publication
    /// cut.
    fn stage_finished_context_items(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
        terminal_tool_calls: &TerminalToolCalls,
    ) -> Vec<FinishedContextProjection> {
        use tau_themes::names;

        let mut projected_calls = terminal_tool_calls.iter();
        #[cfg(test)]
        let mut message_materializations = 0;
        #[cfg(test)]
        let mut message_concat_allocations = 0;
        let items = finished
            .output_items
            .iter()
            .filter_map(|item| match item {
                ContextItem::Message(message) => assistant_text_from_message_item(
                    message,
                    #[cfg(test)]
                    &mut message_concat_allocations,
                )
                .map(|text| {
                    #[cfg(test)]
                    {
                        message_materializations += 1;
                    }
                    FinishedContextProjection::Message(markdown_prefixed_block_with_osc8(
                        &self.resources.theme,
                        names::AGENT_RESPONSE,
                        COMPLETED_AGENT_RESPONSE_PREFIX,
                        &text,
                        self.presentation.osc8_links,
                    ))
                }),
                ContextItem::ToolCall(_) => {
                    let call = projected_calls
                        .next()
                        .expect("terminal projection preserves tool-call order");
                    Some(FinishedContextProjection::ToolCall {
                        call_id: call.call_id.clone(),
                        name: call.name.clone(),
                        admitted: call.admitted,
                    })
                }
                ContextItem::Compaction(_) => Some(FinishedContextProjection::Compaction(
                    render_compaction_block(
                        &self.resources.theme,
                        Self::compaction_success_status(
                            finished.compaction_original_input_tokens,
                            finished.compaction_output_tokens,
                        ),
                        CompactionStatus::Success,
                    ),
                )),
                _ => None,
            })
            .collect();
        #[cfg(test)]
        {
            self.editor
                .final_semantic_projection
                .message_materializations += message_materializations;
            self.editor
                .final_semantic_projection
                .message_concat_allocations += message_concat_allocations;
        }
        let projected_calls_exhausted = projected_calls.next().is_none();
        // ast-grep-ignore: debug-assert-expression-must-not-mutate
        debug_assert!(projected_calls_exhausted);
        items
    }

    /// Renders the optional empty or output-length terminal placeholder.
    fn stage_finished_placeholder(
        &self,
        finished: &tau_proto::ProviderResponseFinished,
        terminal_tool_calls: &TerminalToolCalls,
    ) -> Option<tau_cli_term::StyledBlock> {
        use tau_themes::names;

        let text = if finished.output_items.is_empty() {
            finished
                .error
                .as_deref()
                .unwrap_or("(provider returned an empty response)")
        } else if finished.stop_reason != tau_proto::ProviderStopReason::Length {
            return None;
        } else if matches!(
            finished.output_length_disposition,
            tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
        ) {
            "Output limit reached; continuing once from retained reasoning."
        } else if !terminal_tool_calls.is_empty() {
            "Model reached its output-token limit while producing a tool call. The incomplete call was not executed."
        } else if finished
            .output_items
            .iter()
            .any(|item| matches!(item, ContextItem::Message(_)))
        {
            "Model reached its output-token limit before completing the turn. The displayed response may be incomplete."
        } else {
            "Model reached its output-token limit before completing the turn. No assistant answer or executable tool call was produced."
        };
        Some(markdown_prefixed_block_with_osc8(
            &self.resources.theme,
            names::AGENT_RESPONSE,
            COMPLETED_AGENT_RESPONSE_PREFIX,
            text,
            self.presentation.osc8_links,
        ))
    }

    fn prepare_tool_summary_for_finished_calls(
        &mut self,
        tool_call_count: usize,
    ) -> Option<tau_cli_term::BlockId> {
        if tool_call_count == 0 {
            self.finish_prompt_tool_summary();
            return None;
        }
        if matches!(
            self.presentation.show_tools,
            tau_config::settings::ShowTools::SummarizePrompt
        ) {
            return Some(self.create_or_update_prompt_tool_summary(tool_call_count as u64));
        }
        Some(self.create_turn_tool_summary(tool_call_count as u64))
    }

    fn create_or_update_prompt_tool_summary(&mut self, total_delta: u64) -> tau_cli_term::BlockId {
        if let Some(id) = self.transcript.status.prompt_tool_summary {
            if let Some(summary) = self.transcript.status.tool_summaries.get_mut(&id) {
                summary.total += total_delta;
            }
            if self.transcript.status.prompt_tool_summary_active {
                self.update_tool_summary_block(id);
                return id;
            }
            if let Some(summary) = self.transcript.status.tool_summaries.remove(&id) {
                return self.create_prompt_tool_summary(summary);
            }
        }
        let summary = ToolSummaryDisplay {
            total: total_delta,
            ..ToolSummaryDisplay::default()
        };
        self.create_prompt_tool_summary(summary)
    }

    fn create_prompt_tool_summary(&mut self, summary: ToolSummaryDisplay) -> tau_cli_term::BlockId {
        let block = self.render_summary_block(&summary);
        let id = self
            .resources
            .handle
            .new_block("tool-summary:prompt", block);
        self.resources.handle.push_above_active(id);
        self.transcript.status.tool_summaries.insert(id, summary);
        self.transcript.status.prompt_tool_summary = Some(id);
        self.transcript.status.prompt_tool_summary_active = true;
        id
    }

    pub(super) fn finish_prompt_tool_summary(&mut self) {
        let Some(block_id) = self.transcript.status.prompt_tool_summary.take() else {
            self.transcript.status.prompt_tool_summary_active = false;
            return;
        };
        self.transcript.status.prompt_tool_summary_active = false;
        let Some(summary) = self.transcript.status.tool_summaries.remove(&block_id) else {
            return;
        };
        self.resources.handle.remove_block(block_id);
        let new_block_id = self
            .resources
            .handle
            .print_output("tool-summary", self.render_summary_block(&summary));
        self.transcript
            .status
            .tool_summaries
            .insert(new_block_id, summary);
    }

    fn create_turn_tool_summary(&mut self, total: u64) -> tau_cli_term::BlockId {
        let summary = ToolSummaryDisplay {
            total,
            ..ToolSummaryDisplay::default()
        };
        let block = self.render_summary_block(&summary);
        let id = self.resources.handle.new_block("tool-summary:turn", block);
        self.resources.handle.push_above_active(id);
        self.transcript.status.tool_summaries.insert(id, summary);
        id
    }

    fn render_finished_context_item(
        &mut self,
        item: FinishedContextProjection,
        summary_block_id: Option<tau_cli_term::BlockId>,
    ) {
        match item {
            FinishedContextProjection::Message(block) => {
                self.resources.handle.print_output("agent-response", block);
            }
            FinishedContextProjection::ToolCall {
                call_id,
                name,
                admitted: true,
            } => {
                self.render_tool_call_placeholder(&call_id, &name, summary_block_id);
            }
            FinishedContextProjection::ToolCall {
                admitted: false, ..
            } => {}
            FinishedContextProjection::Compaction(block) => {
                self.resources
                    .handle
                    .print_output("compaction-completed", block);
            }
        }
    }

    fn render_tool_call_placeholder(
        &mut self,
        call_id: &tau_proto::ToolCallId,
        name: &tau_proto::ToolName,
        summary_block_id: Option<tau_cli_term::BlockId>,
    ) {
        if self.transcript.runtime.tool_calls.contains_key(call_id) {
            return;
        }
        let history_id = self.resources.handle.new_block(
            format!("tool-call-history:{name}:{call_id}"),
            Self::empty_block(),
        );
        self.resources.handle.push_history(history_id);
        self.transcript.runtime.tool_calls.insert(
            call_id.clone(),
            ToolCallState {
                history_block_id: Some(history_id),
                summary_block_id,
                is_main_delegate: name.as_str() == AGENT_START_TOOL_NAME,
                ..ToolCallState::default()
            },
        );
    }
}
