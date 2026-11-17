//! `/playlist_*` -- personal playlist commands.

use poise::serenity_prelude as serenity;

use crate::commands::playback::{enqueue_multiple_known, ensure_call};
use crate::pagination;
use crate::playlist::{self, Scope};
use crate::ytdlp::TrackStream;
use crate::{Context, Error};

/// Creates a new personal playlist.
#[poise::command(slash_command, guild_only)]
pub async fn playlist_create(
    ctx: Context<'_>,
    #[description = "The name of the new playlist."] name: String,
) -> Result<(), Error> {
    let user_id = ctx.author().id.get() as i64;
    let message = playlist::create(&ctx.data().db, Scope::Solo, &name, user_id, user_id).await?;
    ctx.send(
        poise::CreateReply::default()
            .content(message)
            .ephemeral(true),
    )
    .await?;
    Ok(())
}

/// Adds a song or a YouTube playlist to your playlist.
#[poise::command(slash_command, guild_only)]
pub async fn playlist_add(
    ctx: Context<'_>,
    #[description = "The name of the playlist to add to."] playlist_name: String,
    #[description = "A song URL, search term, or YouTube playlist URL."] query: String,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let user_id = ctx.author().id.get() as i64;

    let Some(playlist) =
        playlist::find(&ctx.data().db, Scope::Solo, user_id, &playlist_name).await?
    else {
        ctx.say(format!("❌ Playlist '{playlist_name}' not found."))
            .await?;
        return Ok(());
    };

    // A playlist URL either links this playlist to a source (first import)
    // or is rejected in favour of `/playlist_sync` -- bulk-adding it again
    // would duplicate the whole source instead of mirroring it.
    let mark_synced = playlist::is_playlist_url(&query);
    if mark_synced && let Some(existing) = playlist.source_url.as_deref() {
        let message = if playlist::same_source_url(existing, &query) {
            format!(
                "ℹ️ Playlist '{playlist_name}' already mirrors that source. \
                 Use `/playlist_sync` to apply upstream changes."
            )
        } else {
            format!(
                "❌ Playlist '{playlist_name}' already mirrors a different source ({existing}). \
                 Use `/playlist_sync` with the new URL to switch it."
            )
        };
        ctx.say(message).await?;
        return Ok(());
    }

    let mut stream = match TrackStream::spawn(&query).await {
        Ok(s) => s,
        Err(e) => {
            ctx.say(e.to_string()).await?;
            return Ok(());
        }
    };

    let result =
        playlist::import_tracks_from_stream(&ctx.data().db, playlist.id, &mut stream, mark_synced)
            .await?;

    if result.imported == 0 {
        let message = match stream.finish().await {
            Err(e) => e.to_string(),
            Ok(()) => {
                if mark_synced {
                    playlist::set_source_url(&ctx.data().db, playlist.id, Some(query.trim()))
                        .await?;
                }
                "❌ No addable tracks were found for that URL.".to_string()
            }
        };
        ctx.say(message).await?;
        return Ok(());
    }

    if mark_synced {
        playlist::set_source_url(&ctx.data().db, playlist.id, Some(query.trim())).await?;
    }

    let mut message = format!(
        "✅ Added {} track{} to playlist '{playlist_name}'.",
        result.imported,
        if result.imported == 1 { "" } else { "s" }
    );
    if result.truncated {
        message.push_str(&format!(
            "\n⚠️ This source has more than {} tracks; only the first {} were added.",
            playlist::MAX_BULK_ADD,
            playlist::MAX_BULK_ADD
        ));
    }
    if mark_synced {
        message.push_str("\n🔗 Linked this playlist to that source for future `/playlist_sync`.");
    }
    ctx.say(message).await?;
    Ok(())
}

/// Mirrors a personal playlist onto its linked external source.
///
/// Preserves tracks added explicitly.
#[poise::command(slash_command, guild_only)]
pub async fn playlist_sync(
    ctx: Context<'_>,
    #[description = "The name of the playlist to sync."] name: String,
    #[description = "Playlist URL to mirror (defaults to the linked source)."] source_url: Option<
        String,
    >,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let user_id = ctx.author().id.get() as i64;

    let Some(playlist) = playlist::find(&ctx.data().db, Scope::Solo, user_id, &name).await? else {
        ctx.say(format!("❌ Playlist '{name}' not found.")).await?;
        return Ok(());
    };

    let explicit_source = source_url.is_some();
    let target = match source_url {
        Some(url) => url,
        None => match playlist.source_url.clone() {
            Some(url) => url,
            None => {
                ctx.say(format!(
                    "❌ Playlist '{name}' has no linked source. Provide a playlist URL."
                ))
                .await?;
                return Ok(());
            }
        },
    };

    let stream = match TrackStream::spawn(&target).await {
        Ok(s) => s,
        Err(e) => {
            ctx.say(e.to_string()).await?;
            return Ok(());
        }
    };

    let result = match playlist::sync_tracks_from_stream(&ctx.data().db, playlist.id, stream).await
    {
        Ok(result) => result,
        Err(e) => {
            ctx.say(format!(
                "❌ Sync failed; existing tracks were left unchanged: {e}"
            ))
            .await?;
            return Ok(());
        }
    };

    if explicit_source {
        playlist::set_source_url(&ctx.data().db, playlist.id, Some(target.trim())).await?;
    }

    let mut message = format!(
        "✅ Synced playlist '{name}': {} added, {} removed, {} explicit track{} preserved.",
        result.added,
        result.removed,
        result.preserved_explicit,
        if result.preserved_explicit == 1 {
            ""
        } else {
            "s"
        }
    );
    if result.truncated {
        message.push_str(&format!(
            "\n⚠️ This source has more than {} tracks; only the first {} were considered. \
             Existing synced tracks were not removed from an incomplete listing.",
            playlist::MAX_BULK_ADD,
            playlist::MAX_BULK_ADD
        ));
    }
    ctx.say(message).await?;
    Ok(())
}

/// Plays a specified personal playlist.
#[poise::command(slash_command, guild_only)]
pub async fn playlist_play(
    ctx: Context<'_>,
    #[description = "The name of the playlist you want to play."] name: String,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let user_id = ctx.author().id.get() as i64;

    let Some(playlist) = playlist::find(&ctx.data().db, Scope::Solo, user_id, &name).await? else {
        ctx.say(format!("❌ Playlist '{name}' not found or is empty."))
            .await?;
        return Ok(());
    };
    let tracks = playlist::tracks(&ctx.data().db, playlist.id).await?;
    if tracks.is_empty() {
        ctx.say(format!("❌ Playlist '{name}' not found or is empty."))
            .await?;
        return Ok(());
    }

    let call = ensure_call(&ctx).await?;
    let count = tracks.len();
    enqueue_multiple_known(&ctx, &call, &tracks, ctx.author().id).await?;

    ctx.say(format!("🔄 Queued {count} tracks from playlist '{name}'."))
        .await?;
    Ok(())
}

/// Shows your playlists or the contents of one.
#[poise::command(slash_command, guild_only)]
pub async fn playlist_show(
    ctx: Context<'_>,
    #[description = "The name of the playlist to show (optional)."] name: Option<String>,
) -> Result<(), Error> {
    let user_id = ctx.author().id.get() as i64;

    match name {
        None => {
            let playlists = playlist::list(&ctx.data().db, Scope::Solo, user_id).await?;
            if playlists.is_empty() {
                ctx.say("You have no personal playlists.").await?;
                return Ok(());
            }
            let mut lines = Vec::with_capacity(playlists.len());
            for p in &playlists {
                let count = playlist::tracks(&ctx.data().db, p.id).await?.len();
                lines.push(format!("📁 **{}** ({count} tracks)", p.name));
            }
            pagination::send_paginated(
                &ctx,
                format!("{}'s Playlists", ctx.author().display_name()),
                serenity::Colour::PURPLE,
                lines,
                format!("Total {} playlists", playlists.len()),
                true,
            )
            .await?;
        }
        Some(name) => {
            let Some(playlist) =
                playlist::find(&ctx.data().db, Scope::Solo, user_id, &name).await?
            else {
                ctx.say(format!("❌ Playlist '{name}' not found.")).await?;
                return Ok(());
            };
            let tracks = playlist::tracks(&ctx.data().db, playlist.id).await?;
            let lines = tracks
                .iter()
                .enumerate()
                .map(|(i, t)| format!("`{}.` **{}**", i + 1, t.title))
                .collect();
            pagination::send_paginated(
                &ctx,
                format!("Playlist: {name}"),
                serenity::Colour::PURPLE,
                lines,
                format!("Total {} tracks", tracks.len()),
                true,
            )
            .await?;
        }
    }
    Ok(())
}

/// Deletes an existing personal playlist.
#[poise::command(slash_command, guild_only)]
pub async fn playlist_delete(
    ctx: Context<'_>,
    #[description = "The name of the playlist to delete."] name: String,
) -> Result<(), Error> {
    let user_id = ctx.author().id.get() as i64;
    let Some(playlist) = playlist::find(&ctx.data().db, Scope::Solo, user_id, &name).await? else {
        ctx.say(format!("❌ Playlist '{name}' not found.")).await?;
        return Ok(());
    };
    playlist::delete(&ctx.data().db, playlist.id).await?;
    ctx.say(format!("✅ Deleted playlist '{name}'.")).await?;
    Ok(())
}

/// Removes a specific track from a playlist.
#[poise::command(slash_command, guild_only)]
pub async fn playlist_remove_track(
    ctx: Context<'_>,
    #[description = "The name of the playlist."] playlist_name: String,
    #[description = "The number of the track to remove."] number: i64,
) -> Result<(), Error> {
    let user_id = ctx.author().id.get() as i64;
    let Some(playlist) =
        playlist::find(&ctx.data().db, Scope::Solo, user_id, &playlist_name).await?
    else {
        ctx.say(format!("❌ Playlist '{playlist_name}' not found."))
            .await?;
        return Ok(());
    };
    match playlist::remove_track(&ctx.data().db, playlist.id, number).await? {
        Some(track) => {
            ctx.say(format!("🗑️ Removed **{}** from the queue.", track.title))
                .await?
        }
        None => ctx.say("❌ Invalid number.").await?,
    };
    Ok(())
}

/// Moves a track to a new position in a playlist.
#[poise::command(slash_command, guild_only)]
pub async fn playlist_move(
    ctx: Context<'_>,
    #[description = "The name of the playlist."] playlist_name: String,
    #[description = "The current number of the track to move."] from_number: i64,
    #[description = "The new position number for the track."] to_number: i64,
) -> Result<(), Error> {
    let user_id = ctx.author().id.get() as i64;
    let Some(playlist) =
        playlist::find(&ctx.data().db, Scope::Solo, user_id, &playlist_name).await?
    else {
        ctx.say(format!("❌ Playlist '{playlist_name}' not found."))
            .await?;
        return Ok(());
    };
    let moved = playlist::move_track(&ctx.data().db, playlist.id, from_number, to_number).await?;
    if moved {
        ctx.say(format!("✅ Moved track to position {to_number}."))
            .await?;
    } else {
        ctx.say("❌ Invalid number. Check the 'from' and 'to' positions.")
            .await?;
    }
    Ok(())
}

/// Renames a playlist.
#[poise::command(slash_command, guild_only)]
pub async fn playlist_rename(
    ctx: Context<'_>,
    #[description = "The current name of the playlist."] old_name: String,
    #[description = "The new name for the playlist."] new_name: String,
) -> Result<(), Error> {
    let user_id = ctx.author().id.get() as i64;
    let Some(playlist) = playlist::find(&ctx.data().db, Scope::Solo, user_id, &old_name).await?
    else {
        ctx.say(format!("❌ Playlist '{old_name}' not found."))
            .await?;
        return Ok(());
    };
    if playlist::find(&ctx.data().db, Scope::Solo, user_id, &new_name)
        .await?
        .is_some()
    {
        ctx.say(format!("❌ A playlist named '{new_name}' already exists."))
            .await?;
        return Ok(());
    }
    if playlist::rename(&ctx.data().db, playlist.id, &new_name).await? {
        ctx.say(format!(
            "✅ Renamed playlist from '{old_name}' to '{new_name}'."
        ))
        .await?;
    } else {
        ctx.say(format!("❌ A playlist named '{new_name}' already exists."))
            .await?;
    }
    Ok(())
}
