//! Bounded asynchronous UI cache for extension-host path completion.
//!
//! Drafts and replacement ranges stay here. Only decoded tokens leave the UI.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use tau_cli_term::{Candidate, RemoteCompleter, TermHandle};
use tau_proto::AgentId;
use tau_shell_interaction::{
    Client, CompletionKind, Context, Operation, PathCandidate, Request, Target,
};

use super::{InputRoutingState, MUTEX_POISONED, WriterHandle, send_frame};

/// Complete local identity of one completion attempt.
#[derive(Clone, PartialEq, Eq)]
struct Draft {
    /// Raw terminal interaction generation, including preview and dismissal.
    generation: u64,
    /// Never transmitted; used only for stale rejection and replacement.
    buffer: String,
    /// Byte cursor, validated at construction.
    cursor: usize,
    /// Selected agent whose extension-owned cwd is used.
    agent: Option<AgentId>,
    /// Selected live process incarnations, not merely configured names.
    targets: Vec<Target>,
    /// Matching policy from the terminal's rule selection.
    fuzzy: bool,
}

/// One bounded correlation awaiting an authenticated response.
struct Pending {
    /// Echoed session and agent binding.
    context: Context,
    /// Process selected when this draft was evaluated.
    target: Target,
    /// UI deadline, independent of executor enumeration bounds.
    deadline: Instant,
}

/// At most one draft, one bounded fanout, and one result page.
#[derive(Default)]
struct State {
    /// Generation of the last lookup, also retained for instance-name menus.
    menu_generation: Option<u64>,
    /// Incremented on every input invalidation, including revisiting old text.
    revision: u64,
    /// Current cache key.
    draft: Option<Draft>,
    /// Pending replies for this revision only.
    pending: HashMap<String, Pending>,
    /// Attributed candidates for the current draft.
    results: Vec<Candidate>,
}

/// Shared UI completion cache with one debouncing worker.
#[derive(Clone)]
pub(super) struct ShellCompletion {
    /// Dropping all UI handles closes the worker wake channel.
    wake: mpsc::SyncSender<()>,
    /// Cache shared with callbacks and authenticated renderer delivery.
    state: Arc<Mutex<State>>,
}

impl ShellCompletion {
    /// Start one bounded worker. It never executes commands or retries
    /// requests.
    pub(super) fn new(client: Arc<Mutex<Client>>, writer: WriterHandle) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let weak = Arc::downgrade(&state);
        let (wake, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            while receiver.recv().is_ok() {
                // Restart the debounce window when another edit wakes us.
                while receiver.recv_timeout(Duration::from_millis(80)).is_ok() {}
                let Some(state) = weak.upgrade() else { break };
                let mut state = state.lock().expect(MUTEX_POISONED);
                let Some(draft) = state.draft.clone() else {
                    continue;
                };
                let Some(token) = token(&draft.buffer, draft.cursor) else {
                    continue;
                };
                let revision = state.revision;
                let requests = {
                    let client = client.lock().expect(MUTEX_POISONED);
                    draft
                        .targets
                        .iter()
                        .take(32)
                        .map(|target| Request {
                            context: client.context(draft.agent.clone()),
                            target: Some(target.clone()),
                            operation: Operation::Complete {
                                token: token.decoded.clone(),
                                matching: if draft.fuzzy {
                                    CompletionKind::PathFuzzy
                                } else {
                                    CompletionKind::Path
                                },
                            },
                        })
                        .collect::<Vec<_>>()
                };
                for request in &requests {
                    state.pending.insert(
                        request.context.request_id.clone(),
                        Pending {
                            context: request.context.clone(),
                            target: request.target.clone().expect("completion target"),
                            deadline: Instant::now() + Duration::from_millis(800),
                        },
                    );
                }
                drop(state);
                for request in requests {
                    // A changed draft can abandon enumeration, never execution.
                    if weak.upgrade().is_none_or(|state| {
                        state.lock().expect(MUTEX_POISONED).revision != revision
                    }) {
                        break;
                    }
                    let _ = send_frame(&writer, &tau_shell_interaction::request_message(&request));
                }
            }
        });
        Self { wake, state }
    }

    /// Invalidate even when the new buffer no longer matches a completion rule.
    pub(super) fn invalidate(&self) {
        let mut state = self.state.lock().expect(MUTEX_POISONED);
        state.revision = state.revision.wrapping_add(1);
        state.draft = None;
        state.pending.clear();
        state.results.clear();
    }

    /// Build an immediate cache lookup; no I/O runs on the terminal thread.
    pub(super) fn completer(
        &self,
        client: Arc<Mutex<Client>>,
        routing: InputRoutingState,
        handle: TermHandle,
    ) -> RemoteCompleter {
        let this = self.clone();
        Arc::new(move |buffer, cursor, fuzzy| {
            let generation = handle.completion_refresh_generation();
            this.state.lock().expect(MUTEX_POISONED).menu_generation = Some(generation);
            if !buffer.is_char_boundary(cursor) {
                return Vec::new();
            }
            let client = client.lock().expect(MUTEX_POISONED);
            let selector_end = buffer.find(char::is_whitespace).unwrap_or(buffer.len());
            if fuzzy.is_none() {
                let bangs = if buffer.starts_with("!!") { "!!" } else { "!" };
                let Some(prefix) = buffer.get(bangs.len()..cursor) else {
                    return Vec::new();
                };
                return client
                    .endpoints()
                    .filter(|endpoint| {
                        endpoint.execute && endpoint.target.instance.as_str().starts_with(prefix)
                    })
                    .take(100)
                    .map(|endpoint| {
                        let value = format!("{bangs}{} ", endpoint.target.instance);
                        Candidate {
                            label: format!("{bangs}{}", endpoint.target.instance),
                            description: "Execute in this extension instance".to_owned(),
                            cursor: value.len(),
                            replacement: format!("{value}{}", buffer[selector_end..].trim_start()),
                            acceptance: None,
                        }
                    })
                    .collect();
            }
            let selected = buffer
                .starts_with('!')
                .then(|| buffer[..selector_end].trim_start_matches('!'));
            let targets = client
                .endpoints()
                .filter(|endpoint| {
                    endpoint.complete
                        && selected.is_none_or(|name| endpoint.target.instance.as_str() == name)
                })
                .take(32)
                .map(|endpoint| endpoint.target.clone())
                .collect();
            drop(client);
            let draft = Draft {
                generation,
                buffer: buffer.to_owned(),
                cursor,
                agent: routing.selected_side_agent_id(),
                targets,
                fuzzy: fuzzy.expect("path mode"),
            };
            let mut state = this.state.lock().expect(MUTEX_POISONED);
            if state.draft.as_ref() != Some(&draft) {
                state.revision = state.revision.wrapping_add(1);
                state.pending.clear();
                state.results.clear();
                state.draft = Some(draft);
                let _ = this.wake.try_send(());
            }
            state.results.clone()
        })
    }

    /// Accept only the outstanding draft's exact authenticated response.
    pub(super) fn observe(
        &self,
        context: Context,
        target: Target,
        candidates: Vec<PathCandidate>,
        handle: &TermHandle,
    ) {
        let generation = self
            .state
            .lock()
            .expect(MUTEX_POISONED)
            .draft
            .as_ref()
            .map(|draft| draft.generation);
        if generation == Some(handle.completion_refresh_generation())
            && self.accept(context, target, candidates)
        {
            handle
                .request_completion_refresh_if_generation(generation.expect("checked generation"));
        }
    }

    /// Refresh only the interaction that last consulted this source; inventory
    /// changes cannot reopen a menu dismissed since that lookup.
    pub(super) fn refresh(&self, handle: &TermHandle) {
        if let Some(generation) = self.state.lock().expect(MUTEX_POISONED).menu_generation {
            handle.request_completion_refresh_if_generation(generation);
        }
    }

    /// Validate before touching the cache; tests exercise this without a
    /// terminal.
    fn accept(&self, context: Context, target: Target, candidates: Vec<PathCandidate>) -> bool {
        let mut state = self.state.lock().expect(MUTEX_POISONED);
        let Some(pending) = state.pending.get(&context.request_id) else {
            return false;
        };
        if pending.context != context
            || pending.target != target
            || Instant::now() >= pending.deadline
        {
            return false;
        }
        state.pending.remove(&context.request_id);
        let Some(draft) = state.draft.as_ref() else {
            return false;
        };
        let Some(token) = token(&draft.buffer, draft.cursor) else {
            return false;
        };
        let results = candidates
            .into_iter()
            .take(100)
            .map(|candidate| {
                let insertion = quote(&candidate.value);
                Candidate {
                    label: candidate.value,
                    description: format!(
                        "{} {}",
                        target.instance,
                        if candidate.directory {
                            "directory"
                        } else {
                            "file"
                        }
                    ),
                    replacement: format!("{}{insertion}{}", token.before, token.after),
                    cursor: token.before.len() + insertion.len(),
                    acceptance: None,
                }
            })
            .collect::<Vec<_>>();
        state.results.extend(results);
        state.results.sort_by(|a, b| {
            a.label
                .cmp(&b.label)
                .then(a.description.cmp(&b.description))
        });
        state.results.truncate(100);
        true
    }
}

/// Local token parsing result, with shell quotes decoded only in the prefix.
struct Token<'a> {
    /// Full-token replacement prefix, not transmitted.
    before: &'a str,
    /// Full-token replacement suffix, not transmitted.
    after: &'a str,
    /// Decoded token bytes up to the cursor.
    decoded: String,
}

/// Find a shell-like token without evaluating expansion or substitutions.
fn token(buffer: &str, cursor: usize) -> Option<Token<'_>> {
    let token = tau_cli_term::completion_token::completion_token(buffer, cursor)?;
    if token.decoded.len() > tau_shell_interaction::TOKEN_LIMIT {
        return None;
    }
    Some(Token {
        before: token.before,
        after: token.after,
        decoded: token.decoded,
    })
}

/// Escape one path as a single shell word without evaluating its contents.
fn quote(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| {
            if ch.is_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | '~') {
                vec![ch]
            } else {
                vec!['\\', ch]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
