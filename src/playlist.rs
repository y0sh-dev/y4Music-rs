//! Playlist CRUD and permissions, backed by SQLite.

use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::Error;
use crate::models::{Collaborator, Playlist, PlaylistTrack};
use crate::ytdlp::{TrackInfo, TrackStream};

/// Ceiling on how many tracks a single `/playlist_add` /
/// `/serverplaylist_add` import may add, enforced while streaming (see
/// `import_tracks_from_stream`) rather than by resolving the whole source
/// up front.
pub const MAX_BULK_ADD: usize = 1000;

/// Rows buffered before each chunked bulk insert in
/// `import_tracks_from_stream`, so a single `INSERT`'s parameter count
/// stays bounded regardless of playlist size.
const IMPORT_CHUNK_SIZE: usize = 100;

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

/// Outcome of `import_tracks_from_stream`.
pub struct ImportResult {
    /// How many tracks were inserted.
    pub imported: usize,
    /// Whether the source had more tracks than `MAX_BULK_ADD` that were
    /// left unread once the cap was hit.
    pub truncated: bool,
}

/// Drains `stream` into `playlist_id`, in two phases kept strictly
/// separate so a slow network fetch never holds a DB write lock:
///
/// 1. **Receive phase**: no transaction is open. `stream.next_track()` is
///    polled in a loop (this is where the network I/O happens) and results
///    are collected into memory, up to `MAX_BULK_ADD`.
/// 2. **Write phase**: only once receiving is done does this open a
///    transaction, insert the collected tracks in `IMPORT_CHUNK_SIZE`-row
///    batches (via `sqlx::QueryBuilder`), and commit. With nothing left to
///    wait on, this phase is fast, so the write lock it holds is brief.
///
/// An earlier version opened the transaction before the receive loop,
/// which held SQLite's write lock for the entire (potentially
/// tens-of-seconds) network-bound import and starved other guilds'
/// unrelated DB operations with `SQLITE_BUSY`.
///
/// Does not itself check permissions -- callers check `has_permission`
/// first.
pub async fn import_tracks_from_stream(
    db: &SqlitePool,
    playlist_id: i64,
    stream: &mut TrackStream,
) -> Result<ImportResult, Error> {
    // Receive phase -- network-bound, no DB lock held.
    let mut buffer: Vec<TrackInfo> = Vec::new();
    let mut truncated = false;
    loop {
        if buffer.len() >= MAX_BULK_ADD {
            // Peek one more to tell "stream ended right at the cap" apart
            // from "there was more we chose not to read".
            truncated = stream.next_track().await?.is_some();
            break;
        }
        match stream.next_track().await? {
            Some(track) => buffer.push(track),
            None => break,
        }
    }

    let imported = buffer.len();
    if imported == 0 {
        return Ok(ImportResult {
            imported: 0,
            truncated,
        });
    }

    // Write phase -- DB-bound, held only as long as the chunked inserts take.
    let mut tx = db.begin().await?;
    let mut next_order: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(added_order) + 1, 0) FROM playlist_tracks WHERE playlist_id = ?",
    )
    .bind(playlist_id)
    .fetch_one(&mut *tx)
    .await?;

    for chunk in buffer.chunks(IMPORT_CHUNK_SIZE) {
        insert_chunk(&mut tx, playlist_id, chunk, next_order).await?;
        next_order += chunk.len() as i64;
    }

    tx.commit().await?;
    Ok(ImportResult {
        imported,
        truncated,
    })
}

/// Bulk-inserts one chunk of tracks via `QueryBuilder`, `added_order`
/// starting at `start_order`.
async fn insert_chunk(
    tx: &mut Transaction<'_, Sqlite>,
    playlist_id: i64,
    chunk: &[TrackInfo],
    start_order: i64,
) -> Result<(), Error> {
    let mut builder = sqlx::QueryBuilder::new(
        "INSERT INTO playlist_tracks (playlist_id, url, title, uploader, duration, added_order) ",
    );
    builder.push_values(chunk.iter().enumerate(), |mut row, (i, track)| {
        row.push_bind(playlist_id)
            .push_bind(&track.webpage_url)
            .push_bind(&track.title)
            .push_bind(&track.uploader)
            .push_bind(track.duration)
            .push_bind(start_order + i as i64);
    });
    builder.build().execute(&mut **tx).await?;
    Ok(())
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

    let mut tx = db.begin().await?;

    let track = sqlx::query_as::<_, PlaylistTrack>(
        "SELECT id, playlist_id, url, title, uploader, duration, added_order \
         FROM playlist_tracks WHERE playlist_id = ? ORDER BY added_order LIMIT 1 OFFSET ?",
    )
    .bind(playlist_id)
    .bind(index_1based - 1)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some(track) = &track {
        sqlx::query("DELETE FROM playlist_tracks WHERE id = ?")
            .bind(track.id)
            .execute(&mut *tx)
            .await?;

        // Close the gap left by the deleted row so `added_order` stays a
        // dense, gapless sequence.
        sqlx::query(
            "UPDATE playlist_tracks SET added_order = added_order - 1 \
             WHERE playlist_id = ? AND added_order > ?",
        )
        .bind(playlist_id)
        .bind(track.added_order)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
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

/// Atomically toggles a server playlist's lock and returns the new state.
/// The flip happens in SQL (`NOT locked`) rather than being computed from a
/// value read earlier, so two concurrent toggles can't race each other into
/// the same end state.
pub async fn toggle_lock(db: &SqlitePool, playlist_id: i64) -> Result<bool, Error> {
    let new_state: bool = sqlx::query_scalar(
        "UPDATE playlists SET locked = NOT locked WHERE id = ? RETURNING locked",
    )
    .bind(playlist_id)
    .fetch_one(db)
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
