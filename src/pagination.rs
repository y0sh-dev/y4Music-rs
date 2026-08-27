//! Generic Prev/Next pagination for long lists, shared by `/playlist_show`
//! and `/serverplaylist_show`. Session state is kept in
//! `Data::list_sessions`, keyed by message ID.

use std::time::{Duration, Instant};

use dashmap::DashMap;
use poise::serenity_prelude as serenity;

use crate::{Context, Data, Error};

const PAGE_SIZE: usize = 10;

/// How long an idle paginator session is kept around before
/// `sweep_stale` reclaims it. See that function's doc comment.
pub const SESSION_MAX_AGE: Duration = Duration::from_secs(15 * 60);

/// One paginated list: pre-formatted lines (one per item) plus the embed
/// chrome around them.
pub struct ListSession {
    pub title: String,
    pub color: serenity::Colour,
    pub lines: Vec<String>,
    pub footer: String,
    pub created_at: Instant,
}

pub type ListSessions = DashMap<serenity::MessageId, ListSession>;

mod ids {
    // Distinct prefixes for Prev/Next so their `custom_id`s never collide --
    // with a single page, both buttons would otherwise target page 0 and
    // produce an identical `custom_id`, which Discord rejects with 400.
    pub const PREFIX_PREV: &str = "listpage:prev:";
    pub const PREFIX_NEXT: &str = "listpage:next:";
    pub const NOOP: &str = "listpage:noop";

    pub fn prev(n: usize) -> String {
        format!("{PREFIX_PREV}{n}")
    }

    pub fn next(n: usize) -> String {
        format!("{PREFIX_NEXT}{n}")
    }
}

fn total_pages(lines: &[String]) -> usize {
    lines.len().div_ceil(PAGE_SIZE).max(1)
}

fn build_embed(session: &ListSession, page: usize) -> serenity::CreateEmbed {
    let page = page.min(total_pages(&session.lines).saturating_sub(1));
    let start = (page * PAGE_SIZE).min(session.lines.len());
    let end = (start + PAGE_SIZE).min(session.lines.len());
    let description = if session.lines.is_empty() {
        "This playlist is empty.".to_string()
    } else {
        session.lines[start..end].join("\n")
    };

    serenity::CreateEmbed::new()
        .title(session.title.clone())
        .description(description)
        .footer(serenity::CreateEmbedFooter::new(session.footer.clone()))
        .color(session.color)
}

fn build_components(lines: &[String], page: usize) -> Vec<serenity::CreateActionRow> {
    let pages = total_pages(lines);
    let page = page.min(pages.saturating_sub(1));
    vec![serenity::CreateActionRow::Buttons(vec![
        serenity::CreateButton::new(ids::prev(page.saturating_sub(1)))
            .label("⏪")
            .style(serenity::ButtonStyle::Secondary)
            .disabled(page == 0),
        serenity::CreateButton::new(ids::NOOP)
            .label(format!("Page {}/{pages}", page + 1))
            .style(serenity::ButtonStyle::Secondary)
            .disabled(true),
        serenity::CreateButton::new(ids::next((page + 1).min(pages.saturating_sub(1))))
            .label("⏩")
            .style(serenity::ButtonStyle::Secondary)
            .disabled(page + 1 >= pages),
    ])]
}

/// Sends a new paginated list, and registers its session so later
/// Prev/Next presses can look the full list back up. `ephemeral` controls
/// visibility of the reply -- personal listings (playlists, search
/// results) pass `true`; a shared listing everyone in the channel should
/// see (e.g. `/queue`) passes `false`.
pub async fn send_paginated(
    ctx: &Context<'_>,
    title: String,
    color: serenity::Colour,
    lines: Vec<String>,
    footer: String,
    ephemeral: bool,
) -> Result<(), Error> {
    let session = ListSession {
        title,
        color,
        lines,
        footer,
        created_at: Instant::now(),
    };
    let embed = build_embed(&session, 0);
    let components = build_components(&session.lines, 0);

    let reply = ctx
        .send(
            poise::CreateReply::default()
                .embed(embed)
                .components(components)
                .ephemeral(ephemeral),
        )
        .await?;
    let message_id = reply.message().await?.id;

    ctx.data().list_sessions.insert(message_id, session);
    Ok(())
}

/// Handles a Prev/Next press. Wired up from `main.rs`'s
/// `FrameworkOptions::event_handler`.
pub async fn handle_component_interaction(
    ctx: &serenity::Context,
    interaction: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let Some(rest) = interaction
        .data
        .custom_id
        .strip_prefix(ids::PREFIX_PREV)
        .or_else(|| interaction.data.custom_id.strip_prefix(ids::PREFIX_NEXT))
    else {
        return Ok(());
    };
    let Ok(page) = rest.parse::<usize>() else {
        return Ok(());
    };

    let Some(session) = data.list_sessions.get(&interaction.message.id) else {
        interaction
            .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
            .await?;
        return Ok(());
    };

    // Drop the DashMap guard before the `.await` below so the shard isn't held.
    let embed = build_embed(&session, page);
    let components = build_components(&session.lines, page);
    drop(session);

    let response = serenity::CreateInteractionResponseMessage::new()
        .embed(embed)
        .components(components);
    interaction
        .create_response(
            ctx,
            serenity::CreateInteractionResponse::UpdateMessage(response),
        )
        .await?;
    Ok(())
}

/// Drops sessions older than `SESSION_MAX_AGE`. Intended to be called
/// periodically (see `main.rs`'s background sweep task).
pub fn sweep_stale(sessions: &ListSessions) {
    let before = sessions.len();
    sessions.retain(|_, session| session.created_at.elapsed() < SESSION_MAX_AGE);
    let removed = before - sessions.len();
    if removed > 0 {
        tracing::debug!("Swept {removed} stale pagination session(s)");
    }
}
