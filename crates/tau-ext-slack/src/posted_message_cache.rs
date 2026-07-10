//! Bounded ownership cache for Slack posts created by registered agents.

use std::collections::{HashMap, VecDeque};

use tau_proto::AgentId;

/// Stable identity of one Slack message within a conversation.
#[derive(Clone, Eq, Hash, PartialEq)]
pub(super) struct PostedMessageKey {
    /// Slack conversation containing the message.
    channel_id: String,
    /// Slack timestamp serving as the message id.
    message_ts: String,
}

impl PostedMessageKey {
    /// Construct a message key from validated Slack metadata.
    pub(super) fn new(channel_id: &str, message_ts: &str) -> Self {
        Self {
            channel_id: channel_id.to_owned(),
            message_ts: message_ts.to_owned(),
        }
    }
}

/// Agent ownership and thread context retained for one bridge-authored post.
#[derive(Clone)]
pub(super) struct PostedMessageOwner {
    /// Registered agent that created the post through `slack_send`.
    pub(super) agent_id: AgentId,
    /// Optional thread root returned by Slack.
    pub(super) thread_ts: Option<String>,
}

/// Bounded insertion-ordered map of bridge-authored Slack post identities.
pub(super) struct PostedMessageCache {
    /// Maximum number of identities retained.
    capacity: usize,
    /// Ownership keyed by semantic Slack message identity.
    owners: HashMap<PostedMessageKey, PostedMessageOwner>,
    /// Insertion order used to evict old identities.
    order: VecDeque<PostedMessageKey>,
}

impl PostedMessageCache {
    /// Create an empty cache with the requested hard capacity.
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            owners: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Insert or update ownership and evict the oldest identities as needed.
    pub(super) fn insert(&mut self, key: PostedMessageKey, owner: PostedMessageOwner) {
        if self.owners.insert(key.clone(), owner).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.owners.remove(&old);
            }
        }
    }

    /// Return ownership for an exact Slack message identity.
    pub(super) fn get(&self, key: &PostedMessageKey) -> Option<&PostedMessageOwner> {
        self.owners.get(key)
    }

    /// Forget every post owned by one agent while preserving synchronization.
    pub(super) fn remove_agent(&mut self, agent_id: &AgentId) {
        self.owners.retain(|_, owner| &owner.agent_id != agent_id);
        self.order.retain(|key| self.owners.contains_key(key));
    }

    /// Forget all post identities.
    pub(super) fn clear(&mut self) {
        self.owners.clear();
        self.order.clear();
    }
}

impl Default for PostedMessageCache {
    fn default() -> Self {
        Self::new(super::POSTED_MESSAGE_CACHE_SIZE)
    }
}
