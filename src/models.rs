//! Plain data types shared across commands, mirroring the DB schema in
//! `migrations/0001_init.sql`.

use sqlx::FromRow;

/// A user's saved playback preferences. Mirrors the `users` table.
#[derive(Debug, Clone, FromRow)]
pub struct UserProfile {
    pub user_id: i64,
    pub default_volume: i64,
    pub default_eq_mode: String,
}

impl UserProfile {
    /// The settings a brand-new user gets before they've saved anything.
    pub fn default_for(user_id: i64) -> Self {
        Self {
            user_id,
            default_volume: 100,
            default_eq_mode: "balanced".to_string(),
        }
    }
}

/// A personal ("solo") or shared ("server") playlist -- a row in the
/// `playlists` table.
#[derive(Debug, Clone, FromRow)]
#[allow(dead_code)]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    pub owner_id: i64,
    pub guild_id: Option<i64>,
    pub scope: String,
    pub locked: bool,
}

/// A single track stored in a playlist.
#[derive(Debug, Clone, FromRow)]
#[allow(dead_code)]
pub struct PlaylistTrack {
    pub id: i64,
    pub playlist_id: i64,
    pub url: String,
    pub title: String,
    pub uploader: Option<String>,
    pub duration: i64,
    pub added_order: i64,
}

/// A user or role granted write access to a locked server playlist.
/// Exactly one of `user_id` / `role_id` is set, per the table's CHECK
/// constraint.
#[derive(Debug, Clone, FromRow)]
#[allow(dead_code)]
pub struct Collaborator {
    pub id: i64,
    pub playlist_id: i64,
    pub user_id: Option<i64>,
    pub role_id: Option<i64>,
}
