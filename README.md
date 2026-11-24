<div align="center">

# y4Music-rs

![Language](https://img.shields.io/badge/language-Rust-orange.svg)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)
[![Docs](https://img.shields.io/badge/docs-00__overview.md-blueviolet.svg)](docs/00_overview.md)
[![Version](https://img.shields.io/badge/version-0.3.0-blue.svg)](https://github.com/y0sh-dev/y4Music-rs/releases/latest)

<br>

**Lightweight, High-Fidelity. A self-hosted Discord Music Bot built with Rust.**<br>
*Engineered for minimal resource footprint and uncompromising stability.*

</div>

---

## Project Vision & Non-Goals

`y4Music-rs` is built to be a **practical, low-overhead self-hosted Discord Music Bot** for small-to-medium communities—focused on reliable playback, responsive UI, clean audio, and effortless playlist reuse.

* **What it IS**:
  * A single-binary Rust service using external `yt-dlp` and `ffmpeg` for resilient media resolution and decoding.
  * Predictable playback with per-user volume/EQ profiles and SQLite-backed personal & server playlists.
* **Non-Goals** *(kept out of scope to avoid turning the bot into an experimental lab)*:
  * Multi-tenant SaaS scaling, sharding orchestration, or web dashboards.
  * Audio-toy bloat (real-time visualizers, dozens of gimmick effects, or over-engineered micro-module splits).
  * In-process lossy codec/container implementations (handled by `ffmpeg`).

---

## Features

### Implemented 
* **Playback & Queue Control**: URL/search playback, `/playnext` priority insertion, seeking (`/seek` & panel buttons), `Off` / `Track` / `Queue` loop modes, and queue-tail shuffle.
* **Stateless Event-Driven UI**: Now Playing panel driven by `songbird` `Play` / `End` events with silent rate-limit cooldowns and 404-safe message tracking.
* **Per-User Audio Profiles**: Persistent default volume (0–200%) and EQ mode (`Balanced` / `Hi-Fi` Proportional-Q + lookahead `compand` + `loudnorm`) via SQLite.
* **Personal & Server Playlists**: Streaming playlist import via `yt-dlp`, track reordering/removal, server-shared playlists with lock & user/role collaborator permissions, and differential mirror sync from a linked external playlist URL (preserving explicitly added tracks).
* **Voice Lifecycle Hygiene**: Auto-leave when the voice channel becomes empty or the queue stays idle, and auto-pause/resume when all listeners are deafened.

### Planned 
* **Post-Dependency-Trim Verification & Fixes**: Any stability or regression fixes surfaced during `0.3.x` soak testing.

---

## Architecture Highlights

* **Direct FFmpeg Pipeline**: `yt-dlp` resolves direct CDN links and passes them to `ffmpeg` (`-reconnect`, EQ filtergraph, `f32le` 48 kHz stereo PCM) feeding `songbird`'s `RawAdapter` (`symphonia` `pcm` only).
* **Stateless Event-Driven UI**: Holds no `is_playing` or `skip_requested` flags; renders directly from `songbird` queue state and track events.
* **Silent Rate-Limit Protection**: Rapid button clicks within the cooldown window return an empty Acknowledge without spamming ephemeral replies.

---

## Prerequisites & Configuration

* **Rust** (2024 edition, for building)
* **ffmpeg** (on `PATH`)
* **yt-dlp** *(Note: If you encounter `403 Forbidden` due to YouTube changes, using the **nightly build** of `yt-dlp` is strongly recommended.)*

Copy `.env.example` to `.env` (or `/etc/y4music-rs/.env` for systemd deployments) and configure:

| Variable | Required | Description |
|---|---|---|
| `DISCORD_TOKEN` | Yes | Discord bot token from the Developer Portal. |
| `DATABASE_URL` | No | SQLite connection URL (default: `sqlite:///var/lib/y4music-rs/data.db`; `.env.example` uses `sqlite://data/y1music.db`). |
| `TEST_GUILD_ID` | No | Guild ID for instant slash-command sync during testing. |
| `YTDLP_EXTRA_ARGS` | No | Space-separated extra flags forwarded to `yt-dlp`. |
| `EQ_HIFI_FILTER` | No | Raw ffmpeg `-af` filtergraph overriding the built-in Hi-Fi profile. |

---

## Setup & Tools

Deployment scripts are provided for Linux environments:

**1. Installation & Daemonization**
```bash
cargo build --release
sudo ./setup.sh
sudo systemctl enable --now y4music-rs
```

**2. Guild Commands Cleanup**
Removes duplicate guild-scoped commands when migrating from a test guild to global sync:
```bash
sudo ./clear_guild_commands.sh
```

---

## Commands

| Command | Description |
|---|---|
| `/join`, `/leave` | Joins your current voice channel or leaves and cleans up guild state. |
| `/play`, `/playnext` | Enqueues a track/playlist from a URL or search query (or inserts as next). |
| `/search` | Searches YouTube by keyword and lets you pick a track from a dropdown menu. |
| `/seek` | Seeks the current track to the specified position (in seconds). |
| `/skip`, `/stop`, `/clear` | Skips the current track, stops playback, or clears upcoming tracks. |
| `/shuffle`, `/loop` | Shuffles the upcoming queue tail or cycles loop mode (`Off` / `Track` / `Queue`). |
| `/queue`, `/nowplaying` | Displays the paginated queue or reposts the Now Playing panel at the bottom. |
| `/profile <show\|volume\|eq>` | Views or updates your default volume and EQ mode (`Balanced` / `Hi-Fi`). |
| `/playlist_*` | Manages and plays personal playlists (`create`, `add`, `sync`, `play`, `show`, `remove_track`, `move`, `rename`, `delete`). |
| `/serverplaylist_*` | Manages and plays server-shared playlists (`create`, `add`, `sync`, `play`, `show`, `remove_track`, `move`, `rename`, `delete`, `lock`, `add_user`, `remove_user`, `add_role`, `remove_role`, `transfer`). |

---

## Release Cadence & Versioning

`y4Music-rs` follows a structured `0.N.x` → `0.(N+1).0` development cycle:

* **`0.N.0`**: Official release baseline.
* **`0.N.1` – `0.N.4`**: Feature additions and planned improvements.
* **`0.N.5` – `0.N.8`**: Stabilization, regression fixes, and bug fixes for issues found through `.N.4` (complex items are deferred to `0.(N+1).0`).
* **`0.N.9`**: Documentation pass and release preparation.
* **`0.(N+1).0`**: Next official release.

---

## Internals

For design notes on seeking with a custom `Compose` source, player module layering, DSP filtergraphs, and Rust async lock discipline, see the [docs/](docs/00_overview.md) directory.

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file for details.
