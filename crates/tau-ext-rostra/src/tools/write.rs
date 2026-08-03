//! Serialized authenticated Rostra publication tools.

use std::collections::BTreeSet;
use std::str::FromStr as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::sync::{OnceLock, mpsc};

use rostra_client::{Client, ExternalEventId};
use rostra_core::event::content_kind::PersonasTagsSelector;
use rostra_core::event::{PersonaTag, SocialPost};
use rostra_core::id::RostraIdSecretKey;
use tau_proto::ToolStarted;

use super::{ToolFailure, ToolTextResult, decode_args, parse_identity};
use crate::post_rate_limit::{PostRateLimit, PostRateLimitWindow};
use crate::specs::{
    FOLLOW_TOOL, POST_TOOL, PROFILE_UPDATE_TOOL, REACT_TOOL, UNFOLLOW_TOOL, VOTE_TOOL,
};

const MAX_PERSONA_TAGS: usize = 16;

#[cfg(test)]
/// One deterministic pause immediately before a test post invokes upstream
/// signed publication.
struct TestPublicationGate {
    /// Only this call pauses at the admitted-publication boundary.
    call_id: tau_proto::ToolCallId,
    /// Signals that the operation reached the admission boundary.
    entered: mpsc::Sender<()>,
    /// Allows the test to let upstream publication run.
    release: mpsc::Receiver<()>,
    /// Signals that upstream publication returned its locally stored event.
    committed: mpsc::Sender<()>,
}

#[cfg(test)]
/// Global test-only gate; its call-id match prevents unrelated parallel tests
/// from observing the pause.
static TEST_PUBLICATION_GATE: OnceLock<Mutex<Option<TestPublicationGate>>> = OnceLock::new();

#[cfg(test)]
/// Pause a specified test call immediately before upstream publication.
pub(crate) fn pause_before_test_publication(
    call_id: tau_proto::ToolCallId,
) -> (mpsc::Receiver<()>, mpsc::Sender<()>, mpsc::Receiver<()>) {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (committed_tx, committed_rx) = mpsc::channel();
    let gate = TestPublicationGate {
        call_id,
        entered: entered_tx,
        release: release_rx,
        committed: committed_tx,
    };
    let mut slot = TEST_PUBLICATION_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("test publication gate lock");
    assert!(
        slot.is_none(),
        "only one signed test publication gate may be active"
    );
    *slot = Some(gate);
    (entered_rx, release_tx, committed_rx)
}

#[cfg(test)]
fn take_test_publication_gate(call_id: &tau_proto::ToolCallId) -> Option<TestPublicationGate> {
    let mut slot = TEST_PUBLICATION_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("test publication gate lock");
    if slot.as_ref().is_some_and(|gate| gate.call_id == *call_id) {
        slot.take()
    } else {
        None
    }
}

/// Publish one authenticated operation through the one extension-wide write
/// lane.
pub(crate) async fn handle(
    invoke: &ToolStarted,
    client: &Client,
    secret: RostraIdSecretKey,
    write_lock: Arc<tokio::sync::Mutex<()>>,
    post_rate_limit: PostRateLimit,
    post_rate_limit_window: Arc<Mutex<PostRateLimitWindow>>,
    publication_admitted: Arc<AtomicBool>,
) -> ToolTextResult {
    validate(invoke)?;
    let _write_guard = write_lock.lock().await;
    if matches!(invoke.tool_name.as_str(), POST_TOOL | REACT_TOOL) {
        post_rate_limit_window
            .lock()
            .expect("post rate-limit state lock")
            .reserve(post_rate_limit)
            .map_err(|failure| ToolFailure::rate_limited(failure.retry_after_seconds))?;
    }
    client
        .unlock_active(secret)
        .await
        .map_err(|_| ToolFailure::storage())?;
    // Admission is intentionally before the operation-specific upstream call.
    // Once dispatched, its redb effect cannot be reported reliably after timeout.
    publication_admitted.store(true, Ordering::Release);
    match invoke.tool_name.as_str() {
        POST_TOOL => post(invoke, client, secret).await,
        REACT_TOOL => react(invoke, client, secret).await,
        FOLLOW_TOOL => follow(invoke, client, secret).await,
        UNFOLLOW_TOOL => unfollow(invoke, client, secret).await,
        PROFILE_UPDATE_TOOL => profile_update(invoke, client, secret).await,
        VOTE_TOOL => vote(invoke, client, secret).await,
        _ => Err(ToolFailure::invalid("unknown Rostra write tool")),
    }
}

/// Validate every signed request before it can activate the signing client.
fn validate(invoke: &ToolStarted) -> Result<(), ToolFailure> {
    match invoke.tool_name.as_str() {
        POST_TOOL => {
            let args: PostArgs = decode_args(&invoke.arguments)?;
            validate_body(&args.body)?;
            let reply_to = args
                .reply_to
                .as_deref()
                .map(ExternalEventId::from_str)
                .transpose()
                .map_err(|_| ToolFailure::invalid("`reply_to` is not a valid external event id"))?;
            if SocialPost::is_reaction(&reply_to, &args.body).is_some() {
                return Err(ToolFailure::invalid(
                    "reaction-shaped replies require `rostra_react`",
                ));
            }
            let _ = parse_tags(args.persona_tags)?;
        }
        REACT_TOOL => {
            let args: ReactArgs = decode_args(&invoke.arguments)?;
            let post_id = ExternalEventId::from_str(&args.post_id)
                .map_err(|_| ToolFailure::invalid("`post_id` is not a valid external event id"))?;
            validate_reaction(Some(post_id), &args.reaction)?;
        }
        FOLLOW_TOOL | UNFOLLOW_TOOL => {
            let args: IdentityArgs = decode_args(&invoke.arguments)?;
            let _ = parse_identity(&args.identity)?;
        }
        PROFILE_UPDATE_TOOL => {
            let args: ProfileArgs = decode_args(&invoke.arguments)?;
            validate_profile(&args)?;
        }
        VOTE_TOOL => {
            let args: VoteArgs = decode_args(&invoke.arguments)?;
            let _ = ExternalEventId::from_str(&args.post_id)
                .map_err(|_| ToolFailure::invalid("`post_id` is not a valid external event id"))?;
        }
        _ => return Err(ToolFailure::invalid("unknown Rostra write tool")),
    }
    Ok(())
}

/// Strict social-post request.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PostArgs {
    /// Djot source for the post or reply.
    body: String,
    /// Optional full external ID for the replied-to post.
    reply_to: Option<String>,
    /// Optional bounded list of persona tags.
    #[serde(default)]
    persona_tags: Vec<String>,
}

/// Publish one post or reply.
async fn post(invoke: &ToolStarted, client: &Client, secret: RostraIdSecretKey) -> ToolTextResult {
    let args: PostArgs = decode_args(&invoke.arguments)?;
    validate_body(&args.body)?;
    let reply_to = args
        .reply_to
        .as_deref()
        .map(ExternalEventId::from_str)
        .transpose()
        .map_err(|_| ToolFailure::invalid("`reply_to` is not a valid external event id"))?;
    let tags = parse_tags(args.persona_tags)?;
    #[cfg(test)]
    let test_gate = take_test_publication_gate(&invoke.call_id);
    #[cfg(test)]
    if let Some(gate) = &test_gate {
        gate.entered.send(()).expect("test waits for signed commit");
        tokio::task::block_in_place(|| {
            gate.release.recv().expect("test releases signed commit");
        });
    }
    let event = client
        .publish_event(secret, SocialPost::new_text(args.body, reply_to, tags))
        .call()
        .await
        .map_err(|_| ToolFailure::storage())?;
    #[cfg(test)]
    if let Some(gate) = test_gate {
        gate.committed
            .send(())
            .expect("test waits for signed local commit");
    }
    Ok(result(
        client,
        event.event_id,
        if reply_to.is_some() { "reply" } else { "post" },
    ))
}

/// Strict emoji-reaction request.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReactArgs {
    /// Full external ID of the reacted-to post.
    post_id: String,
    /// Exactly one supported emoji grapheme.
    reaction: String,
}

/// Publish one upstream social-post reaction.
async fn react(invoke: &ToolStarted, client: &Client, secret: RostraIdSecretKey) -> ToolTextResult {
    let args: ReactArgs = decode_args(&invoke.arguments)?;
    let post_id = ExternalEventId::from_str(&args.post_id)
        .map_err(|_| ToolFailure::invalid("`post_id` is not a valid external event id"))?;
    let reply_to = Some(post_id);
    let reaction = validate_reaction(reply_to, &args.reaction)?;
    let event = client
        .social_post(secret, reaction, reply_to, BTreeSet::new())
        .await
        .map_err(|_| ToolFailure::storage())?;
    Ok(result(client, event.event_id, "reaction"))
}

/// Strict identity-only request.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityArgs {
    /// Followee identity.
    identity: String,
}

/// Follow every persona tag for one identity.
async fn follow(
    invoke: &ToolStarted,
    client: &Client,
    secret: RostraIdSecretKey,
) -> ToolTextResult {
    let args: IdentityArgs = decode_args(&invoke.arguments)?;
    let identity = parse_identity(&args.identity)?;
    let event = client
        .follow(secret, identity, PersonasTagsSelector::default())
        .await
        .map_err(|_| ToolFailure::storage())?;
    Ok(result(client, event.event_id, "follow"))
}

/// Stop following one identity.
async fn unfollow(
    invoke: &ToolStarted,
    client: &Client,
    secret: RostraIdSecretKey,
) -> ToolTextResult {
    let args: IdentityArgs = decode_args(&invoke.arguments)?;
    let identity = parse_identity(&args.identity)?;
    let event = client
        .unfollow(secret, identity)
        .await
        .map_err(|_| ToolFailure::storage())?;
    Ok(result(client, event.event_id, "unfollow"))
}

/// Strict text-profile request.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileArgs {
    /// New display name.
    display_name: String,
    /// New profile biography.
    bio: String,
}

/// Replace the effective text profile without supporting an avatar attachment.
async fn profile_update(
    invoke: &ToolStarted,
    client: &Client,
    secret: RostraIdSecretKey,
) -> ToolTextResult {
    let args: ProfileArgs = decode_args(&invoke.arguments)?;
    validate_profile(&args)?;
    let event = client
        .post_social_profile_update(secret, args.display_name, args.bio, None)
        .await
        .map_err(|_| ToolFailure::storage())?;
    Ok(result(client, event.event_id, "profile_update"))
}

/// Strict social-vote request.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VoteArgs {
    /// Full external ID of the voted-on post.
    post_id: String,
    /// Requested effective vote state.
    vote: Vote,
}

/// Supported social-vote values.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum Vote {
    /// Publish an up vote.
    Up,
    /// Publish a down vote.
    Down,
    /// Publish a neutral effective vote.
    Clear,
}

/// Publish one social-vote state.
async fn vote(invoke: &ToolStarted, client: &Client, secret: RostraIdSecretKey) -> ToolTextResult {
    let args: VoteArgs = decode_args(&invoke.arguments)?;
    let post_id = ExternalEventId::from_str(&args.post_id)
        .map_err(|_| ToolFailure::invalid("`post_id` is not a valid external event id"))?;
    let upvote = match args.vote {
        Vote::Up => Some(true),
        Vote::Down => Some(false),
        Vote::Clear => None,
    };
    let event = client
        .set_social_vote(secret, post_id, upvote)
        .await
        .map_err(|_| ToolFailure::storage())?;
    Ok(result(client, event.event_id, "vote"))
}

/// Validate the bounded Djot source accepted by Tau.
pub(crate) fn validate_body(body: &str) -> Result<(), ToolFailure> {
    if body.is_empty() || crate::MAX_DJOT_BYTES < body.len() {
        return Err(ToolFailure::invalid(
            "`body` must be nonempty and at most 65536 UTF-8 bytes",
        ));
    }
    Ok(())
}

/// Validate an explicit upstream emoji reaction and return its trimmed text.
fn validate_reaction(
    reply_to: Option<ExternalEventId>,
    reaction: &str,
) -> Result<String, ToolFailure> {
    let reaction = reaction.trim();
    if reaction.is_empty()
        || 8 < reaction.len()
        || SocialPost::is_reaction(&reply_to, reaction) != Some(reaction)
    {
        return Err(ToolFailure::invalid(
            "`reaction` must be exactly one supported emoji grapheme of at most 8 UTF-8 bytes",
        ));
    }
    Ok(reaction.to_owned())
}

/// Validate text-profile lengths before activation.
fn validate_profile(args: &ProfileArgs) -> Result<(), ToolFailure> {
    if 100 < args.display_name.len() || 1_000 < args.bio.len() {
        return Err(ToolFailure::invalid(
            "`display_name` and `bio` exceed their 100 and 1000 UTF-8 byte limits",
        ));
    }
    Ok(())
}

/// Parse the bounded explicit persona-tag set.
pub(crate) fn parse_tags(tags: Vec<String>) -> Result<BTreeSet<PersonaTag>, ToolFailure> {
    if MAX_PERSONA_TAGS < tags.len() {
        return Err(ToolFailure::invalid(
            "`persona_tags` contains more than 16 entries",
        ));
    }
    tags.into_iter()
        .map(|tag| {
            PersonaTag::new(tag)
                .map_err(|_| ToolFailure::invalid("`persona_tags` contains an invalid tag"))
        })
        .collect()
}

/// Format the stable local-commit acknowledgement for one signed event.
fn result(client: &Client, event_id: rostra_core::EventId, operation: &str) -> String {
    serde_json::json!({
        "identity": client.rostra_id().to_string(),
        "event_id": ExternalEventId::new(client.rostra_id(), event_id).to_string(),
        "operation": operation,
        "local_state": "stored",
        "publication": "asynchronous_best_effort"
    })
    .to_string()
}
