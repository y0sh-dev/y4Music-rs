-- Fixes a gap in 0001's `UNIQUE (owner_id, guild_id, scope, name)`: SQLite
-- treats every NULL as distinct for UNIQUE purposes, and `guild_id` is
-- always NULL for solo playlists, so that constraint gave zero protection
-- against duplicate solo playlist names for the same owner. It also didn't
-- match `playlist::find`'s actual lookup key for server playlists (which
-- looks up by guild_id + name only, not owner_id + guild_id).
--
-- This expression index enforces uniqueness on the real lookup key: for
-- solo playlists that's (scope, owner_id, name); for server playlists it's
-- (scope, guild_id, name). COALESCE(guild_id, owner_id) picks whichever one
-- is actually meaningful for each scope.
CREATE UNIQUE INDEX IF NOT EXISTS idx_playlists_unique_name
ON playlists (scope, COALESCE(guild_id, owner_id), name);
