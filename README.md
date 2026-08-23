
<div align="center">

# y4Music-rs

![Language](https://img.shields.io/badge/language-Rust-orange.svg)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)
[![Docs](https://img.shields.io/badge/docs-00__overview.md-blueviolet.svg)](docs/00_overview.md)
[![Version](https://img.shields.io/badge/version-0.1.0-blue.svg)](https://github.com/y0sh-dev/y4Music-rs/releases/latest)

<br>

**Lightweight, High-Fidelity. A self-hosted Discord Music Bot built with Rust.**<br>
*Engineered for minimal resource footprint and uncompromising stability.*

</div>

---

## Architecture Highlights

This bot adopts several aggressive approaches to eliminate the "state inconsistencies" and "network vulnerabilities" commonly found in typical Music Bot designs.

* **Direct FFmpeg Pipeline**
  Bypasses standard wrapper library streaming. `yt-dlp` is used solely to resolve direct CDN links, passing the URL directly to the `ffmpeg` process to handle HTTP communication. This leverages FFmpeg's native, robust network reconnect capabilities (`-reconnect`) while directly applying per-user EQ filters (Balanced / Hi-Fi) at the decoding stage, feeding raw PCM straight into the mixer.

* **Stateless Event-Driven UI**
  Holds absolutely no internal state flags like `is_playing` or `skip_requested`. The "Now Playing" panel listens exclusively to raw `Play` / `End` events fired from the deepest layer of the audio driver (`songbird`), rendering the actual queue state as-is. This fundamentally eradicates any desync between the UI and the actual playback state.

* **Silent Rate-Limit Protection**
  To combat the biggest enemy of Discord button UIs—API rate limits (429 errors) caused by rapid clicking—it implements a "silent cooldown." Instead of consuming API calls to send warning messages, it silently returns an empty Acknowledge and drops the background process. This keeps the chat clean and minimizes bot load.

## Prerequisites

* **Rust** (for building)
* **ffmpeg**
* **yt-dlp**
  *(Note: If you encounter playback errors such as 403 Forbidden due to YouTube's specification changes, using the **nightly build** of `yt-dlp` is strongly recommended.)*

## Setup & Tools

We provide scripts to simplify deployment for Linux environments.

**1. Installation & Daemonization**
After building, run the included setup script to automatically configure an FHS-compliant directory structure (`/etc/`, `/var/lib/`) and register the `systemd` service.
```bash
cargo build --release
sudo ./setup.sh
sudo systemctl enable --now y4music-rs
```

**2. Guild Commands Cleanup**
A tool to prevent duplicate command displays when migrating from a test environment (guild-specific sync) to a production environment (global sync).
```bash
sudo ./clear_guild_commands.sh
```

## Commands

| Command | Description |
|---|---|
| `/play`, `/playnext` | Adds a track from a URL to the queue (or interrupts as next) and plays it. |
| `/search` | Searches YouTube by keyword and lets you select a track from a menu. |
| `/seek` | Seeks the currently playing track to the specified seconds. |
| `/skip`, `/stop`, `/clear` | Skips the track, stops playback entirely, or clears the upcoming queue. |
| `/shuffle`, `/loop` | Shuffles the queue or toggles the loop mode (Off/Track/Queue). |
| `/nowplaying` | Brings the Now Playing panel back to the bottom of the chat. |
| `/profile <volume/eq/show>` | Sets your default volume and EQ mode (Balanced/Hi-Fi). |
| `/playlist_*` | Manages and plays personal playlists (SQLite backend). |
| `/serverplaylist_*` | Manages and plays server-shared playlists with permission controls. |

## Internals

For deeper insights into the design intentions behind the code—such as how seeking is achieved with a custom audio source, or Rust-specific asynchronous design patterns (deadlock avoidance, task self-cancellation issues)—please refer to the documentation in the `doc/` directory.

## License

This project is licensed under the MIT License. See the `LICENSE` file for details.
