//! Renderer-local queued-prompt reconciliation for cancellation broadcasts.

use std::collections::VecDeque;

use super::QueuedUserBlock;
use super::renderer_state::WatchActivityState;
use crate::renderer_handle::RendererHandle;

/// Returns whether the exact queue front is the user projection for `text`.
///
/// Queue records lack provenance, so only their front entry can establish that
/// a submitted or steered prompt is the user projection to promote.
pub(super) fn user_front_matches(
    queued_user_blocks: &VecDeque<QueuedUserBlock>,
    text: &str,
) -> bool {
    queued_user_blocks.front().is_some_and(|queued| {
        queued.text == text && queued.message_class == tau_proto::PromptMessageClass::User
    })
}

/// Reconciles queued projections with the cancellation path for the exact
/// prompt.
///
/// Cancellation before termination is user cancellation and retains hidden
/// internal continuations. Termination before cancellation is side-agent
/// preemption: an unmarked owner loses its whole harness queue, while the
/// completion-before-termination marker identifies the queue-retaining marked
/// owner path.
pub(super) fn reconcile(
    cancel: &tau_proto::UiCancelPrompt,
    watches: &WatchActivityState,
    queued_user_blocks: &mut VecDeque<QueuedUserBlock>,
    handle: &RendererHandle,
) {
    let Some(prompt_id) = cancel.agent_prompt_id.as_ref() else {
        return;
    };
    // A marked side-agent owner has both provider completion and explicit
    // termination before its cancel wakeup, and that path retains its queue.
    // User cancellation publishes cancel before termination even during the
    // post-provider tool phase; ordinary side-agent preemption has no marked
    // provider completion and clears its queue before the wakeup.
    let terminated = watches.terminated_agent_prompts.contains(prompt_id);
    if terminated
        && watches
            .provider_finished_before_termination
            .contains(prompt_id)
    {
        return;
    }
    let mut removed_block_ids = Vec::new();
    queued_user_blocks.retain(|queued| {
        if queued.message_class.is_internal() && !terminated {
            return true;
        }
        removed_block_ids.extend(queued.id);
        false
    });
    let removed_visible_block = !removed_block_ids.is_empty();
    for block_id in removed_block_ids {
        handle.remove_block(block_id);
    }
    if removed_visible_block {
        handle.redraw();
    }
}
