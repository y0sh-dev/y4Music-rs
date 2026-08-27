//! `/serverplaylist_*` -- shared, server-wide playlist commands.

use poise::serenity_prelude as serenity;
use serenity::Mentionable;

use crate::commands::playback::{enqueue_multiple_known, ensure_call};
use crate::models::Playlist;
use crate::pagination;
use crate::playlist::{self, Scope};
use crate::ytdlp::TrackStream;
use crate::{Context, Error};

/// Whether the invoking member has the `Administrator` permission. Backs
/// `serverplaylist_create`, which is restricted to server admins.
async fn is_admin(ctx: &Context<'_>) -> Result<bool, Error> {
    let (Some(member), Some(guild)) = (ctx.author_member().await, ctx.guild()) else {
        return Ok(false);
    };
    Ok(guild.member_permissions(member.as_ref()).administrator())
}

/// The invoking member's guild role IDs, as `i64`s ready for
/// `playlist::has_permission`.
async fn role_ids(ctx: &Context<'_>) -> Vec<i64> {
    match ctx.author_member().await {
        Some(member) => member.roles.iter().map(|r| r.get() as i64).collect(),
        None => Vec::new(),
    }
}

/// Loads a server playlist and checks whether the invoking user may modify
/// it. Returns `Ok(None)` (after replying) if the playlist doesn't exist or
/// permission is denied.
async fn find_writable(ctx: &Context<'_>, name: &str) -> Result<Option<Playlist>, Error> {
    let guild_id = ctx.guild_id().expect("guild_only").get() as i64;
    let Some(playlist) = playlist::find(&ctx.data().db, Scope::Server, guild_id, name).await?
    else {
        ctx.say(format!("❌ Shared playlist '{name}' not found."))
            .await?;
        return Ok(None);
    };

    if playlist.locked {
        let collaborators = playlist::collaborators(&ctx.data().db, playlist.id).await?;
        let roles = role_ids(ctx).await;
        let user_id = ctx.author().id.get() as i64;
        if !playlist::has_permission(&playlist, user_id, &roles, &collaborators) {
            ctx.say("❌ This playlist is locked. Only the owner or collaborators can modify it.")
                .await?;
            return Ok(None);
        }
    }

    Ok(Some(playlist))
}

/// Loads a server playlist and checks that the invoking user is its owner
/// (or, for `delete` only, a server admin).
async fn find_owned(
    ctx: &Context<'_>,
    name: &str,
    allow_admin: bool,
) -> Result<Option<Playlist>, Error> {
    let guild_id = ctx.guild_id().expect("guild_only").get() as i64;
    let Some(playlist) = playlist::find(&ctx.data().db, Scope::Server, guild_id, name).await?
    else {
        ctx.say(format!("❌ Playlist '{name}' not found.")).await?;
        return Ok(None);
    };

    let user_id = ctx.author().id.get() as i64;
    let is_owner = playlist.owner_id == user_id;
    let admin_ok = allow_admin && is_admin(ctx).await?;
    if !is_owner && !admin_ok {
        let who = if allow_admin {
            "the playlist owner or a server administrator"
        } else {
            "the playlist owner"
        };
        ctx.say(format!("❌ This action can only be performed by {who}."))
            .await?;
        return Ok(None);
    }

    Ok(Some(playlist))
}

/// Loads a server playlist and checks that the invoking user has
/// `has_permission` on it (owner, a user collaborator, or a role
/// collaborator), regardless of whether the playlist is locked.
async fn find_permitted(ctx: &Context<'_>, name: &str) -> Result<Option<Playlist>, Error> {
    let guild_id = ctx.guild_id().expect("guild_only").get() as i64;
    let Some(playlist) = playlist::find(&ctx.data().db, Scope::Server, guild_id, name).await?
    else {
        ctx.say(format!("❌ Playlist '{name}' not found.")).await?;
        return Ok(None);
    };

    let collaborators = playlist::collaborators(&ctx.data().db, playlist.id).await?;
    let roles = role_ids(ctx).await;
    let user_id = ctx.author().id.get() as i64;
    if !playlist::has_permission(&playlist, user_id, &roles, &collaborators) {
        ctx.say("❌ This action can only be performed by the playlist owner or a collaborator.")
            .await?;
        return Ok(None);
    }

    Ok(Some(playlist))
}

/// Shows server playlists or the contents of one.
#[poise::command(slash_command, guild_only)]
pub async fn serverplaylist_show(
    ctx: Context<'_>,
    #[description = "The name of the playlist to show (optional)."] name: Option<String>,
) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only").get() as i64;

    match name {
        None => {
            let playlists = playlist::list(&ctx.data().db, Scope::Server, guild_id).await?;
            if playlists.is_empty() {
                ctx.say("There are no shared playlists on this server yet.")
                    .await?;
                return Ok(());
            }
            let mut lines = Vec::with_capacity(playlists.len());
            for p in &playlists {
                let count = playlist::tracks(&ctx.data().db, p.id).await?.len();
                let lock_icon = if p.locked { " 🔐" } else { "" };
                lines.push(format!(
                    "📁 **{}**{lock_icon}\n> Owner: <@{}>, Tracks: {count}",
                    p.name, p.owner_id
                ));
            }
            let guild_name = ctx.guild().map(|g| g.name.clone()).unwrap_or_default();
            pagination::send_paginated(
                &ctx,
                format!("Shared Playlists in '{guild_name}'"),
                serenity::Colour::ORANGE,
                lines,
                format!("Total {} playlists", playlists.len()),
                true,
            )
            .await?;
        }
        Some(name) => {
            let Some(playlist) =
                playlist::find(&ctx.data().db, Scope::Server, guild_id, &name).await?
            else {
                ctx.say(format!("❌ Shared playlist '{name}' not found."))
                    .await?;
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
                format!("Server Playlist: {name}"),
                serenity::Colour::ORANGE,
                lines,
                format!("Total {} tracks", tracks.len()),
                true,
            )
            .await?;
        }
    }
    Ok(())
}

/// Plays a shared server playlist.
#[poise::command(slash_command, guild_only)]
pub async fn serverplaylist_play(
    ctx: Context<'_>,
    #[description = "The name of the playlist you want to play."] name: String,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let guild_id = ctx.guild_id().expect("guild_only").get() as i64;

    let Some(playlist) = playlist::find(&ctx.data().db, Scope::Server, guild_id, &name).await?
    else {
        ctx.say(format!(
            "❌ Server playlist '{name}' not found or is empty."
        ))
        .await?;
        return Ok(());
    };
    let tracks = playlist::tracks(&ctx.data().db, playlist.id).await?;
    if tracks.is_empty() {
        ctx.say(format!(
            "❌ Server playlist '{name}' not found or is empty."
        ))
        .await?;
        return Ok(());
    }

    let call = ensure_call(&ctx).await?;
    let count = tracks.len();
    enqueue_multiple_known(&ctx, &call, &tracks, ctx.author().id).await?;

    ctx.say(format!(
        "✅ Queued {count} tracks from the server playlist '{name}'."
    ))
    .await?;
    Ok(())
}

/// Adds a song or YouTube playlist to a shared playlist.
#[poise::command(slash_command, guild_only)]
pub async fn serverplaylist_add(
    ctx: Context<'_>,
    #[description = "The name of the playlist to add to."] playlist_name: String,
    #[description = "Song URL, search term, or YouTube playlist URL."] query: String,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;

    let Some(playlist) = find_writable(&ctx, &playlist_name).await? else {
        return Ok(());
    };

    let mut stream = match TrackStream::spawn(&query).await {
        Ok(s) => s,
        Err(e) => {
            ctx.say(e.to_string()).await?;
            return Ok(());
        }
    };

    let result =
        playlist::import_tracks_from_stream(&ctx.data().db, playlist.id, &mut stream).await?;

    if result.imported == 0 {
        let message = match stream.finish().await {
            Err(e) => e.to_string(),
            Ok(()) => "❌ No addable tracks were found for that URL.".to_string(),
        };
        ctx.say(message).await?;
        return Ok(());
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
    ctx.say(message).await?;
    Ok(())
}

/// Removes a specific track from a shared playlist.
#[poise::command(slash_command, guild_only)]
pub async fn serverplaylist_remove_track(
    ctx: Context<'_>,
    #[description = "The name of the playlist."] playlist_name: String,
    #[description = "The number of the track to remove."] number: i64,
) -> Result<(), Error> {
    // Deleting a track is destructive and irreversible, unlike adding one --
    // `find_permitted` requires owner/collaborator status regardless of the
    // playlist's lock state, so an unlocked shared playlist can't have its
    // tracks wiped by an arbitrary member.
    let Some(playlist) = find_permitted(&ctx, &playlist_name).await? else {
        return Ok(());
    };
    match playlist::remove_track(&ctx.data().db, playlist.id, number).await? {
        Some(track) => ctx.say(format!("🗑️ Removed **{}**.", track.title)).await?,
        None => ctx.say("❌ Invalid number.").await?,
    };
    Ok(())
}

/// [Admin] Creates a new shared server playlist.
#[poise::command(slash_command, guild_only)]
pub async fn serverplaylist_create(
    ctx: Context<'_>,
    #[description = "The name of the new playlist."] name: String,
) -> Result<(), Error> {
    if !is_admin(&ctx).await? {
        ctx.say("❌ This command can only be used by server administrators.")
            .await?;
        return Ok(());
    }
    let guild_id = ctx.guild_id().expect("guild_only").get() as i64;
    let user_id = ctx.author().id.get() as i64;
    let message = playlist::create(&ctx.data().db, Scope::Server, &name, guild_id, user_id).await?;
    ctx.send(
        poise::CreateReply::default()
            .content(message)
            .ephemeral(true),
    )
    .await?;
    Ok(())
}

/// [Owner/Admin] Deletes a shared playlist.
#[poise::command(slash_command, guild_only)]
pub async fn serverplaylist_delete(
    ctx: Context<'_>,
    #[description = "The name of the playlist to delete."] name: String,
) -> Result<(), Error> {
    let Some(playlist) = find_owned(&ctx, &name, true).await? else {
        return Ok(());
    };
    playlist::delete(&ctx.data().db, playlist.id).await?;
    ctx.say(format!("✅ Deleted playlist '{name}'.")).await?;
    Ok(())
}

/// [Owner] Renames a shared playlist.
#[poise::command(slash_command, guild_only)]
pub async fn serverplaylist_rename(
    ctx: Context<'_>,
    #[description = "The current name of the playlist."] playlist_name: String,
    #[description = "The new name for the playlist."] new_name: String,
) -> Result<(), Error> {
    let Some(playlist) = find_owned(&ctx, &playlist_name, false).await? else {
        return Ok(());
    };
    let guild_id = ctx.guild_id().expect("guild_only").get() as i64;
    if playlist::find(&ctx.data().db, Scope::Server, guild_id, &new_name)
        .await?
        .is_some()
    {
        ctx.say(format!("❌ A playlist named '{new_name}' already exists."))
            .await?;
        return Ok(());
    }
    if playlist::rename(&ctx.data().db, playlist.id, &new_name).await? {
        ctx.say(format!(
            "✅ Renamed playlist from '{playlist_name}' to '{new_name}'."
        ))
        .await?;
    } else {
        ctx.say(format!("❌ A playlist named '{new_name}' already exists."))
            .await?;
    }
    Ok(())
}

/// [Owner] Moves a track to a new position in a shared playlist.
#[poise::command(slash_command, guild_only)]
pub async fn serverplaylist_move(
    ctx: Context<'_>,
    #[description = "The playlist name."] playlist_name: String,
    #[description = "Current track number."] from_number: i64,
    #[description = "New track number."] to_number: i64,
) -> Result<(), Error> {
    let Some(playlist) = find_permitted(&ctx, &playlist_name).await? else {
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

/// [Owner] Toggles the lock on a shared playlist.
#[poise::command(slash_command, guild_only)]
pub async fn serverplaylist_lock(
    ctx: Context<'_>,
    #[description = "The name of the playlist to lock/unlock."] name: String,
) -> Result<(), Error> {
    let Some(playlist) = find_owned(&ctx, &name, false).await? else {
        return Ok(());
    };
    let new_state = playlist::toggle_lock(&ctx.data().db, playlist.id).await?;
    let label = if new_state {
        "locked 🔐"
    } else {
        "unlocked 🔓"
    };
    ctx.say(format!("✅ Playlist '{name}' is now {label}."))
        .await?;
    Ok(())
}

/// [Owner] Adds a user as a collaborator to a playlist.
#[poise::command(slash_command, guild_only)]
pub async fn serverplaylist_add_user(
    ctx: Context<'_>,
    #[description = "The playlist name."] name: String,
    #[description = "The user to add as a collaborator."] user: serenity::User,
) -> Result<(), Error> {
    let Some(playlist) = find_owned(&ctx, &name, false).await? else {
        return Ok(());
    };
    let added =
        playlist::add_collaborator_user(&ctx.data().db, playlist.id, user.id.get() as i64).await?;
    if added {
        ctx.say(format!(
            "✅ Added {} as a collaborator to '{name}'.",
            user.mention()
        ))
        .await?;
    } else {
        ctx.say(format!("ℹ️ {} is already a collaborator.", user.mention()))
            .await?;
    }
    Ok(())
}

/// [Owner] Removes a user collaborator from a playlist.
#[poise::command(slash_command, guild_only)]
pub async fn serverplaylist_remove_user(
    ctx: Context<'_>,
    #[description = "The playlist name."] name: String,
    #[description = "The user to remove from collaborators."] user: serenity::User,
) -> Result<(), Error> {
    let Some(playlist) = find_owned(&ctx, &name, false).await? else {
        return Ok(());
    };
    let removed =
        playlist::remove_collaborator_user(&ctx.data().db, playlist.id, user.id.get() as i64)
            .await?;
    if removed {
        ctx.say(format!(
            "✅ Removed {} from collaborators of '{name}'.",
            user.mention()
        ))
        .await?;
    } else {
        ctx.say(format!("ℹ️ {} is not a collaborator.", user.mention()))
            .await?;
    }
    Ok(())
}

/// [Owner] Adds a role as a collaborator to a playlist.
#[poise::command(slash_command, guild_only)]
pub async fn serverplaylist_add_role(
    ctx: Context<'_>,
    #[description = "The playlist name."] name: String,
    #[description = "The role to add as a collaborator."] role: serenity::Role,
) -> Result<(), Error> {
    let Some(playlist) = find_owned(&ctx, &name, false).await? else {
        return Ok(());
    };
    let added =
        playlist::add_collaborator_role(&ctx.data().db, playlist.id, role.id.get() as i64).await?;
    if added {
        ctx.say(format!(
            "✅ Added the role '{}' as a collaborator to '{name}'.",
            role.name
        ))
        .await?;
    } else {
        ctx.say(format!(
            "ℹ️ The role '{}' is already a collaborator.",
            role.name
        ))
        .await?;
    }
    Ok(())
}

/// [Owner] Removes a role collaborator from a playlist.
#[poise::command(slash_command, guild_only)]
pub async fn serverplaylist_remove_role(
    ctx: Context<'_>,
    #[description = "The playlist name."] name: String,
    #[description = "The role to remove from collaborators."] role: serenity::Role,
) -> Result<(), Error> {
    let Some(playlist) = find_owned(&ctx, &name, false).await? else {
        return Ok(());
    };
    let removed =
        playlist::remove_collaborator_role(&ctx.data().db, playlist.id, role.id.get() as i64)
            .await?;
    if removed {
        ctx.say(format!(
            "✅ Removed the role '{}' from collaborators of '{name}'.",
            role.name
        ))
        .await?;
    } else {
        ctx.say(format!(
            "ℹ️ The role '{}' is not a collaborator.",
            role.name
        ))
        .await?;
    }
    Ok(())
}

/// [Owner] Transfers ownership of a playlist to another user.
#[poise::command(slash_command, guild_only)]
pub async fn serverplaylist_transfer(
    ctx: Context<'_>,
    #[description = "The playlist name."] name: String,
    #[description = "The user to become the new owner."] user: serenity::User,
) -> Result<(), Error> {
    if user.bot {
        ctx.say("❌ Cannot transfer ownership to a bot.").await?;
        return Ok(());
    }
    let Some(playlist) = find_owned(&ctx, &name, false).await? else {
        return Ok(());
    };
    if playlist.owner_id == user.id.get() as i64 {
        ctx.say("ℹ️ You cannot transfer ownership to yourself.")
            .await?;
        return Ok(());
    }
    playlist::transfer_ownership(&ctx.data().db, playlist.id, user.id.get() as i64).await?;
    ctx.say(format!(
        "✅ Transferred ownership of playlist '{name}' to {}.",
        user.mention()
    ))
    .await?;
    Ok(())
}
