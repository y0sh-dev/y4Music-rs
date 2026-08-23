# 00. Overview and Architecture Map

This set of documents explains not "what you can understand by reading the code" of y4Music-rs, but rather "what is hard to understand even if you read it" — why this design was chosen, and how the constraints of Rust/songbird were bypassed. Please grasp the overall map in this file first, and then proceed to each file according to the topics you are interested in.

## Layer Architecture

This Bot operates by stacking four libraries.

```text
poise        … Slash command routing / argument parsing (macro-driven)
  ↓
songbird     … Discord voice connection / audio mixer / playback queue
  ↓
tokio        … Asynchronous runtime (all I/O runs on this)
  ↓
sqlx(SQLite) … Persistence of profiles / playlists
```

Both `poise` and `songbird` have independent event sources. `poise` fires slash commands (like `/play`), while `songbird` fires voice events (`TrackEvent::Play`/`End`, gateway `VoiceStateUpdate`) and button presses (`InteractionCreate::Component`). Since the latter is outside poise's command dispatch, `FrameworkOptions::event_handler` (`on_event`) in `main.rs` routes them manually. These "two lines of event sources" are the premise of the architecture and will be the main subject of `doc/03_event_driven_ui.md`.

## Module Dependency Map

```text
main.rs ─┬─ commands/ ─┬─ playback.rs   (/play /skip /seek ...)
         │             ├─ playlist.rs   (/playlist_*)
         │             ├─ server_playlist.rs (/serverplaylist_*)
         │             ├─ profile.rs    (/profile volume|eq)
         │             ├─ search.rs     (/search + pagination)
         │             └─ mod.rs        (List of command registrations)
         │
         ├─ player.rs      … Now Playing panel, per-guild playback state, seeking
         ├─ audio_source.rs … Custom audio source (FfmpegEqSource)
         ├─ eq.rs          … Structured definition of EQ filters (Balanced/Hi-Fi)
         ├─ playlist.rs    … Playlist persistence logic (DB operations)
         ├─ ytdlp.rs       … Metadata search for /search (yt-dlp -j)
         ├─ pagination.rs  … Prev/Next generic pagination
         ├─ db.rs          … SQLite connection / migration execution
         └─ models.rs      … Raw data types corresponding to DB rows
```

The direction of dependencies is generally top-down (`commands/*` calls `player.rs`/`audio_source.rs`/`playlist.rs`), with no circular dependencies. `player.rs` knows about `audio_source::FfmpegEqSource`, but conversely, `audio_source.rs` knows nothing about `player.rs` — the `Track` reconstruction logic after seeking (`player::seek_to`) is designed to assemble and pass the audio source side details (`eq_filter`/`seek_time`).

## Responsibilities of Each Module (One-liners)

| File | Responsibility |
|---|---|
| `main.rs` | Startup process, construction of `Data` (poise's shared state), routing of raw events |
| `player.rs` | Now Playing panel per guild, loop modes, auto-leave timers, seeking |
| `audio_source.rs` | URL resolution via `yt-dlp` → EQ application via `ffmpeg` → passing raw PCM to songbird |
| `eq.rs` | Defines Balanced/Hi-Fi EQ parameters as structs and generates `-af` strings |
| `commands/playback.rs` | Basic playback commands like `/join`, `/play`, `/skip`, `/seek` |
| `commands/playlist.rs` / `server_playlist.rs` | CRUD commands for personal and server-shared playlists |
| `playlist.rs` | Core DB operations for playlists (implementation of the thin wrappers above) |
| `commands/profile.rs` | Reading/writing default volume/EQ modes per user |
| `commands/search.rs` + `ytdlp.rs` | Keyword search and session management for `/search` |
| `pagination.rs` | Prev/Next pagination for long lists like playlist displays |
| `db.rs` / `models.rs` | SQLite connection and raw structs corresponding to the schema |

## What to Read Next

| What you want to know | File to read |
|---|---|
| How state is safely shared across guilds | `01_state_management.md` |
| Why `ffmpeg` is invoked manually, and how seeking is implemented | `02_audio_pipeline.md` |
| How the panel auto-updates and buttons work | `03_event_driven_ui.md` |
| Traps actually encountered in this code (especially task self-cancellation) | `04_pitfalls.md` |
| Structure of EQ settings and steps to add features | `05_eq_and_extending.md` |
