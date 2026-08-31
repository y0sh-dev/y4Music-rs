//! `/search` -- interactive YouTube search with pagination and a track
//! picker, plus the component-interaction handling it needs.
//!
//! In-flight results live in `Data::search_sessions`, keyed by the
//! ephemeral reply's message ID, and are swept when stale by `sweep_stale`.

use std::time::{Duration, Instant};

use dashmap::DashMap;
use poise::serenity_prelude as serenity;

use crate::commands::playback::{enqueue_known_raw, ensure_call_raw};
use crate::ytdlp::{self, TrackInfo};
use crate::{Context, Data, Error};

const PAGE_SIZE: usize = 10;

/// How long an idle search session is kept around before `sweep_stale`
/// reclaims it.
pub const SESSION_MAX_AGE: Duration = Duration::from_secs(15 * 60);

/// One in-flight `/search` result set, keyed by its message ID.
pub struct SearchSession {
    pub query: String,
    pub tracks: Vec<TrackInfo>,
    pub created_at: Instant,
}

pub type SearchSessions = DashMap<serenity::MessageId, SearchSession>;

/// Drops search sessions older than `SESSION_MAX_AGE`. Intended to be
/// called periodically (see `main.rs`'s background sweep task).
pub fn sweep_stale(sessions: &SearchSessions) {
    let before = sessions.len();
    sessions.retain(|_, session| session.created_at.elapsed() < SESSION_MAX_AGE);
    let removed = before - sessions.len();
    if removed > 0 {
        tracing::debug!("Swept {removed} stale search session(s)");
    }
}

mod ids {
    pub const SELECT: &str = "search:select";
    // Distinct prefixes for Prev/Next so their `custom_id`s never collide --
    // with a single page, both buttons would otherwise target page 0 and
    // produce an identical `custom_id`, which Discord rejects with 400.
    pub const PREFIX_PREV: &str = "search:prev:";
    pub const PREFIX_NEXT: &str = "search:next:";

    pub fn prev(n: usize) -> String {
        format!("{PREFIX_PREV}{n}")
    }

    pub fn next(n: usize) -> String {
        format!("{PREFIX_NEXT}{n}")
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
}

fn format_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

fn total_pages(tracks: &[TrackInfo]) -> usize {
    tracks.len().div_ceil(PAGE_SIZE).max(1)
}

fn build_embed(query: &str, tracks: &[TrackInfo], page: usize) -> serenity::CreateEmbed {
    let page = page.min(total_pages(tracks).saturating_sub(1));
    serenity::CreateEmbed::new()
        .title(format!("🔎 Search Results: `{query}`"))
        .description(format!(
            "Found {} tracks. Please select one from the menu below.",
            tracks.len()
        ))
        .footer(serenity::CreateEmbedFooter::new(format!(
            "Page {}/{}",
            page + 1,
            total_pages(tracks)
        )))
        .color(serenity::Colour::RED)
}

fn build_components(tracks: &[TrackInfo], page: usize) -> Vec<serenity::CreateActionRow> {
    let page = page.min(total_pages(tracks).saturating_sub(1));
    let start = (page * PAGE_SIZE).min(tracks.len());
    let end = (start + PAGE_SIZE).min(tracks.len());

    let options: Vec<serenity::CreateSelectMenuOption> = tracks[start..end]
        .iter()
        .enumerate()
        .map(|(i, track)| {
            let index = start + i;
            let uploader = track.uploader.as_deref().unwrap_or("Unknown");
            let description = format!("👤 {uploader} | 🕒 {}", format_duration(track.duration));
            serenity::CreateSelectMenuOption::new(
                truncate(&format!("{}. {}", index + 1, track.title), 100),
                index.to_string(),
            )
            .description(truncate(&description, 100))
        })
        .collect();

    let select = serenity::CreateSelectMenu::new(
        ids::SELECT,
        serenity::CreateSelectMenuKind::String { options },
    )
    .placeholder("Select a track to play...");

    let pages = total_pages(tracks);
    let prev = serenity::CreateButton::new(ids::prev(page.saturating_sub(1)))
        .label("⏪ Previous")
        .style(serenity::ButtonStyle::Secondary)
        .disabled(page == 0);
    let next = serenity::CreateButton::new(ids::next((page + 1).min(pages.saturating_sub(1))))
        .label("Next ⏩")
        .style(serenity::ButtonStyle::Secondary)
        .disabled(page + 1 >= pages);

    vec![
        serenity::CreateActionRow::SelectMenu(select),
        serenity::CreateActionRow::Buttons(vec![prev, next]),
    ]
}

/// Search for a song on YouTube and choose from the results.
#[poise::command(slash_command, guild_only)]
pub async fn search(
    ctx: Context<'_>,
    #[description = "Keywords to search for."] query: String,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;

    let tracks = match ytdlp::search(&query, 20).await {
        Ok(t) => t,
        Err(e) => {
            ctx.say(e.to_string()).await?;
            return Ok(());
        }
    };
    if tracks.is_empty() {
        ctx.say(format!("❌ No results were found for `{query}`."))
            .await?;
        return Ok(());
    }

    let embed = build_embed(&query, &tracks, 0);
    let components = build_components(&tracks, 0);
    let reply = ctx
        .send(
            poise::CreateReply::default()
                .embed(embed)
                .components(components),
        )
        .await?;
    let message_id = reply.message().await?.id;

    ctx.data().search_sessions.insert(
        message_id,
        SearchSession {
            query,
            tracks,
            created_at: Instant::now(),
        },
    );

    Ok(())
}

/// Handles page-navigation and track-selection presses on a `/search`
/// result message. Wired up from `main.rs`'s `FrameworkOptions::event_handler`.
pub async fn handle_component_interaction(
    ctx: &serenity::Context,
    interaction: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let custom_id = interaction.data.custom_id.clone();

    if custom_id == ids::SELECT {
        handle_select(ctx, interaction, data).await
    } else if let Some(rest) = custom_id
        .strip_prefix(ids::PREFIX_PREV)
        .or_else(|| custom_id.strip_prefix(ids::PREFIX_NEXT))
    {
        let page: usize = rest.parse().unwrap_or(0);
        handle_page(ctx, interaction, data, page).await
    } else {
        Ok(())
    }
}

async fn handle_page(
    ctx: &serenity::Context,
    interaction: &serenity::ComponentInteraction,
    data: &Data,
    page: usize,
) -> Result<(), Error> {
    let Some(session) = data.search_sessions.get(&interaction.message.id) else {
        interaction
            .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
            .await?;
        return Ok(());
    };

    // Build the response, then drop the DashMap guard before awaiting.
    let embed = build_embed(&session.query, &session.tracks, page);
    let components = build_components(&session.tracks, page);
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

async fn handle_select(
    ctx: &serenity::Context,
    interaction: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let serenity::ComponentInteractionDataKind::StringSelect { values } = &interaction.data.kind
    else {
        return Ok(());
    };
    let Some(index) = values.first().and_then(|v| v.parse::<usize>().ok()) else {
        return Ok(());
    };

    let track = {
        let Some(session) = data.search_sessions.get(&interaction.message.id) else {
            interaction
                .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
                .await?;
            return Ok(());
        };
        let Some(track) = session.tracks.get(index) else {
            return Ok(());
        };
        track.clone()
    };

    let Some(guild_id) = interaction.guild_id else {
        return Ok(());
    };

    let voice_channel_id = ctx
        .cache
        .guild(guild_id)
        .and_then(|guild| guild.voice_states.get(&interaction.user.id)?.channel_id);

    let Some(voice_channel_id) = voice_channel_id else {
        let response = serenity::CreateInteractionResponseMessage::new()
            .content("You must be in a voice channel first.")
            .ephemeral(true);
        interaction
            .create_response(ctx, serenity::CreateInteractionResponse::Message(response))
            .await?;
        return Ok(());
    };

    interaction
        .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
        .await?;

    let Some(manager) = songbird::get(ctx).await else {
        return Ok(());
    };
    let call = ensure_call_raw(
        ctx.http.clone(),
        ctx.cache.clone(),
        data.ytdlp_extra_args.clone(),
        data.guild_players.clone(),
        manager,
        guild_id,
        voice_channel_id,
        interaction.channel_id,
    )
    .await?;

    enqueue_known_raw(
        &data.ytdlp_extra_args,
        &data.db,
        &data.eq_hifi_filter,
        &call,
        &track.webpage_url,
        &track.title,
        track.duration,
        interaction.user.id,
        interaction.user.display_name().to_string(),
        track.uploader.clone(),
    )
    .await?;

    data.search_sessions.remove(&interaction.message.id);

    let edit = serenity::EditInteractionResponse::new()
        .content(format!("✅ Added **{}** to the queue.", track.title))
        .embeds(Vec::new())
        .components(Vec::new());
    let _ = interaction.edit_response(ctx, edit).await;

    Ok(())
}
