//! Bounded timeline-list tool.

use std::collections::HashMap;

use rostra_client::{Client, ExternalEventId, RostraId};
use rostra_core::event::content_kind::PersonasTagsSelector;
use tau_proto::ToolStarted;

use super::{ToolFailure, ToolTextResult, decode_args, parse_identity};
use crate::cursor::{self, Position, Timeline};
use crate::projection::{bounded_output, external, format_tags, sanitize_line};

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
/// Strict timeline-list arguments.
struct Args {
    /// Timeline selector.
    timeline: Timeline,
    /// Required only for an author timeline.
    author: Option<String>,
    /// Opaque continuation from the same filter.
    cursor: Option<String>,
    /// Requested bounded page size.
    limit: Option<usize>,
}

/// List one page from a local timeline.
pub(super) async fn handle(invoke: &ToolStarted, client: &Client) -> ToolTextResult {
    let args: Args = decode_args(&invoke.arguments)?;
    let limit = args.limit.unwrap_or(crate::DEFAULT_PAGE_SIZE);
    if limit == 0 || crate::MAX_PAGE_SIZE < limit {
        return Err(ToolFailure::invalid(format!(
            "`limit` must be between 1 and {}",
            crate::MAX_PAGE_SIZE
        )));
    }
    let author = match (args.timeline, args.author.as_deref()) {
        (Timeline::Author, Some(value)) => Some(parse_identity(value)?),
        (Timeline::Author, None) => {
            return Err(ToolFailure::invalid(
                "`author` is required for the author timeline",
            ));
        }
        (Timeline::Following | Timeline::Network, None) => None,
        (Timeline::Following | Timeline::Network, Some(_)) => {
            return Err(ToolFailure::invalid(
                "`author` is allowed only for the author timeline",
            ));
        }
    };
    let author_key = author.map(|identity| identity.to_string());
    let position = cursor::decode(args.cursor.as_deref(), args.timeline, author_key.as_deref())?;
    let (rows, next) = match args.timeline {
        Timeline::Network => list_network(client, position, limit).await?,
        Timeline::Following | Timeline::Author => {
            list_social(client, args.timeline, author, position, limit).await?
        }
    };
    let next_cursor = next.map_or_else(
        || "-".to_owned(),
        |position| cursor::encode(args.timeline, author_key.as_deref(), position),
    );
    let body = if rows.is_empty() {
        "(no matches found)".to_owned()
    } else {
        rows.join("\n")
    };
    bounded_output(format!(
        "timeline: {}\nnext_cursor: {next_cursor}\nformat: post_id timestamp author reply_count persona_tags excerpt\n\n{}",
        args.timeline.as_str(),
        external("post-list", &body)
    ))
}

async fn list_network(
    client: &Client,
    position: Option<Position>,
    limit: usize,
) -> Result<(Vec<String>, Option<Position>), ToolFailure> {
    let cursor = match position {
        None => None,
        Some(Position::Network(cursor)) => Some(cursor),
        Some(Position::Social(_)) => return Err(ToolFailure::invalid("cursor kind is invalid")),
    };
    let (records, next) = client
        .db()
        .paginate_news_posts_by_rank_rev(cursor, limit)
        .await;
    let rows = records
        .into_iter()
        .map(|record| {
            let post = record.post;
            format!(
                "{} {} {} {} {} {}",
                ExternalEventId::new(post.author, post.event_id),
                post.ts,
                post.author,
                post.reply_count,
                format_tags(post.content.persona_tags()),
                sanitize_line(
                    post.content.djot_content.as_deref().unwrap_or_default(),
                    crate::MAX_EXCERPT_CHARS,
                )
            )
        })
        .collect();
    Ok((rows, next.map(Position::Network)))
}

async fn list_social(
    client: &Client,
    timeline: Timeline,
    author: Option<RostraId>,
    position: Option<Position>,
    limit: usize,
) -> Result<(Vec<String>, Option<Position>), ToolFailure> {
    let cursor = match position {
        None => None,
        Some(Position::Social(cursor)) => Some(cursor),
        Some(Position::Network(_)) => return Err(ToolFailure::invalid("cursor kind is invalid")),
    };
    let following: HashMap<RostraId, PersonasTagsSelector> = if timeline == Timeline::Following {
        client
            .db()
            .get_followees(client.rostra_id())
            .await
            .into_iter()
            .collect()
    } else {
        HashMap::new()
    };
    let (records, next) = client
        .db()
        .paginate_social_posts_rev(cursor, limit, move |record| {
            author.map_or_else(
                || {
                    following.get(&record.author).is_some_and(|selector| {
                        selector.matches_tags(&record.content.persona_tags())
                    })
                },
                |wanted| record.author == wanted,
            )
        })
        .await;
    let rows = records
        .into_iter()
        .map(|record| {
            format!(
                "{} {} {} {} {} {}",
                ExternalEventId::new(record.author, record.event_id),
                record.ts,
                record.author,
                record.reply_count,
                format_tags(record.content.persona_tags()),
                sanitize_line(
                    record.content.djot_content.as_deref().unwrap_or_default(),
                    crate::MAX_EXCERPT_CHARS,
                )
            )
        })
        .collect();
    Ok((rows, next.map(Position::Social)))
}
