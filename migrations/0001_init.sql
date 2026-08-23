-- Phase 1: Foundation schema for y1Music-Bot (Rust / SQLite)
-- Mirrors the design in section 6.4 of the replacement design doc.

-- Per-user settings, loaded automatically when a user starts a session
-- (replaces utils/profile_manager.py's JSON-file-per-user storage).
CREATE TABLE IF NOT EXISTS users (
    user_id         INTEGER PRIMARY KEY,   -- Discord user snowflake
    default_volume  INTEGER NOT NULL DEFAULT 100,       -- 0-200 (%)
    default_eq_mode TEXT    NOT NULL DEFAULT 'balanced' -- 'balanced' | 'hifi'
);

-- Personal ("solo") and shared ("server") playlists.
-- Replaces utils/playlist_manager.py's per-owner JSON files.
CREATE TABLE IF NOT EXISTS playlists (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT    NOT NULL,
    owner_id    INTEGER NOT NULL,          -- Discord user snowflake (creator / owner)
    guild_id    INTEGER,                   -- NULL for solo playlists; guild snowflake for server playlists
    scope       TEXT    NOT NULL CHECK (scope IN ('solo', 'server')),
    locked      INTEGER NOT NULL DEFAULT 0, -- boolean: 0 = unlocked, 1 = locked (server scope only)
    UNIQUE (owner_id, guild_id, scope, name)
);

CREATE INDEX IF NOT EXISTS idx_playlists_owner ON playlists (owner_id, scope);
CREATE INDEX IF NOT EXISTS idx_playlists_guild ON playlists (guild_id, scope);

-- Tracks belonging to a playlist, in insertion order.
CREATE TABLE IF NOT EXISTS playlist_tracks (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    playlist_id    INTEGER NOT NULL REFERENCES playlists (id) ON DELETE CASCADE,
    url            TEXT    NOT NULL,
    title          TEXT    NOT NULL,
    uploader       TEXT,
    duration       INTEGER NOT NULL DEFAULT 0, -- seconds
    added_order    INTEGER NOT NULL           -- explicit ordering, since tracks can be reordered
);

CREATE INDEX IF NOT EXISTS idx_playlist_tracks_playlist ON playlist_tracks (playlist_id, added_order);

-- Collaborators (users or roles) granted write access to a locked server playlist.
-- Replaces the collaborator_users / collaborator_roles JSON arrays.
CREATE TABLE IF NOT EXISTS playlist_collaborators (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    playlist_id  INTEGER NOT NULL REFERENCES playlists (id) ON DELETE CASCADE,
    user_id      INTEGER, -- set for a user collaborator
    role_id      INTEGER, -- set for a role collaborator
    CHECK ((user_id IS NOT NULL) <> (role_id IS NOT NULL)), -- exactly one of the two is set
    UNIQUE (playlist_id, user_id, role_id)
);

CREATE INDEX IF NOT EXISTS idx_playlist_collaborators_playlist ON playlist_collaborators (playlist_id);
