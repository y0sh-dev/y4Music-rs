//! Playlist CRUD and permissions, backed by SQLite.

use sqlx::{Sqlite, SqlitePool, Transaction};

use std::collections::HashSet;

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
                "SELECT id, name, owner_id, guild_id, scope, locked, source_url FROM playlists \
             WHERE scope = 'solo' AND owner_id = ? AND name = ?",
            )
            .bind(lookup_id)
            .bind(name)
            .fetch_optional(db)
            .await?
        }
        Scope::Server => {
            sqlx::query_as::<_, Playlist>(
                "SELECT id, name, owner_id, guild_id, scope, locked, source_url FROM playlists \
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
                "SELECT id, name, owner_id, guild_id, scope, locked, source_url FROM playlists \
             WHERE scope = 'solo' AND owner_id = ? ORDER BY name",
            )
            .bind(lookup_id)
            .fetch_all(db)
            .await?
        }
        Scope::Server => {
            sqlx::query_as::<_, Playlist>(
                "SELECT id, name, owner_id, guild_id, scope, locked, source_url FROM playlists \
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
        "SELECT id, playlist_id, url, title, uploader, duration, added_order, is_synced \
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

/// Outcome of a differential sync (`sync_tracks_from_stream`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncResult {
    /// Upstream tracks newly inserted as synced.
    pub added: usize,
    /// Stale synced rows removed because they are no longer upstream.
    pub removed: usize,
    /// Explicit (`is_synced = 0`) rows left untouched by the sync.
    pub preserved_explicit: usize,
    /// Whether the upstream listing was cut off at `MAX_BULK_ADD`.
    pub truncated: bool,
}

/// Heuristic for "this import targets a whole external playlist rather than
/// a single video or a search term".
///
/// Only `http(s)` URLs qualify, so a plain search phrase can never be
/// mistaken for a playlist. Recognised shapes cover YouTube (`list=`),
/// Spotify/YouTube-style `/playlist/` paths, SoundCloud `/sets/`, and
/// album-style `/album/` paths. A single-video URL carries none of these
/// markers and stays explicit.
pub fn is_playlist_url(query: &str) -> bool {
    let trimmed = query.trim();
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    lower.contains("list=")
        || lower.contains("/playlist/")
        || lower.contains("/playlist?")
        || lower.contains("/sets/")
        || lower.contains("/album/")
}

/// Comparison key for upstream source URLs: trimmed, with a single trailing
/// slash removed so `.../playlist?list=X` and `.../playlist?list=X/` compare
/// equal. Anything beyond that stays exact -- rewriting query parameters
/// risks conflating genuinely different playlists.
fn normalize_source_url(url: &str) -> &str {
    url.trim().trim_end_matches('/')
}

/// Whether two upstream playlist URLs identify the same source.
pub fn same_source_url(a: &str, b: &str) -> bool {
    normalize_source_url(a) == normalize_source_url(b)
}

/// Records (or clears) the upstream URL a playlist mirrors.
pub async fn set_source_url(
    db: &SqlitePool,
    playlist_id: i64,
    source_url: Option<&str>,
) -> Result<(), Error> {
    sqlx::query("UPDATE playlists SET source_url = ? WHERE id = ?")
        .bind(source_url)
        .bind(playlist_id)
        .execute(db)
        .await?;
    Ok(())
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
/// `is_synced` marks the inserted rows as belonging to the playlist's
/// mirrored source (so a later sync may delete them). Synced imports skip
/// URLs already present in the playlist, avoiding a duplicate when the
/// user had already added the same track explicitly.
///
/// Does not itself check permissions -- callers check `has_permission`
/// first.
pub async fn import_tracks_from_stream(
    db: &SqlitePool,
    playlist_id: i64,
    stream: &mut TrackStream,
    is_synced: bool,
) -> Result<ImportResult, Error> {
    // Receive phase -- network-bound, no DB lock held.
    let mut received = receive_tracks(stream).await?;

    if is_synced {
        let existing: Vec<String> =
            sqlx::query_scalar("SELECT url FROM playlist_tracks WHERE playlist_id = ?")
                .bind(playlist_id)
                .fetch_all(db)
                .await?;
        let mut present: HashSet<String> = existing.into_iter().collect();
        received
            .tracks
            .retain(|track| present.insert(track.webpage_url.clone()));
    }

    let imported = received.tracks.len();
    if imported == 0 {
        return Ok(ImportResult {
            imported: 0,
            truncated: received.truncated,
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

    for chunk in received.tracks.chunks(IMPORT_CHUNK_SIZE) {
        insert_chunk(&mut tx, playlist_id, chunk, next_order, is_synced).await?;
        next_order += chunk.len() as i64;
    }

    tx.commit().await?;
    Ok(ImportResult {
        imported,
        truncated: received.truncated,
    })
}

/// Tracks collected from one complete (or deliberately truncated) receive
/// pass, before any DB write happens.
struct ReceivedTracks {
    tracks: Vec<TrackInfo>,
    truncated: bool,
}

/// Receive phase shared by imports and syncs: drain the network stream up
/// to `MAX_BULK_ADD`, without holding any DB lock.
async fn receive_tracks(stream: &mut TrackStream) -> Result<ReceivedTracks, Error> {
    let mut tracks: Vec<TrackInfo> = Vec::new();
    let mut truncated = false;
    loop {
        if tracks.len() >= MAX_BULK_ADD {
            // Peek one more to tell "stream ended right at the cap" apart
            // from "there was more we chose not to read".
            truncated = stream.next_track().await?.is_some();
            break;
        }
        match stream.next_track().await? {
            Some(track) => tracks.push(track),
            None => break,
        }
    }
    Ok(ReceivedTracks { tracks, truncated })
}

/// Differential sync: mirror `playlist_id` onto an external playlist while
/// preserving every explicitly added track.
///
/// Receive and write phases stay strictly separate (see
/// `import_tracks_from_stream`). A stream error aborts before any write, and
/// a non-zero `yt-dlp` exit is surfaced *before* opening the transaction, so
/// a failed or partial fetch can never wipe synced tracks. When the listing
/// itself was truncated at `MAX_BULK_ADD`, stale synced rows are left alone
/// too: their absence from the partial upstream view does not prove they
/// were removed upstream.
pub async fn sync_tracks_from_stream(
    db: &SqlitePool,
    playlist_id: i64,
    mut stream: TrackStream,
) -> Result<SyncResult, Error> {
    let received = receive_tracks(&mut stream).await?;
    // A fully drained stream is waited on so a non-zero `yt-dlp` exit is
    // caught here, before the write phase. A truncated stream is abandoned
    // instead (its `Drop` kills the child).
    let finish = if received.truncated {
        None
    } else {
        Some(stream.finish().await)
    };
    commit_sync(
        db,
        playlist_id,
        &received.tracks,
        received.truncated,
        finish,
    )
    .await
}

/// Last line of defence for the sync pipeline: reject an incomplete fetch
/// before touching the playlist, then apply the write phase.
async fn commit_sync(
    db: &SqlitePool,
    playlist_id: i64,
    upstream: &[TrackInfo],
    truncated: bool,
    finish: Option<Result<(), Error>>,
) -> Result<SyncResult, Error> {
    if let Some(finish) = finish {
        finish?;
    }
    apply_sync(db, playlist_id, upstream, truncated).await
}

/// Write phase of a sync, in one transaction:
///
/// - delete `is_synced = 1` rows whose URL is absent upstream (never when
///   `truncated`, and never `is_synced = 0` rows);
/// - insert upstream URLs that are not already present (under either flag)
///   as `is_synced = 1`, in upstream order, appended after the current
///   maximum `added_order`.
///
/// Existing rows keep their `added_order` (explicit tracks are therefore
/// untouched, including their position); only newly inserted tracks get
/// sequential orders. Upstream duplicates collapse to their first
/// occurrence.
async fn apply_sync(
    db: &SqlitePool,
    playlist_id: i64,
    upstream: &[TrackInfo],
    truncated: bool,
) -> Result<SyncResult, Error> {
    let mut seen = HashSet::new();
    let upstream_unique: Vec<&TrackInfo> = upstream
        .iter()
        .filter(|track| seen.insert(track.webpage_url.as_str()))
        .collect();

    let mut tx = db.begin().await?;
    let existing: Vec<(i64, String, bool)> = sqlx::query_as(
        "SELECT id, url, is_synced FROM playlist_tracks WHERE playlist_id = ? ORDER BY added_order",
    )
    .bind(playlist_id)
    .fetch_all(&mut *tx)
    .await?;

    let mut removed = 0_usize;
    if !truncated {
        let upstream_urls: HashSet<&str> = upstream_unique
            .iter()
            .map(|track| track.webpage_url.as_str())
            .collect();
        for (id, url, is_synced) in &existing {
            if *is_synced && !upstream_urls.contains(url.as_str()) {
                sqlx::query("DELETE FROM playlist_tracks WHERE id = ?")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                removed += 1;
            }
        }
    }

    let preserved_explicit = existing.iter().filter(|(_, _, synced)| !*synced).count();
    let mut present: HashSet<String> = existing.into_iter().map(|(_, url, _)| url).collect();
    let to_insert: Vec<TrackInfo> = upstream_unique
        .into_iter()
        .filter(|track| present.insert(track.webpage_url.clone()))
        .cloned()
        .collect();
    let added = to_insert.len();

    if added > 0 {
        let mut next_order: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(added_order) + 1, 0) FROM playlist_tracks WHERE playlist_id = ?",
        )
        .bind(playlist_id)
        .fetch_one(&mut *tx)
        .await?;

        for chunk in to_insert.chunks(IMPORT_CHUNK_SIZE) {
            insert_chunk(&mut tx, playlist_id, chunk, next_order, true).await?;
            next_order += chunk.len() as i64;
        }
    }

    tx.commit().await?;
    Ok(SyncResult {
        added,
        removed,
        preserved_explicit,
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
    is_synced: bool,
) -> Result<(), Error> {
    let mut builder = sqlx::QueryBuilder::new(
        "INSERT INTO playlist_tracks (playlist_id, url, title, uploader, duration, added_order, is_synced) ",
    );
    builder.push_values(chunk.iter().enumerate(), |mut row, (i, track)| {
        row.push_bind(playlist_id)
            .push_bind(&track.webpage_url)
            .push_bind(&track.title)
            .push_bind(&track.uploader)
            .push_bind(track.duration)
            .push_bind(start_order + i as i64)
            .push_bind(is_synced);
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
        "SELECT id, playlist_id, url, title, uploader, duration, added_order, is_synced \
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

#[cfg(test)]
mod tests {
    use sqlx::SqlitePool;
    use sqlx::sqlite::SqlitePoolOptions;

    use super::{SyncResult, apply_sync, commit_sync, has_permission};
    use crate::models::{Collaborator, Playlist};
    use crate::ytdlp::TrackInfo;

    async fn memory_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");
        pool
    }

    async fn seed_playlist(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO playlists (name, owner_id, guild_id, scope, locked) \
             VALUES ('test', 1, NULL, 'solo', 0) RETURNING id",
        )
        .fetch_one(pool)
        .await
        .expect("insert playlist")
    }

    async fn insert_track(
        pool: &SqlitePool,
        playlist_id: i64,
        url: &str,
        order: i64,
        synced: bool,
    ) {
        sqlx::query(
            "INSERT INTO playlist_tracks \
             (playlist_id, url, title, uploader, duration, added_order, is_synced) \
             VALUES (?, ?, ?, NULL, 0, ?, ?)",
        )
        .bind(playlist_id)
        .bind(url)
        .bind(format!("Title {url}"))
        .bind(order)
        .bind(synced)
        .execute(pool)
        .await
        .expect("insert track");
    }

    async fn rows(pool: &SqlitePool, playlist_id: i64) -> Vec<(String, bool)> {
        sqlx::query_as(
            "SELECT url, is_synced FROM playlist_tracks WHERE playlist_id = ? ORDER BY added_order",
        )
        .bind(playlist_id)
        .fetch_all(pool)
        .await
        .expect("fetch rows")
    }

    fn track(url: &str) -> TrackInfo {
        TrackInfo {
            title: format!("Title {url}"),
            uploader: None,
            duration: 0,
            webpage_url: url.to_string(),
            thumbnail: None,
        }
    }

    #[tokio::test]
    async fn sync_mirrors_upstream_and_preserves_explicit_tracks() {
        let pool = memory_pool().await;
        let playlist_id = seed_playlist(&pool).await;
        insert_track(&pool, playlist_id, "explicit", 0, false).await;
        insert_track(&pool, playlist_id, "stale", 1, true).await;
        insert_track(&pool, playlist_id, "survivor", 2, true).await;

        let result = apply_sync(
            &pool,
            playlist_id,
            &[track("survivor"), track("new")],
            false,
        )
        .await
        .expect("sync");

        assert_eq!(
            result,
            SyncResult {
                added: 1,
                removed: 1,
                preserved_explicit: 1,
                truncated: false,
            }
        );
        assert_eq!(
            rows(&pool, playlist_id).await,
            vec![
                ("explicit".to_string(), false),
                ("survivor".to_string(), true),
                ("new".to_string(), true),
            ]
        );
    }

    #[tokio::test]
    async fn sync_does_not_duplicate_an_existing_explicit_track() {
        let pool = memory_pool().await;
        let playlist_id = seed_playlist(&pool).await;
        insert_track(&pool, playlist_id, "same", 0, false).await;

        let result = apply_sync(&pool, playlist_id, &[track("same")], false)
            .await
            .expect("sync");

        assert_eq!(result.added, 0);
        assert_eq!(result.removed, 0);
        assert_eq!(result.preserved_explicit, 1);
        assert_eq!(
            rows(&pool, playlist_id).await,
            vec![("same".to_string(), false)]
        );
    }

    #[tokio::test]
    async fn failed_sync_leaves_existing_synced_tracks_intact() {
        let pool = memory_pool().await;
        let playlist_id = seed_playlist(&pool).await;
        insert_track(&pool, playlist_id, "synced", 0, true).await;
        insert_track(&pool, playlist_id, "explicit", 1, false).await;
        let before = rows(&pool, playlist_id).await;

        let result = commit_sync(
            &pool,
            playlist_id,
            &[],
            false,
            Some(Err("yt-dlp exited with an error".into())),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(rows(&pool, playlist_id).await, before);
    }

    #[tokio::test]
    async fn truncated_sync_adds_without_removing_unseen_tracks() {
        let pool = memory_pool().await;
        let playlist_id = seed_playlist(&pool).await;
        insert_track(&pool, playlist_id, "unseen", 0, true).await;

        let result = apply_sync(&pool, playlist_id, &[track("new")], true)
            .await
            .expect("sync");

        assert_eq!(result.added, 1);
        assert_eq!(result.removed, 0);
        assert_eq!(
            rows(&pool, playlist_id).await,
            vec![("unseen".to_string(), true), ("new".to_string(), true)]
        );
    }

    #[test]
    fn has_permission_accepts_owner_user_and_role_collaborators_only() {
        let playlist = Playlist {
            id: 1,
            name: "shared".to_string(),
            owner_id: 10,
            guild_id: Some(1),
            scope: "server".to_string(),
            locked: true,
            source_url: None,
        };
        let collaborators = vec![
            Collaborator {
                id: 1,
                playlist_id: 1,
                user_id: Some(20),
                role_id: None,
            },
            Collaborator {
                id: 2,
                playlist_id: 1,
                user_id: None,
                role_id: Some(30),
            },
        ];

        assert!(has_permission(&playlist, 10, &[], &collaborators), "owner");
        assert!(
            has_permission(&playlist, 20, &[], &collaborators),
            "user collaborator"
        );
        assert!(
            has_permission(&playlist, 21, &[30], &collaborators),
            "role collaborator"
        );
        assert!(
            !has_permission(&playlist, 21, &[31], &collaborators),
            "stranger"
        );
    }
}
