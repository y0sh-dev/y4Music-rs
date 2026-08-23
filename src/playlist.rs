//! Playlist CRUD and permissions, backed by SQLite.

use sqlx::SqlitePool;

use crate::Error;
use crate::models::{Collaborator, Playlist, PlaylistTrack};
use crate::ytdlp::TrackInfo;

/// Whether `err` is a `UNIQUE` constraint violation.
fn is_unique_violation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .is_some_and(|db_err| db_err.is_unique_violation())
}

/// Whether a playlist is personal or shared server-wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Solo,
    Server,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Solo => "solo",
            Scope::Server => "server",
        }
    }
}

/// Looks up a playlist by name. `lookup_id` is a user ID for `Scope::Solo`
/// or a guild ID for `Scope::Server`.
pub async fn find(
    db: &SqlitePool,
    scope: Scope,
    lookup_id: i64,
    name: &str,
) -> Result<Option<Playlist>, Error> {
    let row = match scope {
        Scope::Solo => {
            sqlx::query_as::<_, Playlist>(
                "SELECT id, name, owner_id, guild_id, scope, locked FROM playlists \
             WHERE scope = 'solo' AND owner_id = ? AND name = ?",
            )
            .bind(lookup_id)
            .bind(name)
            .fetch_optional(db)
            .await?
        }
        Scope::Server => {
            sqlx::query_as::<_, Playlist>(
                "SELECT id, name, owner_id, guild_id, scope, locked FROM playlists \
             WHERE scope = 'server' AND guild_id = ? AND name = ?",
            )
            .bind(lookup_id)
            .bind(name)
            .fetch_optional(db)
            .await?
        }
    };
    Ok(row)
}

/// Lists every playlist for a user (solo) or guild (server).
pub async fn list(db: &SqlitePool, scope: Scope, lookup_id: i64) -> Result<Vec<Playlist>, Error> {
    let rows = match scope {
        Scope::Solo => {
            sqlx::query_as::<_, Playlist>(
                "SELECT id, name, owner_id, guild_id, scope, locked FROM playlists \
             WHERE scope = 'solo' AND owner_id = ? ORDER BY name",
            )
            .bind(lookup_id)
            .fetch_all(db)
            .await?
        }
        Scope::Server => {
            sqlx::query_as::<_, Playlist>(
                "SELECT id, name, owner_id, guild_id, scope, locked FROM playlists \
             WHERE scope = 'server' AND guild_id = ? ORDER BY name",
            )
            .bind(lookup_id)
            .fetch_all(db)
            .await?
        }
    };
    Ok(rows)
}

/// Creates a new, empty playlist. `creator_id` is only used for
/// `Scope::Server`, as the owner; for `Scope::Solo` the owner is
/// `lookup_id`.
pub async fn create(
    db: &SqlitePool,
    scope: Scope,
    name: &str,
    lookup_id: i64,
    creator_id: i64,
) -> Result<String, Error> {
    if find(db, scope, lookup_id, name).await?.is_some() {
        return Ok(format!("❌ A playlist named '{name}' already exists."));
    }

    let (owner_id, guild_id) = match scope {
        Scope::Solo => (lookup_id, None),
        Scope::Server => (creator_id, Some(lookup_id)),
    };

    let result = sqlx::query(
        "INSERT INTO playlists (name, owner_id, guild_id, scope, locked) VALUES (?, ?, ?, ?, 0)",
    )
    .bind(name)
    .bind(owner_id)
    .bind(guild_id)
    .bind(scope.as_str())
    .execute(db)
    .await;

    match result {
        Ok(_) => Ok(format!("✅ Created playlist '{name}'.")),
        Err(e) if is_unique_violation(&e) => {
            Ok(format!("❌ A playlist named '{name}' already exists."))
        }
        Err(e) => Err(e.into()),
    }
}

/// Deletes a playlist and (via `ON DELETE CASCADE`) its tracks and
/// collaborator entries.
pub async fn delete(db: &SqlitePool, playlist_id: i64) -> Result<(), Error> {
    sqlx::query("DELETE FROM playlists WHERE id = ?")
        .bind(playlist_id)
        .execute(db)
        .await?;
    Ok(())
}

/// Renames a playlist. Returns `false` if `new_name` was already taken by
/// another playlist in the same scope.
pub async fn rename(db: &SqlitePool, playlist_id: i64, new_name: &str) -> Result<bool, Error> {
    let result = sqlx::query("UPDATE playlists SET name = ? WHERE id = ?")
        .bind(new_name)
        .bind(playlist_id)
        .execute(db)
        .await;

    match result {
        Ok(_) => Ok(true),
        Err(e) if is_unique_violation(&e) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// All tracks in a playlist, in playback/display order.
pub async fn tracks(db: &SqlitePool, playlist_id: i64) -> Result<Vec<PlaylistTrack>, Error> {
    Ok(sqlx::query_as::<_, PlaylistTrack>(
        "SELECT id, playlist_id, url, title, uploader, duration, added_order \
         FROM playlist_tracks WHERE playlist_id = ? ORDER BY added_order",
    )
    .bind(playlist_id)
    .fetch_all(db)
    .await?)
}

/// Appends tracks to the end of a playlist, returning how many were added.
/// Does not itself check permissions -- callers check `has_permission`
/// first.
pub async fn add_tracks(
    db: &SqlitePool,
    playlist_id: i64,
    new_tracks: &[TrackInfo],
) -> Result<usize, Error> {
    let mut tx = db.begin().await?;

    let mut next_order: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(added_order) + 1, 0) FROM playlist_tracks WHERE playlist_id = ?",
    )
    .bind(playlist_id)
    .fetch_one(&mut *tx)
    .await?;

    for track in new_tracks {
        sqlx::query(
            "INSERT INTO playlist_tracks (playlist_id, url, title, uploader, duration, added_order) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(playlist_id)
        .bind(&track.webpage_url)
        .bind(&track.title)
        .bind(&track.uploader)
        .bind(track.duration)
        .bind(next_order)
        .execute(&mut *tx)
        .await?;
        next_order += 1;
    }

    tx.commit().await?;
    Ok(new_tracks.len())
}

/// Removes the `index`-th (1-based) track from a playlist, returning it if
/// it existed.
pub async fn remove_track(
    db: &SqlitePool,
    playlist_id: i64,
    index_1based: i64,
) -> Result<Option<PlaylistTrack>, Error> {
    if index_1based < 1 {
        return Ok(None);
    }

    let track = sqlx::query_as::<_, PlaylistTrack>(
        "SELECT id, playlist_id, url, title, uploader, duration, added_order \
         FROM playlist_tracks WHERE playlist_id = ? ORDER BY added_order LIMIT 1 OFFSET ?",
    )
    .bind(playlist_id)
    .bind(index_1based - 1)
    .fetch_optional(db)
    .await?;

    if let Some(track) = &track {
        sqlx::query("DELETE FROM playlist_tracks WHERE id = ?")
            .bind(track.id)
            .execute(db)
            .await?;
    }

    Ok(track)
}

/// Moves a track from one 1-based position to another, renumbering
/// `added_order` for the whole playlist inside a transaction. Returns
/// `false` if either index was out of range.
pub async fn move_track(
    db: &SqlitePool,
    playlist_id: i64,
    from_1based: i64,
    to_1based: i64,
) -> Result<bool, Error> {
    let mut tx = db.begin().await?;

    let mut ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM playlist_tracks WHERE playlist_id = ? ORDER BY added_order",
    )
    .bind(playlist_id)
    .fetch_all(&mut *tx)
    .await?;

    let from = from_1based - 1;
    let to = to_1based - 1;
    if from < 0 || to < 0 || from as usize >= ids.len() || to as usize >= ids.len() {
        return Ok(false);
    }

    let moved = ids.remove(from as usize);
    ids.insert(to as usize, moved);

    for (position, id) in ids.iter().enumerate() {
        sqlx::query("UPDATE playlist_tracks SET added_order = ? WHERE id = ?")
            .bind(position as i64)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(true)
}

/// All collaborators (users and roles) granted access to a playlist.
pub async fn collaborators(db: &SqlitePool, playlist_id: i64) -> Result<Vec<Collaborator>, Error> {
    Ok(sqlx::query_as::<_, Collaborator>(
        "SELECT id, playlist_id, user_id, role_id FROM playlist_collaborators WHERE playlist_id = ?",
    )
    .bind(playlist_id)
    .fetch_all(db)
    .await?)
}

/// Whether `user_id` (with the given guild role IDs) may modify a locked
/// server playlist: the owner, a collaborator user, or a member of a
/// collaborator role.
pub fn has_permission(
    playlist: &Playlist,
    user_id: i64,
    role_ids: &[i64],
    collaborators: &[Collaborator],
) -> bool {
    if playlist.owner_id == user_id {
        return true;
    }
    collaborators
        .iter()
        .any(|c| c.user_id == Some(user_id) || c.role_id.is_some_and(|rid| role_ids.contains(&rid)))
}

/// Toggles a server playlist's lock, returning the new state.
pub async fn toggle_lock(
    db: &SqlitePool,
    playlist_id: i64,
    currently_locked: bool,
) -> Result<bool, Error> {
    let new_state = !currently_locked;
    sqlx::query("UPDATE playlists SET locked = ? WHERE id = ?")
        .bind(new_state)
        .bind(playlist_id)
        .execute(db)
        .await?;
    Ok(new_state)
}

/// Adds a user collaborator. Returns `false` if they already were one.
pub async fn add_collaborator_user(
    db: &SqlitePool,
    playlist_id: i64,
    user_id: i64,
) -> Result<bool, Error> {
    let existing = collaborators(db, playlist_id).await?;
    if existing.iter().any(|c| c.user_id == Some(user_id)) {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO playlist_collaborators (playlist_id, user_id, role_id) VALUES (?, ?, NULL)",
    )
    .bind(playlist_id)
    .bind(user_id)
    .execute(db)
    .await?;
    Ok(true)
}

/// Removes a user collaborator. Returns `false` if they weren't one.
pub async fn remove_collaborator_user(
    db: &SqlitePool,
    playlist_id: i64,
    user_id: i64,
) -> Result<bool, Error> {
    let result =
        sqlx::query("DELETE FROM playlist_collaborators WHERE playlist_id = ? AND user_id = ?")
            .bind(playlist_id)
            .bind(user_id)
            .execute(db)
            .await?;
    Ok(result.rows_affected() > 0)
}

/// Adds a role collaborator. Returns `false` if it already was one.
pub async fn add_collaborator_role(
    db: &SqlitePool,
    playlist_id: i64,
    role_id: i64,
) -> Result<bool, Error> {
    let existing = collaborators(db, playlist_id).await?;
    if existing.iter().any(|c| c.role_id == Some(role_id)) {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO playlist_collaborators (playlist_id, user_id, role_id) VALUES (?, NULL, ?)",
    )
    .bind(playlist_id)
    .bind(role_id)
    .execute(db)
    .await?;
    Ok(true)
}

/// Removes a role collaborator. Returns `false` if it wasn't one.
pub async fn remove_collaborator_role(
    db: &SqlitePool,
    playlist_id: i64,
    role_id: i64,
) -> Result<bool, Error> {
    let result =
        sqlx::query("DELETE FROM playlist_collaborators WHERE playlist_id = ? AND role_id = ?")
            .bind(playlist_id)
            .bind(role_id)
            .execute(db)
            .await?;
    Ok(result.rows_affected() > 0)
}

/// Transfers ownership of a server playlist, and drops the new owner from
/// the collaborator list if they were on it.
pub async fn transfer_ownership(
    db: &SqlitePool,
    playlist_id: i64,
    new_owner_id: i64,
) -> Result<(), Error> {
    let mut tx = db.begin().await?;
    sqlx::query("UPDATE playlists SET owner_id = ? WHERE id = ?")
        .bind(new_owner_id)
        .bind(playlist_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM playlist_collaborators WHERE playlist_id = ? AND user_id = ?")
        .bind(playlist_id)
        .bind(new_owner_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
