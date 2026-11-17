-- Playlist differential sync: remember each playlist's upstream source and
-- distinguish mirrored tracks from tracks users added explicitly.
--
-- `source_url` is NULL until a playlist is linked to an external source.
-- `is_synced` defaults to 0 so every existing row (and every single-track /
-- search import) is treated as explicit and is never deleted by a sync.
ALTER TABLE playlists ADD COLUMN source_url TEXT;

ALTER TABLE playlist_tracks ADD COLUMN is_synced INTEGER NOT NULL DEFAULT 0;
